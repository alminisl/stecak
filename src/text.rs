//! Font loading, shaping (ligatures) and glyph rasterization, via swash.
//!
//! The terminal grid is shaped in *runs*: consecutive cells in a row that share a style
//! and color. Each run goes through the OpenType shaper with `calt`/`liga` enabled, so
//! programming ligatures (`->`, `=>`, `!=`, `===`…) come out the way the font designer
//! intended, while every glyph still snaps to its cell column. Shaped runs are cached by
//! their text, so a static screen costs almost nothing to re-render.

use std::collections::HashMap;
use std::sync::Arc;

use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::ShapeContext;
use swash::zeno::Format;
use swash::{CacheKey, FontRef};

use crate::config::Config;

/// Style index into the first four faces.
pub const REGULAR: u8 = 0;
pub const BOLD: u8 = 1;
pub const ITALIC: u8 = 2;
/// Face slot used for Bosančica mode (falls back to regular if the font isn't installed).
pub const BOSANCICA: u8 = 4;
/// First fallback face slot.
const FIRST_FALLBACK: usize = 5;

/// A font face backed by a memory-mapped file. Mapped pages are clean and shared with the
/// OS file cache, so they don't count toward our footprint (unlike copying the file into a
/// Vec, which for a CJK collection alone is tens of MB).
struct Face {
    data: Arc<memmap2::Mmap>,
    offset: u32,
    key: CacheKey,
}

impl Face {
    fn open(src: &FontSource) -> Option<Face> {
        let file = std::fs::File::open(&src.path).ok()?;
        // SAFETY: font files are not expected to be modified while mapped; worst case a
        // concurrent rewrite produces garbled glyphs, never memory unsafety in safe code.
        let data = Arc::new(unsafe { memmap2::Mmap::map(&file) }.ok()?);
        let font = FontRef::from_index(&data, src.index as usize)?;
        let (offset, key) = (font.offset, font.key);
        Some(Face { data, offset, key })
    }

    fn font(&self) -> FontRef<'_> {
        FontRef { data: &self.data, offset: self.offset, key: self.key }
    }
}

/// Where a face lives on disk. Fallbacks are only opened the first time a character
/// actually needs them.
#[derive(Clone)]
struct FontSource {
    path: std::path::PathBuf,
    index: u32,
}

/// One positioned glyph of a shaped run.
#[derive(Clone, Copy)]
pub struct Shaped {
    pub face: u16,
    pub glyph: u16,
    /// Cell column relative to the start of the run.
    pub col: u16,
    /// Offset in pixels from that cell's left edge / the baseline.
    pub x: f32,
    pub y: f32,
}

pub struct RasterGlyph {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// RGBA (emoji) when true, otherwise 8-bit coverage.
    pub color: bool,
    pub data: Vec<u8>,
}

pub struct Text {
    /// 0 regular, 1 bold, 2 italic, 3 bold italic, 4 Bosančica, then fallbacks (opened lazily).
    faces: Vec<Option<Face>>,
    /// Whether the configured Bosančica font is installed.
    pub has_bosancica: bool,
    bosancica_scale: f32,
    bosancica_embolden: f32,
    fallbacks: Vec<FontSource>,
    /// Every installed face, searched last (like the OS's own font cascade) for characters no
    /// configured font has. Only paths are kept; a face is opened only when it matches.
    system_fonts: Vec<FontSource>,
    /// Characters missing from the primary font → face that has them.
    fallback_for: HashMap<char, u16>,
    pub px: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    pub baseline: f32,
    ligatures: bool,
    shape_ctx: ShapeContext,
    scale_ctx: ScaleContext,
    shape_cache: HashMap<(String, u8), Arc<Vec<Shaped>>>,
}

fn source_of(db: &fontdb::Database, id: fontdb::ID) -> Option<FontSource> {
    let face = db.face(id)?;
    match &face.source {
        fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => Some(FontSource { path: path.clone(), index: face.index }),
        fontdb::Source::Binary(_) => None,
    }
}

/// Closest face to `weight`/`style` within an installed family. (fontdb's own query misses
/// families that are only listed under an alternate name, e.g. "JetBrainsMono Nerd Font Mono".)
fn find_in_family(db: &fontdb::Database, name: &str, weight: fontdb::Weight, style: fontdb::Style) -> Option<FontSource> {
    let best = db
        .faces()
        .filter(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)))
        .min_by_key(|f| (f.style != style) as i32 * 10_000 + (f.weight.0 as i32 - weight.0 as i32).abs())?;
    source_of(db, best.id)
}

