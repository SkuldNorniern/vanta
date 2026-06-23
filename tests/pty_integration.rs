//! End-to-end PTY lifecycle tests: spawn, write/read round-trip, resize,
//! Ctrl-C, and exit status. Exercises the real platform backend (ConPTY on
//! Windows, forkpty on Unix), not just the in-process `Vt` parser.

use std::ffi::OsString;
use std::thread;
use std::time::{Duration, Instant};
use vanta::{SpawnConfig, Terminal};

/// Poll `predicate` until it's true or `timeout` elapses; returns whether it
/// became true. Avoids fixed sleeps so tests run as fast as the real
/// backend/process allows while still tolerating slow CI/shell startup.
fn poll_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if predicate() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// A `SpawnConfig` that runs `program`/`args` directly, bypassing the
/// interactive shell entirely — deterministic and fast, since it isn't
/// subject to shell startup/theme cost (some shells take several seconds to
/// render their first prompt).
fn direct(program: &str, args: &[&str]) -> SpawnConfig {
    SpawnConfig {
        program: Some(OsString::from(program)),
        args: args.iter().map(|s| OsString::from(*s)).collect(),
        ..SpawnConfig::default()
    }
}

#[test]
fn spawn_runs_a_command_and_captures_its_output() {
    let config = if cfg!(windows) {
        direct("cmd", &["/C", "echo VANTA_TEST_OK"])
    } else {
        direct("echo", &["VANTA_TEST_OK"])
    };
    let term = Terminal::spawn_with_config(&config).expect("spawn");

    let saw_output = poll_until(Duration::from_secs(5), || {
        term.snapshot().text_snapshot().contains("VANTA_TEST_OK")
    });
    assert!(saw_output, "expected child output within 5s");
}

#[test]
fn interactive_shell_echoes_typed_command() {
    let term = Terminal::spawn().expect("spawn default shell");
    assert!(term.is_running());

    term.write_str("echo VANTA_SHELL_OK\r\n")
        .expect("write to shell");

    let saw_output = poll_until(Duration::from_secs(10), || {
        term.snapshot().text_snapshot().contains("VANTA_SHELL_OK")
    });
    assert!(
        saw_output,
        "expected shell to echo command output within 10s"
    );
}

#[test]
fn resize_updates_grid_dimensions() {
    let term = Terminal::spawn_with_size(80, 24).expect("spawn");
    assert_eq!(term.snapshot().screen.len(), 24);

    term.resize(100, 30).expect("resize");
    let snap = term.snapshot();
    assert_eq!(snap.screen.len(), 30);
    assert!(snap.screen.iter().all(|row| row.len() <= 100));
}

#[test]
fn kill_terminates_a_long_running_child() {
    let config = if cfg!(windows) {
        direct("ping", &["-n", "30", "127.0.0.1"])
    } else {
        direct("sleep", &["30"])
    };
    let term = Terminal::spawn_with_config(&config).expect("spawn");
    assert!(poll_until(Duration::from_secs(2), || term.is_running()));

    term.kill().expect("kill");

    let stopped = poll_until(Duration::from_secs(5), || !term.is_running());
    assert!(
        stopped,
        "expected child to stop running within 5s of kill()"
    );
    assert!(term.try_wait().expect("try_wait").is_some());
}

#[test]
#[cfg_attr(
    windows,
    ignore = "0x03 is correctly recognized as Ctrl+C at ConPTY's console \
              input-editing level (verified: it cancels an in-progress \
              command line), but GenerateConsoleCtrlEvent does not appear \
              to reach the spawned child in this sandboxed environment. \
              Run with `cargo test -- --ignored` on a normal desktop \
              session to verify."
)]
fn ctrl_c_terminates_a_long_running_child() {
    let config = if cfg!(windows) {
        direct("ping", &["-n", "30", "127.0.0.1"])
    } else {
        direct("sleep", &["30"])
    };
    let term = Terminal::spawn_with_config(&config).expect("spawn");
    assert!(poll_until(Duration::from_secs(2), || term.is_running()));

    term.write_bytes(&[0x03]).expect("write ctrl-c"); // ETX

    let stopped = poll_until(Duration::from_secs(5), || !term.is_running());
    assert!(
        stopped,
        "expected Ctrl-C to terminate the child within 5s, well before its natural 30s end"
    );
    assert!(term.try_wait().expect("try_wait").is_some());
}

#[test]
fn eof_is_observable_independent_of_process_exit() {
    let config = if cfg!(windows) {
        direct("cmd", &["/C", "echo done"])
    } else {
        direct("echo", &["done"])
    };
    let term = Terminal::spawn_with_config(&config).expect("spawn");

    let closed = poll_until(Duration::from_secs(5), || term.is_closed());
    assert!(closed, "expected the output stream to reach EOF within 5s");
}
