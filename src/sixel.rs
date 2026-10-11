//! Sixel images (`ESC P … q <data> ESC \`), as in Windows Terminal 1.22, xterm and foot.
//!
//! alacritty_terminal ignores sixel, so the PTY reader thread lifts sixel sequences out of
//! the byte stream (`Splitter`) and decodes them there (`Decoder`), off the UI thread. A
//! decoded image is anchored in the grid by an invisible OSC 8 link in the first cell of
//! each row it covers (`place`), so it scrolls, clears, and leaves the scrollback together
//! with the text, with no changes to the terminal grid. Drawing looks for those cells
//! (`draw`), only in panes that have images.
//!
//! On macOS/Linux a sixel pixel is a screen pixel at the font size it arrived at, like xterm
//! and foot (apps size images from `CSI 16 t`). On Windows, images are laid out on ConPTY's
//! 10×20 virtual cell (see `layout_cell`). Either way they zoom with the font afterwards.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;

use crate::pane::Listener;
use crate::renderer::Renderer;

/// Largest image dimension decoded; pixels beyond it are dropped. Bounds memory per image
/// (2048² RGBA = 16 MB) against untrusted input, and fits the GPU image atlas.
pub const MAX_SIDE: u32 = 2048;
/// Decoded images kept per pane (their markers may still be on screen or in scrollback).
/// shortcut: oldest images are dropped past this, even if still in the scrollback; track
/// marker lifetimes if people keep many large images in history.
const MAX_STORE_BYTES: usize = 64 << 20;
/// Link scheme of the marker cells.
const MARKER: &str = "stecak-img:";

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn is_marker(uri: &str) -> bool {
    uri.starts_with(MARKER)
}

pub struct Image {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    /// The sixel cursor's band top when the image ended (past the image after a final `-`);
    /// the text cursor ends in the cell row holding it.
    last_band: u32,
}

/// A placed image: pixels plus the cell height it was laid out with (it zooms with the font).
pub struct Placed {
    pub id: u64,
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub cell_h: f32,
}

#[derive(Default)]
pub struct Store {
    images: Vec<Placed>,
    bytes: usize,
}

impl Store {
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    fn push(&mut self, p: Placed) {
        self.bytes += p.rgba.len();
        self.images.push(p);
        while self.bytes > MAX_STORE_BYTES && self.images.len() > 1 {
            self.bytes -= self.images.remove(0).rgba.len();
        }
    }

    fn get(&self, id: u64) -> Option<&Placed> {
        self.images.iter().find(|p| p.id == id)
    }
}

// ---- stream splitting ---------------------------------------------------------------------

pub enum Piece<'a> {
    Text(&'a [u8]),
    Image(Image),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Normal,
    /// The last read ended on ESC.
    Esc,
    /// After `ESC P`: collecting parameters until we know whether this DCS is sixel.
    Params,
    Sixel,
    /// ESC inside sixel data: `ESC \` ends the image.
    SixelEsc,
}

pub struct Splitter {
    state: State,
    /// Bytes of a sequence that may or may not be sixel, given to the parser if it isn't.
    held: Vec<u8>,
    decoder: Option<Box<Decoder>>,
}

impl Default for Splitter {
    fn default() -> Self {
        Splitter { state: State::Normal, held: Vec::new(), decoder: None }
    }
}

