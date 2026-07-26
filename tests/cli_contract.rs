//! The output contract, enforced.
//!
//! rx promises exactly one JSON object per invocation, on stdout, carrying a
//! typed `code` when it is an error. That promise was documented long before
//! anything checked it, and nineteen call sites had drifted to bare stderr text
//! by 0.3.1 — an agent that had been told to parse stdout got an empty stream
//! and no way to tell *why* the call failed.
//!
//! Most cases here are client-side rejections: no host is contacted, so they run
//! anywhere, with no network and no rxd. The few transport-shaped cases install a
//! fake `ssh` at the front of PATH and still never leave the machine.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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

/// The exit status a given error code answers with: 2 when the *call* is
/// unusable as written, 1 when a usable call failed.
///
/// Spelled out here rather than imported so the test states the rule
/// independently of the implementation it is checking.
fn expected_exit(code: &str) -> i32 {
    match code {
        "bad_request" | "bad_host" | "invalid_process_id" => 2,
        _ => 1,
    }
}

/// Assert the invocation failed with a typed error object on stdout, and return
/// the parsed object.
fn expect_error(args: &[&str], code: &str) -> serde_json::Value {
    let out = rx(args);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(
        out.status.code(),
        Some(expected_exit(code)),
        "expected exit {} for {args:?}\nstdout: {stdout}\nstderr: {}",
        expected_exit(code),
        String::from_utf8_lossy(&out.stderr)
    );

    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout must be one JSON object for {args:?}: {e}\nstdout: {stdout}")
    });
    assert_eq!(value["type"], "error", "for {args:?}");
    assert_eq!(value["code"], code, "for {args:?}: {value}");
    value
}

/// A value rx refuses is exit 2, exactly like a flag clap refuses.
///
/// The two are one class of mistake — the command line cannot be used — and
/// which of them notices is an implementation seam: `--mode` is parsed by hand
/// today and could be a clap `value_parser` tomorrow. Deriving the status from
/// the code is what keeps that refactor invisible to a caller, the same rule
/// `invalid_process_id` already follows across rx and rxd.
#[test]
fn an_unusable_argument_exits_two_wherever_it_was_caught() {
    for args in [
        vec!["put", "/etc/hostname", "somehost"],
        vec!["run", "h", "--env", "NOEQUALS", "--", "true"],
        vec!["put", "/etc/hostname", "h:/tmp/x", "--mode", "9999"],
        vec!["run", "-oProxyCommand=touch /tmp/pwn", "--", "true"],
        vec!["status", "h", "NOTANID"],
    ] {
        let out = rx(&args);
        assert_eq!(out.status.code(), Some(2), "for {args:?}");
    }

    // …and a usable call that failed stays exit 1. `deploy --offline` against an
    // empty cache needs no host and no network, so this runs anywhere.
    let out = rx(&[
        "deploy",
        "h",
        "--offline",
        "--binary",
        "/nonexistent/rxd-build",
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a well-formed call that failed is exit 1: {}",
        String::from_utf8_lossy(&out.stdout)
    );
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
    expect_error(
        &["run", "h", "--env", "NOEQUALS", "--", "true"],
        "bad_request",
    );
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
    assert_eq!(status.code(), Some(2));
}

/// clap rejects what rx never sees. The contract calls that exit 2, and stdout
/// stays empty — the one case where there is no object, because there was no
/// call to answer.
#[test]
fn unparseable_invocations_exit_two_with_empty_stdout() {
    for args in [
        vec!["bogus-subcommand"],
        vec!["run"],                                         // missing HOST and COMMAND
        vec!["stdout", "host"],                              // missing ID
        vec!["put", "--mode", "0644"],                       // missing both positionals
        vec!["run", "h", "--timeout", "soon", "--", "true"], // not a number
        vec!["--compact", "--pretty", "ping", "h"],          // mutually exclusive
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
    // No `changed` on a query. The idempotence rule is about verbs that ensure
    // a state; `status` changes nothing, so the field could only ever be false
    // and would invite a caller to branch on something that cannot happen.
    assert!(
        value.get("changed").is_none(),
        "a query must not report `changed`: {value}"
    );

    // Stopping a daemon that is not running is the requested state.
    let out = rx(&["--compact", "daemon", "stop"]);
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("daemon stop must print one JSON object");
    assert_eq!(value["type"], "daemon");
    if value["running"] == serde_json::json!(false) && value["changed"] == serde_json::json!(false)
    {
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

fn fake_ssh_dir(script: &str, tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rx-fake-ssh-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fake ssh dir");
    let ssh = dir.join("ssh");
    std::fs::write(&ssh, script).expect("fake ssh script");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700))
        .expect("fake ssh executable");
    dir
}

fn path_with_fake_ssh(fake: &std::path::Path) -> String {
    let old = std::env::var("PATH").unwrap_or_default();
    format!("{}:{old}", fake.display())
}

fn rx_write_with_piped_stdin(fake: &std::path::Path, payload: &[u8], pipe_status: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rx"))
        .args(["--compact", "write", "h", "deadbeef"])
        .env("PATH", path_with_fake_ssh(fake))
        .env("RX_FAKE_STATUS", "running")
        .env("RX_FAKE_PIPE_STATUS", pipe_status)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rx should be runnable");

    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(payload)
        .expect("payload should be written to rx");
    child.wait_with_output().expect("rx should exit")
}

fn fake_ssh_for_piped_write() -> &'static str {
    r#"#!/bin/sh
mode=""
for arg in "$@"; do
    if [ "$arg" = "serve" ]; then
        mode="serve"
    fi
    if [ "$arg" = "pipe-stdin" ]; then
        mode="pipe"
    fi
done

if [ "$mode" = "serve" ]; then
    read request
    case "$RX_FAKE_STATUS" in
        running)
            printf '%s\n' '{"type":"status","id":"deadbeef","state":"running","cmd":"cat","started":1,"ended":null,"exit_code":null,"signal":null,"stdout_size":0,"stderr_size":0}'
            exit 0
            ;;
        not_found)
            printf '%s\n' '{"type":"error","message":"process not found: deadbeef","code":"process_not_found","retryable":false}'
            exit 0
            ;;
    esac
