//! The output contract, enforced.
//!
//! rx promises exactly one JSON object per invocation, on stdout, carrying a
//! typed `code` when it is an error. That promise was documented long before
//! anything checked it, and nineteen call sites had drifted to bare stderr text
//! by 0.3.1 — an agent that had been told to parse stdout got an empty stream
//! and no way to tell *why* the call failed.
//!
//! Every case here is a client-side rejection: no host is contacted, so these
//! run anywhere, with no network and no rxd.

use std::process::{Command, Output, Stdio};

/// Run rx with `args` and closed stdin.
///
/// Closing stdin matters: `run` feeds piped stdin to the remote, so an inherited
/// pipe that never closes would hang the test the way it once hung the CLI.
fn rx(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rx"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("rx should be runnable")
}

/// Assert the invocation failed with a typed error object on stdout, and return
/// the parsed object.
fn expect_error(args: &[&str], code: &str) -> serde_json::Value {
    let out = rx(args);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 for {args:?}\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout must be one JSON object for {args:?}: {e}\nstdout: {stdout}")
    });
    assert_eq!(value["type"], "error", "for {args:?}");
    assert_eq!(value["code"], code, "for {args:?}: {value}");
    value
}

#[test]
fn malformed_arguments_answer_with_a_typed_object_on_stdout() {
    // Destination shape: the HOST:PATH forms are the one place rx takes a
    // compound argument, so the error names the form it wanted.
    let put = expect_error(&["put", "/etc/hostname", "somehost"], "bad_request");
    assert!(
        put["hint"].as_str().unwrap_or_default().contains("rx put"),
        "the hint should show a correct invocation: {put}"
    );
    expect_error(&["get", "somehost", "/tmp/x"], "bad_request");

    // Option values rx parses itself.
    expect_error(&["run", "h", "--env", "NOEQUALS", "--", "true"], "bad_request");
    expect_error(
        &["put", "/etc/hostname", "h:/tmp/x", "--mode", "9999"],
        "bad_request",
    );
}

/// A rejected call must not consume the pipeline's input first: an agent that
/// pipes a payload into a command with a typo should get the typo back, not a
/// hang on an idle pipe and not a silently swallowed payload.
#[test]
fn arguments_are_rejected_before_stdin_is_read() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rx"))
        .args(["run", "h", "--env", "NOEQUALS", "--", "true"])
        // An open pipe nothing ever writes to or closes — what an agent harness
        // hands over when it is not redirecting stdin.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rx should be runnable");

    let start = std::time::Instant::now();
    let mut done = None;
    while start.elapsed() < std::time::Duration::from_secs(10) {
        if let Some(status) = child.try_wait().expect("wait should work") {
            done = Some(status);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let status = match done {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("rx blocked on stdin for a call it was going to reject");
        }
    };
    assert_eq!(status.code(), Some(1));
}

/// clap rejects what rx never sees. The contract calls that exit 2, and stdout
/// stays empty — the one case where there is no object, because there was no
/// call to answer.
#[test]
fn unparseable_invocations_exit_two_with_empty_stdout() {
    for args in [
        vec!["bogus-subcommand"],
        vec!["run"],                        // missing HOST and COMMAND
        vec!["stdout", "host"],             // missing ID
        vec!["put", "--mode", "0644"],      // missing both positionals
        vec!["--auto-deploy=bogus", "ping", "h"], // not one of the three choices
        vec!["--compact", "--pretty", "ping", "h"], // mutually exclusive
    ] {
        let out = rx(&args);
        assert_eq!(out.status.code(), Some(2), "for {args:?}");
        assert!(
            out.stdout.is_empty(),
            "stdout must stay empty for {args:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

/// Daemon control is a command like any other: it answers with an object, and
/// asking for a state that already holds is success, not failure.
#[test]
fn daemon_control_answers_with_an_object() {
    let out = rx(&["--compact", "daemon", "status"]);
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("daemon status must print one JSON object");
    assert_eq!(value["type"], "daemon");
    assert_eq!(value["action"], "status");
    assert!(value["running"].is_boolean());

    // Stopping a daemon that is not running is the requested state.
    let out = rx(&["--compact", "daemon", "stop"]);
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("daemon stop must print one JSON object");
    assert_eq!(value["type"], "daemon");
    if value["running"] == serde_json::json!(false) && value["changed"] == serde_json::json!(false) {
        assert_eq!(out.status.code(), Some(0), "no-op stop must succeed");
    }
}

/// Output that is not going to a terminal is compact by default: an agent
/// should not have to know a flag exists to stop paying for indentation.
#[test]
fn piped_output_is_compact_without_being_asked() {
    // `Command::output()` gives rx a pipe, not a tty — the agent's situation.
    let out = rx(&["daemon", "status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim().lines().count(),
        1,
        "piped JSON should be one line: {stdout}"
    );

    // …and --pretty still overrides it.
    let out = rx(&["--pretty", "daemon", "status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().lines().count() > 1,
        "--pretty must win over the pipe default: {stdout}"
    );
}

/// The guide is the product of `rx skill`, so it is text — and it must say which
/// binary produced it, so a stale copy can be recognised as one.
#[test]
fn skill_prints_the_guide_stamped_with_its_version() {
    let out = rx(&["skill"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "the skill file must carry its version"
    );
    assert!(
        !text.contains("{{VERSION}}"),
        "the version placeholder must be substituted"
    );
}
