//! Download, check and install a release (macOS).
//!
//! 1. `curl` fetches the `.dmg` blob; its SHA-256 must match the manifest.
//! 2. The image is mounted read-only. The app inside must carry a valid
//!    Developer ID signature from [`TEAM_ID`], pass Gatekeeper (notarized),
//!    have the running app's bundle identifier and the announced build.
//! 3. It is copied next to the running app as `<name>.app.new`, and a small
//!    shell script waits for this process to end, swaps the two bundles and
//!    starts the new one.
//!
//! When the folder holding the app is not writable (a standard user and
//! `/Applications`), the checked `.dmg` is opened in the Finder instead.
//!
//! The command-line tools (`curl`, `shasum`, `hdiutil`, `codesign`, `spctl`,
//! `ditto`) ship with macOS; `curl` uses the system trust store and honours
//! `https_proxy`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// Monads AG's Apple Developer team; any other signer is refused.
pub const TEAM_ID: &str = "97H6W5B7MN";

/// The `.app` bundle the executable runs from
/// (`…/SCCM RC.app/Contents/MacOS/sccm-rc-viewer` → `…/SCCM RC.app`).
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then(|| app.to_path_buf())
}

/// `key=value` lines of `codesign -dv` (it prints them on stderr).
pub fn codesign_field<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    output.lines().find_map(|l| {
        l.strip_prefix(key)
            .and_then(|r| r.strip_prefix('='))
            .map(str::trim)
    })
}

/// Quote for `/bin/sh` (single quotes; embedded ones closed and escaped).
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The script that swaps `new` in for `app` once `pid` has exited, then
/// starts `app`. On a failed swap the old bundle is put back.
pub fn swap_script(pid: u32, app: &Path, new: &Path) -> String {
    let app = sh_quote(&app.to_string_lossy());
    let new = sh_quote(&new.to_string_lossy());
    format!(
        r#"#!/bin/sh
# Written by SCCM RC to finish an update; safe to delete.
APP={app}
NEW={new}
i=0
while kill -0 {pid} 2>/dev/null; do
  i=$((i + 1))
  if [ "$i" -gt 600 ]; then
    rm -rf "$NEW"
    rm -f "$0"
    exit 1
  fi
  sleep 0.2
done
rm -rf "$APP.old"
if mv "$APP" "$APP.old"; then
  if mv "$NEW" "$APP"; then
    rm -rf "$APP.old"
  else
    mv "$APP.old" "$APP"
  fi
fi
rm -rf "$NEW"
xattr -dr com.apple.quarantine "$APP" 2>/dev/null
open "$APP"
rm -f "$0"
"#
    )
}

/// Whether `dir` takes new files (tried, not guessed from permissions).
pub fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".sccm-rc-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Outcome of [`prepare`].
#[derive(Debug)]
pub enum Prepared {
    /// Staged next to the app; quit and run the script to finish.
    Staged { script: PathBuf },
    /// The app's folder is not writable; the checked `.dmg` to open.
    OpenImage(PathBuf),
}

#[cfg(target_os = "macos")]
pub use mac::{launch_script, open_image, prepare};

#[cfg(target_os = "macos")]
mod mac {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use anyhow::{anyhow, bail, Context, Result};
    use tracing::info;

    use super::super::registry::{self, Release};
    use super::super::{info_plist, plist_string, Installed};
    use super::{check_identity, swap_script, writable, Prepared};

