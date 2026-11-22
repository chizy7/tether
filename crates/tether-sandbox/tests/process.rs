#![cfg(unix)]

use std::path::{Path, PathBuf};

use tether_sandbox::process::ProcessSandbox;
use tether_sandbox::{Exec, Handle, Network, Sandbox, SandboxError, SandboxSpec};

/// A fresh, empty workspace directory for one test.
fn workspace(test: &str) -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = std::fs::remove_dir_all(&workspace);
    workspace
}

/// Start a sandbox on `spec` and run one shell script in it.
async fn run_in(
    spec: SandboxSpec,
    script: &str,
) -> tether_sandbox::Result<tether_sandbox::ExecResult> {
    let sandbox = ProcessSandbox::new();
    let handle = sandbox.start(&spec).await?;
    sandbox.exec(&handle, sh(script)).await
}

async fn start(test: &str, time_limit_secs: Option<u64>) -> (ProcessSandbox, Handle, PathBuf) {
    let workspace = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = std::fs::remove_dir_all(&workspace);
    let sandbox = ProcessSandbox::new();
    let spec = SandboxSpec {
        time_limit_secs,
        ..SandboxSpec::new(workspace.clone())
    };
    let handle = sandbox.start(&spec).await.expect("start");
    (sandbox, handle, workspace)
}

fn sh(script: &str) -> Exec {
    Exec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        cwd: None,
        env: vec![],
        stdin: None,
    }
}

#[tokio::test]
async fn exec_runs_in_the_workspace_with_a_scrubbed_env() {
    let (sandbox, h, workspace) = start("process_exec", None).await;
    std::env::set_var("TETHER_TEST_SECRET", "leak");

    let out = sandbox
        .exec(
            &h,
            sh("pwd; echo ${TETHER_TEST_SECRET:-unset}; echo oops >&2; exit 3"),
        )
        .await
        .expect("exec");
    let stdout = String::from_utf8(out.stdout).expect("utf-8");
    let lines: Vec<&str> = stdout.lines().collect();
    let workspace = workspace.canonicalize().expect("canonicalize");
    assert_eq!(lines, [workspace.to_str().expect("utf-8"), "unset"]);
    assert_eq!(out.stderr, b"oops\n");
    assert_eq!(out.exit_code, 3);
}

#[tokio::test]
async fn exec_passes_stdin_and_explicit_env() {
    let (sandbox, h, _) = start("process_stdin", None).await;
    let mut cmd = sh("cat; echo $GREETING");
    cmd.stdin = Some(b"from stdin\n".to_vec());
    cmd.env = vec![("GREETING".into(), "hello".into())];

    let out = sandbox.exec(&h, cmd).await.expect("exec");
    assert_eq!(out.stdout, b"from stdin\nhello\n");
}

#[tokio::test]
async fn write_then_read_roundtrips_inside_the_workspace() {
    let (sandbox, h, workspace) = start("process_files", None).await;
    sandbox
        .write(&h, Path::new("src/a.txt"), b"hi".to_vec())
        .await
        .expect("write");
    assert_eq!(
        sandbox
            .read(&h, Path::new("src/a.txt"))
            .await
            .expect("read"),
        b"hi"
    );
    assert!(workspace.join("src/a.txt").is_file());
}

#[tokio::test]
async fn paths_that_leave_the_workspace_are_refused() {
    let (sandbox, h, _) = start("process_escape", None).await;
    for bad in ["../outside.txt", "/etc/passwd", "a/../../b"] {
        let err = sandbox.read(&h, Path::new(bad)).await.expect_err(bad);
        assert!(matches!(err, SandboxError::Other(_)), "{bad}: {err}");
        let err = sandbox
            .write(&h, Path::new(bad), vec![])
            .await
            .expect_err(bad);
        assert!(matches!(err, SandboxError::Other(_)), "{bad}: {err}");
    }
    let mut cmd = sh("pwd");
    cmd.cwd = Some("..".into());
    assert!(sandbox.exec(&h, cmd).await.is_err());
}

#[tokio::test]
async fn a_command_past_the_time_limit_is_stopped() {
    let (sandbox, h, _) = start("process_timeout", Some(1)).await;
    let err = sandbox
        .exec(&h, sh("sleep 30"))
        .await
        .expect_err("must time out");
    assert!(err.to_string().contains("timed out"), "{err}");
}

