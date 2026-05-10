use std::path::PathBuf;
use std::process::Command;

use crate::error::{RemExecError, Result};
use crate::protocol::{PROTOCOL_VERSION, Response};
use crate::ssh::{RemoteArgs, ssh_exec};

/// Result of a successful deployment.
pub struct DeployResult {
    pub host: String,
    pub arch: String,
    pub version: String,
}

/// Detect the remote host's CPU architecture via `ssh host uname -m`.
/// Maps to our supported arch names: x86_64, aarch64, riscv64.
pub fn detect_arch(host: &str) -> Result<String> {
    let output = Command::new("ssh")
        .arg(host)
        .args(["uname", "-m"])
        .output()
        .map_err(RemExecError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RemExecError::Ssh(format!(
            "failed to detect arch on {host}: {stderr}"
        )));
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    match raw.as_str() {
        "x86_64" => Ok("x86_64".to_string()),
        "aarch64" => Ok("aarch64".to_string()),
        "riscv64" => Ok("riscv64".to_string()),
        other => Err(RemExecError::Other(format!(
            "unsupported architecture on {host}: {other}"
        ))),
    }
}

/// Return the local directory where arch-specific rem-execd binaries are stored.
/// Default: ~/.local/share/rem-exec/bin/
pub fn binary_store_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".local/share/rem-exec/bin")
}

/// Return the path to the local rxd binary for the given architecture.
/// Errors if the binary does not exist.
fn binary_for_arch(arch: &str) -> Result<PathBuf> {
    let path = binary_store_dir().join(format!("rxd-{arch}"));
    if !path.exists() {
        return Err(RemExecError::Other(format!(
            "no binary for {arch} at {} — build with install.sh first",
            path.display()
        )));
    }
    Ok(path)
}

/// Deploy rem-execd to a remote host.
///
/// 1. Detect remote architecture via `uname -m`
/// 2. Find matching binary in local store
/// 3. Ensure ~/.local/bin exists on remote
/// 4. SCP binary to remote
/// 5. Verify with version check
pub fn deploy_to_host(host: &str) -> Result<DeployResult> {
    let arch = detect_arch(host)?;
    let local_binary = binary_for_arch(&arch)?;

    // Ensure remote directory exists
    let mkdir = Command::new("ssh")
        .arg(host)
        .args(["mkdir", "-p", ".local/bin"])
        .output()
        .map_err(RemExecError::Io)?;

    if !mkdir.status.success() {
        let stderr = String::from_utf8_lossy(&mkdir.stderr);
        return Err(RemExecError::Ssh(format!(
            "failed to create ~/.local/bin on {host}: {stderr}"
        )));
    }

    // SCP binary to remote
    let scp = Command::new("scp")
        .arg(local_binary.to_str().unwrap())
        .arg(format!("{host}:.local/bin/rxd"))
        .output()
        .map_err(RemExecError::Io)?;

    if !scp.status.success() {
        let stderr = String::from_utf8_lossy(&scp.stderr);
        return Err(RemExecError::Ssh(format!("scp to {host} failed: {stderr}")));
    }

    // Verify deployment with version check
    let version = verify_remote_version(host)?;

    Ok(DeployResult {
        host: host.to_string(),
        arch,
        version,
    })
}

/// Check the remote rem-execd version. Returns the version string on success.
fn verify_remote_version(host: &str) -> Result<String> {
    let args = RemoteArgs::version();
    let resp = ssh_exec(host, &args.as_str_slice())?;
    match resp {
        Response::Version {
            version, protocol, ..
        } => {
            if protocol != PROTOCOL_VERSION {
                return Err(RemExecError::Protocol(format!(
                    "remote protocol {protocol} != local {PROTOCOL_VERSION} after deploy"
                )));
            }
            Ok(version)
        }
        _ => Err(RemExecError::Protocol(
            "unexpected response to version command after deploy".to_string(),
        )),
    }
}

/// Check if auto-deploy should be attempted based on the error.
pub fn should_auto_deploy(err: &RemExecError) -> bool {
    match err {
        RemExecError::Ssh(msg) => {
            msg.contains("not found")
                || msg.contains("No such file")
                || msg.contains("Permission denied")
        }
        RemExecError::Protocol(_) => true,
        _ => false,
    }
}

/// Returns true if REM_EXEC_AUTO_DEPLOY=1 is set.
pub fn auto_deploy_enabled() -> bool {
    std::env::var("REM_EXEC_AUTO_DEPLOY")
        .map(|v| v == "1")
        .unwrap_or(false)
}
