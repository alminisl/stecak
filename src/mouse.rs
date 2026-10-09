//! Mouse helpers: xterm mouse-report encoding and URL detection.

use std::path::{Path, PathBuf};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use winit::keyboard::ModifiersState;

use crate::pane::Listener;

#[derive(Clone, Copy, PartialEq)]
pub enum Button {
    Left = 0,
    Middle = 1,
    Right = 2,
    WheelUp = 64,
    WheelDown = 65,
}

/// Encode a mouse event for the application (vim, tmux, htop…). `col`/`row` are 0-based
/// viewport cells. Returns None if the app didn't ask for this kind of event.
pub fn report(mode: TermMode, button: Button, pressed: bool, motion: bool, col: usize, row: usize, mods: ModifiersState) -> Option<Vec<u8>> {
    if motion && !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
        return None;
    }
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let mut code = button as u8;
    if motion {
        code += 32;
    }
    code += 4 * mods.shift_key() as u8 + 8 * mods.alt_key() as u8 + 16 * mods.control_key() as u8;
    let (x, y) = (col + 1, row + 1);
    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{code};{x};{y}{suffix}").into_bytes());
    }
    // Legacy X10 encoding: release is button 3 and coordinates are limited to 223.
    if !pressed && !matches!(button, Button::WheelUp | Button::WheelDown) {
        code = (code & !3) | 3;
    }
    if x > 223 || y > 223 {
        return None;
    }
    Some(vec![0x1b, b'[', b'M', 32 + code, 32 + x as u8, 32 + y as u8])
}

/// URL under (viewport) cell `col` on grid line `line`: (start col, end col inclusive, url).
pub fn url_at(term: &Term<Listener>, line: Line, col: usize) -> Option<(usize, usize, String)> {
    let text = row_chars(term, line)?;
    const SCHEMES: [&str; 5] = ["https://", "http://", "file://", "ftp://", "mailto:"];
    let s: String = text.iter().collect();
    for scheme in SCHEMES {
        let mut from = 0;
        while let Some(byte_idx) = s[from..].find(scheme) {
            let start = s[..from + byte_idx].chars().count();
            let mut end = start;
            while end < text.len() && !matches!(text[end], ' ' | '\t' | '\0' | '"' | '\'' | '<' | '>' | '`' | '|') {
                end += 1;
            }
            // Trim trailing punctuation; keep ')' only if the URL has a matching '('.
            while end > start {
                let last = text[end - 1];
                let unbalanced_paren = last == ')' && !text[start..end].contains(&'(');
                if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | ']' | '}') || unbalanced_paren {
                    end -= 1;
                } else {
                    break;
                }
            }
            if end > start + scheme.len() && (start..end).contains(&col) {
                return Some((start, end - 1, text[start..end].iter().collect()));
            }
            from += byte_idx + scheme.len();
        }
    }
    None
}

/// OSC 8 hyperlink under the cell (agents print clickable file and web links this way):
/// (start col, end col inclusive, uri), spanning the adjacent cells with the same link.
pub fn hyperlink_at(term: &Term<Listener>, line: Line, col: usize) -> Option<(usize, usize, String)> {
    let row = &term.grid()[line];
    let link = row[Column(col)].hyperlink()?;
    let same = |c: usize| row[Column(c)].hyperlink().is_some_and(|h| h.id() == link.id() && h.uri() == link.uri());
    let (mut a, mut b) = (col, col);
    while a > 0 && same(a - 1) {
        a -= 1;
    }
    while b + 1 < term.columns() && same(b + 1) {
        b += 1;
    }
    Some((a, b, link.uri().to_string()))
}

/// Something ⌘-clickable under the mouse.
#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    Url(String),
    /// An existing file or folder, with an optional `:line:col`.
    File(PathBuf, Option<u32>, Option<u32>),
}

