//! claude-mem (github.com/thedotmack/claude-mem) keeps Claude Code's memory across sessions in
//! a local worker. While that worker answers, the context bar shows how many observations it
//! holds, hovering that shows the project's latest ones, and the palette opens its viewer.
//!
//! shortcut: shells out to curl (like the CI chip) instead of linking an HTTP client.

use std::process::{Command, Stdio};

use winit::event_loop::EventLoopProxy;

use crate::pane::UserEvent;

/// The worker's address: the port in its settings, else its per-user default.
pub fn url() -> String {
    let settings = dirs::home_dir().and_then(|h| std::fs::read(h.join(".claude-mem/settings.json")).ok());
    let configured = settings.and_then(|s| serde_json::from_slice::<serde_json::Value>(&s).ok()).and_then(|v| {
        let p = &v["CLAUDE_MEM_WORKER_PORT"];
        p.as_u64().or_else(|| p.as_str()?.parse().ok())
    });
    #[cfg(unix)]
    let default = 37700 + unsafe { libc::getuid() } as u64 % 100;
    // shortcut: Windows has no uid; assumes the worker's default there is 37700, else set the port in its settings.
    #[cfg(not(unix))]
    let default = 37700;
    format!("http://127.0.0.1:{}", configured.unwrap_or(default))
}

/// Whether claude-mem was ever set up here (it creates ~/.claude-mem on first run).
pub fn installed() -> bool {
    dirs::home_dir().is_some_and(|h| h.join(".claude-mem").is_dir())
}

/// One observation claude-mem recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct Memory {
    /// discovery, change, bugfix, feature, refactor, decision…
    pub kind: String,
    pub title: String,
    pub subtitle: String,
    /// Milliseconds since the Unix epoch.
    pub created_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    /// Observations across all projects.
    pub total: u64,
    /// The project's newest observations, newest first.
    pub recent: Vec<Memory>,
    /// Whether the project has more than `recent`.
    pub more: bool,
}

/// How many recent memories the hover panel lists.
const RECENT: usize = 8;

/// Ask the worker on a background thread; the answer (None when the worker isn't running)
/// arrives as `UserEvent::Mem`. `project` is claude-mem's name for it: the folder's name.
pub fn fetch_async(project: Option<String>, proxy: EventLoopProxy<UserEvent>) {
    let _ = std::thread::Builder::new().name("claude-mem".into()).stack_size(256 * 1024).spawn(move || {
        let _ = proxy.send_event(UserEvent::Mem(fetch(project.as_deref())));
    });
}

fn get(path: &str) -> Option<Vec<u8>> {
    let mut cmd = Command::new("curl");
    let out = crate::ai::no_console(cmd.args(["-fsS", "--max-time", "2"]).arg(format!("{}{path}", url()))).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then_some(out.stdout)
}

fn fetch(project: Option<&str>) -> Option<Stats> {
    if !installed() {
        return None;
    }
    let total = parse(&get("/api/stats")?)?;
    let (recent, more) = project
        .and_then(|p| get(&format!("/api/observations?limit={RECENT}&project={}", crate::ci::encode(p))))
        .and_then(|json| parse_recent(&json))
        .unwrap_or_default();
    Some(Stats { total, recent, more })
}

fn parse(json: &[u8]) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(json).ok()?["database"]["observations"].as_u64()
}

fn parse_recent(json: &[u8]) -> Option<(Vec<Memory>, bool)> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let s = |o: &serde_json::Value, k: &str| o[k].as_str().unwrap_or_default().trim().to_string();
    let items = v["items"].as_array()?.iter().map(|o| Memory { kind: s(o, "type"), title: s(o, "title"), subtitle: s(o, "subtitle"), created_ms: o["created_at_epoch"].as_u64().unwrap_or(0) });
    Some((items.filter(|m| !m.title.is_empty()).collect(), v["hasMore"] == true))
}

/// "now", "5m", "3h", "2d", "6w" since `ms`.
pub fn age(ms: u64, now_ms: u64) -> String {
    match now_ms.saturating_sub(ms) / 60_000 {
        0 => "now".into(),
        m @ 1..60 => format!("{m}m"),
        m @ 60..1440 => format!("{}h", m / 60),
        m @ 1440..20160 => format!("{}d", m / 1440),
        m => format!("{}w", m / 10080),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stats() {
        // As claude-mem 13.39 answers (its docs describe an older shape).
        let json = br#"{"worker":{"version":"13.39.0","port":37701},"database":{"path":"/x/claude-mem.db","observations":245,"sessions":3}}"#;
        assert_eq!(parse(json), Some(245));
        assert!(parse(b"<html>").is_none());
    }

    #[test]
    fn parses_recent_observations() {
        let json = br#"{"items":[{"id":22,"project":"terminal-test","type":"bugfix","title":"CI failure resolved after rerun","subtitle":"Linux job passes","created_at_epoch":1791980842432},{"type":"change","title":"","subtitle":""}],"hasMore":true,"offset":0,"limit":8}"#;
        let (recent, more) = parse_recent(json).unwrap();
        assert!(more);
        assert_eq!(recent, vec![Memory { kind: "bugfix".into(), title: "CI failure resolved after rerun".into(), subtitle: "Linux job passes".into(), created_ms: 1791980842432 }]);
    }

    #[test]
    fn formats_age() {
        let now = 10_000_000_000;
        assert_eq!([age(now, now), age(now - 300_000, now), age(now - 3 * 3_600_000, now), age(now - 2 * 86_400_000, now), age(now - 21 * 86_400_000, now)], ["now", "5m", "3h", "2d", "3w"]);
    }
}