    fn run(cmd: &mut Command) -> Result<std::process::Output> {
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("{:?} failed to start", cmd.get_program()))?;
        if !out.status.success() {
            bail!(
                "{:?} failed: {}",
                cmd.get_program(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }

    fn download(repo: &str, r: &Release, token: &str, dest: &Path) -> Result<()> {
        let _ = std::fs::remove_file(dest);
        run(Command::new("/usr/bin/curl")
            .args(["-fsSL", "--proto", "=https", "--max-time", "900"])
            .args(["-H", &format!("Authorization: Bearer {token}")])
            .arg("-o")
            .arg(dest)
            .arg(registry::blob_url(repo, &r.digest)))
        .context("download failed")?;
        let out = run(Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .arg(dest))?;
        let got = String::from_utf8_lossy(&out.stdout);
        let got = got.split_whitespace().next().unwrap_or("");
        if !got.eq_ignore_ascii_case(r.sha256()) {
            let _ = std::fs::remove_file(dest);
            bail!("download is corrupt (checksum mismatch)");
        }
        Ok(())
    }

    /// Mounted disk image, detached on drop.
    struct Mount(PathBuf);

    impl Mount {
        fn attach(image: &Path, at: &Path) -> Result<Self> {
            // A mount left by an attempt that crashed would block this one.
            detach(at);
            let _ = std::fs::create_dir_all(at);
            run(Command::new("/usr/bin/hdiutil")
                .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-quiet"])
                .arg("-mountpoint")
                .arg(at)
                .arg(image))
            .context("could not open the disk image")?;
            Ok(Self(at.to_path_buf()))
        }

        fn app(&self) -> Result<PathBuf> {
            std::fs::read_dir(&self.0)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .find(|p| p.extension().is_some_and(|x| x == "app"))
                .ok_or_else(|| anyhow!("no app in the disk image"))
        }
    }

    /// Best effort: detach whatever is mounted at `at` and remove the folder.
    fn detach(at: &Path) {
        if at.exists() {
            let _ = Command::new("/usr/bin/hdiutil")
                .args(["detach", "-quiet", "-force"])
                .arg(at)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = std::fs::remove_dir(at);
        }
    }

    impl Drop for Mount {
        fn drop(&mut self) {
            detach(&self.0);
        }
    }

    /// Signature, notarization, identity and build of the app in the image.
    fn verify(app: &Path, installed: &Installed, r: &Release) -> Result<()> {
        run(Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app))
        .context("the new version's signature is invalid")?;
        let out = run(Command::new("/usr/bin/codesign").arg("-dv").arg(app))?;
        check_identity(&String::from_utf8_lossy(&out.stderr), &installed.bundle_id)?;
        run(Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(app))
        .context("Gatekeeper rejects the new version (not notarized?)")?;
        let build = info_plist(app)
            .and_then(|p| plist_string(&p, "CFBundleVersion"))
            .and_then(|b| b.parse::<u64>().ok())
            .ok_or_else(|| anyhow!("the new version has no build number"))?;
        if build != r.build {
            bail!("the disk image holds build {build}, not {}", r.build);
        }
        Ok(())
    }

    /// Download and check `r`, then stage it next to the running app.
    pub fn prepare(installed: &Installed, r: &Release) -> Result<Prepared> {
        let repo = registry::repo();
        let dir = crate::paths::data_dir().join("update");
        std::fs::create_dir_all(&dir)?;
        let token = super::super::fetch_token(&repo)?;
        let image = dir.join(format!("SCCM-RC-{}.dmg", r.label));
        info!(label = %r.label, build = r.build, "downloading update");
        download(&repo, r, &token, &image)?;
        let staged = stage(installed, r, &dir, &image);
        if !matches!(staged, Ok(Prepared::OpenImage(_))) {
            // Staged, or refused: the image is of no further use.
            let _ = std::fs::remove_file(&image);
        }
        staged
    }

    /// Mount, verify and copy the downloaded `image`.
    fn stage(installed: &Installed, r: &Release, dir: &Path, image: &Path) -> Result<Prepared> {
        let mount = Mount::attach(image, &dir.join("mnt"))?;
        let new_app = mount.app()?;
        verify(&new_app, installed, r)?;
        info!("update verified (signature, notarization, identity, build)");

        let parent = installed
            .bundle
            .parent()
            .ok_or_else(|| anyhow!("app has no parent folder"))?;
        if !writable(parent) {
            drop(mount);
            return Ok(Prepared::OpenImage(image.to_path_buf()));
        }
        let mut staged = installed.bundle.clone().into_os_string();
        staged.push(".new");
        let staged = PathBuf::from(staged);
        let _ = std::fs::remove_dir_all(&staged);
        if let Err(e) = run(Command::new("/usr/bin/ditto").arg(&new_app).arg(&staged)) {
            // No half-copied bundle left next to the app.
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e.context("could not copy the new version"));
        }
        drop(mount);

        let script = dir.join("finish-update.sh");
        std::fs::write(
            &script,
            swap_script(std::process::id(), &installed.bundle, &staged),
        )?;
        Ok(Prepared::Staged { script })
    }

