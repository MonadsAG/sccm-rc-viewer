//! Recent connection targets (most-recent-first), persisted to
//! `%LOCALAPPDATA%\sccm-rc\recent.txt` — one host per line.

const CAP: usize = 12;

fn path() -> std::path::PathBuf {
    let dir = crate::paths::data_dir();
    dir.join("recent.txt")
}

/// Recent hosts, most-recent-first (capped).
pub fn load() -> Vec<String> {
    std::fs::read_to_string(path())
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .take(CAP)
        .collect()
}

/// Record `host` as the most-recent target (dedup, case-insensitive).
pub fn add(host: &str) {
    let host = host.trim();
    if host.is_empty() {
        return;
    }
    let mut list = load();
    list.retain(|h| !h.eq_ignore_ascii_case(host));
    list.insert(0, host.to_string());
    list.truncate(CAP);
    // Atomic write: a fully-written temp file renamed over the target, so a
    // crash or a second viewer instance can't clobber it or leave it partial.
    let p = path();
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, list.join("\r\n").as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn last_user_path() -> std::path::PathBuf {
    crate::paths::data_dir().join("last_user.txt")
}

/// The `DOMAIN\user` last used for an explicit (NTLM) logon. Never the password.
#[cfg_attr(windows, allow(dead_code))]
pub fn last_user() -> Option<String> {
    std::fs::read_to_string(last_user_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg_attr(windows, allow(dead_code))]
pub fn set_last_user(user: &str) {
    let _ = std::fs::write(last_user_path(), user.trim());
}
