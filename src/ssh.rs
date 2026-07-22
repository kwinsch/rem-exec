use std::io::Write;
use std::process::{Child, Command, Output, Stdio};

use crate::deploy;
use crate::error::{RemExecError, Result};
use crate::process::remote_base;
use crate::protocol::{Request, Response};

/// The remote binary path. Uses ~/.local/bin since it may not be on the
/// non-login SSH PATH.
pub const REMOTE_BIN: &str = ".local/bin/rxd";

/// Build an `ssh` command to `host` with connection multiplexing enabled.
///
/// ControlMaster reuses one SSH connection across every rx operation to a host
/// (the poll-heavy background path especially), so only the first call pays the
/// handshake. `auto` falls back to a fresh connection if the master can't be
/// created, so this never makes rx fail.
pub fn ssh_command(host: &str) -> Command {
    let mut cmd = Command::new("ssh");
    let dir = remote_base().join("ssh");
    let _ = std::fs::create_dir_all(&dir);
    let control_path = dir.join("cm-%C");
    cmd.arg("-o")
        .arg("ControlMaster=auto")
        .arg("-o")
        .arg(format!("ControlPath={}", control_path.display()))
        .arg("-o")
        .arg("ControlPersist=30")
        .arg(host);
    cmd
}

/// Send one framed request to `rxd serve` and return the decoded response.
///
/// Writes the JSON request line plus optional raw body to the SSH channel's
/// stdin, then reads the single JSON response from stdout. rxd reads all of
/// stdin before emitting, so writing the whole body then reading cannot
/// deadlock even when the body exceeds the OS pipe buffer.
pub fn serve_request(host: &str, request: &Request, body: &[u8]) -> Result<Response> {
    let mut child = ssh_command(host)
        .arg(REMOTE_BIN)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RemExecError::Io)?;

    {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        stdin.write_all(&line)?;
        if !body.is_empty() {
            stdin.write_all(body)?;
        }
        // Dropping stdin closes the write side (EOF) so rxd stops reading.
    }

    parse_serve_output(child.wait_with_output().map_err(RemExecError::Io)?)
}

/// Like [`serve_request`] but streams the body from a reader (used by `cp`),
/// so large files never sit fully in memory on either side.
pub fn serve_request_stream(
    host: &str,
    request: &Request,
    body: &mut dyn std::io::Read,
) -> Result<Response> {
    let mut child = ssh_command(host)
        .arg(REMOTE_BIN)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RemExecError::Io)?;

    {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        stdin.write_all(&line)?;
        std::io::copy(body, &mut stdin).map_err(RemExecError::Io)?;
    }

    parse_serve_output(child.wait_with_output().map_err(RemExecError::Io)?)
}

/// Validate an `ssh ... serve` invocation and decode its single JSON response.
fn parse_serve_output(output: Output) -> Result<Response> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RemExecError::Ssh(format!(
            "SSH exited with {}: {stderr}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    if stdout.is_empty() {
        return Err(RemExecError::Ssh("empty response from remote".to_string()));
    }

    serde_json::from_str(stdout)
        .map_err(|e| RemExecError::Protocol(format!("invalid JSON from remote: {e}: {stdout}")))
}

/// [`serve_request`] with auto-deploy: when REM_EXEC_AUTO_DEPLOY=1 and the error
/// looks like a missing/old rxd, deploy the correct binary and retry once.
pub fn serve_request_auto_deploy(host: &str, request: &Request, body: &[u8]) -> Result<Response> {
    match serve_request(host, request, body) {
        Ok(resp) => Ok(resp),
        Err(e) if deploy::auto_deploy_enabled() && deploy::should_auto_deploy(&e) => {
            deploy::deploy_to_host(host)
                .map_err(|de| RemExecError::Ssh(format!("auto-deploy to {host} failed: {de} (original: {e})")))?;
            serve_request(host, request, body)
        }
        Err(e) => Err(e),
    }
}

/// Execute a rxd command over SSH via argv (used only for the `version`
/// bootstrap handshake, whose arguments are fixed and shell-safe).
pub fn ssh_exec(host: &str, args: &[&str]) -> Result<Response> {
    let output = ssh_raw(host, args)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RemExecError::Ssh(format!(
            "SSH exited with {}: {stderr}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    if stdout.is_empty() {
        return Err(RemExecError::Ssh("empty response from remote".to_string()));
    }

    serde_json::from_str(stdout)
        .map_err(|e| RemExecError::Protocol(format!("invalid JSON from remote: {e}: {stdout}")))
}

/// Spawn an SSH process with stdin piped (raw streaming to remote).
pub fn ssh_spawn_piped_stdin(host: &str, args: &[&str]) -> Result<Child> {
    let mut cmd = ssh_command(host);
    cmd.arg(REMOTE_BIN);
    cmd.args(args);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::inherit());
    cmd.spawn().map_err(RemExecError::Io)
}

/// Execute a raw SSH command, returning the Output.
pub fn ssh_raw(host: &str, args: &[&str]) -> Result<Output> {
    let mut cmd = ssh_command(host);
    cmd.arg(REMOTE_BIN);
    cmd.args(args);
    cmd.output().map_err(RemExecError::Io)
}

/// Raw-streaming rxd argument builders (follow / pipe-stdin) and the `version`
/// bootstrap. Structured actions go through [`serve_request`] instead.
pub struct RemoteArgs {
    args: Vec<String>,
}

impl RemoteArgs {
    pub fn version() -> Self {
        Self {
            args: vec!["version".to_string()],
        }
    }

    pub fn follow(id: &str) -> Self {
        Self {
            args: vec!["follow".to_string(), id.to_string(), "stdout".to_string()],
        }
    }

    pub fn pipe_stdin(id: &str, no_close: bool) -> Self {
        let mut args = vec!["pipe-stdin".to_string(), id.to_string()];
        if no_close {
            args.push("--no-close".to_string());
        }
        Self { args }
    }

    pub fn as_str_slice(&self) -> Vec<&str> {
        self.args.iter().map(|s| s.as_str()).collect()
    }
}
