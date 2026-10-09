//! "A new version is available" check against GitHub releases.
//! shortcut: notify-and-link rather than self-update; installing in place needs a signed,
//! notarized app (Sparkle-style). Upgrade once releases are signed.

use winit::event_loop::EventLoopProxy;

use crate::pane::UserEvent;

const LATEST: &str = "https://api.github.com/repos/alminisl/stecak/releases/latest";

/// Ask GitHub for the latest release on a background thread; reports only if it's newer.
/// shortcut: shells out to curl (present on macOS, Linux and Windows 10+) instead of adding
/// an HTTP/TLS stack to the binary.
pub fn check_async(proxy: EventLoopProxy<UserEvent>) {
    let _ = std::thread::Builder::new().name("update-check".into()).stack_size(256 * 1024).spawn(move || {
        let Ok(out) = std::process::Command::new("curl")
            .args(["-fsSL", "--max-time", "8", "-H", "Accept: application/vnd.github+json", LATEST])
            .output()
        else {
            return;
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return };
        let (Some(tag), Some(url)) = (v["tag_name"].as_str(), v["html_url"].as_str()) else { return };
        if is_newer(tag, env!("CARGO_PKG_VERSION")) {
            let _ = proxy.send_event(UserEvent::UpdateAvailable(tag.trim_start_matches('v').to_string(), url.to_string()));
        }
    });
}

/// Semver-ish comparison of "v1.2.3" against "1.2.0"; anything unparsable is "not newer".
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Option<Vec<u64>> { s.trim_start_matches('v').split(['.', '-']).take(3).map(|p| p.parse().ok()).collect() };
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.3.0", "0.2.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.9", "0.2.0"));
        assert!(!is_newer("nightly", "0.2.0"));
    }
}
