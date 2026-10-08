//! Turning a pane's terminal grid into GPU instances, with a per-row cache driven by
//! alacritty_terminal's damage tracking: a row is re-shaped and rebuilt only when the
//! terminal reports it changed; every other row replays its instances from the cache.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::Match;
use alacritty_terminal::term::{Term, TermDamage};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};

use crate::pane::Listener;
use crate::renderer::{Instance, Renderer};
use crate::text;
use crate::theme::{dim, luma, rgba, Theme};

/// Everything besides cell contents that affects how rows look. Any change → full rebuild.
#[derive(Clone, Copy, PartialEq)]
pub struct CacheKey {
    pub origin: (i32, i32),
    pub cols: usize,
    pub rows: usize,
    pub theme_gen: u64,
    pub atlas_gen: u64,
    pub search_gen: u64,
    pub hover: Option<(i32, usize, usize)>,
    pub selection: Option<SelectionRange>,
    pub display_offset: usize,
}

#[derive(Default)]
pub struct PaneCache {
    key: Option<CacheKey>,
    rows: Vec<Vec<Instance>>,
}

#[derive(Default, Clone, Copy)]
pub struct DrawStats {
    pub rows_built: usize,
    pub rows_cached: usize,
}

pub struct Highlights<'a> {
    pub matches: &'a [Match],
    pub current: Option<&'a Match>,
    /// URL under the mouse while the open-link modifier is held: (line, start col, end col).
    pub hover: Option<(i32, usize, usize)>,
}

/// A run of adjacent cells in one row sharing style and color, shaped as one unit.
struct Run {
    col0: usize,
    style: u8,
    fg: [f32; 3],
    cells: Vec<(char, u16)>,
}

#[allow(clippy::too_many_arguments)]
pub fn draw_pane(
    r: &mut Renderer,
    term: &mut Term<Listener>,
    cache: &mut PaneCache,
    theme: &Theme,
    gx: f32,
    gy: f32,
    theme_gen: u64,
    search_gen: u64,
    hl: &Highlights,
) -> DrawStats {
    let rows = term.screen_lines();
    let cols = term.columns();
    let display_offset = term.grid().display_offset();
    let selection = term.selection.as_ref().and_then(|s| s.to_range(term));
    let key = CacheKey {
        origin: (gx as i32, gy as i32),
        cols,
        rows,
        theme_gen,
        atlas_gen: r.atlas_gen(),
        search_gen,
        hover: hl.hover,
        selection,
        display_offset,
    };

    // Which rows changed since the last frame?
    let mut damaged = vec![false; rows];
    let full = match term.damage() {
        TermDamage::Full => true,
        TermDamage::Partial(it) => {
            for d in it {
                if d.line < rows {
                    damaged[d.line] = true;
                }
            }
            false
        }
    };
    term.reset_damage();
    let full = full || cache.key != Some(key) || cache.rows.len() != rows;
    if full {
        cache.rows.resize_with(rows, Vec::new);
        cache.key = Some(key);
    }

    let mut stats = DrawStats::default();
    for row in 0..rows {
        if full || damaged[row] {
            let mark = r.mark();
            build_row(r, term, theme, row, display_offset, cols, gx, gy, selection, hl);
            let slot = &mut cache.rows[row];
            slot.clear();
            slot.extend_from_slice(r.since(mark));
            stats.rows_built += 1;
        } else {
            r.extend(&cache.rows[row]);
            stats.rows_cached += 1;
        }
    }
    stats
}

