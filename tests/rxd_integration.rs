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
