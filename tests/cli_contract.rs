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

// ---------------------------------------------------------------------------
// Discovery vs. operations.
//
// The contract governs commands that DO something. Discovery — --help, -h,
// help, --version, skill, and a bare invocation — prints for a reader and emits
// no object, so a person's first keystroke is not answered with JSON. Anything
// else the parser rejects is an operation the caller got wrong and gets the
// typed object every other failure produces, on stderr with stdout left empty.
// ---------------------------------------------------------------------------

/// Assert a parser rejection: exit 2, stdout byte-empty, one typed object that
/// is the WHOLE of stderr (a caller doing `JSON.parse(stderr)` must not trip
/// over a usage block printed beside it).
fn expect_parse_error(args: &[&str]) -> serde_json::Value {
    let out = rx(args);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(2), "expected exit 2 for {args:?}");
    assert!(
        out.stdout.is_empty(),
        "stdout must stay byte-empty on exit 2 for {args:?}, got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let value: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap_or_else(|e| {
        panic!("stderr must be exactly one JSON object for {args:?}: {e}\nstderr: {stderr}")
    });
    assert_eq!(value["type"], "error", "for {args:?}");
    assert_eq!(value["code"], "bad_request", "for {args:?}: {value}");
    assert_eq!(value["retryable"], false, "for {args:?}");
    assert!(value["hint"].is_string(), "for {args:?}: {value}");
    value
}

#[test]
fn parser_rejections_are_typed_objects_on_stderr() {
    // An unknown subcommand, a missing required argument, a bad enum value and
    // a surplus positional are one class of mistake and answer the same way.
    expect_parse_error(&["nosuchcommand"]);
    expect_parse_error(&["run"]);
    expect_parse_error(&["--auto-deploy", "bogus", "ping", "h"]);
    expect_parse_error(&["ping", "h1", "h2"]);
    expect_parse_error(&["--nosuchflag"]);
}

#[test]
fn the_parser_object_keeps_claps_own_wording() {
    // clap's message (including its "tip: …" suggestions) survives inside
    // `message`, so nothing is lost by not printing its prose separately.
    let value = expect_parse_error(&["setup"]);
    let message = value["message"].as_str().expect("message is a string");
    assert!(message.contains("unrecognized subcommand"), "{message}");
    // One line: a compact object is what a caller reads.
    assert!(!message.contains('\n'), "message must be one line: {message}");
    // No ANSI escapes leaked into the JSON string.
    assert!(!message.contains('\u{1b}'), "message must be unstyled: {message}");
}

#[test]
fn discovery_prints_for_a_reader_and_emits_no_object() {
    for args in [&["--help"][..], &["-h"][..], &["help"][..], &["--version"][..]] {
        let out = rx(args);
        assert_eq!(out.status.code(), Some(0), "expected exit 0 for {args:?}");
        assert!(!out.stdout.is_empty(), "{args:?} must print to stdout");
        assert!(
            serde_json::from_slice::<serde_json::Value>(&out.stdout).is_err(),
            "{args:?} must not emit a JSON object"
        );
    }
}

#[test]
fn skill_is_discovery_too() {
    let out = rx(&["skill"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!out.stdout.is_empty(), "the guide goes to stdout");
    assert!(
        out.stderr.is_empty(),
        "skill emits no object: stderr must be empty, got {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A bare invocation is discovery — help, not an object — but it named no
/// operation, so it still fails. Answering a human's first keystroke with a
/// JSON blob is exactly what the discovery split exists to prevent.
#[test]
fn a_bare_invocation_prints_help_without_an_object() {
    let out = rx(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "exit 2 keeps stdout empty");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Usage:"), "help text expected: {stderr}");
    assert!(
        serde_json::from_str::<serde_json::Value>(stderr.trim()).is_err(),
        "a bare invocation must not emit a JSON object: {stderr}"
    );
}
