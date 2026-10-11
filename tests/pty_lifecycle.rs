//! Exercise the actual launcher, OS PTY, input worker and child supervisor.
#![cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]

use std::path::Path;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use ratty::config::AppConfig;
use ratty::runtime::{RuntimeOptions, TerminalRuntime};

fn spawn(script: &str) -> TerminalRuntime {
    TerminalRuntime::spawn_with_launcher(
        &AppConfig::default(),
        &RuntimeOptions {
            command: Some(vec!["/bin/sh".into(), "-c".into(), script.into()]),
            working_dir: None,
        },
        Path::new(env!("CARGO_BIN_EXE_ratty")),
    )
    .expect("spawn PTY")
}

fn until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "PTY operation timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn assert_reaped(runtime: &TerminalRuntime) {
    let pid = fuxix::process::Pid::from_raw(runtime.child_process_id().expect("PID") as i32)
        .expect("positive PID");
    until(|| runtime.child_exit_code().is_some());
    assert!(
        !fuxix::process::exists(pid),
        "a zombie is still an existing process"
    );
}

#[test]
fn launcher_preserves_environment_cwd_tty_and_real_exit_status() {
    let mut config = AppConfig::default();
    config.terminal.default_rows = 31;
    config.terminal.default_cols = 97;
    config
        .env
        .insert("RATTY_PTY_TEST_VALUE".into(), "value with spaces".into());
    config.env.insert("TERM".into(), "test-term".into());
    let cwd = std::env::temp_dir().canonicalize().expect("temp directory");
    let mut runtime = TerminalRuntime::spawn_with_launcher(&config, &RuntimeOptions {
        command: Some(vec!["/bin/sh".into(), "-c".into(),
            "test -t 0 && test -t 1 && test -t 2 || exit 99; printf control-tty >/dev/tty || exit 98; printf '%s|%s|' \"$RATTY_PTY_TEST_VALUE\" \"$TERM\"; pwd -P; stty size; exit 37".into()]),
        working_dir: Some(cwd.clone()),
    }, Path::new(env!("CARGO_BIN_EXE_ratty"))).expect("spawn");
    let mut output = Vec::new();
    until(|| match runtime.try_recv() {
        Ok(bytes) => {
            output.extend(bytes);
            false
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => true,
    });
    assert_reaped(&runtime);
    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains(&format!("value with spaces|test-term|{}", cwd.display())),
        "{text}"
    );
    assert!(text.contains("31 97"), "{text}");
    assert!(text.contains("control-tty"), "{text}");
    assert_eq!(runtime.child_exit_code(), Some(37));
    assert_eq!(runtime.child_state(), "exited");
    runtime.shutdown();
    assert_eq!(
        runtime.child_exit_code(),
        Some(37),
        "shutdown must retain normal exit status"
    );
}

#[test]
fn resize_notifies_the_foreground_process_with_committed_dimensions() {
    let mut runtime = spawn(
        "trap 'stty size; printf resized' WINCH; printf ready; while :; do read answer || :; done",
    );
    let mut output = Vec::new();
    until(|| {
        if let Ok(bytes) = runtime.try_recv() {
            output.extend(bytes);
        }
        output.windows(5).any(|s| s == b"ready")
    });
    runtime.resize(103, 41, 1440, 900).expect("resize");
    // Some shells defer traps while their read builtin waits for a line.
    runtime.write_input(b"\n").expect("release shell read");
    until(|| {
        if let Ok(bytes) = runtime.try_recv() {
            output.extend(bytes);
        }
        output.windows(7).any(|s| s == b"resized")
    });
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("41 103"), "{text}");
    runtime.shutdown();
    assert_reaped(&runtime);
}

#[test]
fn child_is_a_session_leader_and_is_reaped_without_ui_polling() {
    let mut runtime = spawn("read answer; exit 23");
    let pid = fuxix::process::Pid::from_raw(runtime.child_process_id().expect("PID") as i32)
        .expect("PID");
    assert_eq!(fuxix::process::session(pid), Some(pid));
    runtime.write_input(b"done\n").expect("input");
    // Do not read output or ask the runtime for exit status before the OS
    // reports the child gone: the independent supervisor must reap it.
    until(|| !fuxix::process::exists(pid));
    assert_eq!(runtime.child_exit_code(), Some(23));
    runtime.shutdown();
}

