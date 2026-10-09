//! Session browser (Cmd+Shift+S): a searchable list of saved Claude Code and Codex
//! sessions; Enter or a click resumes one in a new tab.

use crate::agents::{self, Session};

#[derive(Default)]
pub struct Browser {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    all: Vec<Session>,
}

pub enum Key<'a> {
    Up,
    Down,
    Enter,
    Escape,
    Backspace,
    Text(&'a str),
}

impl Browser {
    /// Re-scan on every open so the list is current; reads only each file's first lines.
    pub fn open(&mut self) {
        *self = Browser { open: true, all: agents::list_sessions(300), ..Default::default() };
    }

    pub fn visible(&self) -> Vec<&Session> {
        self.all.iter().filter(|s| s.matches(&self.query)).collect()
    }

    /// Returns the session to resume, if Enter picked one.
    pub fn handle(&mut self, key: Key) -> Option<Session> {
        let n = self.visible().len();
        match key {
            Key::Up => self.selected = self.selected.saturating_sub(1),
            Key::Down => self.selected = (self.selected + 1).min(n.saturating_sub(1)),
            Key::Escape => self.open = false,
            Key::Backspace => {
                self.query.pop();
                self.selected = 0;
            }
            Key::Text(t) => {
                self.query.push_str(t);
                self.selected = 0;
            }
            Key::Enter => return self.pick(self.selected),
        }
        None
    }

    pub fn pick(&mut self, index: usize) -> Option<Session> {
        let s = self.visible().get(index).map(|s| (*s).clone());
        if s.is_some() {
            self.open = false;
        }
        s
    }
}

/// "3m", "5h", "2d" since `t`.
pub fn ago(t: std::time::SystemTime) -> String {
    let s = t.elapsed().map_or(0, |d| d.as_secs());
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Paths under the home directory shown as `~/…`.
pub fn tilde(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(|p| p.to_path_buf())) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}