    /// Start the swap script detached from this process.
    pub fn launch_script(script: &Path) -> Result<()> {
        use std::os::unix::process::CommandExt;
        Command::new("/bin/sh")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .context("could not start the update script")?;
        Ok(())
    }

    pub fn open_image(image: &Path) -> Result<()> {
        run(Command::new("/usr/bin/open").arg(image)).map(|_| ())
    }
}

/// Team and bundle identifier from `codesign -dv` must match ours.
pub fn check_identity(codesign_dv: &str, bundle_id: &str) -> Result<()> {
    match codesign_field(codesign_dv, "TeamIdentifier") {
        Some(TEAM_ID) => {}
        Some(other) => bail!("the new version is signed by team {other}, not {TEAM_ID}"),
        None => bail!("the new version has no team signature"),
    }
    match codesign_field(codesign_dv, "Identifier") {
        Some(id) if id == bundle_id => Ok(()),
        Some(id) => bail!("the new version is {id}, not {bundle_id}"),
        None => bail!("the new version has no identifier"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_bundle_of_executable() {
        let exe = Path::new("/Applications/SCCM RC.app/Contents/MacOS/sccm-rc-viewer");
        assert_eq!(
            bundle_of(exe),
            Some(PathBuf::from("/Applications/SCCM RC.app"))
        );
        assert_eq!(bundle_of(Path::new("/usr/local/bin/sccm-rc-viewer")), None);
        assert_eq!(
            bundle_of(Path::new("/tmp/x/Contents/MacOS/sccm-rc-viewer")),
            None
        );
    }

    const DV: &str = "Executable=/Applications/SCCM RC.app/Contents/MacOS/sccm-rc-viewer\n\
Identifier=ch.monads.sccm-rc-viewer\n\
Format=app bundle with Mach-O universal (x86_64 arm64)\n\
TeamIdentifier=97H6W5B7MN\n";

    #[test]
    fn accepts_our_identity_only() {
        assert!(check_identity(DV, "ch.monads.sccm-rc-viewer").is_ok());
        assert!(check_identity(DV, "io.github.other").is_err());
        let other = DV.replace("97H6W5B7MN", "ABCDE12345");
        assert!(check_identity(&other, "ch.monads.sccm-rc-viewer").is_err());
        let adhoc = DV.replace("TeamIdentifier=97H6W5B7MN", "TeamIdentifier=not set");
        assert!(check_identity(&adhoc, "ch.monads.sccm-rc-viewer").is_err());
        assert!(check_identity("", "ch.monads.sccm-rc-viewer").is_err());
    }

    #[test]
    fn quotes_for_shell() {
        assert_eq!(sh_quote("SCCM RC.app"), "'SCCM RC.app'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn swap_script_quotes_paths_and_waits_for_pid() {
        let s = swap_script(
            4242,
            Path::new("/Applications/SCCM RC.app"),
            Path::new("/Applications/SCCM RC.app.new"),
        );
        assert!(s.contains("APP='/Applications/SCCM RC.app'\n"));
        assert!(s.contains("NEW='/Applications/SCCM RC.app.new'\n"));
        assert!(s.contains("kill -0 4242"));
        assert!(s.contains(r#"open "$APP""#));
    }

    #[cfg(unix)]
    #[test]
    fn swap_script_swaps_bundles() {
        let dir = std::env::temp_dir().join(format!("sccm-rc-swap-{}", std::process::id()));
        let app = dir.join("Test App.app");
        let new = dir.join("Test App.app.new");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(app.join("v"), "old").unwrap();
        std::fs::write(new.join("v"), "new").unwrap();
        // A pid that is gone; `open` is stubbed out so the test opens nothing.
        let script = swap_script(u32::MAX - 1, &app, &new).replace("open \"$APP\"", "true");
        let path = dir.join("s.sh");
        std::fs::write(&path, script).unwrap();
        let ok = std::process::Command::new("/bin/sh")
            .arg(&path)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert_eq!(std::fs::read_to_string(app.join("v")).unwrap(), "new");
        assert!(!new.exists());
        assert!(!dir.join("Test App.app.old").exists());
        assert!(!path.exists(), "script removes itself");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writable_probe() {
        assert!(writable(&std::env::temp_dir()));
        assert!(!writable(Path::new("/nonexistent-sccm-rc-dir")));
    }
}