impl Splitter {
    /// Split `bytes` into text for the VT parser and decoded images, in stream order. Output
    /// without `ESC P` passes through as one piece (one memchr scan per ESC).
    pub fn feed(&mut self, bytes: &[u8], mut out: impl FnMut(Piece)) {
        let (mut i, mut start) = (0, 0);
        while i < bytes.len() {
            match self.state {
                State::Normal => match memchr::memchr(0x1b, &bytes[i..]) {
                    None => i = bytes.len(),
                    Some(k) => {
                        i += k;
                        match bytes.get(i + 1) {
                            Some(b'P') => {
                                out(Piece::Text(&bytes[start..i]));
                                self.held.extend_from_slice(b"\x1bP");
                                self.state = State::Params;
                                i += 2;
                            }
                            Some(_) => i += 1,
                            None => {
                                out(Piece::Text(&bytes[start..i]));
                                self.held.push(0x1b);
                                self.state = State::Esc;
                                i += 1;
                            }
                        }
                    }
                },
                State::Esc => {
                    if bytes[i] == b'P' {
                        self.held.push(b'P');
                        self.state = State::Params;
                        i += 1;
                    } else {
                        self.release(&mut out);
                        start = i;
                    }
                }
                State::Params => {
                    let b = bytes[i];
                    if b.is_ascii_digit() || b == b';' {
                        self.held.push(b);
                        i += 1;
                    } else if b == b'q' {
                        let params = std::str::from_utf8(&self.held[2..]).unwrap_or_default();
                        let mut p = params.split(';').map(|s| s.parse::<u32>().unwrap_or(0));
                        let (p1, p2) = (p.next().unwrap_or(0), p.next().unwrap_or(0));
                        self.decoder = Some(Box::new(Decoder::new(p1, p2)));
                        self.held.clear();
                        self.state = State::Sixel;
                        i += 1;
                    } else {
                        // Another DCS (DECRQSS, tmux passthrough…): the parser handles it.
                        self.release(&mut out);
                        start = i;
                    }
                }
                State::Sixel => {
                    let rest = &bytes[i..];
                    let k = memchr::memchr3(0x1b, 0x18, 0x1a, rest).unwrap_or(rest.len());
                    if let Some(d) = self.decoder.as_mut() {
                        d.feed(&rest[..k]);
                    }
                    i += k;
                    if let Some(&b) = bytes.get(i) {
                        i += 1;
                        if b == 0x1b {
                            self.state = State::SixelEsc;
                        } else {
                            // CAN / SUB cancel the sequence.
                            self.decoder = None;
                            self.state = State::Normal;
                            start = i;
                        }
                    }
                }
                State::SixelEsc => {
                    let terminated = bytes[i] == b'\\';
                    if let Some(img) = self.decoder.take().and_then(|d| d.finish()) {
                        out(Piece::Image(img));
                    }
                    self.state = State::Normal;
                    if terminated {
                        i += 1;
                    } else {
                        // ESC starting another sequence also ends the image; keep that ESC.
                        out(Piece::Text(b"\x1b"));
                    }
                    start = i;
                }
            }
        }
        if self.state == State::Normal && start < bytes.len() {
            out(Piece::Text(&bytes[start..]));
        }
    }

    fn release(&mut self, out: &mut impl FnMut(Piece)) {
        out(Piece::Text(&self.held));
        self.held.clear();
        self.state = State::Normal;
    }
}

// ---- decoding -----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Cmd {
    None,
    Color,
    Repeat,
    Raster,
}

pub struct Decoder {
    /// Screen pixel rows per sixel pixel (2 by default, 1 for `"1;1`).
    aspect: u32,
    transparent: bool,
    palette: [u32; 256],
    color: u32,
    x: u32,
    y: u32,
    /// Extent written so far, and the buffer's allocated width (rows are `stride` wide).
    w: u32,
    h: u32,
    stride: u32,
    px: Vec<u32>,
    cmd: Cmd,
    params: [u32; 5],
    nparams: usize,
    repeat: u32,
}

impl Decoder {
    fn new(p1: u32, p2: u32) -> Self {
        // DEC's P1 → pixel aspect ratio (Windows Terminal's table). Raster attributes override it.
        let aspect = match p1 {
            2 => 5,
            3 | 4 => 3,
            7..=9 => 1,
            _ => 2,
        };
        Decoder {
            aspect,
            transparent: p2 == 1,
            palette: default_palette(),
            color: 15,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            stride: 0,
            px: Vec::new(),
            cmd: Cmd::None,
            params: [0; 5],
            nparams: 0,
            repeat: 1,
        }
    }

    fn feed(&mut self, data: &[u8]) {
        for &b in data {
            if self.cmd != Cmd::None {
                match b {
                    b'0'..=b'9' => {
                        if self.nparams == 0 {
                            self.nparams = 1;
                        }
                        let p = &mut self.params[self.nparams - 1];
                        *p = p.saturating_mul(10).saturating_add((b - b'0') as u32).min(1 << 16);
                        continue;
                    }
                    b';' => {
                        self.nparams = (self.nparams.max(1) + 1).min(5);
                        continue;
                    }
                    _ => self.run_cmd(),
                }
            }
            match b {
                b'?'..=b'~' => {
                    let n = std::mem::replace(&mut self.repeat, 1);
                    self.sixel(b - b'?', n);
                }
                b'#' => self.start_cmd(Cmd::Color),
                b'!' => self.start_cmd(Cmd::Repeat),
                b'"' => self.start_cmd(Cmd::Raster),
                b'$' => self.x = 0,
                b'-' => {
                    self.x = 0;
                    self.y += 6 * self.aspect;
                }
                _ => {}
            }
        }
    }