/// Characters of the row as one char per column, so offsets map straight back to columns.
fn row_chars(term: &Term<Listener>, line: Line) -> Option<Vec<char>> {
    if line.0 < -(term.history_size() as i32) || line.0 >= term.screen_lines() as i32 {
        return None;
    }
    let row = &term.grid()[line];
    Some((0..term.columns()).map(|c| if row[Column(c)].flags.contains(Flags::WIDE_CHAR_SPACER) { ' ' } else { row[Column(c)].c }).collect())
}

/// The visible screen as plain text (for "Explain last error" without shell integration).
pub fn screen_text(term: &Term<Listener>) -> String {
    let rows: Vec<String> = (0..term.screen_lines() as i32).filter_map(|l| row_chars(term, Line(l))).map(|r| r.into_iter().collect::<String>().trim_end().to_string()).collect();
    rows.join("\n").trim().to_string()
}

/// A file path under the cell, like `src/main.rs:120:5` in compiler or agent output, that
/// exists relative to `cwd`: (start col, end col inclusive, link).
pub fn path_at(term: &Term<Listener>, line: Line, col: usize, cwd: Option<&Path>) -> Option<(usize, usize, Link)> {
    let text = row_chars(term, line)?;
    let (start, end, token) = path_token(&text, col)?;
    let (path, line_no, col_no) = split_location(&token);
    let path = match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest),
        None if Path::new(path).is_absolute() => PathBuf::from(path),
        // git diff prefixes: a/src/x.rs, b/src/x.rs
        None => {
            let base = cwd?;
            let direct = base.join(path);
            match path.strip_prefix("a/").or_else(|| path.strip_prefix("b/")) {
                Some(rest) if !direct.exists() => base.join(rest),
                _ => direct,
            }
        }
    };
    path.exists().then(|| (start, end, Link::File(path, line_no, col_no)))
}

/// The path-like word around `col`: no spaces or quotes/brackets, and it has a '/' or '.'.
fn path_token(text: &[char], col: usize) -> Option<(usize, usize, String)> {
    let stop = |c: char| c.is_whitespace() || matches!(c, '\0' | '"' | '\'' | '`' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | ',' | '│');
    if col >= text.len() || stop(text[col]) {
        return None;
    }
    let mut a = col;
    while a > 0 && !stop(text[a - 1]) {
        a -= 1;
    }
    let mut b = col + 1;
    while b < text.len() && !stop(text[b]) {
        b += 1;
    }
    // Trailing punctuation ("see main.rs." or "main.rs:").
    while b > a && matches!(text[b - 1], '.' | ':' | ';' | '!' | '?') {
        b -= 1;
    }
    let token: String = text[a..b].iter().collect();
    let looks_like_path = token.chars().count() > 1 && (token.contains('/') || token.contains('.')) && !token.contains("://");
    (looks_like_path && b > col).then(|| (a, b - 1, token))
}

/// "file:12:5" → ("file", Some(12), Some(5)).
fn split_location(token: &str) -> (&str, Option<u32>, Option<u32>) {
    let mut parts = token.rsplitn(3, ':');
    let last = parts.next().unwrap_or(token);
    let mid = parts.next();
    let first = parts.next();
    match (first, mid.map(str::parse::<u32>), last.parse::<u32>()) {
        (Some(f), Some(Ok(l)), Ok(c)) => (f, Some(l), Some(c)),
        (None, Some(_), Ok(l)) => (mid.unwrap(), Some(l), None),
        (Some(f), Some(Err(_)), Ok(l)) => (&token[..f.len() + 1 + mid.unwrap().len()], Some(l), None),
        _ => (token, None, None),
    }
}

/// Programs that need a terminal: these open in a new tab instead of a window.
const TERMINAL_EDITORS: [&str; 10] = ["vim", "nvim", "vi", "hx", "helix", "nano", "micro", "kak", "emacs", "jed"];

