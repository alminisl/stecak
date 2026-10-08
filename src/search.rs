//! Find-in-scrollback (Cmd+F), built on alacritty_terminal's regex search.

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::Term;

use crate::pane::{Listener, PaneId};

#[derive(Default)]
pub struct Search {
    pub open: bool,
    pub pane: PaneId,
    pub query: String,
    regex: Option<RegexSearch>,
    pub current: Option<Match>,
    pub no_match: bool,
}

/// Plain-text, case-insensitive (unless the query has uppercase, like vim's smartcase).
fn pattern(query: &str) -> String {
    let mut p = String::new();
    if !query.chars().any(char::is_uppercase) {
        p.push_str("(?i)");
    }
    for c in query.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            p.push('\\');
        }
        p.push(c);
    }
    p
}

impl Search {
    pub fn open(&mut self, pane: PaneId) {
        self.open = true;
        self.pane = pane;
    }

    pub fn close(&mut self) {
        *self = Search::default();
    }

    pub fn set_query(&mut self, query: String, term: &mut Term<Listener>) {
        self.query = query;
        self.regex = if self.query.is_empty() { None } else { RegexSearch::new(&pattern(&self.query)).ok() };
        self.current = None;
        // Re-search from the bottom of the screen so typing jumps to the nearest match.
        self.step(term, Direction::Left);
    }

    /// Jump to the next match: `Left` = older (up), `Right` = newer (down).
    pub fn step(&mut self, term: &mut Term<Listener>, dir: Direction) {
        let Some(regex) = self.regex.as_mut() else {
            self.no_match = false;
            return;
        };
        let origin = match (&self.current, dir) {
            (Some(m), Direction::Left) => *m.start(),
            (Some(m), Direction::Right) => *m.end(),
            (None, _) => {
                let last_line = Line(term.screen_lines() as i32 - 1 - term.grid().display_offset() as i32);
                Point::new(last_line, Column(term.columns() - 1))
            }
        };
        // Step off the current match so we don't find it again.
        let origin = match (&self.current, dir) {
            (Some(_), Direction::Left) => origin.sub(term, alacritty_terminal::index::Boundary::None, 1),
            (Some(_), Direction::Right) => origin.add(term, alacritty_terminal::index::Boundary::None, 1),
            _ => origin,
        };
        let side = if dir == Direction::Left { Side::Right } else { Side::Left };
        self.current = term.search_next(regex, origin, dir, side, None);
        self.no_match = self.current.is_none();
        if let Some(m) = &self.current {
            scroll_into_view(term, *m.start());
        }
    }

    /// All matches currently on screen, for highlighting.
    pub fn visible_matches(&mut self, term: &Term<Listener>) -> Vec<Match> {
        let Some(regex) = self.regex.as_mut() else { return Vec::new() };
        let offset = term.grid().display_offset() as i32;
        let start = Point::new(Line(-offset), Column(0));
        let end = Point::new(Line(term.screen_lines() as i32 - 1 - offset), Column(term.columns() - 1));
        RegexIter::new(start, end, Direction::Right, term, regex).take(1000).collect()
    }
}

fn scroll_into_view(term: &mut Term<Listener>, point: Point) {
    let offset = term.grid().display_offset() as i32;
    let top = -offset;
    let bottom = term.screen_lines() as i32 - 1 - offset;
    let rows = term.screen_lines() as i32;
    if point.line.0 < top {
        // Center the match vertically.
        term.scroll_display(Scroll::Delta(top - point.line.0 + rows / 2));
    } else if point.line.0 > bottom {
        term.scroll_display(Scroll::Delta(bottom - point.line.0 - rows / 2));
    }
}

#[cfg(test)]
mod tests {
    use super::pattern;

    #[test]
    fn smartcase_and_escaping() {
        assert_eq!(pattern("a.b"), "(?i)a\\.b");
        assert_eq!(pattern("Foo"), "Foo");
    }
}