    fn start_cmd(&mut self, cmd: Cmd) {
        self.cmd = cmd;
        self.params = [0; 5];
        self.nparams = 0;
    }

    fn run_cmd(&mut self) {
        let p = self.params;
        match self.cmd {
            Cmd::Repeat => self.repeat = p[0].max(1),
            Cmd::Color => {
                let reg = (p[0] % 256) as usize;
                if self.nparams >= 5 {
                    self.palette[reg] = match p[1] {
                        1 => hls(p[2], p[3], p[4]),
                        _ => rgba(pct(p[2]), pct(p[3]), pct(p[4])),
                    };
                }
                self.color = reg as u32;
            }
            Cmd::Raster => {
                if p[0] > 0 && p[1] > 0 {
                    self.aspect = p[0].div_ceil(p[1]).clamp(1, 10);
                }
                // Declared size: the area the background fills.
                if self.nparams >= 4 {
                    self.w = self.w.max(p[2].min(MAX_SIDE));
                    self.h = self.h.max(p[3].min(MAX_SIDE));
                    self.reserve(self.w, self.h);
                }
            }
            Cmd::None => {}
        }
        self.cmd = Cmd::None;
    }

    /// One sixel (6 vertical pixels, `aspect` rows each), repeated `n` times.
    fn sixel(&mut self, bits: u8, n: u32) {
        let x0 = self.x;
        self.x = self.x.saturating_add(n);
        let x1 = self.x.min(MAX_SIDE);
        let band_h = 6 * self.aspect;
        if x0 >= MAX_SIDE || self.y >= MAX_SIDE {
            return;
        }
        self.w = self.w.max(x1);
        self.h = self.h.max((self.y + band_h).min(MAX_SIDE));
        if bits == 0 {
            return;
        }
        self.reserve(x1, self.h);
        let color = self.palette[self.color as usize];
        for bit in 0..6 {
            if bits & (1 << bit) == 0 {
                continue;
            }
            for row in self.y + bit * self.aspect..(self.y + (bit + 1) * self.aspect).min(MAX_SIDE) {
                let base = (row * self.stride) as usize;
                self.px[base + x0 as usize..base + x1 as usize].fill(color);
            }
        }
    }

    /// Grow the buffer to hold `w × h`, doubling the width so wide images re-lay rows rarely.
    fn reserve(&mut self, w: u32, h: u32) {
        if w > self.stride {
            let stride = w.max(self.stride * 2).max(64).min(MAX_SIDE);
            let rows = self.px.len() / self.stride.max(1) as usize;
            let mut px = vec![0u32; rows * stride as usize];
            for r in 0..rows {
                let old = &self.px[r * self.stride as usize..(r + 1) * self.stride as usize];
                px[r * stride as usize..r * stride as usize + old.len()].copy_from_slice(old);
            }
            self.px = px;
            self.stride = stride;
        }
        let need = h as usize * self.stride as usize;
        if self.px.len() < need {
            self.px.resize(need, 0);
        }
    }

    fn finish(mut self: Box<Self>) -> Option<Image> {
        if self.cmd != Cmd::None {
            self.run_cmd();
        }
        let (w, h) = (self.w, self.h);
        if w == 0 || h == 0 {
            return None;
        }
        self.reserve(w, h);
        // Pixels never drawn are see-through when P2 = 1, else color register 0.
        let bg = if self.transparent { 0 } else { self.palette[0] };
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for r in 0..h as usize {
            let row = &self.px[r * self.stride as usize..r * self.stride as usize + w as usize];
            rgba.extend(row.iter().flat_map(|&p| (if p == 0 { bg } else { p }).to_le_bytes()));
        }
        Some(Image { rgba, w, h, last_band: self.y })
    }
}

fn pct(v: u32) -> u8 {
    (v.min(100) * 255 / 100) as u8
}