/// The Bosančica font: an installed family name, or a path to a font file (`~` expanded).
fn bosancica_source(db: &fontdb::Database, font: &str) -> Option<FontSource> {
    let font = font.trim();
    let is_path = font.contains('/') || font.contains('\\') || [".ttf", ".otf", ".ttc"].iter().any(|e| font.to_ascii_lowercase().ends_with(e));
    if !is_path {
        return find_in_family(db, font, fontdb::Weight::NORMAL, fontdb::Style::Normal);
    }
    let path = match font.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest),
        None => std::path::PathBuf::from(font),
    };
    path.is_file().then_some(FontSource { path, index: 0 })
}

/// The first configured family that is installed, else the system monospace font.
fn find_face(db: &fontdb::Database, families: &[String], weight: fontdb::Weight, style: fontdb::Style) -> Option<FontSource> {
    families.iter().find_map(|name| find_in_family(db, name, weight, style)).or_else(|| {
        let id = db.query(&fontdb::Query { families: &[fontdb::Family::Monospace], weight, style, ..Default::default() })?;
        source_of(db, id)
    })
}

/// Free function so callers can borrow `faces` and the shaping contexts disjointly.
fn face_in(faces: &[Option<Face>], i: u16) -> &Face {
    faces[i as usize].as_ref().or(faces[0].as_ref()).expect("primary face is always loaded")
}

impl Text {
    /// Scans system fonts, resolves the faces we need, then drops the font database
    /// (it holds metadata for every installed font, which we don't need to keep around).
    pub fn load(cfg: &Config, scale: f32) -> Text {
        use fontdb::{Style, Weight};
        let mut db = fontdb::Database::new();
        db.load_system_fonts();

        let fam = &cfg.font.family;
        let regular_src = find_face(&db, fam, Weight::NORMAL, Style::Normal).expect("no monospace font found on this system");
        let regular = Face::open(&regular_src).expect("failed to open primary font");
        let open_or_regular = |src: Option<FontSource>| src.and_then(|s| Face::open(&s)).or_else(|| Face::open(&regular_src));
        let bold = open_or_regular(find_face(&db, fam, Weight::BOLD, Style::Normal));
        let italic = open_or_regular(find_face(&db, fam, Weight::NORMAL, Style::Italic));
        let bold_italic = open_or_regular(find_face(&db, fam, Weight::BOLD, Style::Italic));

        let mut names = cfg.font.fallback.clone();
        // Any installed Nerd Font provides the icon glyphs prompts and `eza --icons` use.
        if let Some(nerd) = db.faces().flat_map(|f| f.families.iter()).map(|(n, _)| n).find(|n| n.contains("Nerd Font Mono")) {
            names.push(nerd.clone());
        }
        let fallbacks: Vec<FontSource> = names
            .iter()
            .filter_map(|name| find_in_family(&db, name, Weight::NORMAL, Style::Normal))
            .collect();
        let bosancica = bosancica_source(&db, &cfg.bosancica.font).and_then(|s| Face::open(&s));
        let has_bosancica = bosancica.is_some();
        let system_fonts: Vec<FontSource> = db.faces().filter_map(|f| source_of(&db, f.id)).collect();
        drop(db);

        let px = (cfg.font.size * scale).round();
        let font = regular.font();
        let m = font.metrics(&[]).scale(px);
        let natural_h = m.ascent + m.descent + m.leading;
        let cell_h = (natural_h * cfg.font.line_height).round();
        let glyph_m = font.glyph_metrics(&[]).scale(px);
        let cell_w = glyph_m.advance_width(font.charmap().map('M')).round();
        let baseline = ((cell_h - natural_h) / 2.0 + m.ascent).round();

        // Display fonts like BoSanko2 draw small letters for their em size; scale that face so
        // its capitals match the main font's, otherwise the text looks shrunken in the cells.
        let bosancica_scale = bosancica.as_ref().map_or(1.0, |b| {
            let cap = |f: FontRef| {
                let m = f.metrics(&[]);
                let h = if m.cap_height > 0.0 { m.cap_height } else { m.ascent * 0.7 };
                h / m.units_per_em as f32
            };
            (cap(regular.font()) / cap(b.font()) * cfg.bosancica.size).clamp(0.5, 3.0)
        });
        let bosancica_embolden = cfg.bosancica.weight.max(0.0);
        let mut faces = vec![Some(regular), bold, italic, bold_italic, bosancica];
        faces.extend(fallbacks.iter().map(|_| None));

        Text {
            faces,
            has_bosancica,
            bosancica_scale,
            bosancica_embolden,
            fallbacks,
            system_fonts,
            fallback_for: HashMap::new(),
            px,
            cell_w,
            cell_h,
            baseline,
            ligatures: cfg.font.ligatures,
            shape_ctx: ShapeContext::new(),
            scale_ctx: ScaleContext::new(),
            shape_cache: HashMap::new(),
        }
    }

    fn face_px(&self, face: u16) -> f32 {
        if face == BOSANCICA as u16 { (self.px * self.bosancica_scale).round() } else { self.px }
    }

