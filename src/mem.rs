//! claude-mem (github.com/thedotmack/claude-mem) keeps Claude Code's memory across sessions in
//! a local worker. While that worker answers, the context bar shows how many observations it
//! holds for the project, and the palette opens its viewer.
//!
//! shortcut: shells out to curl (like the CI chip) instead of linking an HTTP client.

use std::collections::HashMap;
use std::process::{Command, Stdio};

use winit::event_loop::EventLoopProxy;

use crate::pane::UserEvent;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    /// Observations per project (claude-mem names a project after its folder).
    pub by_project: HashMap<String, u64>,
    pub total: u64,
}

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

/// Ask the worker for its counts on a background thread; they (None when it isn't running)
/// arrive as `UserEvent::Mem`.
pub fn fetch_async(proxy: EventLoopProxy<UserEvent>) {
    let _ = std::thread::Builder::new().name("claude-mem".into()).stack_size(256 * 1024).spawn(move || {
        let _ = proxy.send_event(UserEvent::Mem(fetch()));
    });
}

fn fetch() -> Option<Stats> {
    if !installed() {
        return None;
    }
    let mut cmd = Command::new("curl");
    let out = crate::ai::no_console(cmd.args(["-fsS", "--max-time", "2"]).arg(format!("{}/api/stats", url()))).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    parse(&out.stdout)
}

fn parse(json: &[u8]) -> Option<Stats> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let by_project = v["byProject"].as_object()?.iter().map(|(name, p)| (name.clone(), p["observations"].as_u64().unwrap_or(0))).collect();
    Some(Stats { by_project, total: v["total"]["observations"].as_u64().unwrap_or(0) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stats() {
        let s = parse(br#"{"byProject":{"stecak":{"observations":245,"summaries":12},"x":{}},"total":{"observations":401}}"#).unwrap();
        assert_eq!((s.by_project["stecak"], s.by_project["x"], s.total), (245, 0, 401));
        assert!(parse(b"<html>").is_none());
    }
}
