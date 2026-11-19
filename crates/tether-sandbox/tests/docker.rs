//! Needs a running Docker daemon: `cargo test -p tether-sandbox --features docker`.
#![cfg(feature = "docker")]

use std::path::{Path, PathBuf};

use tether_sandbox::docker::DockerSandbox;
use tether_sandbox::{Exec, Handle, Network, Sandbox, SandboxError, SandboxSpec};

/// Small, and has everything the backend needs.
const IMAGE: &str = "alpine:3";

fn workspace(test: &str) -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = std::fs::remove_dir_all(&workspace);
    workspace
}

async fn start(spec: SandboxSpec) -> (DockerSandbox, Handle) {
    let sandbox = DockerSandbox::with_image(IMAGE);
    let handle = sandbox.start(&spec).await.expect("start");
    (sandbox, handle)
}

fn sh(script: &str) -> Exec {
    Exec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        cwd: None,
        env: vec![],
        stdin: None,
    }
}

async fn stdout(sandbox: &DockerSandbox, h: &Handle, script: &str) -> String {
    let out = sandbox.exec(h, sh(script)).await.expect("exec");
    assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).expect("utf-8")
}

#[tokio::test]
async fn commands_run_in_the_container_not_on_the_host() {
    let (sandbox, h) = start(SandboxSpec::new(workspace("docker_exec"))).await;
    std::env::set_var("TETHER_TEST_SECRET", "leak");

    let seen = stdout(
        &sandbox,
        &h,
        "pwd; grep -q Alpine /etc/os-release && echo alpine; echo ${TETHER_TEST_SECRET:-unset}; id -u",
    )
    .await;
    let lines: Vec<&str> = seen.lines().collect();
    assert_eq!(&lines[..3], ["/workspace", "alpine", "unset"]);
    // Not root inside the container.
    assert_ne!(lines[3], "0");

    let out = sandbox
        .exec(&h, sh("echo oops >&2; exit 3"))
        .await
        .expect("exec");
    assert_eq!((out.exit_code, out.stderr.as_slice()), (3, &b"oops\n"[..]));

    let mut cmd = sh("cat; echo $GREETING; pwd");
    cmd.stdin = Some(b"from stdin\n".to_vec());
    cmd.env = vec![("GREETING".into(), "hello".into())];
    cmd.cwd = Some("sub".into());
    sandbox
        .write(&h, Path::new("sub/.keep"), vec![])
        .await
        .expect("write");
    let out = sandbox.exec(&h, cmd).await.expect("exec");
    assert_eq!(out.stdout, b"from stdin\nhello\n/workspace/sub\n");

    sandbox.stop(h).await.expect("stop");
}

#[tokio::test]
async fn the_workspace_is_shared_with_the_host_and_nothing_else_is() {
    let dir = workspace("docker_mount");
    let (sandbox, h) = start(SandboxSpec::new(&dir)).await;

    sandbox
        .write(&h, Path::new("notes/a.txt"), b"hi".to_vec())
        .await
        .expect("write");
    assert_eq!(
        std::fs::read(dir.join("notes/a.txt")).expect("host read"),
        b"hi"
    );
    assert_eq!(
        sandbox
            .read(&h, Path::new("notes/a.txt"))
            .await
            .expect("read"),
        b"hi"
    );

    stdout(&sandbox, &h, "echo made-inside > made.txt").await;
    assert_eq!(
        std::fs::read_to_string(dir.join("made.txt")).expect("host read"),
        "made-inside\n"
    );

    // The host's files outside the workspace are not there to read.
    let host_file = dir
        .parent()
        .expect("parent")
        .join("docker_mount_outside.txt");
    std::fs::write(&host_file, "host only").expect("host write");
    let out = sandbox
        .exec(&h, sh(&format!("cat '{}'", host_file.display())))
        .await
        .expect("exec");
    assert_ne!(out.exit_code, 0);

    for bad in ["../outside.txt", "/etc/passwd", "a/../../b"] {
        assert!(sandbox.read(&h, Path::new(bad)).await.is_err(), "{bad}");
        assert!(
            sandbox.write(&h, Path::new(bad), vec![]).await.is_err(),
            "{bad}"
        );
    }
    sandbox.stop(h).await.expect("stop");
}

#[tokio::test]
async fn a_symlink_in_the_workspace_resolves_inside_the_container() {
    let (sandbox, h) = start(SandboxSpec::new(workspace("docker_symlink"))).await;
    // On the host this link would point at the host's own file.
    let inside = stdout(&sandbox, &h, "ln -s /etc/hostname link; cat /etc/hostname").await;

    let read = sandbox.read(&h, Path::new("link")).await.expect("read");
    assert_eq!(String::from_utf8(read).expect("utf-8"), inside);
    sandbox.stop(h).await.expect("stop");
}

