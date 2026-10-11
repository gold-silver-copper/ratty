//! Real ConPTY lifecycle coverage for the retained Windows backend.
#![cfg(windows)]

use std::path::Path;
use std::time::{Duration, Instant};

use ratty::config::AppConfig;
use ratty::runtime::{RuntimeOptions, TerminalRuntime};

fn spawn(script: &str) -> TerminalRuntime {
    TerminalRuntime::spawn_with_launcher(
        &AppConfig::default(),
        &RuntimeOptions {
            command: Some(vec![
                "cmd.exe".into(),
                "/D".into(),
                "/Q".into(),
                "/C".into(),
                script.into(),
            ]),
            working_dir: None,
        },
        Path::new(env!("CARGO_BIN_EXE_ratty")),
    )
    .expect("ConPTY spawn")
}

fn until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "ConPTY operation timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn conpty_child_exit_is_reaped_without_ui_polling() {
    let mut runtime = spawn("exit /B 37");
    until(|| runtime.child_exit_code().is_some());
    assert_eq!(runtime.child_exit_code(), Some(37));
    runtime.shutdown();
    assert_eq!(runtime.child_exit_code(), Some(37));
}

#[test]
fn conpty_accepts_input_and_stops_a_waiting_child() {
    let mut runtime = spawn("echo ready & set /p answer= & exit /B 23");
    let mut output = Vec::new();
    until(|| {
        if let Ok(bytes) = runtime.try_recv() {
            output.extend(bytes);
        }
        output.windows(5).any(|s| s == b"ready")
    });
    runtime.write_input(b"done\r\n").expect("queued input");
    until(|| runtime.child_exit_code().is_some());
    assert_eq!(runtime.child_exit_code(), Some(23));
    runtime.shutdown();

    let mut runtime = spawn("set /p answer=");
    let start = Instant::now();
    runtime.shutdown();
    assert!(start.elapsed() < Duration::from_secs(1));
    until(|| runtime.child_exit_code().is_some());
}

#[test]
fn conpty_shutdown_drains_output_while_closing_the_console() {
    let mut runtime = spawn(
        "echo ready & for /L %i in (1,1,2147483647) do @echo 0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ",
    );
    let mut output = Vec::new();
    until(|| {
        if let Ok(bytes) = runtime.try_recv() {
            output.extend(bytes);
        }
        output.windows(5).any(|s| s == b"ready")
    });
    std::thread::sleep(Duration::from_millis(100));
    let start = Instant::now();
    runtime.shutdown();
    assert!(start.elapsed() < Duration::from_secs(1));
    until(|| runtime.child_exit_code().is_some());
}
