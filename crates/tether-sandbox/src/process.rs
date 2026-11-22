//! Default backend: runs commands as plain child processes.
//!
//! **Not a security boundary.** A command runs as the user running Tether and
//! can reach whatever that user can. What this backend does is narrow the
//! damage an ordinary mistake can do:
//!
//! - every command starts in the workspace, with a cleared environment (only
//!   `PATH` passes through), and `read` and `write` refuse paths that leave
//!   the workspace;
//! - a command past `time_limit_secs` is killed, with everything it started;
//! - on Unix, `cpu_time_secs` and `max_open_files` are set as rlimits;
//! - on Linux, `memory_bytes` is set as an address-space rlimit, a seccomp
//!   filter refuses a list of system calls (see `seccomp.rs`), setuid
//!   programs cannot gain privileges, and `Network::None` blocks internet
//!   sockets.
//!
//! What a platform cannot enforce is refused at `start` rather than silently
//! ignored: `memory_bytes` off Linux, and `Network::Allowlist` everywhere.
//! The one exception is `Network::None` off Linux, which is not enforced.
//! `cpu_millis` is a CPU share for the container backends and is ignored.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::{
    inside, Exec, ExecResult, Handle, Network, Result, Sandbox, SandboxError, SandboxSpec,
    SnapshotId,
};

/// What every command in one sandbox is held to.
struct Session {
    workspace: PathBuf,
    time_limit: Option<Duration>,
    limits: Limits,
    #[cfg(target_os = "linux")]
    filter: seccompiler::BpfProgram,
}

/// rlimits, applied in the child between fork and exec.
#[derive(Debug, Clone, Copy, Default)]
struct Limits {
    cpu_time_secs: Option<u64>,
    max_open_files: Option<u64>,
    /// `start` refuses a memory limit on every other platform.
    #[cfg(target_os = "linux")]
    memory_bytes: Option<u64>,
}

#[derive(Default)]
pub struct ProcessSandbox {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    next_id: AtomicU64,
}

impl ProcessSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn session(&self, h: &Handle) -> Result<Arc<Session>> {
        self.sessions()
            .get(&h.0)
            .cloned()
            .ok_or_else(|| SandboxError::Other(format!("no such sandbox: {}", h.0)))
    }

    fn resolve(&self, h: &Handle, path: &Path) -> Result<PathBuf> {
        Ok(self.session(h)?.workspace.join(inside(path)?))
    }
}

