//! Native host identity for `ping` — no remote shell, no `uname`/`cat` fork.
//!
//! Everything here reads the kernel directly (`uname(2)`) or a well-known file
//! (`/etc/os-release`), consistent with rxd's no-shell transport thesis.

/// Host identity fields returned by `ping`.
pub struct HostIdentity {
    /// `uname -s` — operating system name, e.g. "Linux".
    pub os: String,
    /// `uname -r` — kernel release, e.g. "6.12.4".
    pub kernel: String,
    /// `uname -n` — node/host name.
    pub hostname: String,
    /// `uname -m` — machine hardware name, e.g. "x86_64". Reported verbatim
    /// (already canonical on Linux) rather than normalized to our deploy arch
    /// names, so unusual hosts still get a truthful answer.
    pub arch: String,
    /// `ID=` from os-release, e.g. "alpine", "debian", "arch".
    pub distro_id: Option<String>,
    /// `VERSION_ID=` from os-release, e.g. "3.21". Absent on rolling distros.
    pub distro_version: Option<String>,
}

/// Gather host identity. Uname failure degrades to "unknown" fields rather than
/// failing the whole probe — the version/protocol part of ping still matters.
pub fn identity() -> HostIdentity {
    let (os, kernel, hostname, arch) = uname_fields();
    let (distro_id, distro_version) = os_release();
    HostIdentity {
        os,
        kernel,
        hostname,
        arch,
        distro_id,
        distro_version,
    }
}

/// (sysname, release, nodename, machine) from `uname(2)`, or "unknown" each on
/// failure.
fn uname_fields() -> (String, String, String, String) {
    let mut buf = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: uname fully populates the struct on success (rc == 0).
    let rc = unsafe { libc::uname(buf.as_mut_ptr()) };
    if rc != 0 {
        let unknown = || "unknown".to_string();
        return (unknown(), unknown(), unknown(), unknown());
    }
    let u = unsafe { buf.assume_init() };
    (
        c_field(&u.sysname),
        c_field(&u.release),
        c_field(&u.nodename),
        c_field(&u.machine),
    )
}

/// Decode a NUL-terminated fixed C char array (a `utsname` field) into a String.
fn c_field(buf: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parse `ID` and `VERSION_ID` from os-release. Tries `/etc/os-release` then the
/// `/usr/lib/os-release` fallback the spec defines. Missing file → (None, None).
fn os_release() -> (Option<String>, Option<String>) {
    let content = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"));
    let Ok(content) = content else {
        return (None, None);
    };
    parse_os_release(&content)
}

/// Extract `ID`/`VERSION_ID` values from os-release file contents.
fn parse_os_release(content: &str) -> (Option<String>, Option<String>) {
    let mut id = None;
    let mut version = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("ID=") {
            id = Some(unquote(v));
        } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
            version = Some(unquote(v));
        }
    }
    (id, version)
}

/// Strip surrounding whitespace and shell quotes from an os-release value.
fn unquote(value: &str) -> String {
    let v = value.trim();
    let v = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v);
    let v = v
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(v);
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quoted_and_bare_values() {
        let content = "NAME=\"Alpine Linux\"\nID=alpine\nVERSION_ID=3.21\n";
        let (id, version) = parse_os_release(content);
        assert_eq!(id.as_deref(), Some("alpine"));
        assert_eq!(version.as_deref(), Some("3.21"));
    }

    #[test]
    fn rolling_distro_has_no_version_id() {
        let content = "NAME=\"Arch Linux\"\nID=arch\nBUILD_ID=rolling\n";
        let (id, version) = parse_os_release(content);
        assert_eq!(id.as_deref(), Some("arch"));
        assert_eq!(version, None);
    }

    #[test]
    fn does_not_confuse_version_id_with_other_keys() {
        // A substring match on "ID=" must not pick up VARIANT_ID etc.
        let content = "ID=debian\nVERSION_ID=\"12\"\nVARIANT_ID=server\n";
        let (id, version) = parse_os_release(content);
        assert_eq!(id.as_deref(), Some("debian"));
        assert_eq!(version.as_deref(), Some("12"));
    }

    #[test]
    fn unquote_handles_single_quotes_and_whitespace() {
        assert_eq!(unquote("  'x' "), "x");
        assert_eq!(unquote("\"y\""), "y");
        assert_eq!(unquote("z"), "z");
    }

    #[test]
    fn uname_returns_nonempty_os_on_this_host() {
        let (os, _, _, arch) = uname_fields();
        assert!(!os.is_empty());
        assert!(!arch.is_empty());
    }
}
