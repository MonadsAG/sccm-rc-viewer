//! Auto-update for the macOS app bundle.
//!
//! The viewer reads the release feed in the GitHub Container Registry
//! (`registry.rs`) at start and every [`INTERVAL`], compares the feed's build
//! number with its own `CFBundleVersion`, and offers a newer build. On
//! "Install", `install.rs` downloads and checks it, stages it next to the
//! app and lets a small script swap the bundles after the viewer quits.
//!
//! All network work runs on a plain background thread; results come back as
//! [`UpdateEvent`]s through the winit event loop. Without an app bundle
//! (`cargo run`, Windows, Linux) or with `SCCM_RC_UPDATE=0` it is off.

pub mod install;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // rendered by mac_ui only
pub mod notes;
pub mod registry;
pub mod state;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

pub use registry::Release;
pub use state::State;

/// Time between automatic checks while the viewer runs.
pub const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// The running app as installed.
#[derive(Clone, Debug)]
pub struct Installed {
    pub bundle: PathBuf,
    pub bundle_id: String,
    pub build: u64,
    /// Release label for display (`SCCMRCReleaseLabel`, else the version).
    pub label: String,
}

/// Results of background work, delivered to the App.
#[derive(Debug, Clone)]
pub enum UpdateEvent {
    /// A check finished with what the feed offers. `manual`: started from
    /// the menu (answer even when there is nothing new).
    Checked {
        manual: bool,
        result: Result<Release, String>,
    },
    /// The release is staged; quit and run the script.
    Staged(PathBuf),
    /// The folder is not writable; the checked image was opened instead.
    OpenedImage,
    /// Download or checks failed.
    InstallFailed(String),
}

/// Updates are possible: macOS, running from an app bundle with a build
/// number, not turned off.
pub fn installed() -> Option<Installed> {
    if !cfg!(target_os = "macos") || std::env::var("SCCM_RC_UPDATE").as_deref() == Ok("0") {
        return None;
    }
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let bundle = install::bundle_of(&exe)?;
    // One `plutil` run on the main thread at start, not one per key.
    let plist = info_plist(&bundle)?;
    let build = plist_string(&plist, "CFBundleVersion")?.parse().ok()?;
    let bundle_id = plist_string(&plist, "CFBundleIdentifier")?;
    let label = plist_string(&plist, "SCCMRCReleaseLabel")
        .or_else(|| plist_string(&plist, "CFBundleShortVersionString"))
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    Some(Installed {
        bundle,
        bundle_id,
        build,
        label,
    })
}

/// `<app>/Contents/Info.plist` as JSON (one `plutil` run).
pub fn info_plist(app: &Path) -> Option<serde_json::Value> {
    let plist = app.join("Contents").join("Info.plist");
    let out = std::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(&plist)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(())?;
    serde_json::from_slice(&out.stdout).ok()
}

/// A string from an [`info_plist`]; empty or unfilled (`@KEY@`) is none.
pub fn plist_string(plist: &serde_json::Value, key: &str) -> Option<String> {
    let v = plist.get(key)?.as_str()?.trim();
    (!v.is_empty() && !v.starts_with('@')).then(|| v.to_string())
}

fn curl_get(url: &str, headers: &[String]) -> Result<String> {
    let mut cmd = std::process::Command::new("/usr/bin/curl");
    cmd.args(["-fsSL", "--proto", "=https", "--max-time", "30"]);
    for h in headers {
        cmd.arg("-H").arg(h);
    }
    let out = cmd.arg(url).stdin(std::process::Stdio::null()).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "{} ({})",
            String::from_utf8_lossy(&out.stderr).trim(),
            out.status
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Anonymous pull token for a public package.
pub fn fetch_token(repo: &str) -> Result<String> {
    registry::parse_token(&curl_get(&registry::token_url(repo), &[])?)
}

/// What the feed offers right now.
pub fn fetch_latest() -> Result<Release> {
    let repo = registry::repo();
    let token = fetch_token(&repo)?;
    let body = curl_get(
        &registry::manifest_url(&repo),
        &[
            format!("Authorization: Bearer {token}"),
            format!("Accept: {}", registry::MANIFEST_ACCEPT),
        ],
    )?;
    registry::parse_manifest(&body)
}

/// The release to offer to `installed`, if any. `skipped` is the build the
/// user chose to skip (pass `None` for a manual check, which ignores it).
pub fn offer(latest: Release, installed: &Installed, skipped: Option<u64>) -> Option<Release> {
    registry::is_offer(&latest, installed.build, skipped).then_some(latest)
}

/// Read the feed in the background; the answer arrives through `send`.
/// Whether to offer it is decided by the App ([`offer`]), from its own state.
pub fn spawn_check(manual: bool, send: impl Fn(UpdateEvent) + Send + 'static) {
    std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            let result = fetch_latest().map_err(|e| format!("{e:#}"));
            if let Err(e) = &result {
                tracing::warn!(error = %e, "update check failed");
            }
            send(UpdateEvent::Checked { manual, result });
        })
        .ok();
}

/// Download, check and stage `release` in the background.
#[cfg(target_os = "macos")]
pub fn spawn_install(
    installed: Installed,
    release: Release,
    send: impl Fn(UpdateEvent) + Send + 'static,
) {
    std::thread::Builder::new()
        .name("update-install".into())
        .spawn(move || {
            let ev = match install::prepare(&installed, &release) {
                Ok(install::Prepared::Staged { script }) => UpdateEvent::Staged(script),
                Ok(install::Prepared::OpenImage(image)) => match install::open_image(&image) {
                    Ok(()) => UpdateEvent::OpenedImage,
                    Err(e) => UpdateEvent::InstallFailed(format!("{e:#}")),
                },
                Err(e) => UpdateEvent::InstallFailed(format!("{e:#}")),
            };
            if let UpdateEvent::InstallFailed(e) = &ev {
                tracing::warn!(error = %e, "update install failed");
            }
            send(ev);
        })
        .ok();
}

#[cfg(not(target_os = "macos"))]
pub fn spawn_install(
    _installed: Installed,
    _release: Release,
    send: impl Fn(UpdateEvent) + Send + 'static,
) {
    send(UpdateEvent::InstallFailed(
        "not supported on this platform".into(),
    ));
}

/// Run the staged swap script (then the caller quits).
pub fn finish(script: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        install::launch_script(script)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = script;
        anyhow::bail!("not supported on this platform")
    }
}

/// Wake the App every [`INTERVAL`] for the next automatic check. Counted on
/// the wall clock in short steps: `sleep` alone stands still while the Mac
/// sleeps, so a laptop closed overnight would rarely reach the interval.
pub fn spawn_timer(tick: impl Fn() + Send + 'static) {
    std::thread::Builder::new()
        .name("update-timer".into())
        .spawn(move || {
            let mut last = std::time::SystemTime::now();
            loop {
                std::thread::sleep(Duration::from_secs(60));
                let now = std::time::SystemTime::now();
                match now.duration_since(last) {
                    Ok(d) if d >= INTERVAL => {
                        last = now;
                        tick();
                    }
                    Ok(_) => {}
                    // The clock was set backwards: restart the interval.
                    Err(_) => last = now,
                }
            }
        })
        .ok();
}