/// Set one rlimit, soft and hard alike, so the command cannot raise it.
/// A macro because the type of the resource constant differs by platform.
#[cfg(unix)]
macro_rules! rlimit {
    ($resource:expr, $value:expr) => {
        if let Some(value) = $value {
            let limit = libc::rlimit {
                rlim_cur: value as libc::rlim_t,
                rlim_max: value as libc::rlim_t,
            };
            // SAFETY: `limit` is a valid rlimit for the duration of the call.
            if unsafe { libc::setrlimit($resource, &limit) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    };
}

/// Put the command in its own process group and make it apply the session's
/// limits to itself before it execs.
#[cfg(unix)]
fn confine(command: &mut Command, session: &Arc<Session>) {
    // Its own group, so a timeout can kill everything it started.
    command.process_group(0);
    let session = Arc::clone(session);
    let apply = move || {
        let limits = session.limits;
        rlimit!(libc::RLIMIT_CPU, limits.cpu_time_secs);
        rlimit!(libc::RLIMIT_NOFILE, limits.max_open_files);
        #[cfg(target_os = "linux")]
        {
            rlimit!(libc::RLIMIT_AS, limits.memory_bytes);
            // Last, so the calls above are not themselves filtered. This
            // also sets no_new_privs.
            seccompiler::apply_filter(&session.filter).map_err(std::io::Error::other)?;
        }
        Ok(())
    };
    // SAFETY: the closure runs in the forked child before exec. It only makes
    // system calls on data built before the fork and does not allocate on
    // the success path.
    unsafe {
        command.pre_exec(apply);
    }
}

#[cfg(not(unix))]
fn confine(_command: &mut Command, _session: &Arc<Session>) {}

/// The most `read_file` will read.
const MAX_READ: u64 = 16 * 1024 * 1024;

/// The most kept of a command's standard output, and of its standard error.
/// The rest is read and thrown away, so the command is never blocked on a
/// full pipe, and neither memory nor the trace grows with what it prints.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// How long to wait for output still in the pipe after the command is gone.
const DRAIN: Duration = Duration::from_secs(2);

/// One of a command's output streams, read as it is written.
struct Capture {
    kept: Arc<Mutex<(Vec<u8>, bool)>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Capture {
    fn start<R: AsyncRead + Unpin + Send + 'static>(stream: Option<R>) -> Self {
        let kept = Arc::new(Mutex::new((Vec::new(), false)));
        let task = stream.map(|mut stream| {
            let kept = Arc::clone(&kept);
            tokio::spawn(async move {
                let mut chunk = vec![0u8; 64 * 1024];
                loop {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            let mut kept = kept.lock().unwrap_or_else(PoisonError::into_inner);
                            let room = MAX_OUTPUT.saturating_sub(kept.0.len());
                            kept.0.extend_from_slice(&chunk[..n.min(room)]);
                            kept.1 |= n > room;
                        }
                    }
                }
            })
        });
        Self { kept, task }
    }

    /// What was captured. By now the command and its group are gone, so the
    /// stream ends as soon as what is left in the pipe is read; a process
    /// that slipped the group and still holds it open is not waited for.
    async fn finish(mut self) -> Vec<u8> {
        if let Some(task) = self.task.take() {
            let abort = task.abort_handle();
            if tokio::time::timeout(DRAIN, task).await.is_err() {
                abort.abort();
            }
        }
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = std::mem::take(&mut kept.0);
        if kept.1 {
            out.extend_from_slice(
                format!("\n[output cut: only the first {MAX_OUTPUT} bytes are kept]\n").as_bytes(),
            );
        }
        out
    }
}

/// The process groups of commands that are running right now, across every
/// session in this process.
static RUNNING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// One running command's place in [`RUNNING`], given up when it is dropped.
struct Running(Option<u32>);

impl Running {
    fn new(pid: Option<u32>) -> Self {
        if let Some(pid) = pid {
            RUNNING
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(pid);
        }
        Self(pid)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let mut running = RUNNING.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(at) = running.iter().position(|p| *p == pid) {
                running.swap_remove(at);
            }
        }
    }
}

/// Kill every command the process sandbox has running, and what each
/// started. For a program about to exit without unwinding, such as on a
/// second Ctrl-C: a command is in its own process group, so the terminal's
/// signal never reaches it, and nothing else would stop it.
pub fn kill_running() {
    let running: Vec<u32> = RUNNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    for pid in running {
        kill_group(Some(pid));
    }
}

/// Kill every process in the command's group. The group id is the pid of
/// the command. When the command has just been reaped the id is free again,
/// but the kernel hands pids out in order and will not come round to this
/// one for a long while; the call is made at once.
///
/// A process that moved itself to another group or session is not reached.
/// The `process` backend does not claim to hold a program that is trying to
/// get away.
#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: plain system call; a group that is already gone is an
        // error we do not care about.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

