//! Support for AI coding agents (Claude Code, Codex) running inside the terminal:
//! desktop-notification escape sequences, discovering their saved sessions so they can be
//! resumed, and telling whether a pane's foreground process is an agent.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Extract desktop notifications from raw PTY output: OSC 9 (iTerm2 style, which Claude
/// Code's `iterm2` notification channel uses) and OSC 777;notify (rxvt/Ghostty style).
/// shortcut: a sequence split across two reads is missed; fine for short notifications.
pub fn notifications(buf: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(pos) = find(&buf[i..], b"\x1b]") {
        let start = i + pos + 2;
        // The payload ends at BEL or ST (ESC \).
        let end = buf[start..].iter().position(|&b| b == 0x07 || b == 0x1b).map(|e| start + e).unwrap_or(buf.len());
        let body = String::from_utf8_lossy(&buf[start..end]);
        if let Some(msg) = body.strip_prefix("9;") {
            // "9;4;…" is ConEmu's progress-bar sequence, not a notification.
            if !msg.starts_with("4;") && !msg.is_empty() {
                out.push(msg.to_string());
            }
        } else if let Some(rest) = body.strip_prefix("777;notify;") {
            let (title, text) = rest.split_once(';').unwrap_or((rest, ""));
            out.push(if text.is_empty() { title.to_string() } else { format!("{title}: {text}") });
        }
        i = end.max(start);
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tool {
    Claude,
    Codex,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub tool: Tool,
    pub id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub branch: String,
    pub modified: SystemTime,
}

impl Session {
    /// Command that resumes this session (run through the user's login shell).
    pub fn resume_command(&self) -> String {
        match self.tool {
            Tool::Claude => format!("claude --resume {}", self.id),
            Tool::Codex => format!("codex resume {}", self.id),
        }
    }

    /// Modified within the last two minutes: almost certainly still running somewhere.
    pub fn is_live(&self) -> bool {
        self.modified.elapsed().is_ok_and(|d| d.as_secs() < 120)
    }

    pub fn matches(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        q.split_whitespace().all(|w| {
            self.title.to_lowercase().contains(w)
                || self.cwd.to_string_lossy().to_lowercase().contains(w)
                || self.branch.to_lowercase().contains(w)
                || self.tool.name().contains(w)
        })
    }
}

/// Saved Claude Code and Codex sessions, newest first (capped, since listing is done on open).
pub fn list_sessions(limit: usize) -> Vec<Session> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let mut files: Vec<(SystemTime, PathBuf, Tool)> = Vec::new();
    // Claude Code: ~/.claude/projects/<encoded cwd>/<session id>.jsonl
    for dir in read_dirs(&home.join(".claude/projects")) {
        collect_jsonl(&dir, Tool::Claude, &mut files);
    }
    // Codex: ~/.codex/sessions/YYYY/MM/DD/rollout-…-<id>.jsonl
    for y in read_dirs(&home.join(".codex/sessions")) {
        for m in read_dirs(&y) {
            for d in read_dirs(&m) {
                collect_jsonl(&d, Tool::Codex, &mut files);
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().take(limit).filter_map(|(modified, path, tool)| read_session(&path, tool, modified)).collect()
}

fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default()
}

fn collect_jsonl(dir: &Path, tool: Tool, out: &mut Vec<(SystemTime, PathBuf, Tool)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "jsonl") {
            if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                out.push((t, p, tool));
            }
        }
    }
}

/// Read just the head of a session file: enough for its folder, branch and first prompt.
fn read_session(path: &Path, tool: Tool, modified: SystemTime) -> Option<Session> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let mut s = Session {
        tool,
        id: String::new(),
        cwd: PathBuf::new(),
        title: String::new(),
        branch: String::new(),
        modified,
    };
    if tool == Tool::Claude {
        s.id = path.file_stem()?.to_string_lossy().into_owned();
    }
    for line in BufReader::new(file).lines().take(200).map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        match tool {
            Tool::Claude => {
                if s.cwd.as_os_str().is_empty() {
                    if let Some(c) = v["cwd"].as_str() {
                        s.cwd = c.into();
                        s.branch = v["gitBranch"].as_str().unwrap_or_default().to_string();
                    }
                }
                if v["type"] == "user" && v["isMeta"] != true {
                    if let Some(t) = user_text(&v["message"]["content"]) {
                        s.title = t;
                        break;
                    }
                }
            }
            Tool::Codex => {
                if v["type"] == "session_meta" {
                    s.id = v["payload"]["id"].as_str().unwrap_or_default().to_string();
                    s.cwd = v["payload"]["cwd"].as_str().unwrap_or_default().into();
                    s.branch = v["payload"]["git"]["branch"].as_str().unwrap_or_default().to_string();
                } else if v["type"] == "response_item" && v["payload"]["role"] == "user" {
                    if let Some(t) = user_text(&v["payload"]["content"]) {
                        s.title = t;
                        break;
                    }
                }
            }
        }
    }
    if s.id.is_empty() || s.cwd.as_os_str().is_empty() {
        return None;
    }
    if s.title.is_empty() {
        s.title = "(no prompt yet)".into();
    }
    Some(s)
}