/// Packed RGBA as little-endian bytes; alpha 255 marks a pixel as drawn.
fn rgba(r: u8, g: u8, b: u8) -> u32 {
    u32::from_le_bytes([r, g, b, 255])
}

/// DEC HLS (hue 0 = blue, 120 = red, 240 = green) to RGB.
fn hls(h: u32, l: u32, s: u32) -> u32 {
    let h = ((h + 240) % 360) as f32;
    let (l, s) = (l.min(100) as f32 / 100.0, s.min(100) as f32 / 100.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    rgba(to(r), to(g), to(b))
}

/// VT340's 16 colors, then the xterm 256-color cube and grays (Windows Terminal does the same).
fn default_palette() -> [u32; 256] {
    const VT340: [[u32; 3]; 16] = [
        [0, 0, 0], [20, 20, 80], [80, 13, 13], [20, 80, 20], [80, 20, 80], [20, 80, 80], [80, 80, 20], [53, 53, 53],
        [26, 26, 26], [33, 33, 60], [60, 26, 26], [33, 60, 33], [60, 33, 60], [33, 60, 60], [60, 60, 33], [80, 80, 80],
    ];
    let mut p = [0u32; 256];
    for (i, c) in VT340.iter().enumerate() {
        p[i] = rgba(pct(c[0]), pct(c[1]), pct(c[2]));
    }
    let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
    for i in 16..232 {
        let v = i - 16;
        p[i] = rgba(level(v / 36), level(v / 6 % 6), level(v % 6));
    }
    for i in 232..256 {
        let g = (8 + (i - 232) * 10) as u8;
        p[i] = rgba(g, g, g);
    }
    p
}

// ---- placing and drawing ------------------------------------------------------------------

/// The cell size images are laid out on. Under ConPTY (Windows) the console host parses
/// sixel too, on VT340's 10×20 pixel cell, and both sides must agree on where the cursor ends
/// up; images are then scaled to the real cells when drawn, as in Windows Terminal.
fn layout_cell(real: (u16, u16)) -> (u32, u32) {
    if cfg!(windows) { (10, 20) } else { (real.0.max(1) as u32, real.1.max(1) as u32) }
}

/// Bytes for the VT parser that reserve the image's cells at the cursor (one marker cell per
/// row, scrolling as needed) and leave the cursor where DEC terminals do: the image's
/// starting column, on the row holding the sixel cursor. The image is stored.
pub fn place(store: &mut Store, mut img: Image, term: &Term<Listener>, cell: (u16, u16)) -> String {
    let (cw, ch) = layout_cell(cell);
    let col = term.grid().cursor.point.column.0;
    // Crop to the space right of the cursor, like Windows Terminal.
    let avail = (term.columns().saturating_sub(col) as u32 * cw).max(1);
    if img.w > avail {
        let (old_w, w) = (img.w as usize * 4, avail as usize * 4);
        let rgba: Vec<u8> = img.rgba.chunks_exact(old_w).flat_map(|row| &row[..w]).copied().collect();
        img = Image { rgba, w: avail, ..img };
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let rows = img.h.div_ceil(ch).max(1);
    let last = img.last_band / ch;
    let mut s = String::new();
    for row in 0..rows {
        let _ = write!(s, "\x1b[{}G\x1b]8;;{MARKER}{id}:{row}\x1b\\ \x1b]8;;\x1b\\", col + 1);
        if row + 1 < rows {
            s.push('\n');
        }
    }
    if rows - 1 > last {
        let _ = write!(s, "\x1b[{}A", rows - 1 - last);
    }
    for _ in rows..=last {
        s.push('\n'); // after a final `-`, the row below the image
    }
    let _ = write!(s, "\x1b[{}G", col + 1);
    store.push(Placed { id, rgba: img.rgba, w: img.w, h: img.h, cell_h: ch as f32 });
    s
}

/// Draw the images whose marker cells are on screen, clipped to the pane's grid.
#[allow(clippy::too_many_arguments)]
pub fn draw(r: &mut Renderer, term: &Term<Listener>, store: &Store, gx: f32, gy: f32, cols: usize, rows: usize) {
    let (cw, ch) = r.cell();
    let offset = term.grid().display_offset() as i32;
    let rows = rows.min(term.screen_lines());
    let cols = cols.min(term.columns());
    let (clip_x, clip_y) = (gx + cols as f32 * cw, gy + rows as f32 * ch);
    let mut drawn: Vec<u64> = Vec::new();
    for screen_row in 0..rows {
        let row = &term.grid()[Line(screen_row as i32 - offset)];
        for c in 0..cols {
            let cell = &row[Column(c)];
            if cell.extra.is_none() {
                continue;
            }
            let Some(link) = cell.hyperlink() else { continue };
            let Some((id, img_row)) = link.uri().strip_prefix(MARKER).and_then(|m| m.split_once(':')) else { continue };
            let (Ok(id), Ok(img_row)) = (id.parse::<u64>(), img_row.parse::<i32>()) else { continue };
            if drawn.contains(&id) {
                continue;
            }
            drawn.push(id);
            let Some(img) = store.get(id) else { continue };
            let scale = ch / img.cell_h;
            let (x, y) = (gx + c as f32 * cw, gy + (screen_row as i32 - img_row) as f32 * ch);
            let (w, h) = (img.w as f32 * scale, img.h as f32 * scale);
            // Visible part, as fractions of the image.
            let (x0, y0) = (x.max(gx), y.max(gy));
            let (x1, y1) = ((x + w).min(clip_x), (y + h).min(clip_y));
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let frac = [(x0 - x) / w, (y0 - y) / h, (x1 - x0) / w, (y1 - y0) / h];
            r.sixel(id, &img.rgba, img.w, img.h, [x0, y0, x1 - x0, y1 - y0], frac);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Image>) {
        let mut s = Splitter::default();
        let (mut text, mut images) = (Vec::new(), Vec::new());
        for c in chunks {
            s.feed(c, |p| match p {
                Piece::Text(t) => text.extend_from_slice(t),
                Piece::Image(i) => images.push(i),
            });
        }
        (text, images)
    }

    fn px(img: &Image, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.w + x) * 4) as usize;
        img.rgba[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn passes_other_output_through() {
        let (text, images) = split(&[b"a\x1b[31mb\x1b", b"Pzz\x1b\\c\x1bP", b"1$qm\x1b\\"]);
        assert_eq!(text, b"a\x1b[31mb\x1bPzz\x1b\\c\x1bP1$qm\x1b\\");
        assert!(images.is_empty());
    }

    #[test]
    fn decodes_across_reads() {
        // 1:1 aspect, transparent, red register 1; "!3~" = 3 columns of all six pixels,
        // "-" next band, "@" = only the top pixel.
        let (text, images) = split(&[b"x\x1bP0;1", b"q\"1;1;4;12#1;2;100;0;0#1!3~-", b"@\x1b", b"\\y"]);
        assert_eq!(text, b"xy");
        let img = &images[0];
        assert_eq!((img.w, img.h, img.last_band), (4, 12, 6));
        assert_eq!(px(img, 2, 5), [255, 0, 0, 255]);
        assert_eq!(px(img, 3, 0), [0, 0, 0, 0]); // declared width, never drawn: transparent
        assert_eq!(px(img, 0, 6), [255, 0, 0, 255]);
        assert_eq!(px(img, 0, 7), [0, 0, 0, 0]);
    }

    #[test]
    fn default_aspect_and_opaque_background() {
        let (_, images) = split(&[b"\x1bPq#2@\x1b\\"]);
        let img = &images[0];
        // P1 omitted = 2:1, so one sixel is 12 rows; P2 = 0 fills with register 0 (black).
        assert_eq!((img.w, img.h), (1, 12));
        assert_eq!(px(img, 0, 1), [204, 33, 33, 255]);
        assert_eq!(px(img, 0, 2), [0, 0, 0, 255]);
    }

    #[test]
    fn huge_or_cancelled_input_is_bounded() {
        let (text, images) = split(&[b"\x1bPq\"1;1;99999;99999!99999~\x1b\\", b"\x1bPq~~\x18ok"]);
        assert_eq!(text, b"ok");
        assert_eq!((images[0].w, images[0].h), (MAX_SIDE, MAX_SIDE));
        assert_eq!(images.len(), 1);
    }

    #[test]
    fn hls_hues_follow_dec() {
        assert_eq!(hls(0, 50, 100), rgba(0, 0, 255));
        assert_eq!(hls(120, 50, 100), rgba(255, 0, 0));
        assert_eq!(hls(240, 50, 100), rgba(0, 255, 0));
    }
}