#[tokio::test]
async fn snapshot_is_unsupported_and_stop_forgets_the_session() {
    let (sandbox, h, _) = start("process_stop", None).await;
    assert!(matches!(
        sandbox.snapshot(&h).await,
        Err(SandboxError::Unsupported(_))
    ));
    sandbox.stop(h.clone()).await.expect("stop");
    assert!(sandbox.exec(&h, sh("true")).await.is_err());
}

#[tokio::test]
async fn cpu_time_limit_ends_a_command_that_spins() {
    let spec = SandboxSpec {
        cpu_time_secs: Some(1),
        time_limit_secs: Some(30),
        ..SandboxSpec::new(workspace("process_cpu"))
    };
    let out = run_in(spec, "while :; do :; done").await.expect("exec");
    // Killed by the kernel for its CPU use, long before the 30 s wall limit.
    assert_eq!(out.exit_code, -1);
    assert!(out.duration_ms < 15_000, "{} ms", out.duration_ms);
}

#[tokio::test]
async fn open_file_limit_is_enforced() {
    let script =
        "exec 3</dev/null 4</dev/null 5</dev/null && echo three; exec 6</dev/null && echo four";
    let spec = SandboxSpec {
        max_open_files: Some(6),
        ..SandboxSpec::new(workspace("process_nofile"))
    };
    // Descriptors 0 to 5 fit under a limit of 6; the next one does not.
    let out = run_in(spec, script).await.expect("exec");
    assert_eq!(out.stdout, b"three\n");
    assert_ne!(out.exit_code, 0);

    let unlimited = SandboxSpec::new(workspace("process_nofile_unlimited"));
    let out = run_in(unlimited, script).await.expect("exec");
    assert_eq!(out.stdout, b"three\nfour\n");
}

