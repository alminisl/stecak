//! Herdr (herdr.dev) runs several agents inside one pane. Its live snapshot names them, says
//! which are working and which Claude Code session each runs, for the agent strip and the
//! context bar.
//!
//! shortcut: shells out to `herdr api snapshot` for the default session, so a client started
//! with `--session <name>` shows the default session's agents; read its socket if that matters.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::SystemTime;

use winit::event_loop::EventLoopProxy;

use crate::pane::UserEvent;

#[derive(Clone, Debug, PartialEq)]
pub struct Agent {
    /// Herdr's pane name ("reviewer"), else the agent's terminal title.
    pub label: String,
    /// "working", "idle", or whatever else Herdr reports (waiting for input…).
    pub status: String,
    /// Herdr's focused pane.
    pub focused: bool,
    /// Claude Code session id (other agents have none).
    pub session: Option<String>,
    pub cwd: PathBuf,
    /// Tokens in context after the session's last turn.
    pub used: Option<u64>,
}

/// Read the snapshot on a background thread (it takes ~30 ms); the agents (None when Herdr
/// isn't reachable) arrive as `UserEvent::Herdr`.
pub fn fetch_async(proxy: EventLoopProxy<UserEvent>) {
    let _ = std::thread::Builder::new().name("herdr".into()).stack_size(256 * 1024).spawn(move || {
        let _ = proxy.send_event(UserEvent::Herdr(fetch()));
    });
}

fn fetch() -> Option<Vec<Agent>> {
    let mut cmd = Command::new("herdr");
    if let Some(path) = crate::ai::user_path().filter(|_| cfg!(unix)) {
        cmd.env("PATH", path);
    }
    let out = crate::ai::no_console(cmd.args(["api", "snapshot"])).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let mut agents = parse(&out.stdout)?;
    for a in &mut agents {
        a.used = a.session.as_deref().and_then(usage);
    }
    Some(agents)
}

fn parse(json: &[u8]) -> Option<Vec<Agent>> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let agents = v["result"]["snapshot"]["agents"].as_array()?;
    Some(
        agents
            .iter()
            .map(|a| {
                let s = |k: &str| a[k].as_str().unwrap_or_default().to_string();
                let label = [s("name"), s("terminal_title_stripped"), s("pane_id")].into_iter().find(|l| !l.is_empty()).unwrap_or_default();
                // It becomes a file name: accept only what session ids look like.
                let session = Some(a["agent_session"]["value"].as_str().unwrap_or_default())
                    .filter(|id| a["agent"] == "claude" && !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
                    .map(str::to_string);
                Agent { label, status: s("agent_status"), focused: a["focused"] == true, session, cwd: s("cwd").into(), used: None }
            })
            .collect(),
    )
}

/// The session's context size, re-reading its transcript only when it changed on disk.
fn usage(session: &str) -> Option<u64> {
    static CACHE: Mutex<Option<HashMap<String, (PathBuf, Option<SystemTime>, Option<u64>)>>> = Mutex::new(None);
    let mut cache = CACHE.lock().ok()?;
    let cache = cache.get_or_insert_with(HashMap::new);
    if !cache.contains_key(session) {
        cache.insert(session.to_string(), (crate::context::transcript_path(session)?, None, None));
    }
    let (path, mtime, used) = cache.get_mut(session)?;
    let now = path.metadata().ok()?.modified().ok();
    if now != *mtime {
        *mtime = now;
        *used = crate::context::last_usage(path).map(|u| u.0);
    }
    *used
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_snapshot() {
        let json = br#"{"result":{"snapshot":{"agents":[
            {"agent":"claude","agent_session":{"value":"7073c98b-0114"},"agent_status":"idle","focused":true,"cwd":"F:\\p","pane_id":"w1:p1","terminal_title_stripped":"Side views"},
            {"agent":"claude","agent_session":{"value":"../evil"},"agent_status":"working","name":"reviewer","pane_id":"w1:p2"},
            {"agent":"codex","agent_session":{"value":"abc"},"agent_status":"working","pane_id":"w1:p3"}]}}}"#;
        let a = parse(json).unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!((a[0].label.as_str(), a[0].focused, a[0].session.as_deref()), ("Side views", true, Some("7073c98b-0114")));
        assert_eq!((a[1].label.as_str(), a[1].status.as_str(), a[1].session.as_deref()), ("reviewer", "working", None));
        assert_eq!((a[2].label.as_str(), a[2].session.as_deref()), ("w1:p3", None));
        assert!(parse(b"not json").is_none());
    }
}