#[test]
fn terminal_interrupt_reaches_the_foreground_job() {
    let mut runtime = spawn("trap 'exit 42' INT; printf ready; while :; do read answer || :; done");
    let mut output = Vec::new();
    until(|| {
        if let Ok(bytes) = runtime.try_recv() {
            output.extend(bytes);
        }
        output.windows(5).any(|s| s == b"ready")
    });
    runtime.write_input(b"\x03\n").expect("terminal interrupt");
    assert_reaped(&runtime);
    assert_eq!(runtime.child_exit_code(), Some(42));
    runtime.shutdown();
}

#[test]
fn hangup_does_not_claim_that_a_still_running_child_exited() {
    let mut runtime = spawn("exec 0<&- 1>&- 2>&-; sleep 30");
    until(|| matches!(runtime.try_recv(), Err(TryRecvError::Disconnected)));
    assert!(runtime.pty_disconnected());
    assert_eq!(runtime.child_state(), "hung_up");
    assert_eq!(runtime.child_exit_code(), None);
    let start = Instant::now();
    runtime.shutdown();
    assert!(start.elapsed() < Duration::from_millis(500));
    assert_reaped(&runtime);
}

#[test]
fn saturated_input_queue_and_full_output_channel_do_not_block_shutdown() {
    for script in [
        "stty raw -echo; printf ready; sleep 30",
        "stty raw -echo; printf ready; while :; do printf '0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ'; done",
    ] {
        let mut runtime = spawn(script);
        let mut output = Vec::new();
        until(|| {
            if let Ok(bytes) = runtime.try_recv() {
                output.extend(bytes);
            }
            output.windows(5).any(|s| s == b"ready")
        });
        std::thread::sleep(Duration::from_millis(50));
        let input = vec![b'x'; 64 * 1024];
        let start = Instant::now();
        let mut saturated = false;
        for _ in 0..100 {
            if let Err(error) = runtime.write_input(&input) {
                assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                saturated = true;
                break;
            }
        }
        assert!(
            saturated,
            "a child that never reads must exert backpressure"
        );
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "queue acceptance blocked"
        );
        let start = Instant::now();
        runtime.shutdown();
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "shutdown blocked on PTY I/O"
        );
        assert_reaped(&runtime);
    }
}

#[test]
fn launch_failures_are_reported_synchronously_and_do_not_leak_children() {
    #[cfg(target_os = "linux")]
    let before = std::fs::read_to_string("/proc/thread-self/children").expect("child list");
    for (program, directory) in [
        ("/this-command-does-not-exist-ratty", None),
        (
            "/bin/sh",
            Some(std::path::PathBuf::from(
                "/this-directory-does-not-exist-ratty",
            )),
        ),
    ] {
        let result = TerminalRuntime::spawn_with_launcher(
            &AppConfig::default(),
            &RuntimeOptions {
                command: Some(vec![program.into()]),
                working_dir: directory,
            },
            Path::new(env!("CARGO_BIN_EXE_ratty")),
        );
        assert!(result.is_err(), "launch failure must reach the caller");
    }
    #[cfg(target_os = "linux")]
    assert_eq!(
        std::fs::read_to_string("/proc/thread-self/children").expect("child list"),
        before
    );
}

#[cfg(target_os = "linux")]
#[test]
fn launcher_closes_inherited_non_cloexec_descriptors() {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open("/dev/null").expect("file");
    rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty()).expect("clear close-on-exec");
    let mut runtime = spawn(&format!(
        "test ! -e /proc/self/fd/{}; exit $?",
        file.as_raw_fd()
    ));
    assert_reaped(&runtime);
    assert_eq!(runtime.child_exit_code(), Some(0));
    runtime.shutdown();
}