    fn face(&self, i: u16) -> &Face {
        face_in(&self.faces, i)
    }

    fn face_for(&mut self, c: char, style: u8) -> u16 {
        if c == ' ' || self.face(style as u16).font().charmap().map(c) != 0 {
            return style as u16;
        }
        if let Some(&f) = self.fallback_for.get(&c) {
            return f;
        }
        let mut found = style as u16;
        for (i, src) in self.fallbacks.iter().enumerate() {
            let slot = &mut self.faces[FIRST_FALLBACK + i];
            if slot.is_none() {
                *slot = Face::open(src);
                if slot.is_none() {
                    continue;
                }
                log::info!("loaded fallback font {}", src.path.display());
            }
            if slot.as_ref().is_some_and(|f| f.font().charmap().map(c) != 0) {
                found = (FIRST_FALLBACK + i) as u16;
                break;
            }
        }
        if found == style as u16 {
            // Last resort: any installed font with the glyph (e.g. ⏵ that Claude Code prints).
            // Runs once per missing character; the answer, found or not, is cached below.
            if let Some(face) = self.system_fonts.iter().find_map(|src| Face::open(src).filter(|f| f.font().charmap().map(c) != 0)) {
                log::info!("system fallback for {c:?}");
                self.faces.push(Some(face));
                found = (self.faces.len() - 1) as u16;
            }
        }
        self.fallback_for.insert(c, found);
        found
    }

    /// Shape a run. `cells` holds each character with its column relative to the run start.
    pub fn shape(&mut self, cells: &[(char, u16)], style: u8) -> Arc<Vec<Shaped>> {
        let s: String = cells.iter().map(|(c, _)| *c).collect();
        let key = (s, style);
        if let Some(hit) = self.shape_cache.get(&key) {
            return hit.clone();
        }
        let s = &key.0;

        // Byte offset → column, so shaper clusters (reported in bytes) map back to cells.
        let mut byte_col = vec![0u16; s.len() + 1];
        let mut faces = Vec::with_capacity(cells.len());
        let mut b = 0;
        for &(c, col) in cells {
            for i in 0..c.len_utf8() {
                byte_col[b + i] = col;
            }
            b += c.len_utf8();
            faces.push(self.face_for(c, style));
        }

        let features: &[(&str, u16)] = if self.ligatures { &[("calt", 1), ("liga", 1)] } else { &[("calt", 0), ("liga", 0)] };
        let mut out = Vec::with_capacity(cells.len());

        // Split into segments that use the same face (primary vs. fallback) and shape each.
        let mut start_char = 0;
        let mut start_byte = 0;
        while start_char < cells.len() {
            let face = faces[start_char];
            let mut end_char = start_char;
            let mut end_byte = start_byte;
            while end_char < cells.len() && faces[end_char] == face {
                end_byte += cells[end_char].0.len_utf8();
                end_char += 1;
            }
            let font = face_in(&self.faces, face).font();
            let px = self.face_px(face);
            let mut shaper = self.shape_ctx.builder(font).size(px).features(features.iter().copied()).build();
            shaper.add_str(&s[start_byte..end_byte]);
            let charmap = font.charmap();
            shaper.shape_with(|cluster| {
                let (a, b) = (start_byte + cluster.source.start as usize, start_byte + cluster.source.end as usize);
                let chars: Vec<(usize, char)> = s[a..b].char_indices().map(|(i, c)| (a + i, c)).collect();
                if cluster.glyphs.len() < chars.len() {
                    // A many-to-one ligature (fi, fl, ffi…): one glyph can't fill several grid
                    // cells, so it would leave a gap. Draw each character in its own cell instead.
                    // Programming ligatures (`->`, `!=`) keep one glyph per cell and pass through.
                    for (byte, c) in chars {
                        out.push(Shaped { face, glyph: charmap.map(c), col: byte_col[byte], x: 0.0, y: 0.0 });
                    }
                    return;
                }
                let col = byte_col[a];
                let mut pen = 0.0;
                for g in cluster.glyphs {
                    out.push(Shaped { face, glyph: g.id, col, x: pen + g.x, y: g.y });
                    pen += g.advance;
                }
            });
            start_char = end_char;
            start_byte = end_byte;
        }

        let out = Arc::new(out);
        if self.shape_cache.len() > 2048 {
            self.shape_cache.clear();
        }
        self.shape_cache.insert(key, out.clone());
        out
    }

