use std::io::Write;
use std::process::{Child, Command, Output, Stdio};

use crate::deploy;
use crate::error::{RemExecError, Result};
use crate::process::remote_base;
use crate::protocol::{ErrorCode, Request, Response};

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
    serve_with_body(host, request, |stdin| {
        if body.is_empty() {
            Ok(())
        } else {
            stdin.write_all(body)
        }
    })
}

/// Like [`serve_request`] but streams the body from a reader (used by `put`),
/// so large files never sit fully in memory on either side.
pub fn serve_request_stream(
    host: &str,
    request: &Request,
    body: &mut dyn std::io::Read,
) -> Result<Response> {
    serve_with_body(host, request, |stdin| {
        std::io::copy(body, stdin).map(|_| ())
    })
}

/// Like [`serve_request_stream`] but frames the body (see [`crate::framing`]),
/// so a receiver can tell a finished stream from a severed one without knowing
/// the length in advance. Pairs with [`Request::PutStream`].
pub fn serve_request_framed(
    host: &str,
    request: &Request,
    body: &mut dyn std::io::Read,
) -> Result<Response> {
    serve_with_body(host, request, |stdin| {
        crate::framing::write_framed(body, stdin).map(|_| ())
    })
}

/// Send one framed request whose body is produced by `write_body`, then decode
/// the single JSON response.
///
/// Body-write failures are deliberately not fatal on their own. When rxd
/// answers early and exits — an unwritable target directory, say — it closes
/// the pipe and our write dies with EPIPE partway through a large body. That
/// EPIPE is a symptom; rxd's typed response (or ssh's stderr, which is what
/// tells auto-deploy that rxd is missing) is the actual cause, and reporting
/// the symptom instead made error quality depend on whether the payload
/// happened to fit in the pipe buffer.
fn serve_with_body(
    host: &str,
    request: &Request,
    write_body: impl FnOnce(&mut std::process::ChildStdin) -> std::io::Result<()>,
) -> Result<Response> {
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');

    let mut child = ssh_command(host)
        .arg(REMOTE_BIN)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RemExecError::Io)?;

    let write_result = {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        let r = stdin.write_all(&line).and_then(|()| write_body(&mut stdin));
        // Closes the write side (EOF) so rxd stops reading and answers.
        drop(stdin);
        r
    };

    let output = child.wait_with_output().map_err(RemExecError::Io)?;

    match (parse_serve_output(output), write_result) {
        // rxd explained itself — that beats any local write symptom.
        (Ok(resp @ Response::Error { .. }), _) => Ok(resp),
        (Ok(resp), Ok(())) => Ok(resp),
        // A success response we can't trust: the body never fully arrived, so
        // whatever rxd acted on is not what we meant to send.
        (Ok(_), Err(e)) => Err(RemExecError::Io(e)),
        (Err(transport_err), _) => Err(transport_err),
    }
}

/// Spawn `rxd serve` for a streaming *download*: write the request line, then
/// hand back the child so the caller reads the response (a JSON header line,
/// then raw bytes) from its stdout. The download mirror of
/// [`serve_request_stream`], where the payload comes back on stdout instead of
/// going up on stdin.
pub fn serve_stream_download(host: &str, request: &Request) -> Result<Child> {
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
        // Drop stdin (EOF) so rxd serve stops reading and starts responding.
    }

    Ok(child)
}

/// Classify an OpenSSH transport failure from its stderr text into a typed
/// [`ErrorCode`] the CLI can surface as JSON, so agents branch on `code` for
/// connectivity/auth failures the same way they do for rxd-side errors.
///
/// Returns `None` for text that doesn't clearly indicate an unreachable host or
/// an authentication failure (e.g. a host-key mismatch), leaving those as an
/// untyped transport error rather than mislabeling them.
pub fn classify_ssh_failure(stderr: &str) -> Option<ErrorCode> {
    let s = stderr.to_ascii_lowercase();
    // Authentication is unambiguous when present, so check it first.
    if s.contains("permission denied")
        || s.contains("publickey")
        || s.contains("too many authentication failures")
        || s.contains("no supported authentication methods")
    {
        return Some(ErrorCode::SshAuth);
    }
    if s.contains("connection refused")
        || s.contains("could not resolve")
        || s.contains("name or service not known")
        || s.contains("no route to host")
        || s.contains("network is unreachable")
        || s.contains("connection timed out")
        || s.contains("operation timed out")
    {
        return Some(ErrorCode::SshUnreachable);
    }
    None
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
        Err(e) => {
            // A clear connectivity/auth failure is not a deploy problem — surface
            // it directly (the CLI turns it into ssh_unreachable / ssh_auth).
            if classify_ssh_failure(&e.to_string()).is_some() {
                return Err(e);
            }
            // Otherwise the usual cause is a missing/outdated rxd. Probe the
            // stable `version` command for a precise verdict rather than guessing
            // from the failure's stderr text.
            match deploy::remote_deploy_status(host) {
                deploy::DeployStatus::Current { .. } | deploy::DeployStatus::Unknown => Err(e),
                _ if deploy::policy().implicit_deploy() => {
                    let opts = deploy::DeployOpts::for_policy(deploy::policy());
                    deploy::deploy_to_host_with(host, &opts).map_err(|de| {
                        RemExecError::Ssh(format!(
                            "auto-deploy to {host} failed: {de} (original: {e})"
                        ))
                    })?;
                    serve_request(host, request, body)
                }
                status => Ok(deploy::not_deployed_response(host, &status)),
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ssh_failure_maps_known_phrases() {
        assert_eq!(
            classify_ssh_failure("ssh: Could not resolve hostname foo: Name or service not known"),
            Some(ErrorCode::SshUnreachable)
        );
        assert_eq!(
            classify_ssh_failure("connect to host x port 22: Connection refused"),
            Some(ErrorCode::SshUnreachable)
        );
        assert_eq!(
            classify_ssh_failure("ssh: connect to host x port 22: No route to host"),
            Some(ErrorCode::SshUnreachable)
        );
        assert_eq!(
            classify_ssh_failure("ssh: connect to host x port 22: Operation timed out"),
            Some(ErrorCode::SshUnreachable)
        );
        assert_eq!(
            classify_ssh_failure("foo@bar: Permission denied (publickey)."),
            Some(ErrorCode::SshAuth)
        );
        assert_eq!(
            classify_ssh_failure("Received disconnect from x: Too many authentication failures"),
            Some(ErrorCode::SshAuth)
        );
        // Host-key mismatch and unknown text stay untyped rather than mislabeled.
        assert_eq!(classify_ssh_failure("Host key verification failed."), None);
        assert_eq!(classify_ssh_failure("some unrelated failure"), None);
    }
}
