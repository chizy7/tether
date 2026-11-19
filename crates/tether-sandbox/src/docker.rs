//! Docker backend: one long-lived container per sandbox, driven through the
//! `docker` command line.
//!
//! The workspace is bind-mounted at `/workspace` and is the only part of the
//! host the container sees. Commands run as the host user with every
//! capability dropped. `Network::None` gives the container no network
//! namespace beyond loopback; `Network::All` puts it on the default bridge.
//! Limits become container limits. File reads and writes go through the
//! container too, so a symlink planted in the workspace resolves inside it
//! and cannot point the host at its own files.
//!
//! What this does not isolate is the kernel, which the container shares with
//! the host.
//!
//! The image needs `sh`, `sleep`, `timeout`, `cat`, `mkdir` and `dirname`,
//! which every common base image has.

use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{
    inside, Exec, ExecResult, Handle, Network, Result, Sandbox, SandboxError, SandboxSpec,
    SnapshotId,
};

pub const DEFAULT_IMAGE: &str = "debian:stable-slim";

/// Where the workspace is mounted in the container.
const WORKSPACE: &str = "/workspace";

/// `timeout -s KILL` ends a command with this status.
const KILLED: i32 = 137;

struct Session {
    container: String,
    time_limit: Option<Duration>,
}

pub struct DockerSandbox {
    image: String,
    sessions: Mutex<Vec<Session>>,
}

/// Container names are per process, not per sandbox: two sandboxes in one
/// process must not pick the same name.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

impl Default for DockerSandbox {
    fn default() -> Self {
        Self::with_image(DEFAULT_IMAGE)
    }
}

fn failed(what: &str, output: &Output) -> SandboxError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    SandboxError::Other(format!("docker {what}: {}", stderr.trim()))
}

/// Run `docker` with `args`, feeding it `stdin` if given.
async fn docker(args: &[String], stdin: Option<Vec<u8>>) -> Result<Output> {
    let mut command = Command::new("docker");
    command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| SandboxError::Other(format!("cannot run docker: {e}")))?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A command that exits without reading its input is not an error.
        let _ = pipe.write_all(&input).await;
    }
    Ok(child.wait_with_output().await?)
}

fn args<const N: usize>(fixed: [&str; N]) -> Vec<String> {
    fixed.iter().map(|arg| (*arg).to_owned()).collect()
}

