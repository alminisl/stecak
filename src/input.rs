//! Keyboard → app shortcut or bytes for the PTY (xterm encoding).

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

pub enum Action {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    SelectTab(usize),
    OpenSettings,
    Paste,
    FontBigger,
    FontSmaller,
    FontReset,
    Write(Vec<u8>),
}

/// App shortcuts use Cmd on macOS and Ctrl+Shift elsewhere, so plain Ctrl+letter
/// always reaches the shell.
fn is_app_modifier(m: ModifiersState) -> bool {
    if cfg!(target_os = "macos") {
        m.super_key()
    } else {
        m.control_key() && m.shift_key()
    }
}

fn shortcut(event: &KeyEvent, m: ModifiersState) -> Option<Action> {
    if let Key::Named(NamedKey::Tab) = event.logical_key {
        if m.control_key() {
            return Some(if m.shift_key() { Action::PrevTab } else { Action::NextTab });
        }
    }
    if !is_app_modifier(m) {
        return None;
    }
    let Key::Character(c) = event.key_without_modifiers() else { return None };
    Some(match c.as_str() {
        "t" => Action::NewTab,
        "w" => Action::CloseTab,
        "," => Action::OpenSettings,
        "v" => Action::Paste,
        "]" => Action::NextTab,
        "[" => Action::PrevTab,
        "=" | "+" => Action::FontBigger,
        "-" => Action::FontSmaller,
        "0" => Action::FontReset,
        d if d.len() == 1 && ('1'..='9').contains(&d.chars().next().unwrap()) => {
            Action::SelectTab(d.parse::<usize>().unwrap() - 1)
        }
        _ => return None,
    })
}

pub fn translate(event: &KeyEvent, m: ModifiersState, app_cursor: bool, option_as_alt: bool) -> Option<Action> {
    if let Some(a) = shortcut(event, m) {
        return Some(a);
    }
    if cfg!(target_os = "macos") && m.super_key() {
        return None; // Unbound Cmd shortcuts are swallowed, never sent to the shell.
    }
    let bytes = encode(event, m, app_cursor, option_as_alt)?;
    Some(Action::Write(bytes))
}

fn encode(event: &KeyEvent, m: ModifiersState, app_cursor: bool, option_as_alt: bool) -> Option<Vec<u8>> {
    // xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
    let modp = 1 + m.shift_key() as u8 + 2 * m.alt_key() as u8 + 4 * m.control_key() as u8;
    let csi = |final_: char| -> Vec<u8> {
        if modp > 1 {
            format!("\x1b[1;{modp}{final_}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{final_}").into_bytes()
        } else {
            format!("\x1b[{final_}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if modp > 1 { format!("\x1b[{n};{modp}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
    };

    if let Key::Named(named) = &event.logical_key {
        let out = match named {
            NamedKey::Enter => b"\r".to_vec(),
            NamedKey::Backspace => if m.control_key() { b"\x08".to_vec() } else if m.alt_key() { b"\x1b\x7f".to_vec() } else { b"\x7f".to_vec() },
            NamedKey::Tab => if m.shift_key() { b"\x1b[Z".to_vec() } else { b"\t".to_vec() },
            NamedKey::Escape => b"\x1b".to_vec(),
            NamedKey::Space => if m.control_key() { vec![0] } else if m.alt_key() { b"\x1b ".to_vec() } else { b" ".to_vec() },
            NamedKey::ArrowUp => csi('A'),
            NamedKey::ArrowDown => csi('B'),
            NamedKey::ArrowRight => csi('C'),
            NamedKey::ArrowLeft => csi('D'),
            NamedKey::Home => csi('H'),
            NamedKey::End => csi('F'),
            NamedKey::Insert => tilde(2),
            NamedKey::Delete => tilde(3),
            NamedKey::PageUp => tilde(5),
            NamedKey::PageDown => tilde(6),
            NamedKey::F1 => b"\x1bOP".to_vec(),
            NamedKey::F2 => b"\x1bOQ".to_vec(),
            NamedKey::F3 => b"\x1bOR".to_vec(),
            NamedKey::F4 => b"\x1bOS".to_vec(),
            NamedKey::F5 => tilde(15),
            NamedKey::F6 => tilde(17),
            NamedKey::F7 => tilde(18),
            NamedKey::F8 => tilde(19),
            NamedKey::F9 => tilde(20),
            NamedKey::F10 => tilde(21),
            NamedKey::F11 => tilde(23),
            NamedKey::F12 => tilde(24),
            _ => return None,
        };
        return Some(out);
    }

    // Ctrl+key → C0 control codes.
    if m.control_key() {
        if let Key::Character(c) = event.key_without_modifiers() {
            let ch = c.chars().next()?;
            let code = match ch.to_ascii_lowercase() {
                'a'..='z' => Some(ch.to_ascii_lowercase() as u8 - b'a' + 1),
                '@' | '2' | ' ' => Some(0),
                '[' | '3' => Some(0x1b),
                '\\' | '4' => Some(0x1c),
                ']' | '5' => Some(0x1d),
                '^' | '6' => Some(0x1e),
                '_' | '-' | '7' => Some(0x1f),
                '8' => Some(0x7f),
                _ => None,
            };
            if let Some(code) = code {
                return Some(if m.alt_key() { vec![0x1b, code] } else { vec![code] });
            }
        }
    }

    // Alt/Option+key → ESC prefix (on macOS only when option_as_alt, otherwise Option composes characters).
    let alt_as_meta = m.alt_key() && (!cfg!(target_os = "macos") || option_as_alt);
    if alt_as_meta {
        if let Key::Character(c) = event.key_without_modifiers() {
            let s = if m.shift_key() { c.to_uppercase() } else { c.to_string() };
            let mut out = vec![0x1b];
            out.extend_from_slice(s.as_bytes());
            return Some(out);
        }
    }

    event.text.as_ref().map(|t| t.as_bytes().to_vec())
}
