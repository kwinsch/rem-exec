use std::fs;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rem_exec::base64_encode;
use serde_json::Value;

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

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rxd"))
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .output()
            .unwrap()
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

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "rxd {args:?} failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
            panic!(
                "invalid JSON from rxd {args:?}: {e}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn started_id(response: &Value) -> String {
    assert_eq!(response["type"], "started");
    response["id"].as_str().unwrap().to_string()
}

fn wait_for_exit(runtime: &Runtime, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = Value::Null;

    while Instant::now() < deadline {
        let status = runtime.json(&["status", id]);
        if status["state"].as_str() != Some("running") {
            return status;
        }
        last = status;
        thread::sleep(Duration::from_millis(25));
    }

    panic!("process {id} did not exit before timeout; last status: {last}");
}

#[test]
fn start_exit_and_read_stdout_stderr() {
    let runtime = Runtime::new("start-exit");
    let start = runtime.json(&[
        "start",
        "--",
        "sh",
        "-c",
        "printf 'out\\n'; printf 'err\\n' >&2; exit 42",
    ]);
    let id = started_id(&start);

    let status = wait_for_exit(&runtime, &id);
    assert_eq!(status["state"], "exited(42)");
    assert_eq!(
        status["cmd"],
        "sh -c printf 'out\\n'; printf 'err\\n' >&2; exit 42"
    );

    let stdout = runtime.json(&["read", &id, "stdout"]);
    assert_eq!(stdout["type"], "output");
    assert_eq!(stdout["data"], base64_encode(b"out\n"));
    assert_eq!(stdout["offset"].as_u64(), Some(0));
    assert_eq!(stdout["size"].as_u64(), Some(4));

    let stderr = runtime.json(&["read", &id, "stderr"]);
    assert_eq!(stderr["type"], "output");
    assert_eq!(stderr["data"], base64_encode(b"err\n"));
    assert_eq!(stderr["offset"].as_u64(), Some(0));
    assert_eq!(stderr["size"].as_u64(), Some(4));
}

#[test]
fn write_and_close_stdin() {
    let runtime = Runtime::new("stdin-write");
    let start = runtime.json(&["start", "--", "cat"]);
    let id = started_id(&start);

    let write = runtime.json(&["write", &id, "hello", "--raw"]);
    assert_eq!(write["type"], "written");
    assert_eq!(write["bytes"].as_u64(), Some(5));

    let close = runtime.json(&["close-stdin", &id]);
    assert_eq!(close["type"], "written");
    assert_eq!(close["bytes"].as_u64(), Some(0));

    let status = wait_for_exit(&runtime, &id);
    assert_eq!(status["state"], "exited(0)");

    let stdout = runtime.json(&["read", &id, "stdout"]);
    assert_eq!(stdout["type"], "output");
    assert_eq!(stdout["data"], base64_encode(b"hello"));
}

#[test]
fn large_piped_stdin_is_not_truncated() {
    let runtime = Runtime::new("stdin-large");
    let start = runtime.json(&["start", "--", "wc", "-c"]);
    let id = started_id(&start);

    let input = vec![b'a'; 128 * 1024];
    let pipe = runtime.pipe_stdin(&id, &input);
    assert!(
        pipe.status.success(),
        "pipe-stdin failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pipe.stdout),
        String::from_utf8_lossy(&pipe.stderr)
    );

    let status = wait_for_exit(&runtime, &id);
    assert_eq!(status["state"], "exited(0)");

    let stdout = runtime.json(&["read", &id, "stdout"]);
    assert_eq!(stdout["type"], "output");
    assert_eq!(
        stdout["data"],
        base64_encode(format!("{}\n", input.len()).as_bytes())
    );
}

#[test]
fn process_state_paths_have_private_permissions() {
    let runtime = Runtime::new("permissions");
    let start = runtime.json(&["start", "--", "sleep", "10"]);
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

    let stderr_mode = fs::symlink_metadata(pdir.join("stderr"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(stderr_mode, 0o600);

    let fifo_meta = fs::symlink_metadata(pdir.join("stdin_pipe")).unwrap();
    assert!(fifo_meta.file_type().is_fifo());
    assert_eq!(fifo_meta.permissions().mode() & 0o777, 0o600);

    let kill = runtime.json(&["kill", &id]);
    assert_eq!(kill["type"], "killed");
    let status = wait_for_exit(&runtime, &id);
    assert_eq!(status["state"], "exited(killed)");
}

#[test]
fn invalid_process_id_cannot_escape_runtime_directory() {
    let runtime = Runtime::new("invalid-id");
    let sentinel_dir = runtime.dir.join("sentinel");
    fs::create_dir_all(&sentinel_dir).unwrap();
    fs::write(sentinel_dir.join("stdout"), b"secret").unwrap();

    let read = runtime.json(&["read", "../sentinel", "stdout"]);

    assert_eq!(read["type"], "error");
    assert!(
        read["message"]
            .as_str()
            .unwrap()
            .contains("invalid process ID"),
        "{read}"
    );
    assert_ne!(
        read.get("data"),
        Some(&Value::String(base64_encode(b"secret")))
    );
}
