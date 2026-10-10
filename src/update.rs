//! Updates: check GitHub for a newer release at startup; on macOS and Windows, install it.
//!
//! macOS: downloads the release DMG with curl (so it carries no quarantine flag), verifies its
//! SHA-256 against the digest GitHub publishes for the asset, and swaps the app bundle.
//! Windows: downloads and verifies the setup exe the same way; "Restart" runs it silently, and
//! it upgrades the installed copy and starts it again. Elsewhere, "Download" opens the page.

use std::path::{Path, PathBuf};
use std::process::Command;

use winit::event_loop::EventLoopProxy;

use crate::pane::UserEvent;

const LATEST: &str = "https://api.github.com/repos/alminisl/stecak/releases/latest";

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    /// Release page, opened when we can't install in place.
    pub page: String,
    /// This platform's installer (macOS DMG, Windows setup exe): (download URL, lowercase
    /// hex SHA-256).
    pub installer: Option<(String, String)>,
}

/// Ask GitHub for the latest release on a background thread; reports only if it's newer.
/// shortcut: shells out to curl (present on macOS, Linux and Windows 10+) instead of adding
/// an HTTP/TLS stack to the binary.
/// `manual` (Check for Updates…) also reports "up to date" and failures; the startup check
/// stays silent unless there's something new.
pub fn check_async(proxy: EventLoopProxy<UserEvent>, manual: bool) {
    let _ = std::thread::Builder::new().name("update-check".into()).stack_size(256 * 1024).spawn(move || {
        let fail = |why: &str| {
            if manual {
                let _ = proxy.send_event(UserEvent::UpdateNone(Some(why.to_string())));
            }
        };
        let Ok(out) = crate::ai::no_console(&mut Command::new("curl")).args(["-fsSL", "--max-time", "8", "-H", "Accept: application/vnd.github+json", LATEST]).output() else {
            return fail("curl is not available");
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return fail("couldn't reach GitHub") };
        let (Some(tag), Some(page)) = (v["tag_name"].as_str(), v["html_url"].as_str()) else { return fail("unexpected reply from GitHub") };
        if !is_newer(tag, env!("CARGO_PKG_VERSION")) {
            if manual {
                let _ = proxy.send_event(UserEvent::UpdateNone(None));
            }
            return;
        }
        let suffix = match (cfg!(windows), cfg!(target_arch = "aarch64")) {
            (true, true) => "-windows-arm64-setup.exe",
            (true, false) => "-windows-x64-setup.exe",
            _ => ".dmg",
        };
        let installer = v["assets"].as_array().into_iter().flatten().find_map(|a| {
            let name = a["name"].as_str()?;
            let digest = a["digest"].as_str()?.strip_prefix("sha256:")?;
            let url = a["browser_download_url"].as_str()?;
            name.ends_with(suffix).then(|| (url.to_string(), digest.to_lowercase()))
        });
        let release = Release { version: tag.trim_start_matches('v').to_string(), page: page.to_string(), installer };
        let _ = proxy.send_event(UserEvent::UpdateAvailable(release));
    });
}

/// Semver-ish comparison of "v1.2.3" against "1.2.0"; anything unparsable is "not newer".
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Option<Vec<u64>> { s.trim_start_matches('v').split(['.', '-']).take(3).map(|p| p.parse().ok()).collect() };
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

/// The .app bundle we're running from, if any (macOS).
pub fn running_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let app = exe.parent()?.parent()?.parent()?.to_path_buf();
    app.extension().is_some_and(|e| e == "app").then_some(app)
}

/// Can this release be installed in place right now?
pub fn can_install(release: &Release) -> bool {
    if cfg!(windows) {
        // Only a copy the installer manages (its uninstaller sits next to the exe); a portable
        // or development copy keeps "Download".
        let managed = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("unins000.exe").is_file()));
        return release.installer.is_some() && managed == Some(true);
    }
    cfg!(target_os = "macos")
        && release.installer.is_some()
        && running_bundle().and_then(|a| a.parent().map(Path::to_path_buf)).is_some_and(|dir| writable(&dir))
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".stecak-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(probe);
    ok
}

/// Download, verify and install on a background thread; reports `UpdateInstalled`.
pub fn install_async(release: Release, proxy: EventLoopProxy<UserEvent>) {
    let _ = std::thread::Builder::new().name("update-install".into()).spawn(move || {
        let result = install(&release);
        if let Err(e) = &result {
            log::error!("update failed: {e}");
        }
        let _ = proxy.send_event(UserEvent::UpdateInstalled(result.map_err(|e| e.to_string())));
    });
}

fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Windows: download and verify the setup exe; `relaunch` runs it. Returns its path.
#[cfg(windows)]
fn install(release: &Release) -> Result<PathBuf, String> {
    let (url, sha) = release.installer.as_ref().ok_or("release has no installer")?;
    let work = std::env::temp_dir().join(format!("stecak-update-{}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let setup = work.join("stecak-setup.exe");
    run(crate::ai::no_console(&mut Command::new("curl")).args(["-fsSL", "--max-time", "600", "-o"]).arg(&setup).arg(url))?;
    // Trust boundary: run nothing unless the bytes are exactly the published release.
    let out = run(crate::ai::no_console(&mut Command::new("certutil")).arg("-hashfile").arg(&setup).arg("SHA256"))?;
    if !verify_digest(&certutil_digest(&out), sha) {
        let _ = std::fs::remove_file(&setup);
        return Err("downloaded file doesn't match the release checksum".into());
    }
    Ok(setup)
}

/// `certutil -hashfile` prints a header, the hex digest (older Windows: space-separated
/// bytes) and a status line; return the digest.
#[cfg_attr(not(windows), allow(dead_code))]
fn certutil_digest(out: &str) -> String {
    out.lines().map(|l| l.replace(' ', "")).find(|l| l.len() == 64 && l.chars().all(|c| c.is_ascii_hexdigit())).unwrap_or_default()
}

#[cfg(not(windows))]
fn install(release: &Release) -> Result<PathBuf, String> {
    let (url, sha) = release.installer.as_ref().ok_or("release has no DMG")?;
    let app = running_bundle().ok_or("not running from an app bundle")?;
    let work = std::env::temp_dir().join(format!("stecak-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let result = install_from(url, sha, &app, &work);
    let _ = std::fs::remove_dir_all(&work);
    result.map(|_| app)
}

#[cfg_attr(windows, allow(dead_code))] // macOS install path
fn install_from(url: &str, sha: &str, app: &Path, work: &Path) -> Result<(), String> {
    let dmg = work.join("update.dmg");
    run(Command::new("curl").args(["-fsSL", "--max-time", "600", "-o"]).arg(&dmg).arg(url))?;
    // Trust boundary: install nothing unless the bytes are exactly the published release.
    let actual = run(Command::new("shasum").args(["-a", "256"]).arg(&dmg))?;
    if !verify_digest(&actual, sha) {
        return Err("downloaded file doesn't match the release checksum".into());
    }
    let mount = work.join("mnt");
    run(Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-noverify", "-mountpoint"]).arg(&mount).arg(&dmg))?;
    let copied = copy_app(&mount, app);
    let _ = run(Command::new("hdiutil").args(["detach", "-quiet"]).arg(&mount));
    let new = copied?;
    swap(app, &new)
}

/// `shasum` prints "<hex>  <file>"; compare its hex to GitHub's digest.
fn verify_digest(shasum_output: &str, expected: &str) -> bool {
    let actual = shasum_output.split_whitespace().next().unwrap_or_default();
    actual.len() == 64 && actual.eq_ignore_ascii_case(expected)
}

/// Copy the .app from the mounted DMG next to the installed one (same volume, so the swap
/// below is a pair of atomic renames).
#[cfg_attr(windows, allow(dead_code))]
fn copy_app(mount: &Path, app: &Path) -> Result<PathBuf, String> {
    let src = std::fs::read_dir(mount)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or("no app in the update")?;
    let new = app.with_extension("app.new");
    let _ = std::fs::remove_dir_all(&new);
    run(Command::new("ditto").arg(&src).arg(&new))?;
    Ok(new)
}

#[cfg_attr(windows, allow(dead_code))]
fn swap(app: &Path, new: &Path) -> Result<(), String> {
    let old = app.with_extension("app.old");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(app, &old).map_err(|e| format!("can't move the current app aside: {e}"))?;
    if let Err(e) = std::fs::rename(new, app) {
        // Put the working version back rather than leaving no app at all.
        let _ = std::fs::rename(&old, app);
        return Err(format!("can't install the new app: {e}"));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Start the freshly installed app; the caller exits right after.
#[cfg(not(windows))]
pub fn relaunch(app: &Path) {
    let _ = Command::new("open").arg("-n").arg(app).spawn();
}

/// Run the verified setup silently: it waits for Stećak to close, upgrades it in place and
/// starts it again (`/relaunch=1`, see installer/stecak.iss). The caller exits right after.
#[cfg(windows)]
pub fn relaunch(setup: &Path) {
    let args = ["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS", "/relaunch=1"];
    if let Err(e) = Command::new(setup).args(args).spawn() {
        log::error!("could not start the update installer: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.3.0", "0.2.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.9", "0.2.0"));
        assert!(!is_newer("nightly", "0.2.0"));
    }

    #[test]
    fn digest_must_match_exactly() {
        let good = "6f77e939a338b826078a4747b9888dbb950228a2271d282b5692095cf46352a2";
        assert!(verify_digest(&format!("{good}  /tmp/update.dmg\n"), good));
        assert!(verify_digest(&format!("{}  f", good.to_uppercase()), good));
        assert!(!verify_digest(&format!("{}0  f", &good[..63]), good));
        assert!(!verify_digest("", good));
        assert!(!verify_digest("abc  f", "abc"));
    }

    #[test]
    fn reads_certutil_digest() {
        let hex = "6f77e939a338b826078a4747b9888dbb950228a2271d282b5692095cf46352a2";
        let out = format!("SHA256 hash of C:\\x\\stecak-setup.exe:\r\n{hex}\r\nCertUtil: -hashfile command completed successfully.\r\n");
        assert_eq!(certutil_digest(&out), hex);
        // Older Windows separates the bytes with spaces.
        let spaced: Vec<&str> = (0..32).map(|i| &hex[i * 2..i * 2 + 2]).collect();
        assert_eq!(certutil_digest(&format!("header\n{}\nok\n", spaced.join(" "))), hex);
        assert_eq!(certutil_digest("CertUtil: error"), "");
    }

    #[test]
    fn swap_replaces_and_restores() {
        let dir = std::env::temp_dir().join(format!("stecak-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (app, new) = (dir.join("X.app"), dir.join("X.app.new"));
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("v"), "old").unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("v"), "new").unwrap();
        swap(&app, &new).unwrap();
        assert_eq!(std::fs::read_to_string(app.join("v")).unwrap(), "new");
        assert!(!dir.join("X.app.old").exists() && !new.exists());
        // A missing new bundle must leave the old one in place.
        assert!(swap(&app, &dir.join("missing.app")).is_err());
        assert_eq!(std::fs::read_to_string(app.join("v")).unwrap(), "new");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
