//! Context-window usage of a Claude Code session, for the context bar under the panes.
//!
//! Two sources, both free (no API calls):
//! - the live total comes from the session transcript: every assistant turn records its token
//!   usage, which is the size of the context the model just saw;
//! - the breakdown (system prompt, tools, MCP, memory files, skills, autocompact buffer) comes
//!   from running Claude Code's own `/context` headlessly in the session's folder. That takes
//!   ~10 s (it connects MCP servers), so it runs on a worker thread and is cached per folder.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use winit::event_loop::EventLoopProxy;

use crate::pane::UserEvent;

#[derive(Clone, Debug, PartialEq)]
pub struct Breakdown {
    pub model: String,
    /// Context window size in tokens.
    pub window: u64,
    /// Rows of /context's "Estimated usage by category" table, in order.
    pub categories: Vec<(String, u64)>,
}

pub const MESSAGES: &str = "Messages";
const FREE: &str = "Free space";
const BUFFER: &str = "Autocompact buffer";

impl Breakdown {
    /// Parse the markdown `/context` prints in print mode.
    pub fn parse(md: &str) -> Option<Breakdown> {
        let mut b = Breakdown { model: String::new(), window: 0, categories: Vec::new() };
        let mut in_table = false;
        for line in md.lines().map(str::trim) {
            if let Some(m) = line.strip_prefix("**Model:**") {
                b.model = m.trim().to_string();
            } else if let Some(t) = line.strip_prefix("**Tokens:**") {
                // "13.7k / 1m (1%)"
                b.window = t.split('/').nth(1).and_then(|w| tokens(w.split_whitespace().next()?))?;
            } else if line.starts_with("###") {
                in_table = line.contains("by category");
            } else if in_table && line.starts_with('|') {
                let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
                if let (Some(name), Some(n)) = (cells.first(), cells.get(1).and_then(|c| tokens(c))) {
                    b.categories.push((name.to_string(), n));
                }
            }
        }
        (b.window > 0 && !b.categories.is_empty()).then_some(b)
    }

    /// Deferred tools are listed by /context but not loaded into the context.
    pub fn is_loaded(name: &str) -> bool {
        !name.contains("(deferred)")
    }

    /// Everything loaded before the conversation itself: system prompt, tools, memory, skills…
    pub fn overhead(&self) -> u64 {
        self.categories.iter().filter(|(n, _)| Self::is_loaded(n) && ![MESSAGES, FREE, BUFFER].contains(&n.as_str())).map(|c| c.1).sum()
    }

    pub fn buffer(&self) -> u64 {
        self.categories.iter().find(|c| c.0 == BUFFER).map_or(0, |c| c.1)
    }

    /// The categories to show for a session using `used` tokens: Messages is the live total
    /// minus the overhead, and Free space whatever remains before the autocompact buffer.
    pub fn rows(&self, used: u64) -> Vec<(String, u64)> {
        let messages = used.saturating_sub(self.overhead());
        let free = self.window.saturating_sub(used.max(self.overhead()) + self.buffer());
        let mut rows: Vec<(String, u64)> = self
            .categories
            .iter()
            .map(|(n, t)| match n.as_str() {
                MESSAGES => (n.clone(), messages),
                FREE => (n.clone(), free),
                _ => (n.clone(), *t),
            })
            .collect();
        if !rows.iter().any(|r| r.0 == MESSAGES) {
            rows.push((MESSAGES.into(), messages));
        }
        rows
    }
}

/// "13.7k", "1m", "639", "1.2M" → tokens.
fn tokens(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    let (num, mult) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1e3),
        'm' => (&s[..s.len() - 1], 1e6),
        _ => (s.as_str(), 1.0),
    };
    num.replace(',', "").parse::<f64>().ok().map(|n| (n * mult).round() as u64)
}

/// "67.2k", "1M", "950".
pub fn short(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3).replace(".0k", "k"),
        _ => format!("{:.1}M", n as f64 / 1e6).replace(".0M", "M"),
    }
}

/// `~/.claude/projects/<folder>/<session id>.jsonl`.
pub fn transcript_path(session_id: &str) -> Option<PathBuf> {
    let file = format!("{session_id}.jsonl");
    std::fs::read_dir(dirs::home_dir()?.join(".claude/projects")).ok()?.flatten().map(|e| e.path().join(&file)).find(|p| p.is_file())
}

