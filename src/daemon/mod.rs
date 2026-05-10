pub mod server;
pub mod state;
pub mod stream;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::error::{RemExecError, Result};
use crate::protocol::{DaemonRequest, DaemonResponse};

/// Get the daemon socket path.
pub fn socket_path() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/tmp/rem-exec-{}", unsafe { libc::getuid() }));
    PathBuf::from(runtime_dir)
        .join("rem-exec")
        .join("daemon.sock")
}

/// Get the daemon PID file path.
pub fn pid_path() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/tmp/rem-exec-{}", unsafe { libc::getuid() }));
    PathBuf::from(runtime_dir)
        .join("rem-exec")
        .join("daemon.pid")
}

/// Get the local base directory for cached process output.
pub fn local_base() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/tmp/rem-exec-{}", unsafe { libc::getuid() }));
    PathBuf::from(runtime_dir).join("rem-exec").join("data")
}

/// Check if the daemon is running by attempting to connect to the socket.
pub fn is_running() -> bool {
    let sock = socket_path();
    UnixStream::connect(&sock).is_ok()
}

/// Send a request to the running daemon and get a response.
pub fn send_request(request: &DaemonRequest) -> Result<DaemonResponse> {
    let sock = socket_path();
    let mut stream = UnixStream::connect(&sock).map_err(|_| RemExecError::DaemonNotRunning)?;

    let payload = serde_json::to_vec(request)?;
    stream.write_all(&payload)?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;

    serde_json::from_slice(&buf).map_err(|e| {
        RemExecError::Protocol(format!(
            "invalid daemon response: {e}: {}",
            String::from_utf8_lossy(&buf)
        ))
    })
}