    pub fn rasterize(&mut self, face: u16, glyph: u16) -> Option<RasterGlyph> {
        let font = face_in(&self.faces, face).font();
        let px = self.face_px(face);
        let mut scaler = self.scale_ctx.builder(font).size(px).hint(true).build();
        // Color sources first so emoji fonts (sbix/COLR) render in color; plain fonts fall
        // through to the outline and produce an alpha mask.
        let mut render = Render::new(&[Source::ColorOutline(0), Source::ColorBitmap(StrikeWith::BestFit), Source::Outline]);
        render.format(Format::Alpha);
        if face == BOSANCICA as u16 && self.bosancica_embolden > 0.0 {
            // Thicken thin calligraphic strokes so they stay legible at terminal sizes.
            render.embolden(self.bosancica_embolden * px / 28.0);
        }
        let img = render.render(&mut scaler, glyph)?;
        Some(RasterGlyph {
            left: img.placement.left,
            top: img.placement.top,
            width: img.placement.width,
            height: img.placement.height,
            color: img.content == Content::Color,
            data: img.data,
        })
    }
}

/// Line weights (none / light / heavy) for up, right, down, left of a box-drawing char.
pub fn box_lines(c: char) -> Option<[u8; 4]> {
    Some(match c {
        '─' => [0, 1, 0, 1],
        '━' => [0, 2, 0, 2],
        '│' => [1, 0, 1, 0],
        '┃' => [2, 0, 2, 0],
        '┌' | '╭' => [0, 1, 1, 0],
        '┏' => [0, 2, 2, 0],
        '┐' | '╮' => [0, 0, 1, 1],
        '┓' => [0, 0, 2, 2],
        '└' | '╰' => [1, 1, 0, 0],
        '┗' => [2, 2, 0, 0],
        '┘' | '╯' => [1, 0, 0, 1],
        '┛' => [2, 0, 0, 2],
        '├' => [1, 1, 1, 0],
        '┣' => [2, 2, 2, 0],
        '┤' => [1, 0, 1, 1],
        '┫' => [2, 0, 2, 2],
        '┬' => [0, 1, 1, 1],
        '┳' => [0, 2, 2, 2],
        '┴' => [1, 1, 0, 1],
        '┻' => [2, 2, 0, 2],
        '┼' => [1, 1, 1, 1],
        '╋' => [2, 2, 2, 2],
        '╴' => [0, 0, 0, 1],
        '╵' => [1, 0, 0, 0],
        '╶' => [0, 1, 0, 0],
        '╷' => [0, 0, 1, 0],
        _ => return None,
    })
}

/// Block elements as (x0, y0, x1, y1) fractions of the cell plus alpha.
pub fn block(c: char) -> Option<([f32; 4], f32)> {
    Some(match c {
        '█' => ([0.0, 0.0, 1.0, 1.0], 1.0),
        '▀' => ([0.0, 0.0, 1.0, 0.5], 1.0),
        '▄' => ([0.0, 0.5, 1.0, 1.0], 1.0),
        '▌' => ([0.0, 0.0, 0.5, 1.0], 1.0),
        '▐' => ([0.5, 0.0, 1.0, 1.0], 1.0),
        '░' => ([0.0, 0.0, 1.0, 1.0], 0.25),
        '▒' => ([0.0, 0.0, 1.0, 1.0], 0.5),
        '▓' => ([0.0, 0.0, 1.0, 1.0], 0.75),
        '▁'..='▇' => {
            let eighths = (c as u32 - '▁' as u32 + 1) as f32 / 8.0;
            ([0.0, 1.0 - eighths, 1.0, 1.0], 1.0)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyphs(t: &mut Text, s: &str) -> Vec<u16> {
        let cells: Vec<(char, u16)> = s.chars().enumerate().map(|(i, c)| (c, i as u16)).collect();
        t.shape(&cells, REGULAR).iter().map(|g| g.glyph).collect()
    }

    /// Needs a ligature font (e.g. JetBrains Mono) installed; skips otherwise.
    #[test]
    fn ligatures_substitute_glyphs() {
        let mut t = Text::load(&Config::default(), 2.0);
        let liga = glyphs(&mut t, "->");
        let plain = [glyphs(&mut t, "-")[0], glyphs(&mut t, ">")[0]];
        if liga == plain {
            eprintln!("primary font has no ligatures; skipping");
            return;
        }
        assert_eq!(liga.len(), 2, "ligature glyphs still occupy one cell per character");
    }

    /// "fi"/"fl" must never collapse into one glyph: in a grid that leaves an empty cell.
    #[test]
    fn no_many_to_one_ligatures() {
        let mut t = Text::load(&Config::default(), 2.0);
        for word in ["fi", "fl", "ffi", "Profile", "flag"] {
            let cells: Vec<(char, u16)> = word.chars().enumerate().map(|(i, c)| (c, i as u16)).collect();
            let cols: Vec<u16> = t.shape(&cells, REGULAR).iter().map(|g| g.col).collect();
            let expected: Vec<u16> = (0..word.chars().count() as u16).collect();
            assert_eq!(cols, expected, "{word}: every character needs its own cell");
        }
    }
}