/// Open a file at a line. Returns a command to run in a new tab for terminal editors.
pub fn open_file(path: &Path, line: Option<u32>, col: Option<u32>, editor: &str) -> Option<String> {
    if path.is_dir() {
        open_url(&path.display().to_string());
        return None;
    }
    let quoted = format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let editor = editor.trim();
    if !editor.is_empty() {
        let mut cmd = editor.replace("{line}", &line.unwrap_or(1).to_string()).replace("{col}", &col.unwrap_or(1).to_string());
        cmd = if cmd.contains("{file}") { cmd.replace("{file}", &quoted) } else { format!("{cmd} {quoted}") };
        let program = editor.split_whitespace().next().and_then(|p| p.rsplit('/').next()).unwrap_or_default();
        if TERMINAL_EDITORS.contains(&program) {
            return Some(cmd);
        }
        spawn_login_shell(&cmd, &[]);
        return None;
    }
    // Auto: the first GUI editor with a command-line launcher, else the default app.
    let fallback = if cfg!(target_os = "macos") { "open \"$F\"" } else { "xdg-open \"$F\"" };
    let script = format!(
        "L=\"$F${{LN:+:$LN}}${{CN:+:$CN}}\"; \
         if command -v code >/dev/null; then code -g \"$L\"; \
         elif command -v cursor >/dev/null; then cursor -g \"$L\"; \
         elif command -v zed >/dev/null; then zed \"$L\"; \
         else {fallback}; fi"
    );
    let (ln, cn) = (line.map(|l| l.to_string()).unwrap_or_default(), col.map(|c| c.to_string()).unwrap_or_default());
    spawn_login_shell(&script, &[("F", &path.display().to_string()), ("LN", &ln), ("CN", &cn)]);
    None
}

/// Run a command through the login shell (so PATH has `code`, `zed`…) without waiting.
fn spawn_login_shell(script: &str, env: &[(&str, &str)]) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut cmd = std::process::Command::new(shell);
    cmd.args(["-lc", script]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    match cmd.spawn() {
        // Reap it in the background so it doesn't linger as a zombie.
        Ok(mut child) => {
            let _ = std::thread::Builder::new().name("open-file".into()).stack_size(64 * 1024).spawn(move || child.wait());
        }
        Err(e) => log::error!("could not open file: {e}"),
    }
}

pub fn open_url(url: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("explorer").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(e) = result {
        log::error!("could not open {url}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_paths_with_locations() {
        let text: Vec<char> = "error at src/main.rs:120:5, see (README.md).".chars().collect();
        assert_eq!(path_token(&text, 12).map(|t| t.2), Some("src/main.rs:120:5".into()));
        assert_eq!(path_token(&text, 34).map(|t| t.2), Some("README.md".into()));
        assert!(path_token(&text, 2).is_none()); // "error" isn't path-like
        assert_eq!(split_location("src/main.rs:120:5"), ("src/main.rs", Some(120), Some(5)));
        assert_eq!(split_location("a.rs:7"), ("a.rs", Some(7), None));
        assert_eq!(split_location("a.rs"), ("a.rs", None, None));
        assert_eq!(split_location("/x/a:b.rs:3"), ("/x/a:b.rs", Some(3), None));
    }

    #[test]
    fn sgr_and_legacy_reports() {
        let m = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(report(m, Button::Left, true, false, 4, 2, ModifiersState::empty()).unwrap(), b"\x1b[<0;5;3M");
        assert_eq!(report(m, Button::Left, false, false, 4, 2, ModifiersState::empty()).unwrap(), b"\x1b[<0;5;3m");
        let legacy = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(report(legacy, Button::Left, false, false, 0, 0, ModifiersState::empty()).unwrap(), vec![0x1b, b'[', b'M', 35, 33, 33]);
        assert!(report(TermMode::empty(), Button::Left, true, false, 0, 0, ModifiersState::empty()).is_none());
        assert!(report(legacy, Button::Left, true, true, 0, 0, ModifiersState::empty()).is_none());
    }
}
