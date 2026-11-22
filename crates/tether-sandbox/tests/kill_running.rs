#![cfg(unix)]

//! `kill_running` stops every command the process sandbox has running in
//! this process, so its test has a process to itself: beside other tests it
//! would kill their commands too.

use std::path::PathBuf;

use tether_sandbox::process::ProcessSandbox;
use tether_sandbox::{Exec, Sandbox, SandboxSpec};

#[tokio::test]
async fn kill_running_stops_a_command_in_flight() {
    // What a program does before exiting without unwinding, such as on a
    // second Ctrl-C: nothing else would stop the command.
    let workspace = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("process_kill_running");
    let _ = std::fs::remove_dir_all(&workspace);
    let sandbox = std::sync::Arc::new(ProcessSandbox::new());
    let spec = SandboxSpec {
        time_limit_secs: Some(60),
        ..SandboxSpec::new(workspace.clone())
    };
    let h = sandbox.start(&spec).await.expect("start");
    let running = tokio::spawn({
        let sandbox = sandbox.clone();
        let h = h.clone();
        async move {
            let cmd = Exec {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "sleep 3; echo late > late.txt".into(),
                ],
                cwd: None,
                env: vec![],
                stdin: None,
            };
            sandbox.exec(&h, cmd).await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let started = std::time::Instant::now();
    tether_sandbox::process::kill_running();
    let out = running.await.expect("join").expect("exec");
    assert_ne!(out.exit_code, 0, "the command was not killed");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(!workspace.join("late.txt").exists());
}