fi

if [ "$mode" = "pipe" ]; then
    cat >/dev/null
    exit "${RX_FAKE_PIPE_STATUS:-0}"
fi

exit 99
"#
}

#[test]
fn piped_write_reports_one_written_object() {
    let fake = fake_ssh_dir(fake_ssh_for_piped_write(), "write-ok");
    let out = rx_write_with_piped_stdin(&fake, b"payload", "0");

    assert_eq!(out.status.code(), Some(0));
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("piped write must print one object");
    assert_eq!(value["type"], "written", "{value}");
    assert_eq!(value["bytes"], 7, "{value}");
    assert!(
        out.stderr.is_empty(),
        "fake ssh wrote no stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(fake);
}

#[test]
fn piped_write_remote_failure_is_a_typed_error_not_silent_success() {
    let fake = fake_ssh_dir(fake_ssh_for_piped_write(), "write-fail");
    let out = rx_write_with_piped_stdin(&fake, b"payload", "1");

    assert_eq!(out.status.code(), Some(1));
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("piped write failure must print one object");
    assert_eq!(value["type"], "error", "{value}");
    assert_eq!(value["code"], "process_exited", "{value}");
    let _ = std::fs::remove_dir_all(fake);
}

#[test]
fn piped_write_preflight_keeps_remote_error_codes() {
    let fake = fake_ssh_dir(fake_ssh_for_piped_write(), "write-preflight");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rx"))
        .args(["--compact", "write", "h", "deadbeef"])
        .env("PATH", path_with_fake_ssh(&fake))
        .env("RX_FAKE_STATUS", "not_found")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rx should be runnable");
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("rx should exit");

    assert_eq!(out.status.code(), Some(1));
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("preflight failure must print one object");
    assert_eq!(value["type"], "error", "{value}");
    assert_eq!(value["code"], "process_not_found", "{value}");
    let _ = std::fs::remove_dir_all(fake);
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

/// `start --pipe` puts the remote process's bytes on stdout, so NOTHING else may
/// go there — the same rule `rxv get` holds, and for the same reason: whatever
/// consumes the pipe would otherwise receive a JSON error object as data.
///
/// This failed in 0.4.0. The success path wrote its handle to stderr correctly,
/// but every failure — the host check before dispatch, the transport
/// classifier, and an error answered by rxd — went to stdout. Each site looked
/// right on its own, which is why the routing is now one switch rather than
/// three decisions.
#[test]
fn a_piped_start_never_writes_an_object_to_stdout() {
    for args in [
        // Rejected by the host check in main, before anything is dispatched.
        vec!["start", "--pipe", "bad host", "--", "true"],
        // Rejected inside the start arm, after dispatch and before any SSH.
        vec!["start", "--pipe", "h", "--env", "NOEQUALS", "--", "true"],
    ] {
        let out = rx(&args);
        // Both are unusable calls, so both exit 2 — but the routing here is what
        // the test is about, and it is independent of the status: the object
        // goes to stderr because stdout is this command's byte stream.
        assert_eq!(out.status.code(), Some(2), "expected exit 2 for {args:?}");
        assert!(
            out.stdout.is_empty(),
            "stdout carries the process stream and must stay byte-empty for \
             {args:?}, got {:?}",
            String::from_utf8_lossy(&out.stdout)
        );

        let stderr = String::from_utf8_lossy(&out.stderr);
        let value: serde_json::Value = serde_json::from_str(stderr.trim())
            .unwrap_or_else(|e| panic!("stderr must be one typed object for {args:?}: {e}"));
        assert_eq!(value["type"], "error", "for {args:?}");
        assert!(value["code"].is_string(), "code is always present: {value}");
    }
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
    // An unknown subcommand, a missing required argument, a value the parser
    // cannot use and a surplus positional are one class of mistake and answer
    // the same way.
    expect_parse_error(&["nosuchcommand"]);
    expect_parse_error(&["run"]);
    expect_parse_error(&["run", "h", "--timeout", "soon", "--", "true"]);
    expect_parse_error(&["ping", "h1", "h2"]);
    expect_parse_error(&["--nosuchflag"]);
}

#[test]
fn the_parser_object_keeps_claps_own_wording() {
    // clap's diagnosis survives inside `message`, so nothing is lost by not
    // printing its prose separately.
    let value = expect_parse_error(&["setup"]);
    let message = value["message"].as_str().expect("message is a string");
    assert!(message.contains("unrecognized subcommand"), "{message}");
    // One line: a compact object is what a caller reads.
    assert!(
        !message.contains('\n'),
        "message must be one line: {message}"
    );
    // No ANSI escapes leaked into the JSON string.
    assert!(
        !message.contains('\u{1b}'),
        "message must be unstyled: {message}"
    );
}

/// `message` is one sentence and `hint` names a command — the shapes
/// `docs/CONTRACT.md` describes, not a flattened help page.
///
/// Flattening clap's whole rendering put a `Usage:` block and "For more
/// information, try '--help'" inside a field meant for one short sentence, on
/// the most-hit failure in the tool. The diagnosis and the suggestion are two
/// different things and the contract already has a field for each.
#[test]
fn the_parser_object_splits_the_diagnosis_from_the_fix() {
    // clap offers a tip here: '--nope' could have been meant as a value.
    let value = expect_parse_error(&["run", "h", "--nope", "--", "true"]);
    let message = value["message"].as_str().expect("message is a string");
    let hint = value["hint"].as_str().expect("hint is a string");

    assert!(message.contains("unexpected argument"), "{message}");
    for noise in ["Usage:", "For more information", "tip:"] {
        assert!(
            !message.contains(noise),
            "message must not carry {noise:?}: {message}"
        );
    }
    // The suggestion is not dropped — it moves to the field that means "here is
    // a different command to run".
    assert!(
        hint.contains("-- --nope"),
        "the tip belongs in hint: {hint}"
    );

    // With no tip to offer, the hint still points somewhere useful.
    let value = expect_parse_error(&["setup"]);
    let hint = value["hint"].as_str().expect("hint is a string");
    assert!(hint.contains("rx --help"), "{hint}");
}

/// The local daemon survives a client that hangs up on it.
///
/// This is the regression the first attempt at the SIGPIPE fix caused: restoring
/// the default disposition process-wide meant the forked daemon inherited it,
/// and `daemon start`'s own liveness probe — connect, then close — killed the
/// daemon it had just started with SIGPIPE on the reply. It looked like a
/// daemon that would not stay up, several layers from the change that did it.
///
/// Runs against an isolated `XDG_RUNTIME_DIR`, so it never touches a daemon the
/// developer is using.
#[test]
fn the_daemon_outlives_a_client_that_hangs_up() {
    let runtime = daemon_runtime("hangup");

    // stderr to /dev/null here, deliberately: this test is about the daemon's
    // lifetime, and a regression in the *stderr* behaviour should fail the test
    // below with a message rather than hang this one.
    let daemon = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_rx"))
            .args(args)
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .expect("rx should be runnable")
    };

    let started = daemon(&["--compact", "daemon", "start"]);
    let value: serde_json::Value =
        serde_json::from_slice(&started.stdout).expect("one object from daemon start");
    assert_eq!(value["running"], true, "{value}");

    // A second start is the probe that used to be fatal: it connects to the
    // daemon and closes. If the daemon died, this reports `changed:true` with a
    // new pid instead of the idempotent answer.
    let again = daemon(&["--compact", "daemon", "start"]);
    let value: serde_json::Value =
        serde_json::from_slice(&again.stdout).expect("one object from the second start");
    assert_eq!(
        value["changed"], false,
        "the daemon did not survive the first client: {value}"
    );

    let status = daemon(&["--compact", "daemon", "status"]);
    let value: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("one object from daemon status");
    assert_eq!(value["running"], true, "{value}");

    daemon(&["daemon", "stop"]);
    let _ = std::fs::remove_dir_all(&runtime);
}

/// An isolated `XDG_RUNTIME_DIR`, so a daemon test never touches the one the
/// developer is using. Tagged as well as pid-keyed because these tests run
/// concurrently inside one process.
fn daemon_runtime(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rx-daemon-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("runtime dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("0700 on the runtime dir");
    dir
}

/// `daemon start` returns to its caller, rather than holding the caller's
/// stderr for the daemon's whole life.
///
/// The forked daemon used to keep the stderr it inherited ("keep stderr for
/// logging", though nothing read it), so any caller that reads to EOF —
/// `Command::output()`, `subprocess.run(capture_output=True)`, most agent
/// harnesses — blocked until the daemon *exited*, on a command that had already
/// reported success. It now logs to `daemon.log` in its own base directory.
///
/// Read on a deadline in a thread: if this regresses, the read blocks forever,
/// and the point of the deadline is that the suite fails with this message
/// instead of hanging.
#[test]
fn daemon_start_does_not_hold_its_callers_stderr() {
    use std::io::Read;

    let runtime = daemon_runtime("stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rx"))
        .args(["--compact", "daemon", "start"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rx should be runnable");

    let mut stderr = child.stderr.take().expect("stderr is piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let drained = rx.recv_timeout(std::time::Duration::from_secs(15));

    // Stop the daemon before asserting, so a failure does not leave one behind.
    let _ = Command::new(env!("CARGO_BIN_EXE_rx"))
        .args(["daemon", "stop"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&runtime);

    assert!(
        drained.is_ok(),
        "the daemon is holding its caller's stderr open — reading it to EOF blocked"
    );
}

/// A closed stdout is an ordinary end to a pipeline, not a crash.
///
/// `rx run HOST -- <big output> | head` used to print a Rust panic banner and
/// exit 101 — outside the 0/1/2 the contract documents — but only once the
/// payload outgrew the pipe buffer, so small outputs hid it. The deterministic
/// half of this lives in rx's own unit tests, which drive the writer against a
/// consumer that is already gone; this one checks the real binary.
#[test]
fn a_closed_stdout_is_not_a_panic() {
    use std::io::Read;

    let mut child = Command::new(env!("CARGO_BIN_EXE_rx"))
        .args(["skill"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rx should be runnable");

    // Read one short chunk, then drop the pipe while rx is still writing.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut head = [0u8; 64];
    let _ = stdout.read(&mut head).expect("first chunk");
    drop(stdout);

    let out = child.wait_with_output().expect("rx should exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked"),
        "a closed stdout must not panic: {stderr}"
    );
    assert_ne!(
        out.status.code(),
        Some(101),
        "101 is a panic exit, and is not in the contract"
    );
}

#[test]
fn discovery_prints_for_a_reader_and_emits_no_object() {
    for args in [
        &["--help"][..],
        &["-h"][..],
        &["help"][..],
        &["--version"][..],
    ] {
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