#[async_trait]
impl Sandbox for ProcessSandbox {
    fn backend(&self) -> &'static str {
        "process"
    }

    async fn start(&self, spec: &SandboxSpec) -> Result<Handle> {
        if matches!(spec.network, Network::Allowlist(_)) {
            return Err(SandboxError::Unsupported(
                "a network allowlist; the process backend can only allow or block all of it",
            ));
        }
        if cfg!(not(target_os = "linux")) && spec.memory_bytes.is_some() {
            return Err(SandboxError::Unsupported(
                "a memory limit on this platform; the process backend enforces it on Linux only",
            ));
        }
        if cfg!(not(unix)) && (spec.cpu_time_secs.is_some() || spec.max_open_files.is_some()) {
            return Err(SandboxError::Unsupported(
                "CPU time and open file limits on this platform",
            ));
        }

        tokio::fs::create_dir_all(&spec.workspace).await?;
        let session = Session {
            workspace: tokio::fs::canonicalize(&spec.workspace).await?,
            time_limit: spec.time_limit_secs.map(Duration::from_secs),
            limits: Limits {
                cpu_time_secs: spec.cpu_time_secs,
                max_open_files: spec.max_open_files,
                #[cfg(target_os = "linux")]
                memory_bytes: spec.memory_bytes,
            },
            #[cfg(target_os = "linux")]
            filter: crate::seccomp::filter(matches!(spec.network, Network::None))?,
        };
        let id = format!("process-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        self.sessions().insert(id.clone(), Arc::new(session));
        Ok(Handle(id))
    }

    async fn exec(&self, h: &Handle, cmd: Exec) -> Result<ExecResult> {
        let session = self.session(h)?;
        let (program, args) = cmd
            .argv
            .split_first()
            .ok_or_else(|| SandboxError::Other("empty argv".into()))?;
        let cwd = match &cmd.cwd {
            Some(dir) => session.workspace.join(inside(dir)?),
            None => session.workspace.clone(),
        };

        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(std::env::var_os("PATH").map(|path| ("PATH", path)))
            .envs(cmd.env.iter().map(|(k, v)| (k, v)))
            .stdin(if cmd.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        confine(&mut command, &session);

        let started = Instant::now();
        let mut child = command.spawn()?;
        let pid = child.id();
        // Listed as running until this call returns, however it returns.
        let _running = Running::new(pid);

        // Input and output move at the same time as the command runs, and
        // the time limit covers all of it. Writing the input first would
        // block for good on a command that fills its output before it reads,
        // or never reads at all, and the limit would never start.
        let feed = match (cmd.stdin, child.stdin.take()) {
            (Some(input), Some(mut stdin)) => Some(tokio::spawn(async move {
                // A command that exits without reading its input is not an
                // error. Dropping `stdin` after the write closes it.
                let _ = stdin.write_all(&input).await;
            })),
            _ => None,
        };
        let stdout = Capture::start(child.stdout.take());
        let stderr = Capture::start(child.stderr.take());

        let waited = match session.time_limit {
            Some(limit) => tokio::time::timeout(limit, child.wait()).await.ok(),
            None => Some(child.wait().await),
        };
        // Whether it finished or ran out of time, nothing it started stays
        // behind: a process still holding the output open would also keep
        // the capture below from ever finishing.
        if waited.is_none() {
            let _ = child.start_kill();
        }
        kill_group(pid);
        if let Some(feed) = feed {
            feed.abort();
        }
        let Some(status) = waited else {
            return Err(SandboxError::Other(format!(
                "timed out after {} s",
                session.time_limit.map_or(0, |limit| limit.as_secs())
            )));
        };
        let status = status?;
        Ok(ExecResult {
            // No code means the command was killed by a signal, which is how
            // a CPU limit ends it.
            exit_code: status.code().unwrap_or(-1),
            stdout: stdout.finish().await,
            stderr: stderr.finish().await,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn read(&self, h: &Handle, path: &Path) -> Result<Vec<u8>> {
        let path = self.resolve(h, path)?;
        // A pipe or a device would block or never end, and a file too large
        // to hand to a model is no use read whole.
        let meta = tokio::fs::metadata(&path).await?;
        if !meta.is_file() {
            return Err(SandboxError::Other("not a regular file".into()));
        }
        if meta.len() > MAX_READ {
            return Err(SandboxError::Other(format!(
                "file is {} bytes; read_file takes at most {MAX_READ}",
                meta.len()
            )));
        }
        Ok(tokio::fs::read(path).await?)
    }

    async fn write(&self, h: &Handle, path: &Path, data: Vec<u8>) -> Result<()> {
        let path = self.resolve(h, path)?;
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        Ok(tokio::fs::write(path, data).await?)
    }

    async fn snapshot(&self, _h: &Handle) -> Result<SnapshotId> {
        Err(SandboxError::Unsupported("snapshot"))
    }

    /// Forgets the session. The workspace and its files are left in place.
    async fn stop(&self, h: Handle) -> Result<()> {
        self.sessions().remove(&h.0);
        Ok(())
    }
}
