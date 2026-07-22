use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{RemExecError, Result};
use crate::protocol::{PROTOCOL_VERSION, Response};
use crate::ssh::{RemoteArgs, ssh_command, ssh_exec};

const RELEASE_BASE_URL: &str = "https://github.com/kwinsch/rem-exec/releases/download";
const SUPPORTED_ARCHES: &[&str] = &["x86_64", "aarch64", "riscv64"];

/// Result of a successful deployment.
pub struct DeployResult {
    pub host: String,
    pub arch: String,
    pub version: String,
}

/// Result of preparing the local deploy cache from GitHub release assets.
pub struct SetupResult {
    pub version: String,
    pub binaries: Vec<SetupBinary>,
}

pub struct SetupBinary {
    pub arch: String,
    pub path: PathBuf,
    pub sha256: String,
    pub status: SetupStatus,
}

pub enum SetupStatus {
    Cached,
    Installed,
}

/// Detect the remote host's CPU architecture via `ssh host uname -m`.
/// Maps to our supported arch names: x86_64, aarch64, riscv64.
pub fn detect_arch(host: &str) -> Result<String> {
    let output = ssh_command(host)
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
    match normalize_arch(&raw) {
        Some(arch) => Ok(arch.to_string()),
        other => Err(RemExecError::Other(format!(
            "unsupported architecture on {host}: {}",
            other.unwrap_or(raw.as_str())
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
            "no binary for {arch} at {} — run `rx setup` first",
            path.display(),
        )));
    }
    Ok(path)
}

/// Populate the local deploy cache with static rxd binaries from GitHub Releases.
pub fn setup_release_binaries(
    version: Option<&str>,
    requested_arches: &[String],
    force: bool,
) -> Result<SetupResult> {
    let version = normalize_version(version);
    let arches = normalize_requested_arches(requested_arches)?;
    let sums = download_text(&release_url(&version, "SHA256SUMS"))?;
    let store = binary_store_dir();
    fs::create_dir_all(&store)?;

    let mut binaries = Vec::new();
    for arch in arches {
        let asset = format!("rxd-{arch}");
        let expected = checksum_for_asset(&sums, &asset).ok_or_else(|| {
            RemExecError::Other(format!(
                "SHA256SUMS for {version} does not contain checksum for {asset}"
            ))
        })?;
        let dest = store.join(&asset);

        if !force && dest.exists() {
            let current = sha256_file(&dest)?;
            if current == expected {
                binaries.push(SetupBinary {
                    arch,
                    path: dest,
                    sha256: expected,
                    status: SetupStatus::Cached,
                });
                continue;
            }
        }

        let tmp = store.join(format!("{asset}.download-{}", std::process::id()));
        if tmp.exists() {
            fs::remove_file(&tmp)?;
        }
        download_file(&release_url(&version, &asset), &tmp)?;
        let actual = sha256_file(&tmp)?;
        if actual != expected {
            let _ = fs::remove_file(&tmp);
            return Err(RemExecError::Other(format!(
                "checksum mismatch for {asset}: expected {expected}, got {actual}"
            )));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        }
        fs::rename(&tmp, &dest)?;

        binaries.push(SetupBinary {
            arch,
            path: dest,
            sha256: expected,
            status: SetupStatus::Installed,
        });
    }

    Ok(SetupResult { version, binaries })
}

fn normalize_requested_arches(requested: &[String]) -> Result<Vec<String>> {
    if requested.is_empty() {
        return Ok(SUPPORTED_ARCHES.iter().map(|a| (*a).to_string()).collect());
    }

    let mut arches = Vec::new();
    for raw in requested {
        let arch = normalize_arch(raw).ok_or_else(|| {
            RemExecError::Other(format!(
                "unsupported architecture {raw}; supported: {}",
                SUPPORTED_ARCHES.join(", ")
            ))
        })?;
        if !arches.iter().any(|a| a == arch) {
            arches.push(arch.to_string());
        }
    }
    Ok(arches)
}

fn normalize_arch(raw: &str) -> Option<&'static str> {
    match raw {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        "riscv64" | "riscv64gc" => Some("riscv64"),
        _ => None,
    }
}

fn normalize_version(version: Option<&str>) -> String {
    match version {
        Some(v) if v.starts_with('v') => v.to_string(),
        Some(v) => format!("v{v}"),
        None => format!("v{}", env!("CARGO_PKG_VERSION")),
    }
}

fn release_url(version: &str, asset: &str) -> String {
    format!("{RELEASE_BASE_URL}/{version}/{asset}")
}

fn checksum_for_asset(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let checksum = parts.next()?;
        let filename = parts.next()?;
        (filename == asset).then(|| checksum.to_string())
    })
}

fn download_text(url: &str) -> Result<String> {
    let output = Command::new("curl")
        .args(["-fsSL", url])
        .output()
        .map_err(RemExecError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RemExecError::Other(format!(
            "failed to download {url}: {stderr}"
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn download_file(url: &str, dest: &Path) -> Result<()> {
    let dest = dest.to_str().ok_or_else(|| {
        RemExecError::Other(format!("invalid destination path {}", dest.display()))
    })?;
    let output = Command::new("curl")
        .args(["-fsSL", url, "-o", dest])
        .output()
        .map_err(RemExecError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RemExecError::Other(format!(
            "failed to download {url}: {stderr}"
        )));
    }

    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(RemExecError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RemExecError::Other(format!(
            "sha256sum failed for {}: {stderr}",
            path.display()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
        .ok_or_else(|| {
            RemExecError::Other(format!("sha256sum returned no hash for {}", path.display()))
        })
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
    let mkdir = ssh_command(host)
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
///
/// Covers a missing rxd ("not found"/"No such file") and an rxd too old to
/// understand `serve` (clap emits "unrecognized subcommand"/"unexpected
/// argument"); a protocol error (unparseable JSON) also implies a version
/// mismatch. Deliberately does NOT match "Permission denied" — that fires on
/// SSH auth failure, where a redeploy would fail the same way.
pub fn should_auto_deploy(err: &RemExecError) -> bool {
    match err {
        RemExecError::Ssh(msg) => {
            msg.contains("not found")
                || msg.contains("No such file")
                || msg.contains("unrecognized subcommand")
                || msg.contains("unexpected argument")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_release_versions() {
        assert_eq!(normalize_version(Some("v0.1.1")), "v0.1.1");
        assert_eq!(normalize_version(Some("0.1.1")), "v0.1.1");
    }

    #[test]
    fn normalizes_supported_arches() {
        assert_eq!(normalize_arch("x86_64"), Some("x86_64"));
        assert_eq!(normalize_arch("amd64"), Some("x86_64"));
        assert_eq!(normalize_arch("arm64"), Some("aarch64"));
        assert_eq!(normalize_arch("riscv64gc"), Some("riscv64"));
        assert_eq!(normalize_arch("sparc64"), None);
    }

    #[test]
    fn parses_checksum_for_asset() {
        let sums = "\
aaaaaaaa  rxd-x86_64
bbbbbbbb  rx-x86_64
cccccccc  rxd-aarch64
";
        assert_eq!(
            checksum_for_asset(sums, "rxd-aarch64"),
            Some("cccccccc".to_string())
        );
        assert_eq!(checksum_for_asset(sums, "rxd-riscv64"), None);
    }
}
