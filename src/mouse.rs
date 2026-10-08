//! Mouse helpers: xterm mouse-report encoding and URL detection.

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
    if line.0 < -(term.history_size() as i32) || line.0 >= term.screen_lines() as i32 {
        return None;
    }
    let row = &term.grid()[line];
    // One char per column so string offsets map straight back to columns.
    let text: Vec<char> = (0..term.columns())
        .map(|c| {
            let cell = &row[Column(c)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) { ' ' } else { cell.c }
        })
        .collect();
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
