//! Per-user storage locations, one place for every OS.
//!
//! * Windows: `%LOCALAPPDATA%\sccm-rc` (unchanged from earlier versions).
//! * macOS:   `~/Library/Application Support/sccm-rc`.
//! * Other:   `$XDG_DATA_HOME/sccm-rc` or `~/.local/share/sccm-rc`.
//!
//! Falls back to the temp dir when none of these can be resolved.

use std::path::PathBuf;

/// The user's home directory (`USERPROFILE` on Windows, `HOME` elsewhere).
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).map(PathBuf::from)
}

fn base_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        home_dir().map(|h| h.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(".local").join("share")))
    }
}

/// The viewer's data directory (logs, audit trail, recents, MAC cache).
/// Created on first use; on Unix it is made private to the user (0700),
/// since the audit log names the operator and the hosts they connected to.
pub fn data_dir() -> PathBuf {
    let dir = base_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("sccm-rc");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

/// Where session recordings go: `Videos` on Windows, `Movies` on macOS.
pub fn recordings_dir() -> PathBuf {
    let folder = if cfg!(target_os = "macos") {
        "Movies"
    } else {
        "Videos"
    };
    home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(folder)
        .join("sccm-rc-rec")
}