#[allow(clippy::too_many_arguments)]
fn build_row(
    r: &mut Renderer,
    term: &Term<Listener>,
    theme: &Theme,
    row: usize,
    display_offset: usize,
    cols: usize,
    gx: f32,
    gy: f32,
    selection: Option<SelectionRange>,
    hl: &Highlights,
) {
    let (cw, ch) = r.cell();
    let line = Line(row as i32 - display_offset as i32);
    let y = gy + row as f32 * ch;
    let colors = term.colors();
    let grid_row = &term.grid()[line];
    let mut runs: Vec<Run> = Vec::new();
    let mut cur: Option<Run> = None;

    for col in 0..cols {
        let cell = &grid_row[Column(col)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let x = gx + col as f32 * cw;
        let point = Point::new(line, Column(col));

        let bold = cell.flags.contains(Flags::BOLD);
        let mut fg_color = cell.fg;
        // Bold-as-bright for the 8 base ANSI colors.
        if bold {
            if let Color::Named(n) = fg_color {
                if (n as usize) < 8 {
                    fg_color = Color::Indexed(n as u8 + 8);
                }
            }
        }
        let mut fg = theme.resolve(fg_color, colors);
        let mut bg = theme.resolve(cell.bg, colors);
        let mut default_bg = cell.bg == Color::Named(NamedColor::Background);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
            default_bg = false;
        }
        if cell.flags.contains(Flags::DIM) {
            fg = dim(fg);
        }

        // Search highlights win over selection, which wins over the cell's own colors.
        let in_current = hl.current.is_some_and(|m| m.contains(&point));
        if in_current || hl.matches.iter().any(|m| m.contains(&point)) {
            bg = if in_current { theme.search_current } else { theme.search_match };
            fg = if luma(bg) > 0.5 { [0.05; 3] } else { [0.95; 3] };
            default_bg = false;
        } else if selection.is_some_and(|s| s.contains(point)) {
            bg = theme.selection;
            default_bg = false;
            // Keep selected text readable when its color is close to the selection color
            // (e.g. zsh-syntax-highlighting's dim path separators).
            if (luma(fg) - luma(bg)).abs() < 0.25 {
                fg = if luma(bg) > 0.5 { [0.05; 3] } else { theme.fg };
            }
        }

        let width = if cell.flags.contains(Flags::WIDE_CHAR) { 2.0 } else { 1.0 };
        if !default_bg {
            r.rect(x, y, cw * width, ch, rgba(bg, 1.0));
        }
        let hovered = hl.hover.is_some_and(|(l, a, b)| l == line.0 && (a..=b).contains(&col));
        if cell.flags.intersects(Flags::ALL_UNDERLINES) || hovered {
            r.rect(x, y + ch - 2.0 * r.scale, cw * width, r.scale.max(1.0), rgba(fg, 1.0));
        }
        if cell.flags.contains(Flags::STRIKEOUT) {
            r.rect(x, y + ch / 2.0, cw * width, r.scale.max(1.0), rgba(fg, 1.0));
        }

        let c = if cell.flags.contains(Flags::HIDDEN) { ' ' } else { cell.c };
        let style = bold as u8 * text::BOLD + cell.flags.contains(Flags::ITALIC) as u8 * text::ITALIC;
        match &mut cur {
            Some(run) if run.style == style && run.fg == fg => run.cells.push((c, (col - run.col0) as u16)),
            _ => {
                runs.extend(cur.take());
                cur = Some(Run { col0: col, style, fg, cells: vec![(c, 0)] });
            }
        }
    }
    runs.extend(cur.take());
    // Text after backgrounds so glyphs (and ligatures overhanging a cell) stay on top.
    for run in &runs {
        r.run(&run.cells, run.style, gx + run.col0 as f32 * cw, y, rgba(run.fg, 1.0));
    }
}

/// The cursor is drawn every frame (never cached): it's a handful of instances.
pub fn draw_cursor(r: &mut Renderer, term: &Term<Listener>, theme: &Theme, gx: f32, gy: f32, focused: bool) {
    let content = term.renderable_content();
    let cursor = content.cursor;
    let offset = content.display_offset as i32;
    let row = cursor.point.line.0 + offset;
    if cursor.shape == CursorShape::Hidden || row < 0 || row as usize >= term.screen_lines() {
        return;
    }
    let (cw, ch) = r.cell();
    let x = gx + cursor.point.column.0 as f32 * cw;
    let y = gy + row as f32 * ch;
    let cc = rgba(theme.resolve(Color::Named(NamedColor::Cursor), content.colors), 1.0);
    let t = (r.scale * 2.0).round();
    match (cursor.shape, focused) {
        (CursorShape::Beam, true) => r.rect(x, y, t, ch, cc),
        (CursorShape::Underline, true) => r.rect(x, y + ch - t, cw, t, cc),
        (CursorShape::Block, true) => {
            r.rect(x, y, cw, ch, cc);
            let c = term.grid()[Point::new(cursor.point.line, cursor.point.column)].c;
            r.run(&[(c, 0)], text::REGULAR, x, y, rgba(theme.bg, 1.0));
        }
        _ => {
            // Hollow block (unfocused pane/window or explicit hollow shape).
            r.rect(x, y, cw, t, cc);
            r.rect(x, y + ch - t, cw, t, cc);
            r.rect(x, y, t, ch, cc);
            r.rect(x + cw - t, y, t, ch, cc);
        }
    }
}