/// The first line of a user message, skipping tool/command wrappers like `<command-name>`.
fn user_text(content: &serde_json::Value) -> Option<String> {
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
        _ => return None,
    };
    let text = text.trim();
    if text.is_empty() || text.starts_with('<') {
        return None;
    }
    Some(text.lines().next().unwrap_or(text).chars().take(120).collect())
}

/// Name of the foreground process of a PTY (e.g. "claude", "codex", "zsh").
#[cfg(target_os = "macos")]
pub fn process_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: proc_name writes at most buf.len() bytes and returns the length written.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let name = (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())?;
    // Claude Code's native installer runs ~/.local/share/claude/versions/<version>, so the
    // process is named like "2.1.295": recognize it by its path.
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: proc_pidpath writes at most path.len() bytes and returns the length written.
        let n = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        if n > 0 && String::from_utf8_lossy(&path[..n as usize]).contains("/claude/versions/") {
            return Some("claude".into());
        }
    }
    Some(name)
}

#[cfg(target_os = "linux")]
pub fn process_name(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn process_name(_pid: i32) -> Option<String> {
    None
}

/// The session a running Claude Code process is in: Claude Code keeps
/// `~/.claude/sessions/<pid>.json` with its current `sessionId` (updated on /clear, /resume).
pub fn claude_session_id(pid: i32) -> Option<String> {
    claude_session(pid).map(|s| s.0)
}

/// The running Claude Code process's session id and folder.
pub fn claude_session(pid: i32) -> Option<(String, PathBuf)> {
    let path = dirs::home_dir()?.join(".claude/sessions").join(format!("{pid}.json"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let id = v["sessionId"].as_str()?;
    // It becomes a shell argument: accept only what session ids look like.
    let ok = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    ok.then(|| (id.to_string(), v["cwd"].as_str().unwrap_or_default().into()))
}

pub fn is_agent(process: &str) -> bool {
    let p = process.to_lowercase();
    p.contains("claude") || p.contains("codex")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc_notifications() {
        let out = notifications(b"hi\x1b]9;Claude needs your permission\x07x\x1b]9;4;1;50\x07\x1b]777;notify;Codex;Done\x1b\\");
        assert_eq!(out, vec!["Claude needs your permission".to_string(), "Codex: Done".to_string()]);
        assert!(notifications(b"plain output").is_empty());
    }

    #[test]
    fn reads_claude_session_head() {
        let dir = std::env::temp_dir().join(format!("stecak-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("abc-123.jsonl");
        std::fs::write(
            &f,
            concat!(
                "{\"type\":\"mode\",\"sessionId\":\"abc-123\"}\n",
                "{\"type\":\"user\",\"cwd\":\"/tmp/proj\",\"gitBranch\":\"main\",\"message\":{\"content\":\"<command-name>/clear</command-name>\"}}\n",
                "{\"type\":\"user\",\"cwd\":\"/tmp/proj\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Fix the login bug\\nmore\"}]}}\n"
            ),
        )
        .unwrap();
        let s = read_session(&f, Tool::Claude, SystemTime::now()).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!((s.id.as_str(), s.title.as_str(), s.branch.as_str()), ("abc-123", "Fix the login bug", "main"));
        assert_eq!(s.cwd, PathBuf::from("/tmp/proj"));
        assert_eq!(s.resume_command(), "claude --resume abc-123");
        assert!(s.matches("login proj") && !s.matches("codex"));
    }
}
