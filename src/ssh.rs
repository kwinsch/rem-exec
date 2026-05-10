use std::process::{Child, Command, Output, Stdio};

use crate::deploy;
use crate::error::{RemExecError, Result};
use crate::protocol::Response;

/// Execute a rem-execd command on a remote host via SSH.
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

/// Execute a rem-execd command with auto-deploy on failure.
///
/// When REM_EXEC_AUTO_DEPLOY=1 is set, detects "command not found" or protocol
/// errors, deploys the correct binary for the remote architecture, and retries once.
pub fn ssh_exec_auto_deploy(host: &str, args: &[&str]) -> Result<Response> {
    match ssh_exec(host, args) {
        Ok(resp) => Ok(resp),
        Err(e) if deploy::auto_deploy_enabled() && deploy::should_auto_deploy(&e) => {
            deploy::deploy_to_host(host).map_err(|de| {
                RemExecError::Ssh(format!(
                    "auto-deploy to {host} failed: {de} (original: {e})"
                ))
            })?;
            ssh_exec(host, args)
        }
        Err(e) => Err(e),
    }
}

/// The remote binary name. Uses ~/.local/bin path since it may not be in
/// the non-login SSH PATH.
const REMOTE_BIN: &str = ".local/bin/rxd";

/// Spawn an SSH process with stdin piped (for streaming data to remote).
/// Returns the child process handle. Caller writes to child.stdin and waits.
pub fn ssh_spawn_piped_stdin(host: &str, args: &[&str]) -> Result<Child> {
    let mut cmd = Command::new("ssh");
    cmd.arg(host);
    cmd.arg(REMOTE_BIN);
    cmd.args(args);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::inherit());
    cmd.spawn().map_err(RemExecError::Io)
}

/// Execute a raw SSH command, returning the Output.
pub fn ssh_raw(host: &str, args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new("ssh");
    cmd.arg(host);
    cmd.arg(REMOTE_BIN);
    cmd.args(args);
    cmd.output().map_err(RemExecError::Io)
}

/// Build rem-execd arguments for a given action.
/// Returns the args as owned Strings for lifetime convenience.
pub struct RemoteArgs {
    args: Vec<String>,
}

impl RemoteArgs {
    pub fn start(command: &[String]) -> Self {
        let mut args = vec!["start".to_string(), "--".to_string()];
        args.extend(command.iter().cloned());
        Self { args }
    }

    pub fn status(id: &str) -> Self {
        Self {
            args: vec!["status".to_string(), id.to_string()],
        }
    }

    pub fn read(id: &str, stream: &str, offset: Option<u64>, limit: Option<u64>) -> Self {
        let mut args = vec!["read".to_string(), id.to_string(), stream.to_string()];
        if let Some(off) = offset {
            args.push("--offset".to_string());
            args.push(off.to_string());
        }
        if let Some(lim) = limit {
            args.push("--limit".to_string());
            args.push(lim.to_string());
        }
        Self { args }
    }

    pub fn write(id: &str, input: &str, raw: bool) -> Self {
        let mut args = vec!["write".to_string(), id.to_string(), input.to_string()];
        if raw {
            args.push("--raw".to_string());
        }
        Self { args }
    }

    pub fn close_stdin(id: &str) -> Self {
        Self {
            args: vec!["close-stdin".to_string(), id.to_string()],
        }
    }

    pub fn kill(id: &str) -> Self {
        Self {
            args: vec!["kill".to_string(), id.to_string()],
        }
    }

    pub fn list() -> Self {
        Self {
            args: vec!["list".to_string()],
        }
    }

    pub fn clean() -> Self {
        Self {
            args: vec!["clean".to_string()],
        }
    }

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