/// Tokens in context as of the last main-thread assistant turn (prompt + cached + output),
/// and that turn's model. Reads only the tail of the transcript.
pub fn last_usage(path: &Path) -> Option<(u64, String)> {
    const TAIL: u64 = 512 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines().rev().filter(|l| l.contains("\"usage\"") && l.contains("\"assistant\"")).find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        if v["type"] != "assistant" || v["isSidechain"] == true {
            return None;
        }
        let u = &v["message"]["usage"];
        let n = |k: &str| u[k].as_u64().unwrap_or(0);
        let used = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens") + n("output_tokens");
        (used > 0).then(|| (used, v["message"]["model"].as_str().unwrap_or_default().to_string()))
    })
}

/// Run `/context` in `cwd` through the login shell (so PATH finds `claude`), without saving a
/// session or running the user's hooks. The result arrives as `UserEvent::ContextBreakdown`.
pub fn fetch_async(cwd: PathBuf, proxy: EventLoopProxy<UserEvent>) {
    let _ = std::thread::Builder::new().name("context".into()).spawn(move || {
        let script = r#"claude -p --no-session-persistence --settings '{"disableAllHooks":true}' --output-format json /context"#;
        let Some(mut cmd) = crate::shell::background(script, None) else { return };
        cmd.stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        if cwd.is_dir() {
            cmd.current_dir(&cwd);
        }
        let breakdown = cmd.output().ok().filter(|o| o.status.success()).and_then(|o| {
            let v: serde_json::Value = serde_json::from_slice(&o.stdout).ok()?;
            Breakdown::parse(v["result"].as_str()?)
        });
        if breakdown.is_none() {
            log::warn!("context bar: could not read /context in {}", cwd.display());
        }
        let _ = proxy.send_event(UserEvent::ContextBreakdown(cwd, breakdown));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "## Context Usage\n\n**Model:** claude-opus-5-5[1m]  \n**Tokens:** 13.7k / 1m (1%)\n\n### Estimated usage by category\n\n| Category | Tokens | Percentage |\n|----------|--------|------------|\n| System prompt | 2.4k | 0.2% |\n| MCP tools | 639 | 0.1% |\n| MCP tools (deferred) | 62.3k | 6.2% |\n| Memory files | 1.1k | 0.1% |\n| Skills | 10k | 1.0% |\n| Messages | 10 | 0.0% |\n| Free space | 953.3k | 95.3% |\n| Autocompact buffer | 33k | 3.3% |\n\n### MCP Tools\n\n| Tool | Server | Tokens |\n|------|--------|--------|\n| mcp__x__y | x | 123 |\n";

    #[test]
    fn parses_context_output() {
        let b = Breakdown::parse(SAMPLE).unwrap();
        assert_eq!(b.model, "claude-opus-5-5[1m]");
        assert_eq!(b.window, 1_000_000);
        assert_eq!(b.categories.len(), 8);
        assert_eq!(b.categories[0], ("System prompt".into(), 2_400));
        // Deferred tools, messages, free space and the buffer aren't overhead.
        assert_eq!(b.overhead(), 2_400 + 639 + 1_100 + 10_000);
        assert_eq!(b.buffer(), 33_000);
        let rows = b.rows(100_000);
        assert!(rows.contains(&(MESSAGES.into(), 100_000 - b.overhead())));
        assert!(rows.contains(&(FREE.into(), 1_000_000 - 100_000 - 33_000)));
        assert!(Breakdown::parse("no table").is_none());
    }

    #[test]
    fn formats_tokens() {
        assert_eq!((tokens("13.7k"), tokens("1m"), tokens("639"), tokens("1.2M")), (Some(13_700), Some(1_000_000), Some(639), Some(1_200_000)));
        assert_eq!((short(950), short(67_240), short(1_000_000), short(200_000)), ("950".into(), "67.2k".into(), "1M".into(), "200k".into()));
    }

    #[test]
    fn reads_last_main_thread_usage() {
        let dir = std::env::temp_dir().join(format!("stecak-ctx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s.jsonl");
        std::fs::write(
            &f,
            concat!(
                "{\"type\":\"assistant\",\"message\":{\"model\":\"m1\",\"usage\":{\"input_tokens\":1,\"cache_read_input_tokens\":100,\"output_tokens\":9}}}\n",
                "{\"type\":\"assistant\",\"isSidechain\":true,\"message\":{\"model\":\"m2\",\"usage\":{\"input_tokens\":5}}}\n",
                "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n"
            ),
        )
        .unwrap();
        let got = last_usage(&f);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(got, Some((110, "m1".into())));
    }
}
