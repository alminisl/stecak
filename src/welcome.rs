//! Welcome screen: a stećak drawn in text plus an inscription formula, printed into the
//! first tab at startup (`welcome: false` turns it off).

/// Formulas found on many stećci: (original, English).
const INSCRIPTIONS: &[(&str, &str)] = &[
    ("Ase leži …", "Here lies … (how countless stećak epitaphs begin)"),
    ("Ja sam bil kako vi jeste, a vi ćete biti kako i jesam.", "I was as you are, and you will be as I am."),
    ("Va ime Oca i Sina i Svetoga Duha.", "In the name of the Father, the Son and the Holy Spirit."),
];

const ART: &[&str] = &[
    r"         ╱╲         ",
    r"       ╱    ╲       ",
    r"     ╱   ✻    ╲     ",
    r"   ╱____________╲   ",
    r"   │            │   ",
    r"   │  ❯ _       │   ",
    r"   │            │   ",
    r"   │            │   ",
    r"▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔",
];

const STONE: &str = "\x1b[38;2;217;210;193m";
const AMBER: &str = "\x1b[38;2;242;166;90m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const RESET: &str = "\x1b[0m";

/// The banner as terminal output (CRLF line ends), fitted to `cols`.
pub fn banner(cols: usize, seed: u64) -> String {
    let (original, english) = INSCRIPTIONS[(seed % INSCRIPTIONS.len() as u64) as usize];
    let (legend, keys) = if cfg!(target_os = "macos") {
        ("Press ⌘/ to see all keyboard shortcuts", "⌘, settings · ⌘⇧S sessions")
    } else {
        ("Press Ctrl+Shift+/ to see all keyboard shortcuts", "Ctrl+Shift+, settings · Ctrl+Shift+S sessions")
    };
    let text = [
        format!("{BOLD}Stećak {}{RESET}", env!("CARGO_PKG_VERSION")),
        format!("{DIM}a terminal carved to last{RESET}"),
        String::new(),
        format!("{ITALIC}“{original}”{RESET}"),
        format!("{DIM}{english}{RESET}"),
        String::new(),
        format!("{AMBER}{legend}{RESET}"),
        format!("{DIM}{keys}{RESET}"),
    ];
    let mut out = String::from("\r\n");
    // Text column vertically centred beside the stone.
    let top = (ART.len() - text.len()) / 2;
    for (i, art) in ART.iter().enumerate() {
        // Colour the carvings: rosette and prompt in amber, the stone in limestone.
        let art = art.replace('✻', &format!("{AMBER}✻{STONE}")).replace("❯ _", &format!("{AMBER}❯ _{STONE}"));
        let side = if cols >= 80 { i.checked_sub(top).and_then(|j| text.get(j)).map_or("", |s| s.as_str()) } else { "" };
        out.push_str(&format!("  {STONE}{art}{RESET}    {side}\r\n"));
    }
    if cols < 80 {
        for line in &text {
            out.push_str(&format!("  {line}\r\n"));
        }
    }
    out.push_str("\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_fits_and_rotates() {
        let wide = banner(100, 1);
        assert!(wide.contains("Ja sam bil kako vi jeste"));
        assert!(banner(100, 0).contains("Ase leži"));
        // Narrow windows stack the text under the drawing instead of beside it.
        assert!(banner(60, 2).lines().count() > ART.len() + 5);
        assert!(banner(100, 2).lines().count() < ART.len() + 4);
        // Every art row is the same width so the text column lines up.
        assert!(ART.iter().all(|l| l.chars().count() == ART[0].chars().count()));
    }
}
