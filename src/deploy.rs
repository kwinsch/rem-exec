use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{RemExecError, Result};
use crate::protocol::{ErrorCode, PROTOCOL_VERSION, Response};
use crate::ssh::{RemoteArgs, ssh_command, ssh_exec};

const RELEASE_BASE_URL: &str = "https://github.com/kwinsch/rem-exec/releases/download";
const SUPPORTED_ARCHES: &[&str] = &["x86_64", "aarch64", "riscv64", "armv7"];

/// How much rx may do on its own when a host's rxd is missing or mismatched.
///
/// rx and rxd are two halves of one wire protocol, so the only correct rxd for
/// a given rx is its own version — there is no separate version to pin, and the
/// pin is the rx binary itself. What an operator actually wants to control is
/// *when remote hosts change*, which is what this selects. The default is
/// [`DeployPolicy::Off`]: rx never touches a host you did not tell it to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeployPolicy {
    /// Never deploy as a side effect. A mismatch is reported as `not_deployed`
    /// with the command that fixes it.
    #[default]
    Off,
    /// May deploy from the local cache during another command, but never
    /// downloads. For hosts that should self-heal from a pre-seeded cache with
    /// no network access at run time.
    Local,
    /// May fetch the matching rxd and deploy it during another command.
    On,
}

impl DeployPolicy {
    /// Whether rx may deploy as a side effect of some other command.
    pub fn implicit_deploy(self) -> bool {
        !matches!(self, DeployPolicy::Off)
    }

    /// Whether rx may download a missing binary without being asked to.
    pub fn implicit_fetch(self) -> bool {
        matches!(self, DeployPolicy::On)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DeployPolicy::Off => "off",
            DeployPolicy::Local => "local",
            DeployPolicy::On => "on",
        }
    }
}

/// Parse a policy from a flag value or env var. `1`/`true`/`yes` map to `on`
/// so `REM_EXEC_AUTO_DEPLOY=1` keeps meaning what it did in 0.2.x.
pub fn parse_policy(value: &str) -> Option<DeployPolicy> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "0" | "false" | "no" | "" => Some(DeployPolicy::Off),
        "local" | "cache" => Some(DeployPolicy::Local),
        "on" | "1" | "true" | "yes" => Some(DeployPolicy::On),
        _ => None,
    }
}

static POLICY: std::sync::OnceLock<DeployPolicy> = std::sync::OnceLock::new();

/// Fix the policy for this process (from `--auto-deploy`). First call wins.
pub fn set_policy(policy: DeployPolicy) {
    let _ = POLICY.set(policy);
}

/// The effective policy: `--auto-deploy` if given, else `RX_AUTO_DEPLOY` (or
/// the older `REM_EXEC_AUTO_DEPLOY`), else off.
pub fn policy() -> DeployPolicy {
    if let Some(p) = POLICY.get() {
        return *p;
    }
    std::env::var("RX_AUTO_DEPLOY")
        .or_else(|_| std::env::var("REM_EXEC_AUTO_DEPLOY"))
        .ok()
        .and_then(|v| parse_policy(&v))
        .unwrap_or_default()
}