#[tokio::test]
async fn network_none_has_no_route_out() {
    let routes = "ip route";

    let (sandbox, h) = start(SandboxSpec::new(workspace("docker_net_none"))).await;
    assert_eq!(stdout(&sandbox, &h, routes).await, "");
    sandbox.stop(h).await.expect("stop");

    let spec = SandboxSpec {
        network: Network::All,
        ..SandboxSpec::new(workspace("docker_net_all"))
    };
    let (sandbox, h) = start(spec).await;
    assert!(stdout(&sandbox, &h, routes).await.contains("default via"));
    sandbox.stop(h).await.expect("stop");

    let allowlist = SandboxSpec {
        network: Network::Allowlist(vec!["example.com".into()]),
        ..SandboxSpec::new(workspace("docker_net_allowlist"))
    };
    assert!(matches!(
        DockerSandbox::with_image(IMAGE).start(&allowlist).await,
        Err(SandboxError::Unsupported(_))
    ));
}

#[tokio::test]
async fn limits_become_container_limits() {
    let spec = SandboxSpec {
        memory_bytes: Some(64 * 1024 * 1024),
        cpu_millis: Some(500),
        max_open_files: Some(64),
        ..SandboxSpec::new(workspace("docker_limits"))
    };
    let (sandbox, h) = start(spec).await;
    let seen = stdout(
        &sandbox,
        &h,
        "cat /sys/fs/cgroup/memory.max; cat /sys/fs/cgroup/cpu.max; ulimit -n",
    )
    .await;
    // Half a CPU is 50 ms of every 100 ms period.
    assert_eq!(
        seen.lines().collect::<Vec<_>>(),
        ["67108864", "50000 100000", "64"]
    );
    sandbox.stop(h).await.expect("stop");
}

#[tokio::test]
async fn a_command_past_the_time_limit_is_killed_in_the_container() {
    let spec = SandboxSpec {
        time_limit_secs: Some(1),
        ..SandboxSpec::new(workspace("docker_timeout"))
    };
    let (sandbox, h) = start(spec).await;
    let err = sandbox
        .exec(&h, sh("sleep 30"))
        .await
        .expect_err("must time out");
    assert!(err.to_string().contains("timed out after 1 s"), "{err}");

    // Nothing is left running but the container's own idle process.
    let left = stdout(&sandbox, &h, "ps -o args | grep -c '[s]leep 30' || true").await;
    assert_eq!(left.trim(), "0");
    sandbox.stop(h).await.expect("stop");
}

#[tokio::test]
async fn snapshot_commits_the_container_and_a_new_sandbox_starts_from_it() {
    let (sandbox, h) = start(SandboxSpec::new(workspace("docker_snapshot"))).await;
    stdout(&sandbox, &h, "echo kept > /tmp/marker").await;
    let snapshot = sandbox.snapshot(&h).await.expect("snapshot");
    assert!(snapshot.0.starts_with("sha256:"), "{}", snapshot.0);
    sandbox.stop(h).await.expect("stop");

    let restored = DockerSandbox::with_image(&snapshot.0);
    let h = restored
        .start(&SandboxSpec::new(workspace("docker_snapshot_restored")))
        .await
        .expect("start from snapshot");
    assert_eq!(stdout(&restored, &h, "cat /tmp/marker").await, "kept\n");
    restored.stop(h).await.expect("stop");

    let removed = std::process::Command::new("docker")
        .args(["rmi", "--force", &snapshot.0])
        .output()
        .expect("docker rmi");
    assert!(removed.status.success(), "{removed:?}");
}

#[tokio::test]
async fn stop_removes_the_container_and_so_does_dropping_the_sandbox() {
    let exists = |name: &str| {
        let out = std::process::Command::new("docker")
            .args([
                "ps",
                "--all",
                "--quiet",
                "--filter",
                &format!("name=^{name}$"),
            ])
            .output()
            .expect("docker ps");
        !out.stdout.is_empty()
    };

    let (sandbox, h) = start(SandboxSpec::new(workspace("docker_stop"))).await;
    let name = h.0.clone();
    assert!(exists(&name));
    sandbox.stop(h.clone()).await.expect("stop");
    assert!(!exists(&name));
    assert!(sandbox.exec(&h, sh("true")).await.is_err());

    let (sandbox, h) = start(SandboxSpec::new(workspace("docker_drop"))).await;
    drop(sandbox);
    let mut gone = false;
    for _ in 0..100 {
        gone = !exists(&h.0);
        if gone {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(gone, "container {} outlived its sandbox", h.0);
}