impl DockerSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start containers from `image`: a tag, or the id a snapshot returned.
    pub fn with_image(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            sessions: Mutex::default(),
        }
    }

    fn sessions(&self) -> MutexGuard<'_, Vec<Session>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn time_limit(&self, h: &Handle) -> Result<Option<Duration>> {
        self.sessions()
            .iter()
            .find(|s| s.container == h.0)
            .map(|s| s.time_limit)
            .ok_or_else(|| SandboxError::Other(format!("no such sandbox: {}", h.0)))
    }

    /// A relative path, checked, as a string for the command line.
    fn path(path: &Path) -> Result<String> {
        inside(path)?
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| SandboxError::Other("path is not valid UTF-8".into()))
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    async fn start(&self, spec: &SandboxSpec) -> Result<Handle> {
        let network = match &spec.network {
            Network::None => "none",
            Network::All => "bridge",
            Network::Allowlist(_) => {
                return Err(SandboxError::Unsupported(
                    "a network allowlist; the docker backend can only allow or block all of it",
                ));
            }
        };
        tokio::fs::create_dir_all(&spec.workspace).await?;
        let workspace = tokio::fs::canonicalize(&spec.workspace).await?;
        let workspace = workspace
            .to_str()
            .filter(|path| !path.contains(','))
            .ok_or_else(|| {
                SandboxError::Other("workspace path must be UTF-8 without commas".into())
            })?;

        let container = format!(
            "tether-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::SeqCst)
        );
        let mut run = args([
            "run",
            "--detach",
            "--init",
            "--label",
            "tether=sandbox",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--env",
            "HOME=/tmp",
            "--workdir",
            WORKSPACE,
        ]);
        let mut flag = |name: &str, value: String| run.extend([name.to_owned(), value]);
        flag("--name", container.clone());
        flag("--network", network.to_owned());
        flag(
            "--mount",
            format!("type=bind,source={workspace},target={WORKSPACE}"),
        );
        // The host user, so files the container writes into the workspace
        // belong to whoever is running Tether.
        #[cfg(unix)]
        {
            // SAFETY: these calls cannot fail and touch no memory.
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            flag("--user", format!("{uid}:{gid}"));
        }
        if let Some(millis) = spec.cpu_millis {
            flag("--cpus", format!("{:.3}", f64::from(millis) / 1000.0));
        }
        if let Some(bytes) = spec.memory_bytes {
            flag("--memory", bytes.to_string());
            // Same value for memory plus swap, which leaves no swap.
            flag("--memory-swap", bytes.to_string());
        }
        if let Some(secs) = spec.cpu_time_secs {
            flag("--ulimit", format!("cpu={secs}:{secs}"));
        }
        if let Some(files) = spec.max_open_files {
            flag("--ulimit", format!("nofile={files}:{files}"));
        }
        run.extend([self.image.clone(), "sleep".into(), "infinity".into()]);

        let output = docker(&run, None).await?;
        if !output.status.success() {
            // A failed run can still leave a created container behind.
            let _ = docker(&args(["rm", "--force", &container]), None).await;
            return Err(failed("run", &output));
        }
        self.sessions().push(Session {
            container: container.clone(),
            time_limit: spec.time_limit_secs.map(Duration::from_secs),
        });
        Ok(Handle(container))
    }

    async fn exec(&self, h: &Handle, cmd: Exec) -> Result<ExecResult> {
        let time_limit = self.time_limit(h)?;
        if cmd.argv.is_empty() {
            return Err(SandboxError::Other("empty argv".into()));
        }
        let cwd = match &cmd.cwd {
            Some(dir) => format!("{WORKSPACE}/{}", Self::path(dir)?),
            None => WORKSPACE.to_owned(),
        };

        let mut exec = args(["exec", "--interactive", "--workdir", &cwd]);
        for (key, value) in &cmd.env {
            exec.extend(["--env".to_owned(), format!("{key}={value}")]);
        }
        exec.push(h.0.clone());
        // The time limit is enforced inside the container, where the kill
        // reaches the command. Killing the docker client would not.
        if let Some(limit) = time_limit {
            exec.extend(args([
                "timeout",
                "-s",
                "KILL",
                &limit.as_secs().to_string(),
            ]));
        }
        exec.extend(cmd.argv);

        let timed_out =
            |limit: Duration| SandboxError::Other(format!("timed out after {} s", limit.as_secs()));
        let started = Instant::now();
        let output = match time_limit {
            // A margin on the outside, in case the container itself hangs.
            Some(limit) => {
                let outer = limit + Duration::from_secs(10);
                tokio::time::timeout(outer, docker(&exec, cmd.stdin))
                    .await
                    .map_err(|_| timed_out(limit))??
            }
            None => docker(&exec, cmd.stdin).await?,
        };
        let exit_code = output.status.code().unwrap_or(-1);
        if let Some(limit) = time_limit {
            if exit_code == KILLED && started.elapsed() >= limit {
                return Err(timed_out(limit));
            }
        }
        Ok(ExecResult {
            exit_code,
            stdout: output.stdout,
            stderr: output.stderr,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn read(&self, h: &Handle, path: &Path) -> Result<Vec<u8>> {
        self.time_limit(h)?;
        let path = Self::path(path)?;
        let output = docker(&args(["exec", &h.0, "cat", "--", &path]), None).await?;
        if !output.status.success() {
            return Err(failed("read", &output));
        }
        Ok(output.stdout)
    }

    async fn write(&self, h: &Handle, path: &Path, data: Vec<u8>) -> Result<()> {
        self.time_limit(h)?;
        let path = Self::path(path)?;
        // The path is passed as an argument, never spliced into the script.
        let script = r#"mkdir -p -- "$(dirname -- "$1")" && cat > "$1""#;
        let write = args([
            "exec",
            "--interactive",
            &h.0,
            "sh",
            "-c",
            script,
            "sh",
            &path,
        ]);
        let output = docker(&write, Some(data)).await?;
        if !output.status.success() {
            return Err(failed("write", &output));
        }
        Ok(())
    }

    /// Commits the container to an image and returns the image id. The
    /// snapshot holds the container's own filesystem. It does not hold the
    /// workspace, which is a mount and stays on the host. Start from it with
    /// [`DockerSandbox::with_image`].
    async fn snapshot(&self, h: &Handle) -> Result<SnapshotId> {
        self.time_limit(h)?;
        let output = docker(&args(["commit", &h.0]), None).await?;
        if !output.status.success() {
            return Err(failed("commit", &output));
        }
        Ok(SnapshotId(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ))
    }

    /// Removes the container. The workspace is left in place.
    async fn stop(&self, h: Handle) -> Result<()> {
        self.sessions().retain(|s| s.container != h.0);
        let output = docker(&args(["rm", "--force", &h.0]), None).await?;
        if !output.status.success() {
            return Err(failed("rm", &output));
        }
        Ok(())
    }
}

impl Drop for DockerSandbox {
    /// Containers that were never stopped would run forever. Remove them
    /// without waiting.
    fn drop(&mut self) {
        for session in self.sessions().drain(..) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "--force", &session.container])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
}