/// The rxd version this rx speaks to, as a release tag.
fn own_version() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// `(major, minor, patch)` for ordering. A `v` prefix and any pre-release or
/// build suffix are dropped, so `0.3.1-rc1` orders as `0.3.1` — coarse, but the
/// only question asked of it is "is this host behind us". Anything that is not
/// three numeric components yields `None` so callers decline to guess rather
/// than order two versions wrongly.
fn version_tuple(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Order a remote rxd version against this rx's own. `None` when either side
/// cannot be parsed — the caller must then take whichever action is safe under
/// uncertainty, not assume equality.
pub fn compare_to_own_version(remote: &str) -> Option<std::cmp::Ordering> {
    Some(version_tuple(remote)?.cmp(&version_tuple(env!("CARGO_PKG_VERSION"))?))
}

/// Whether a remote rxd is *provably* behind this rx. Used where the cost of
/// being wrong is a needless deploy, so an unorderable version means "leave it
/// alone".
pub fn is_older_than_own(remote: &str) -> bool {
    matches!(compare_to_own_version(remote), Some(std::cmp::Ordering::Less))
}

/// Whether a remote rxd is *provably* ahead of this rx.
pub fn is_newer_than_own(remote: &str) -> bool {
    matches!(
        compare_to_own_version(remote),
        Some(std::cmp::Ordering::Greater)
    )
}

/// Knobs for a deployment. Defaults suit an explicit `rx deploy`: fetch what is
/// needed, refuse to move a host backwards.
pub struct DeployOpts {
    /// Push this local binary instead of a cached release asset.
    pub binary: Option<PathBuf>,
    /// May download the matching release asset if the cache lacks it.
    pub allow_fetch: bool,
    /// Permit replacing an rxd that speaks a *newer* protocol than this rx.
    pub allow_downgrade: bool,
}

impl Default for DeployOpts {
    fn default() -> Self {
        Self {
            binary: None,
            allow_fetch: true,
            allow_downgrade: false,
        }
    }
}

impl DeployOpts {
    /// The opts an implicit (policy-driven) deploy runs with.
    pub fn for_policy(policy: DeployPolicy) -> Self {
        Self {
            binary: None,
            allow_fetch: policy.implicit_fetch(),
            allow_downgrade: false,
        }
    }
}

/// Result of a successful deployment.
pub struct DeployResult {
    pub host: String,
    pub arch: String,
    pub version: String,
    /// False when the host already ran this exact rxd and nothing was uploaded.
    /// "Ensure rxd is current" is what a caller actually wants, and a fleet-wide
    /// `rx deploy` should be able to say which hosts it touched.
    pub changed: bool,
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

/// Path a cached release asset lives at. The version is part of the name so an
/// upgraded rx can never deploy the previous rx's binary: an unversioned cache
/// passes an existence check and then fails *after* overwriting the remote.
fn cached_binary_path(version: &str, arch: &str) -> PathBuf {
    binary_store_dir().join(format!("rxd-{version}-{arch}"))
}

/// Locate the cached rxd matching this rx, fetching it when allowed.
///
/// Fetching during an explicit `rx deploy` is not "auto" anything — it finishes
/// the job that was asked for. It is refused when the caller says so
/// (`--offline`, or an implicit deploy under `--auto-deploy=local`), and then
/// the error names the exact command that fills the cache.
fn binary_for_arch(arch: &str, allow_fetch: bool) -> Result<PathBuf> {
    let version = own_version();
    let path = cached_binary_path(&version, arch);
    if path.exists() {
        return Ok(path);
    }
    if !allow_fetch {
        return Err(RemExecError::Other(format!(
            "no cached rxd {version} for {arch} at {} — run `rx cache fetch --arch {arch}` \
             (needs network), or pass --binary PATH to deploy a local build",
            path.display(),
        )));
    }

    setup_release_binaries(Some(&version), &[arch.to_string()], false)?;
    if !path.exists() {
        return Err(RemExecError::Other(format!(
            "rxd {version} for {arch} still missing at {} after fetch",
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
        let dest = cached_binary_path(&version, &arch);

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

        let tmp = store.join(format!("{asset}-{version}.download-{}", std::process::id()));
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
        // 32-bit ARMv7 hard-float. `uname -m` reports armv7l; armv6 is NOT
        // mapped (an armv7 binary won't run there) so it fails as unsupported.
        "armv7" | "armv7l" | "arm" | "armhf" => Some("armv7"),
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

/// The reason to refuse replacing `host`'s rxd, if it is ahead of this rx.
///
/// A host can be ahead two ways: a later wire protocol, or a later build of the
/// same protocol. Either means it belongs to a newer rx — someone else's, or a
/// later one of yours — and "fixing" this rx by overwriting it would break that
/// one. Only refuses when the remote is *provably* ahead: an unreadable version
/// must not block an explicit deploy, which is the very thing that repairs a
/// host in an unknown state.
fn downgrade_refusal(host: &str, status: &DeployStatus) -> Option<String> {
    let own = env!("CARGO_PKG_VERSION");
    match status {
        DeployStatus::Incompatible { version, protocol } if *protocol > PROTOCOL_VERSION => {
            Some(format!(
                "refusing to downgrade {host}: it runs rxd {version} (protocol {protocol}), \
                 newer than this rx {own} (protocol {PROTOCOL_VERSION}) — upgrade rx, or pass \
                 --allow-downgrade to overwrite it anyway"
            ))
        }
        DeployStatus::Current { version } | DeployStatus::Incompatible { version, .. } => {
            is_newer_than_own(&version).then(|| {
                format!(
                    "refusing to downgrade {host}: it runs rxd {version}, newer than this rx \
                     {own} — upgrade rx, or pass --allow-downgrade to overwrite it anyway"
                )
            })
        }
        _ => None,
    }
}

/// Deploy rem-execd to a remote host with default options (fetch if needed,
/// never downgrade).
pub fn deploy_to_host(host: &str) -> Result<DeployResult> {
    deploy_to_host_with(host, &DeployOpts::default())
}

/// Deploy rem-execd to a remote host.
///
/// 1. Refuse to move a host backwards unless told to
/// 2. Detect remote architecture via `uname -m`
/// 3. Find the matching binary locally (fetching it when allowed)
/// 4. Ensure ~/.local/bin exists on remote
/// 5. SCP binary to remote
/// 6. Verify with version check
pub fn deploy_to_host_with(host: &str, opts: &DeployOpts) -> Result<DeployResult> {
    // Probed once and reused by both the downgrade guard and the
    // already-current check below, so idempotence costs no extra round trip.
    let remote = remote_deploy_status(host);

    if !opts.allow_downgrade
        && let Some(refusal) = downgrade_refusal(host, &remote)
    {
        return Err(RemExecError::Other(refusal));
    }

    let arch = detect_arch(host)?;

    // Asking for a state that already holds is success, not work — the same
    // rule `rx daemon start` and `rxv unlock` follow. Version equality is the
    // test, not protocol equality: a same-protocol rxd can still be an older
    // build carrying rxd-side fixes, which is exactly the skew `ping` reports.
    //
    // An explicit --binary always uploads. A local build can carry the same
    // version string as the release and still be a different binary, and
    // pushing it is the whole point of the flag.
    if opts.binary.is_none()
        && let DeployStatus::Current { version } = &remote
        && version.trim_start_matches('v') == env!("CARGO_PKG_VERSION")
    {
        return Ok(DeployResult {
            host: host.to_string(),
            arch,
            version: version.clone(),
            changed: false,
        });
    }
    let local_binary = match &opts.binary {
        Some(path) => {
            if !path.exists() {
                return Err(RemExecError::Other(format!(
                    "no such binary: {}",
                    path.display()
                )));
            }
            path.clone()
        }
        None => binary_for_arch(&arch, opts.allow_fetch)?,
    };

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

    // Copy to a temp name, then rename into place. Two reasons, both real:
    // a failed mid-transfer scp must not leave a truncated binary at the live
    // path, and scp opens its target O_TRUNC in place — which returns ETXTBSY
    // when rxd is currently running, so an in-place deploy simply fails on a
    // busy host. rename(2) over a busy text file is allowed.
    //
    // The temp name carries our pid so two concurrent deploys to one host
    // cannot overwrite each other's upload.
    let binary_arg = local_binary.to_str().ok_or_else(|| {
        RemExecError::Other(format!("binary path is not valid UTF-8: {}", local_binary.display()))
    })?;
    let staged = format!(".local/bin/.rxd-deploy.{}.tmp", std::process::id());
    let scp = Command::new("scp")
        .arg("--")
        .arg(binary_arg)
        .arg(format!("{host}:{staged}"))
        .output()
        .map_err(RemExecError::Io)?;

    if !scp.status.success() {
        let stderr = String::from_utf8_lossy(&scp.stderr);
        return Err(RemExecError::Ssh(format!("scp to {host} failed: {stderr}")));
    }

    // Two plain argv commands rather than one `sh -c` string. ssh joins argv
    // with spaces and hands the result to the remote login shell, so a quoted
    // script would have to survive a round of shell parsing we do not control.
    // Every word here is a literal or the staged name (only [.a-z/-] plus our
    // own pid), which passes through that shell unchanged.
    let install = |args: &[&str]| -> Result<()> {
        let out = ssh_command(host).args(args).output().map_err(RemExecError::Io)?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Best effort: don't leave the upload behind on a failed install.
        let _ = ssh_command(host).args(["rm", "-f", &staged]).output();
        Err(RemExecError::Ssh(format!(
            "installing rxd on {host} failed at `{}`: {stderr}",
            args.join(" ")
        )))
    };
    install(&["chmod", "755", &staged])?;
    install(&["mv", "-f", &staged, ".local/bin/rxd"])?;

    // Verify deployment with version check
    let version = verify_remote_version(host)?;

    Ok(DeployResult {
        host: host.to_string(),
        arch,
        version,
        changed: true,
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
                // The copy succeeded but the binary is the wrong one — a
                // hand-picked --binary, or a stale cache entry.
                return Err(RemExecError::Protocol(format!(
                    "deployed rxd {version} on {host} speaks protocol {protocol}, but rx \
                     {} needs {PROTOCOL_VERSION} — the deployed binary is not the matching \
                     build (retry without --binary, or `rx cache fetch --force`)",
                    env!("CARGO_PKG_VERSION"),
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

/// Whether rx may deploy to `host` as a side effect of the current command, per
/// the effective [`policy`].
pub fn auto_deploy_enabled() -> bool {
    policy().implicit_deploy()
}

/// What the remote's rxd looks like, determined by probing the stable `version`
/// command. Lets a `serve` failure become a precise, actionable error instead of
/// a guess from clap/shell stderr wording.
#[derive(Debug)]
pub enum DeployStatus {
    /// rxd present and speaking our protocol.
    Current { version: String },
    /// rxd present but a different protocol than ours (older or newer).
    Incompatible { version: String, protocol: u32 },
    /// rxd absent, or too old/broken to answer `version` parseably.
    Missing,
    /// Couldn't tell — a connectivity/auth failure, not a deploy problem.
    Unknown,
}

/// Probe the remote rxd via the stable `version` command and classify it.
///
/// Robust across versions: a parseable answer yields an exact protocol number,
/// and an unparseable/absent one is treated as needing (re)deploy — never a
/// guess from clap/shell stderr. A connectivity/auth failure is reported as
/// `Unknown` so it is never mistaken for a deploy problem.
pub fn remote_deploy_status(host: &str) -> DeployStatus {
    match ssh_exec(host, &RemoteArgs::version().as_str_slice()) {
        Ok(Response::Version { version, protocol }) => {
            if protocol == PROTOCOL_VERSION {
                DeployStatus::Current { version }
            } else {
                DeployStatus::Incompatible { version, protocol }
            }
        }
        Ok(_) => DeployStatus::Missing,
        Err(e) => {
            if crate::ssh::classify_ssh_failure(&e.to_string()).is_some() {
                DeployStatus::Unknown
            } else {
                DeployStatus::Missing
            }
        }
    }
}

/// Build the actionable "remote rxd needs (re)deploying" error for the CLI to
/// print as JSON. Names the remote's state and both ways to fix it, so an agent
/// deploys (or sets RX_AUTO_DEPLOY) rather than falling back to raw ssh.
pub fn not_deployed_response(host: &str, status: &DeployStatus) -> Response {
    let detail = match status {
        DeployStatus::Incompatible { version, protocol } => format!(
            "remote rxd {version} on {host} speaks protocol {protocol}, but rx needs protocol {PROTOCOL_VERSION}"
        ),
        _ => format!("rxd is missing or unversioned on {host}"),
    };
    // One named fix first, the alternative second — the shape `empty_stream`
    // uses. This is the most-seen hint in the tool (every first contact with a
    // host), so it names RX_AUTO_DEPLOY: pointing an agent at the older
    // REM_EXEC_* spelling teaches the name we are moving away from.
    Response::error_code(ErrorCode::NotDeployed, detail).with_hint(format!(
        "run `rx deploy {host}` to install rxd {}; or --auto-deploy=on \
         (env RX_AUTO_DEPLOY=on) to deploy during a command instead",
        own_version(),
    ))
}

/// Collapse a subprocess's multi-line stderr into one line.
///
/// curl and scp report failures across several lines and with a trailing
/// newline; embedded in a JSON string that renders as `\n` in the middle of a
/// compact one-line object, which is exactly where a caller is reading it.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Turn a failed deploy into the same typed error shape every other command
/// produces.
///
/// A deploy failure used to be reported as `{"type":"deployed",
/// "status":"failed","error":"<raw curl output>"}` — a failure wearing a
/// success type, with no `code` to branch on, on the one path every new host
/// must cross. SSH-level failures keep their own codes (and therefore their own
/// retryability) by reusing the classifier the rest of rx uses, so an
/// unreachable host reports `ssh_unreachable` here exactly as it would
/// anywhere else.
pub fn deploy_error_response(host: &str, err: &RemExecError) -> Response {
    if let RemExecError::Ssh(detail) = err
        && let Some(code) = crate::ssh::classify_ssh_failure(detail)
    {
        return Response::error_code(code, one_line(&format!("deploy to {host} failed: {detail}")));
    }
    Response::error_code(
        ErrorCode::DeployFailed,
        one_line(&format!("deploy to {host} failed: {err}")),
    )
    .with_hint(
        "pass --binary PATH to push a local rxd build, or --offline to use only \
         what the deploy cache already has",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_failure_is_a_typed_error_not_a_deployed_object() {
        let err = RemExecError::Other("failed to download SHA256SUMS: curl: (22) 404\n".into());
        let response = deploy_error_response("host1", &err);
        let json = serde_json::to_value(&response).expect("serializes");

        assert_eq!(json["type"], "error");
        assert_eq!(json["code"], "deploy_failed");
        // A release that 404s does not appear on a second attempt.
        assert_eq!(json["retryable"], false);
        assert!(json["hint"].as_str().expect("hint").contains("--binary"));
    }

    /// The message reaches the caller as one line: a compact JSON object is
    /// what an agent reads, and a raw `\n` in the middle of it is noise.
    #[test]
    fn deploy_failure_message_is_collapsed_to_one_line() {
        let err = RemExecError::Other("failed to download:\ncurl: (22) 404\n".into());
        let response = deploy_error_response("host1", &err);
        let json = serde_json::to_value(&response).expect("serializes");

        let message = json["message"].as_str().expect("message");
        assert!(!message.contains('\n'), "message must be one line: {message:?}");
    }

    /// An unreachable host is not a deploy problem, and saying `deploy_failed`
    /// would send an agent looking for a missing release instead of retrying.
    #[test]
    fn ssh_transport_failures_keep_their_own_code_during_deploy() {
        let err = RemExecError::Ssh("ssh: Could not resolve hostname host1".into());
        let response = deploy_error_response("host1", &err);
        let json = serde_json::to_value(&response).expect("serializes");

        assert_eq!(json["code"], "ssh_unreachable");
        assert_eq!(json["retryable"], true);
    }

    #[test]
    fn policy_parses_flag_values_and_the_legacy_env_form() {
        assert_eq!(parse_policy("off"), Some(DeployPolicy::Off));
        assert_eq!(parse_policy("local"), Some(DeployPolicy::Local));
        assert_eq!(parse_policy("on"), Some(DeployPolicy::On));
        // REM_EXEC_AUTO_DEPLOY=1 kept meaning what it did in 0.2.x.
        assert_eq!(parse_policy("1"), Some(DeployPolicy::On));
        assert_eq!(parse_policy("0"), Some(DeployPolicy::Off));
        assert_eq!(parse_policy(" On "), Some(DeployPolicy::On));
        assert_eq!(parse_policy("sometimes"), None);
        // Default is the careful one: hosts never change as a side effect.
        assert_eq!(DeployPolicy::default(), DeployPolicy::Off);
    }

    #[test]
    fn policy_gates_side_effects_not_explicit_deploys() {
        assert!(!DeployPolicy::Off.implicit_deploy());
        assert!(DeployPolicy::Local.implicit_deploy());
        assert!(!DeployPolicy::Local.implicit_fetch());
        assert!(DeployPolicy::On.implicit_fetch());
    }

    #[test]
    fn version_ordering_handles_the_shapes_rxd_reports() {
        assert_eq!(version_tuple("0.3.1"), Some((0, 3, 1)));
        assert_eq!(version_tuple("v0.3.1"), Some((0, 3, 1)));
        assert_eq!(version_tuple("1.10.0"), Some((1, 10, 0)));
        // Pre-release/build metadata is dropped rather than mis-ordered.
        assert_eq!(version_tuple("0.3.1-rc1"), Some((0, 3, 1)));
        // Anything else is unorderable, not "equal".
        assert_eq!(version_tuple("0.3"), None);
        assert_eq!(version_tuple("0.3.1.2"), None);
        assert_eq!(version_tuple("nightly"), None);
        assert_eq!(version_tuple(""), None);
        // 10 > 9 numerically, not lexically.
        assert!(version_tuple("0.10.0") > version_tuple("0.9.0"));
    }

    #[test]
    fn version_comparisons_only_fire_when_provable() {
        let own = env!("CARGO_PKG_VERSION");
        assert!(!is_older_than_own(own));
        assert!(!is_newer_than_own(own));
        assert!(is_older_than_own("0.0.1"));
        assert!(is_newer_than_own("99.0.0"));
        // Unorderable: neither older nor newer, so callers take the safe path
        // (no needless deploy, and no block on an explicit one).
        assert!(!is_older_than_own("nightly"));
        assert!(!is_newer_than_own("nightly"));
    }

    #[test]
    fn cached_binaries_are_keyed_by_version() {
        // The 0.2.x cache was version-blind, so an upgraded rx could deploy the
        // previous binary and only notice after overwriting the remote.
        let old = cached_binary_path("v0.2.1", "x86_64");
        let new = cached_binary_path("v0.3.0", "x86_64");
        assert_ne!(old, new);
        assert!(new.ends_with("rxd-v0.3.0-x86_64"));
    }

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
        assert_eq!(normalize_arch("armv7l"), Some("armv7"));
        assert_eq!(normalize_arch("arm"), Some("armv7"));
        assert_eq!(normalize_arch("armv6l"), None); // armv6 can't run armv7 builds
        assert_eq!(normalize_arch("sparc64"), None);
    }

    #[test]
    fn auto_deploy_triggers_only_on_missing_or_old_rxd() {
        // Missing / old rxd → deploy and retry.
        assert!(should_auto_deploy(&RemExecError::Ssh(
            "bash: line 1: .local/bin/rxd: not found".into()
        )));
        assert!(should_auto_deploy(&RemExecError::Ssh(
            "No such file or directory".into()
        )));
        assert!(should_auto_deploy(&RemExecError::Ssh(
            "error: unrecognized subcommand 'serve'".into()
        )));
        assert!(should_auto_deploy(&RemExecError::Protocol(
            "invalid JSON".into()
        )));

        // Auth failure / other errors → do NOT redeploy (would fail the same).
        assert!(!should_auto_deploy(&RemExecError::Ssh(
            "Permission denied (publickey).".into()
        )));
        assert!(!should_auto_deploy(&RemExecError::Io(
            std::io::Error::other("boom")
        )));
    }

    #[test]
    fn not_deployed_response_is_typed_and_names_the_fix() {
        use crate::protocol::Response;

        let incompatible = not_deployed_response(
            "host1",
            &DeployStatus::Incompatible {
                version: "0.1.1".into(),
                protocol: 1,
            },
        );
        match incompatible {
            Response::Error {
                code,
                message,
                hint,
                retryable,
            } => {
                assert_eq!(code, Some(ErrorCode::NotDeployed));
                assert!(retryable, "a deploy could make the retry succeed");
                assert!(message.contains("protocol 1"), "{message}");
                let hint = hint.expect("hint present");
                assert!(hint.contains("rx deploy host1"), "{hint}");
                // The most-seen hint in the tool must teach the current env
                // name, not the one being retired.
                assert!(hint.contains("RX_AUTO_DEPLOY"), "{hint}");
                assert!(!hint.contains("REM_EXEC_AUTO_DEPLOY"), "{hint}");
            }
            other => panic!("expected error response, got {other:?}"),
        }

        // The missing case still names the fix.
        match not_deployed_response("h2", &DeployStatus::Missing) {
            Response::Error { code, hint, .. } => {
                assert_eq!(code, Some(ErrorCode::NotDeployed));
                assert!(hint.unwrap().contains("rx deploy h2"));
            }
            other => panic!("expected error response, got {other:?}"),
        }
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