#[tokio::test]
async fn a_timeout_kills_what_the_command_started_too() {
    let dir = workspace("process_group");
    let spec = SandboxSpec {
        time_limit_secs: Some(1),
        ..SandboxSpec::new(&dir)
    };
    // The shell starts a long sleep in the background and records its pid.
    let err = run_in(spec, "sleep 300 & echo $! > pid; wait")
        .await
        .expect_err("must time out");
    assert!(err.to_string().contains("timed out"), "{err}");

    let pid = std::fs::read_to_string(dir.join("pid")).expect("pid file");
    let mut alive = true;
    for _ in 0..50 {
        // `kill -0` succeeds only while the process exists.
        alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run kill")
            .success();
        if !alive {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        !alive,
        "background process {} outlived the timeout",
        pid.trim()
    );
}

#[tokio::test]
async fn limits_the_backend_cannot_enforce_are_refused_at_start() {
    let sandbox = ProcessSandbox::new();
    let allowlist = SandboxSpec {
        network: Network::Allowlist(vec!["example.com".into()]),
        ..SandboxSpec::new(workspace("process_allowlist"))
    };
    assert!(matches!(
        sandbox.start(&allowlist).await,
        Err(SandboxError::Unsupported(_))
    ));

    let memory = SandboxSpec {
        memory_bytes: Some(256 * 1024 * 1024),
        ..SandboxSpec::new(workspace("process_memory_spec"))
    };
    let started = sandbox.start(&memory).await;
    if cfg!(target_os = "linux") {
        started.expect("Linux enforces a memory limit");
    } else {
        assert!(matches!(started, Err(SandboxError::Unsupported(_))));
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    /// Builds a 200 MB string in the shell, which needs that much memory.
    const HUNGRY: &str = r#"x=$(head -c 200000000 /dev/zero | tr '\0' a); echo ${#x}"#;

    #[tokio::test]
    async fn memory_limit_stops_a_command_that_wants_too_much() {
        let limited = SandboxSpec {
            memory_bytes: Some(64 * 1024 * 1024),
            time_limit_secs: Some(60),
            ..SandboxSpec::new(workspace("process_memory"))
        };
        let out = run_in(limited, HUNGRY).await.expect("exec");
        assert_ne!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stdout));

        let unlimited = SandboxSpec {
            time_limit_secs: Some(60),
            ..SandboxSpec::new(workspace("process_memory_unlimited"))
        };
        let out = run_in(unlimited, HUNGRY).await.expect("exec");
        assert_eq!(out.stdout, b"200000000\n");
    }

    /// Opens a TCP connection to a local port nothing listens on.
    const CONNECT: &str = "exec 3<>/dev/tcp/127.0.0.1/9";

    fn bash(script: &str) -> Exec {
        Exec {
            argv: vec!["bash".into(), "-c".into(), script.into()],
            cwd: None,
            env: vec![],
            stdin: None,
        }
    }

    async fn connect(test: &str, network: Network) -> String {
        let sandbox = ProcessSandbox::new();
        let spec = SandboxSpec {
            network,
            ..SandboxSpec::new(workspace(test))
        };
        let handle = sandbox.start(&spec).await.expect("start");
        let out = sandbox.exec(&handle, bash(CONNECT)).await.expect("exec");
        assert_ne!(out.exit_code, 0);
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    #[tokio::test]
    async fn network_none_refuses_internet_sockets() {
        // With the network on, the connection gets as far as being refused
        // by the port. With it off, the socket is never created.
        let open = connect("process_net_all", Network::All).await;
        assert!(open.contains("refused"), "{open}");

        let closed = connect("process_net_none", Network::None).await;
        assert!(!closed.contains("refused"), "{closed}");
        assert!(closed.contains("not permitted"), "{closed}");
    }

    #[tokio::test]
    async fn filtered_system_calls_fail_and_ordinary_commands_still_work() {
        let spec = SandboxSpec::new(workspace("process_seccomp"));
        // `unshare` needs the unshare system call, which the filter refuses.
        let out = run_in(
            spec,
            "echo before; ls / > /dev/null && echo listed; unshare --user true",
        )
        .await
        .expect("exec");
        assert_eq!(out.stdout, b"before\nlisted\n");
        assert_ne!(out.exit_code, 0);
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("not permitted"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[tokio::test]
async fn input_the_command_never_reads_does_not_outlast_the_time_limit() {
    // More than a pipe holds, to a command that never reads it. Writing it
    // before starting the clock used to block for good.
    let (sandbox, h, _) = start("process_stdin_limit", Some(1)).await;
    let cmd = Exec {
        stdin: Some(vec![b'x'; 1024 * 1024]),
        ..sh("sleep 30")
    };
    let started = std::time::Instant::now();
    let err = sandbox.exec(&h, cmd).await.expect_err("must time out");
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn large_input_and_output_at_once_do_not_deadlock() {
    // `cat` fills its output before it has read all its input.
    let (sandbox, h, _) = start("process_stdin_echo", Some(20)).await;
    let cmd = Exec {
        argv: vec!["cat".into()],
        cwd: None,
        env: vec![],
        stdin: Some(vec![b'y'; 1024 * 1024]),
    };
    let out = sandbox.exec(&h, cmd).await.expect("cat");
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout.len(), 1024 * 1024);
}

#[tokio::test]
async fn what_a_command_leaves_running_is_stopped_when_it_returns() {
    let (sandbox, h, workspace) = start("process_background", Some(20)).await;
    // The background job would write the file two seconds from now.
    let out = sandbox
        .exec(
            &h,
            sh("(sleep 2; echo late > late.txt) >/dev/null 2>&1 & echo started"),
        )
        .await
        .expect("exec");
    assert_eq!(out.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "started");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(
        !workspace.join("late.txt").exists(),
        "a background job outlived the command that started it"
    );
}

#[tokio::test]
async fn output_past_the_cap_is_cut_and_says_so() {
    let (sandbox, h, _) = start("process_output_cap", Some(30)).await;
    // About 16 MB, four times what is kept.
    let out = sandbox
        .exec(&h, sh("head -c 16000000 /dev/zero | tr '\\0' 'a'"))
        .await
        .expect("exec");
    assert!(
        out.stdout.len() < 5 * 1024 * 1024,
        "{} bytes kept",
        out.stdout.len()
    );
    assert!(String::from_utf8_lossy(&out.stdout[out.stdout.len() - 80..]).contains("output cut"));
}

#[tokio::test]
async fn read_refuses_what_is_not_a_plain_file() {
    let (sandbox, h, workspace) = start("process_read_kinds", Some(10)).await;
    std::fs::create_dir_all(workspace.join("dir")).expect("mkdir");
    let err = sandbox
        .read(&h, Path::new("dir"))
        .await
        .expect_err("a directory is not a file");
    assert!(err.to_string().contains("regular file"), "{err}");
}
