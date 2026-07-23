use std::fs;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rem_exec::{Encoding, base64_decode};
use serde_json::{Value, json};

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(0);

struct Runtime {
    dir: PathBuf,
}

impl Runtime {
    fn new(name: &str) -> Self {
        let id = NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rem-exec-rxd-test-{}-{id}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn remote_base(&self) -> PathBuf {
        self.dir.join("rem-exec")
    }

    /// Send one framed request (JSON line + optional raw body) to `rxd serve`.
    fn serve(&self, request: Value, body: &[u8]) -> Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rxd"))
            .arg("serve")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        {
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(request.to_string().as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
            if !body.is_empty() {
                stdin.write_all(body).unwrap();
            }
        }

        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "rxd serve failed\nrequest: {request}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "invalid JSON from rxd serve: {e}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    }

    fn status(&self, id: &str) -> Value {
        self.serve(json!({"action": "status", "id": id}), &[])
    }

    fn pipe_stdin(&self, id: &str, input: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rxd"))
            .args(["pipe-stdin", id])
            .env("XDG_RUNTIME_DIR", &self.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        child.stdin.as_mut().unwrap().write_all(input).unwrap();
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Decode a response text field per its declared encoding.
fn decode_field(value: &Value, data_key: &str, enc_key: &str) -> Vec<u8> {
    let data = value[data_key].as_str().unwrap();
    match serde_json::from_value::<Encoding>(value[enc_key].clone()).unwrap() {
        Encoding::Utf8 => data.as_bytes().to_vec(),
        Encoding::Base64 => base64_decode(data).unwrap(),
    }
}

fn started_id(response: &Value) -> String {
    assert_eq!(response["type"], "started", "{response}");
    response["id"].as_str().unwrap().to_string()
}

fn wait_for_exit(runtime: &Runtime, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let status = runtime.status(id);
        if status["state"].as_str() != Some("running") {
            return status;
        }
        last = status;
        thread::sleep(Duration::from_millis(25));
    }
    panic!("process {id} did not exit before timeout; last status: {last}");
}

#[test]
fn run_returns_exit_code_and_text_output_in_one_call() {
    let runtime = Runtime::new("run-basic");
    let resp = runtime.serve(
        json!({
            "action": "run",
            "command": ["sh", "-c", "printf 'out\\n'; printf 'err\\n' >&2; exit 42"],
        }),
        &[],
    );

    assert_eq!(resp["type"], "completed", "{resp}");
    assert_eq!(resp["exit_code"], 42);
    assert!(resp["signal"].is_null());
    // Text output is inlined verbatim (utf8), not base64 — no decode dance.
    assert_eq!(resp["stdout"], "out\n");
    assert_eq!(resp["stdout_encoding"], "utf8");
    assert_eq!(resp["stderr"], "err\n");
    assert_eq!(resp["stdout_truncated"], false);
}

#[test]
fn run_ephemeral_by_default_removes_process_dir() {
    let runtime = Runtime::new("run-ephemeral");
    let resp = runtime.serve(
        json!({"action": "run", "command": ["printf", "ok\n"]}),
        &[],
    );
    assert_eq!(resp["type"], "completed", "{resp}");
    let id = resp["id"].as_str().unwrap();

    // Process dir is gone — status is process_not_found.
    let status = runtime.status(id);
    assert_eq!(status["type"], "error", "{status}");
    assert_eq!(status["code"], "process_not_found");
    assert!(!runtime.remote_base().join(id).exists());
}

#[test]
fn run_keep_retains_process_dir() {
    let runtime = Runtime::new("run-keep");
    let resp = runtime.serve(
        json!({"action": "run", "command": ["printf", "ok\n"], "ephemeral": false}),
        &[],
    );
    assert_eq!(resp["type"], "completed", "{resp}");
    let id = resp["id"].as_str().unwrap();

    let status = runtime.status(id);
    assert_eq!(status["type"], "status", "{status}");
    assert_eq!(status["state"], "exited(0)");
    assert!(runtime.remote_base().join(id).exists());
}

#[test]
fn run_ephemeral_skips_when_truncated() {
    let runtime = Runtime::new("run-trunc");
    // Produce more than RUN_INLINE_CAP (256 KiB) so the response is truncated.
    let resp = runtime.serve(
        json!({
            "action": "run",
            "command": ["dd", "if=/dev/zero", "bs=1024", "count=300", "status=none"],
            "timeout_ms": 10_000,
        }),
        &[],
    );
    assert_eq!(resp["type"], "completed", "{resp}");
    assert_eq!(resp["stdout_truncated"], true, "{resp}");
    let id = resp["id"].as_str().unwrap();

    // Truncated → dir retained so the agent can drain with --offset.
    let status = runtime.status(id);
    assert_eq!(status["type"], "status", "{status}");
    assert!(runtime.remote_base().join(id).exists());
}

#[test]
fn run_ephemeral_keeps_dir_when_inlined_but_over_ephemeral_cap() {
    // Output between RUN_EPHEMERAL_CAP (16 KiB) and RUN_INLINE_CAP (256 KiB):
    // fully inlined (not truncated) yet possibly larger than the agent saw, so
    // the dir must be kept for re-paging rather than deleted.
    let runtime = Runtime::new("run-midband");
    let resp = runtime.serve(
        json!({
            "action": "run",
            "command": ["dd", "if=/dev/zero", "bs=1024", "count=64", "status=none"],
            "timeout_ms": 10_000,
        }),
        &[],
    );
    assert_eq!(resp["type"], "completed", "{resp}");
    assert_eq!(resp["stdout_truncated"], false, "{resp}");
    assert!(resp["stdout_size"].as_u64().unwrap() > 16 * 1024, "{resp}");
    let id = resp["id"].as_str().unwrap();

    // Kept: still readable, not process_not_found.
    let status = runtime.status(id);
    assert_eq!(status["type"], "status", "{status}");
    assert!(runtime.remote_base().join(id).exists());
}

#[test]
fn run_ephemeral_skips_when_backgrounded() {
    let runtime = Runtime::new("run-bg");
    let resp = runtime.serve(
        json!({"action": "run", "command": ["sleep", "10"], "timeout_ms": 150}),
        &[],
    );
    assert_eq!(resp["type"], "running", "{resp}");
    let id = resp["id"].as_str().unwrap();
    assert!(runtime.remote_base().join(id).exists());

    runtime.serve(json!({"action": "kill", "id": id}), &[]);
    wait_for_exit(&runtime, id);
}

#[test]
fn run_transports_shell_metacharacters_as_exact_argv() {
    // The payload never touches a shell: JSON stdin transport + direct execvp.
    // A shell-based transport would expand $(...), split on ; and |, and choke
    // on the embedded newline. Here the argument arrives byte-for-byte.
    let runtime = Runtime::new("run-escaping");
    let hostile = "a; b $(whoami) | c 'q' \"d\" \n tab\there";
    let resp = runtime.serve(
        json!({"action": "run", "command": ["printf", "%s", hostile]}),
        &[],
    );

    assert_eq!(resp["type"], "completed", "{resp}");
    assert_eq!(resp["exit_code"], 0);
    assert_eq!(resp["stdout"].as_str().unwrap(), hostile);
}

#[test]
fn run_applies_cwd() {
    let runtime = Runtime::new("run-cwd");
    let workdir = runtime.dir.join("workdir");
    fs::create_dir_all(&workdir).unwrap();
    let expected = fs::canonicalize(&workdir).unwrap();

    let resp = runtime.serve(
        json!({"action": "run", "command": ["pwd", "-P"], "cwd": workdir.to_str().unwrap()}),
        &[],
    );

    assert_eq!(resp["type"], "completed", "{resp}");
    assert_eq!(resp["exit_code"], 0);
    assert_eq!(resp["stdout"].as_str().unwrap().trim_end(), expected.to_str().unwrap());
}

#[test]
fn run_bad_cwd_fails_with_diagnostic() {
    let runtime = Runtime::new("run-cwd-bad");
    let resp = runtime.serve(
        json!({"action": "run", "command": ["true"], "cwd": "/no/such/dir/xyzzy"}),
        &[],
    );
    assert_eq!(resp["type"], "completed", "{resp}");
    assert_eq!(resp["exit_code"], 127);
    assert!(resp["stderr"].as_str().unwrap().contains("chdir"), "{resp}");
}

#[test]
fn run_reports_missing_command_as_typed_exec_error() {
    // A missing command must be distinguishable from a command that ran and
    // exited 127: exec_error is set, exit_code/signal are null, and stderr
    // explains why. The marker also surfaces in a later status query.
    let runtime = Runtime::new("run-nocmd");
    let resp = runtime.serve(
        json!({
            "action": "run",
            "command": ["rx-no-such-command-zzz", "arg"],
            "ephemeral": false,
        }),
        &[],
    );

    assert_eq!(resp["type"], "completed", "{resp}");
    assert!(resp["exit_code"].is_null(), "{resp}");
    assert!(resp["signal"].is_null(), "{resp}");
    assert_eq!(resp["exec_error"], "command_not_found", "{resp}");
    let stderr = resp["stderr"].as_str().unwrap();
    assert!(stderr.contains("rx: exec"), "{resp}");
    assert!(stderr.contains("rx-no-such-command-zzz"), "{resp}");

    let id = resp["id"].as_str().unwrap();
    let status = runtime.status(id);
    assert_eq!(status["type"], "status", "{status}");
    assert_eq!(status["state"], "exec_failed(command_not_found)", "{status}");
    assert!(status["exit_code"].is_null(), "{status}");
}

#[test]
fn start_reports_missing_command_via_status() {
    let runtime = Runtime::new("start-nocmd");
    let start = runtime.serve(
        json!({"action": "start", "command": ["rx-no-such-command-zzz"]}),
        &[],
    );
    let id = started_id(&start);

    let status = wait_for_exit(&runtime, &id);
    assert_eq!(status["state"], "exec_failed(command_not_found)", "{status}");
    assert!(status["exit_code"].is_null(), "{status}");
}

#[test]
fn ping_reports_identity_without_creating_state() {
    let runtime = Runtime::new("ping");
    let resp = runtime.serve(json!({"action": "ping"}), &[]);

    assert_eq!(resp["type"], "ping", "{resp}");
    assert!(!resp["version"].as_str().unwrap().is_empty(), "{resp}");
    assert_eq!(resp["protocol"], 2, "{resp}");
    // uname-derived fields are always present and non-empty on Linux.
    assert!(!resp["os"].as_str().unwrap().is_empty(), "{resp}");
    assert!(!resp["kernel"].as_str().unwrap().is_empty(), "{resp}");
    assert!(!resp["arch"].as_str().unwrap().is_empty(), "{resp}");
    assert!(resp.get("hostname").is_some(), "{resp}");

    // Ping is a pure probe: it answers before any state dir is created.
    assert!(!runtime.remote_base().exists(), "ping must not create state dir");
}

#[test]
fn run_applies_env_overrides() {
    let runtime = Runtime::new("run-env");
    let resp = runtime.serve(
        json!({"action": "run", "command": ["printenv", "RX_MYVAR"], "env": {"RX_MYVAR": "hello"}}),
        &[],
    );
    assert_eq!(resp["type"], "completed", "{resp}");
    assert_eq!(resp["stdout"], "hello\n");
}

#[test]
fn wait_blocks_until_exit() {
    let runtime = Runtime::new("wait-exit");
    let start = runtime.serve(
        json!({"action": "start", "command": ["sh", "-c", "sleep 0.2; exit 5"]}),
        &[],
    );
    let id = started_id(&start);

    let resp = runtime.serve(json!({"action": "wait", "id": id}), &[]);
    assert_eq!(resp["type"], "completed", "{resp}");
    assert_eq!(resp["exit_code"], 5);
}

#[test]
fn wait_times_out_to_running_handle() {
    let runtime = Runtime::new("wait-timeout");
    let start = runtime.serve(json!({"action": "start", "command": ["sleep", "10"]}), &[]);
    let id = started_id(&start);

    let resp = runtime.serve(json!({"action": "wait", "id": id, "timeout_ms": 150}), &[]);
    assert_eq!(resp["type"], "running", "{resp}");

    runtime.serve(json!({"action": "kill", "id": id}), &[]);
    wait_for_exit(&runtime, &id);
}

#[test]
fn wait_on_missing_process_is_typed_error() {
    let runtime = Runtime::new("wait-missing");
    let resp = runtime.serve(json!({"action": "wait", "id": "0123abcd"}), &[]);
    assert_eq!(resp["type"], "error");
    assert_eq!(resp["code"], "process_not_found");
}

#[test]
fn run_feeds_stdin_body_and_sends_eof() {
    let runtime = Runtime::new("run-stdin");
    let resp = runtime.serve(json!({"action": "run", "command": ["cat"]}), b"hello stdin");

    assert_eq!(resp["type"], "completed", "{resp}");
    assert_eq!(resp["exit_code"], 0);
    assert_eq!(resp["stdout"], "hello stdin");
}

#[test]
fn run_backgrounds_when_it_outlives_the_timeout() {
    let runtime = Runtime::new("run-timeout");
    let resp = runtime.serve(
        json!({"action": "run", "command": ["sleep", "10"], "timeout_ms": 200}),
        &[],
    );

    assert_eq!(resp["type"], "running", "{resp}");
    let id = resp["id"].as_str().unwrap();
    assert!(!id.is_empty());

    // The handle is live and can be killed.
    let kill = runtime.serve(json!({"action": "kill", "id": id}), &[]);
    assert_eq!(kill["type"], "killed");
    let status = wait_for_exit(&runtime, id);
    assert_eq!(status["state"], "exited(killed)");
}

#[test]
fn concurrent_fast_runs_never_self_heal_to_unknown() {
    // Regression for a status-file race: a torn read (empty file mid-write) or a
    // self-heal during the runner-pid write window could report a cleanly-exited
    // process as exited(unknown)/null. Hammer many fast runs concurrently; every
    // one must return its true exit code, never null.
    let runtime = Runtime::new("concurrent-exit");
    thread::scope(|scope| {
        for _ in 0..16 {
            scope.spawn(|| {
                for _ in 0..20 {
                    let resp = runtime.serve(
                        json!({"action": "run", "command": ["sh", "-c", "exit 7"]}),
                        &[],
                    );
                    assert_eq!(resp["type"], "completed", "{resp}");
                    assert_eq!(resp["exit_code"], 7, "{resp}");
                    assert!(resp["signal"].is_null(), "{resp}");
                }
            });
        }
    });
}

#[test]
fn run_reports_signal_termination_structurally() {
    let runtime = Runtime::new("run-signal");
    let resp = runtime.serve(
        json!({"action": "run", "command": ["sh", "-c", "kill -TERM $$"]}),
        &[],
    );

    assert_eq!(resp["type"], "completed", "{resp}");
    assert!(resp["exit_code"].is_null(), "{resp}");
    assert_eq!(resp["signal"], 15);
}

#[test]
fn start_status_read_roundtrip_via_serve() {
    let runtime = Runtime::new("start-read");
    let start = runtime.serve(
        json!({
            "action": "start",
            "command": ["sh", "-c", "printf 'out\\n'; printf 'err\\n' >&2; exit 7"],
        }),
        &[],
    );
    let id = started_id(&start);

    let status = wait_for_exit(&runtime, &id);
    assert_eq!(status["state"], "exited(7)");
    assert_eq!(status["exit_code"], 7);

    let stdout = runtime.serve(json!({"action": "read", "id": id, "stream": "stdout"}), &[]);
    assert_eq!(stdout["type"], "output");
    assert_eq!(stdout["encoding"], "utf8");
    assert_eq!(decode_field(&stdout, "data", "encoding"), b"out\n");
    assert_eq!(stdout["size"].as_u64(), Some(4));

    let stderr = runtime.serve(json!({"action": "read", "id": id, "stream": "stderr"}), &[]);
    assert_eq!(decode_field(&stderr, "data", "encoding"), b"err\n");
}

#[test]
fn write_body_transports_arbitrary_bytes() {
    let runtime = Runtime::new("write-binary");
    let start = runtime.serve(json!({"action": "start", "command": ["cat"]}), &[]);
    let id = started_id(&start);

    // Bytes that no shell-argv transport could carry: NUL and high bytes.
    let payload = [0u8, 1, 2, 0xff, 0xfe, b'\n', b'x'];
    let write = runtime.serve(json!({"action": "write", "id": id}), &payload);
    assert_eq!(write["type"], "written");
    assert_eq!(write["bytes"].as_u64(), Some(payload.len() as u64));

    let close = runtime.serve(json!({"action": "close_stdin", "id": id}), &[]);
    assert_eq!(close["type"], "written");

    let status = wait_for_exit(&runtime, &id);
    assert_eq!(status["state"], "exited(0)");

    let stdout = runtime.serve(json!({"action": "read", "id": id, "stream": "stdout"}), &[]);
    assert_eq!(stdout["encoding"], "base64"); // binary → base64
    assert_eq!(decode_field(&stdout, "data", "encoding"), payload);
}

#[test]
fn put_writes_file_atomically_with_mode() {
    let runtime = Runtime::new("put");
    let target = runtime.dir.join("out.conf");
    // Binary payload: NUL and high bytes no shell-argv transport could carry.
    let payload = b"key = value\nbinary:\x00\xff\x01\n";

    let resp = runtime.serve(
        json!({"action": "put", "path": target.to_str().unwrap(), "mode": 0o640}),
        payload,
    );

    assert_eq!(resp["type"], "copied", "{resp}");
    assert_eq!(resp["bytes"].as_u64(), Some(payload.len() as u64));
    assert_eq!(fs::read(&target).unwrap(), payload);
    let mode = fs::symlink_metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
    // No temp files left behind.
    let leftovers: Vec<_> = fs::read_dir(&runtime.dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".rxd-put-"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
}

#[test]
fn put_rejects_incomplete_transfer_without_installing_file() {
    let runtime = Runtime::new("put-incomplete");
    let target = runtime.dir.join("out.bin");
    // Declare far more bytes than the body carries: a short stream (as a dropped
    // connection produces) must be rejected, not renamed into place truncated.
    let payload = b"short body";
    let resp = runtime.serve(
        json!({"action": "put", "path": target.to_str().unwrap(), "size": 9_999}),
        payload,
    );

    assert_eq!(resp["type"], "error", "{resp}");
    assert_eq!(resp["code"], "incomplete_transfer", "{resp}");
    assert_eq!(resp["retryable"], true, "{resp}");
    assert!(!target.exists(), "a truncated file must not be installed");
    // The temp file must be cleaned up, not left behind.
    let leftovers: Vec<_> = fs::read_dir(&runtime.dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".rxd-put-"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
}

#[test]
fn put_accepts_matching_declared_size() {
    let runtime = Runtime::new("put-sized");
    let target = runtime.dir.join("out.bin");
    let payload = b"exactly these bytes\x00\xff";
    let resp = runtime.serve(
        json!({"action": "put", "path": target.to_str().unwrap(), "size": payload.len()}),
        payload,
    );

    assert_eq!(resp["type"], "copied", "{resp}");
    assert_eq!(resp["bytes"].as_u64(), Some(payload.len() as u64));
    assert_eq!(fs::read(&target).unwrap(), payload);
}

#[test]
fn put_to_missing_directory_errors_without_partial_file() {
    let runtime = Runtime::new("put-baddir");
    let resp = runtime.serve(
        json!({"action": "put", "path": "/no/such/dir/xyzzy/file"}),
        b"data",
    );
    assert_eq!(resp["type"], "error", "{resp}");
}

#[test]
fn invalid_process_id_is_rejected_with_typed_error() {
    let runtime = Runtime::new("invalid-id");
    let sentinel_dir = runtime.dir.join("sentinel");
    fs::create_dir_all(&sentinel_dir).unwrap();
    fs::write(sentinel_dir.join("stdout"), b"secret").unwrap();

    let read = runtime.serve(
        json!({"action": "read", "id": "../sentinel", "stream": "stdout"}),
        &[],
    );

    assert_eq!(read["type"], "error");
    assert_eq!(read["code"], "invalid_process_id");
    assert!(read.get("data").is_none());
}

#[test]
fn command_with_nul_byte_is_rejected() {
    let runtime = Runtime::new("nul-arg");
    let resp = runtime.serve(
        json!({"action": "run", "command": ["ec\u{0}ho", "hi"]}),
        &[],
    );
    assert_eq!(resp["type"], "error", "{resp}");
    assert_eq!(resp["code"], "bad_request");
}

#[test]
fn env_with_nul_byte_is_rejected() {
    let runtime = Runtime::new("nul-env");
    let resp = runtime.serve(
        json!({"action": "run", "command": ["true"], "env": {"FOO": "ba\u{0}r"}}),
        &[],
    );
    assert_eq!(resp["type"], "error", "{resp}");
    assert_eq!(resp["code"], "bad_request");
}

#[test]
fn invalid_stream_is_bad_request() {
    let runtime = Runtime::new("bad-stream");
    let start = runtime.serve(json!({"action": "start", "command": ["true"]}), &[]);
    let id = started_id(&start);
    let resp = runtime.serve(json!({"action": "read", "id": id, "stream": "bogus"}), &[]);
    assert_eq!(resp["type"], "error", "{resp}");
    assert_eq!(resp["code"], "bad_request");
}

#[test]
fn missing_process_reports_process_not_found_code() {
    let runtime = Runtime::new("not-found");
    let status = runtime.status("0123abcd");
    assert_eq!(status["type"], "error");
    assert_eq!(status["code"], "process_not_found");
}

#[test]
fn large_piped_stdin_is_not_truncated() {
    let runtime = Runtime::new("stdin-large");
    let start = runtime.serve(json!({"action": "start", "command": ["wc", "-c"]}), &[]);
    let id = started_id(&start);

    let input = vec![b'a'; 128 * 1024];
    let pipe = runtime.pipe_stdin(&id, &input);
    assert!(
        pipe.status.success(),
        "pipe-stdin failed\nstderr: {}",
        String::from_utf8_lossy(&pipe.stderr)
    );

    let status = wait_for_exit(&runtime, &id);
    assert_eq!(status["state"], "exited(0)");

    let stdout = runtime.serve(json!({"action": "read", "id": id, "stream": "stdout"}), &[]);
    assert_eq!(
        decode_field(&stdout, "data", "encoding"),
        format!("{}\n", input.len()).as_bytes()
    );
}

#[test]
fn process_state_paths_have_private_permissions() {
    let runtime = Runtime::new("permissions");
    let start = runtime.serve(json!({"action": "start", "command": ["sleep", "10"]}), &[]);
    let id = started_id(&start);
    let pdir = runtime.remote_base().join(&id);

    let dir_mode = fs::symlink_metadata(&pdir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700);

    let stdout_mode = fs::symlink_metadata(pdir.join("stdout"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(stdout_mode, 0o600);

    let fifo_meta = fs::symlink_metadata(pdir.join("stdin_pipe")).unwrap();
    assert!(fifo_meta.file_type().is_fifo());
    assert_eq!(fifo_meta.permissions().mode() & 0o777, 0o600);

    let kill = runtime.serve(json!({"action": "kill", "id": id}), &[]);
    assert_eq!(kill["type"], "killed");
    let status = wait_for_exit(&runtime, &id);
    assert_eq!(status["state"], "exited(killed)");
}
