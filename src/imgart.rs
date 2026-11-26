//! Image -> terminal art for truecolor, Unicode terminals.
//!
//! Every renderer splits the picture into terminal cells of 8x8 pixels and, for each cell, picks the glyph plus a
//! foreground and background colour that best reproduce the cell's pixels. The glyph comes from a fixed set whose
//! shapes are defined here as 8x8 bitmaps:
//!
//! | mode    | glyphs                                                        | font needs                     |
//! |---------|---------------------------------------------------------------|--------------------------------|
//! | Octant  | blocks + lines + sextants + octants (332)                     | Unicode 16 octants             |
//! | Sextant | blocks + lines + sextants (102)                               | Symbols for Legacy Computing   |
//! | Blocks  | eighth blocks, quadrants, lines, junctions, diagonals (43)    | every font (default)           |
//! | Ascii   | one luminance-ramp character per cell, coloured               | every font                     |
//!
//! Finer glyph sets reproduce more detail, especially thin lines and small text, at the cost of needing a font that
//! draws them. A font without a glyph shows a box with a question mark in its place, so the default is the set every
//! font has.

use image::RgbImage;
use image::imageops::{FilterType, resize};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;

/// Never render wider than this, however wide the terminal is: huge ASCII pictures just look noisy.
pub const MAX_ART_COLS: usize = 110;

/// Never render taller than this many rows. A tall picture scrolls like any message, but a very tall one is a wall.
pub const MAX_ART_ROWS: usize = 28;

/// About how many cells a picture takes, whatever its shape: see [`budget`].
const ART_CELLS: f32 = 1000.0;

/// Pixels per cell side in the fitting grid.
const CELL: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ImgMode {
    /// Blocks, lines, sextants and octants.
    Octant,
    /// Blocks, lines and sextants (Symbols for Legacy Computing).
    Sextant,
    /// Block fills, quadrants, lines and diagonals (present in every font).
    Blocks,
    Ascii,
    Off,
}

impl ImgMode {
    pub const DEFAULT: Self = Self::Blocks;

    pub fn next(self) -> Self {
        match self {
            Self::Octant => Self::Sextant,
            Self::Sextant => Self::Blocks,
            Self::Blocks => Self::Ascii,
            Self::Ascii => Self::Off,
            Self::Off => Self::Octant,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Octant => "octant",
            Self::Sextant => "sextant 🬗",
            Self::Blocks => "blocks ▚",
            Self::Ascii => "ascii @#",
            Self::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "octant" | "octants" => Some(Self::Octant),
            "sextant" | "sextants" => Some(Self::Sextant),
            "blocks" | "block" => Some(Self::Blocks),
            "ascii" => Some(Self::Ascii),
            "off" | "none" => Some(Self::Off),
            _ => None,
        }
    }

    /// Pixels sampled per cell as (width, height).
    fn grid(self) -> (usize, usize) {
        match self {
            Self::Octant | Self::Sextant | Self::Blocks => (CELL, CELL),
            Self::Ascii | Self::Off => (1, 1),
        }
    }
}

/// Cells (columns, rows) for an `w`x`h` picture inside `max_cols` x `max_rows`, assuming a cell is twice as tall as
/// it is wide. Never exceeds either bound and never upscales past one column per source pixel.
pub fn fit(w: u32, h: u32, max_cols: usize, max_rows: usize) -> (usize, usize) {
    let (w, h) = (w.max(1) as f32, h.max(1) as f32);
    let max_cols = max_cols.max(1);
    let max_rows = max_rows.max(1);
    let mut cols = max_cols.min(MAX_ART_COLS).min(w as usize).max(1);
    let mut rows = (cols as f32 * h / w * 0.5).round().max(1.0) as usize;
    if rows > max_rows {
        rows = max_rows;
        cols = ((rows as f32 * 2.0 * w / h).round() as usize).clamp(1, max_cols);
    }
    (cols, rows)
}

/// How large to draw `img` in a pane `width` columns wide: (columns, rows) at most.
///
/// Pictures are meant to be made out, not read, so every picture takes up about the same number of cells whatever its
/// shape: a square one is about 45 columns by 22 rows, a tall narrow one gets more rows and fewer columns, a wide one
/// more columns and fewer rows, up to [`MAX_ART_COLS`] x [`MAX_ART_ROWS`] and the width of the pane. (A flat box
/// would make a wide strip tiny in one direction and a tall screenshot a thin sliver.) A cell is twice as tall as it
/// is wide, so `cols * rows` cells of a picture `w` wide and `h` high are `cols` by `2 * rows` in shape.
pub fn budget(img: &RgbImage, width: usize) -> (usize, usize) {
    let aspect = img.height().max(1) as f32 / img.width().max(1) as f32;
    let cols = (2.0 * ART_CELLS / aspect).sqrt().round() as usize;
    (cols.clamp(1, MAX_ART_COLS).min(width.max(1)), MAX_ART_ROWS)
}

/// The cells (columns, rows) the art of `img` really takes in a pane `width` columns wide: [`render`] fits the picture
/// inside the [`budget`], and what it draws is exactly this big. A picture drawn with the terminal's graphics takes the
/// same rectangle, so that nothing else in the transcript moves when one replaces the other.
pub fn art_cells(img: &RgbImage, width: usize) -> (usize, usize) {
    let (cols, rows) = budget(img, width);
    fit(img.width(), img.height(), cols, rows)
}

/// Render `img` as styled lines, each exactly the same number of columns, at most `max_cols` x `max_rows`.
pub fn render(
    img: &RgbImage,
    mode: ImgMode,
    max_cols: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    if mode == ImgMode::Off {
        return Vec::new();
    }
    let (cols, rows) = fit(img.width(), img.height(), max_cols, max_rows);
    let (gw, gh) = mode.grid();
    let mut px = resize(
        img,
        (cols * gw) as u32,
        (rows * gh) as u32,
        FilterType::Lanczos3,
    );
    auto_levels(&mut px);
    match mode {
        ImgMode::Ascii => render_ascii(&px, cols, rows),
        _ => render_fit(&px, glyphs(mode), cols, rows),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn luma(p: [u8; 3]) -> f32 {
    0.299 * f32::from(p[0]) + 0.587 * f32::from(p[1]) + 0.114 * f32::from(p[2])
}

/// Stretch contrast when a picture is flat or washed out (1st..99th luminance percentile -> full range).
/// Pictures that already use the range are left alone.
fn auto_levels(img: &mut RgbImage) {
    let mut hist = [0usize; 256];
    for p in img.pixels() {
        hist[luma(p.0) as usize] += 1;
    }
    let total: usize = hist.iter().sum();
    let (mut lo, mut hi, mut acc) = (0usize, 255usize, 0usize);
    for (i, &n) in hist.iter().enumerate() {
        acc += n;
        if acc * 100 >= total {
            lo = i;
            break;
        }
    }
    acc = 0;
    for (i, &n) in hist.iter().enumerate().rev() {
        acc += n;
        if acc * 100 >= total {
            hi = i;
            break;
        }
    }
    if hi <= lo + 8 || (hi - lo) > 215 {
        return;
    }
    let scale = 255.0 / (hi - lo) as f32;
    for p in img.pixels_mut() {
        for c in &mut p.0 {
            *c = ((f32::from(*c) - lo as f32) * scale).clamp(0.0, 255.0) as u8;
        }
    }
}

/// Appends `text` with `style`, merging with the previous span when the style is identical.
fn push(spans: &mut Vec<Span<'static>>, text: char, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push(text);
        return;
    }
    spans.push(Span::styled(text.to_string(), style));
}

fn rgb(c: [f32; 3]) -> Color {
    Color::Rgb(
        c[0].round().clamp(0.0, 255.0) as u8,
        c[1].round().clamp(0.0, 255.0) as u8,
        c[2].round().clamp(0.0, 255.0) as u8,
    )
}

// ---------------------------------------------------------------------------
// Glyph sets
// ---------------------------------------------------------------------------

/// A character and the pixels of its 8x8 cell that show the foreground colour. Bit 63 is the top-left pixel, bit 0
/// the bottom-right, so a row is one byte.
#[derive(Clone, Copy, Debug)]
struct Glyph {
    ch: char,
    bitmap: u64,
}

/// The bitmap of the pixels `(x, y)` for which `inside` holds.
fn bitmap(inside: impl Fn(usize, usize) -> bool) -> u64 {
    let mut bits = 0u64;
    for y in 0..CELL {
        for x in 0..CELL {
            if inside(x, y) {
                bits |= 1u64 << (63 - (y * CELL + x));
            }
        }
    }
    bits
}

/// Simplest glyph first, so that equally good fits prefer calmer ones (the sort is stable).
fn simplest_first(mut glyphs: Vec<Glyph>) -> Vec<Glyph> {
    glyphs.sort_by_key(|g| {
        let ink = g.bitmap.count_ones();
        ink.min(64 - ink)
    });
    glyphs
}

/// `base` followed by the glyphs of `extra` that add something: neither their character nor their pattern is already
/// in it (the middle-band sextant, for instance, draws the same pixels as the line `─`).
fn union(mut base: Vec<Glyph>, extra: Vec<Glyph>) -> Vec<Glyph> {
    for g in extra {
        if !base.iter().any(|b| b.ch == g.ch || b.bitmap == g.bitmap) {
            base.push(g);
        }
    }
    simplest_first(base)
}

/// The quadrant blocks, indexed by a mask in which bit `row * 2 + column` is a filled quarter.
const QUADRANTS: [char; 16] = [
    ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█',
];

/// Eighth blocks, quadrants, thin lines with their junctions, and diagonals: everything here is in ordinary fonts.
fn block_glyphs() -> Vec<Glyph> {
    let mut v = vec![Glyph { ch: ' ', bitmap: 0 }];
    let mut add = |ch: char, bitmap: u64| v.push(Glyph { ch, bitmap });
    // A fill from the left edge of 1..7 eighths, and from the bottom edge. The same patterns filled from the other
    // side draw the same picture with the two colours swapped, so they are not repeated.
    for (k, ch) in ['▏', '▎', '▍', '▌', '▋', '▊', '▉'].into_iter().enumerate() {
        add(ch, bitmap(|x, _| x <= k));
    }
    for (k, ch) in ['▁', '▂', '▃', '▄', '▅', '▆', '▇'].into_iter().enumerate() {
        add(ch, bitmap(|_, y| y >= 7 - k));
    }
    // Quadrant patterns not already covered by the halves above.
    for mask in [1usize, 2, 4, 6, 7, 8, 9, 11, 13, 14] {
        add(
            QUADRANTS[mask],
            bitmap(|x, y| mask >> ((y / 4) * 2 + x / 4) & 1 == 1),
        );
    }
    // Lines two pixels thick through the middle of the cell.
    let horizontal = |y: usize| (3..=4).contains(&y);
    let vertical = |x: usize| (3..=4).contains(&x);
    add('─', bitmap(|_, y| horizontal(y)));
    add('│', bitmap(|x, _| vertical(x)));
    add('┼', bitmap(|x, y| horizontal(y) || vertical(x)));
    add(
        '┌',
        bitmap(|x, y| (horizontal(y) && x >= 3) || (vertical(x) && y >= 3)),
    );
    add(
        '┐',
        bitmap(|x, y| (horizontal(y) && x <= 4) || (vertical(x) && y >= 3)),
    );
    add(
        '└',
        bitmap(|x, y| (horizontal(y) && x >= 3) || (vertical(x) && y <= 4)),
    );
    add(
        '┘',
        bitmap(|x, y| (horizontal(y) && x <= 4) || (vertical(x) && y <= 4)),
    );
    add('├', bitmap(|x, y| vertical(x) || (horizontal(y) && x >= 3)));
    add('┤', bitmap(|x, y| vertical(x) || (horizontal(y) && x <= 4)));
    add('┬', bitmap(|x, y| horizontal(y) || (vertical(x) && y >= 3)));
    add('┴', bitmap(|x, y| horizontal(y) || (vertical(x) && y <= 4)));
    add('╴', bitmap(|x, y| horizontal(y) && x <= 4));
    add('╶', bitmap(|x, y| horizontal(y) && x >= 3));
    add('╵', bitmap(|x, y| vertical(x) && y <= 4));
    add('╷', bitmap(|x, y| vertical(x) && y >= 3));
    // Diagonal bands.
    let rising = |x: usize, y: usize| (x + y).abs_diff(7) <= 1;
    let falling = |x: usize, y: usize| x.abs_diff(y) <= 1;
    add('╱', bitmap(rising));
    add('╲', bitmap(falling));
    add('╳', bitmap(|x, y| rising(x, y) || falling(x, y)));
    simplest_first(v)
}

/// The character for a sextant mask (cells 1 2 / 3 4 / 5 6, cell k = bit k-1). Sextants occupy U+1FB00.. in mask
/// order, except that the patterns which are the left half, the right half and the full block use the older
/// characters.
fn sextant_char(mask: u8) -> char {
    match mask {
        0 => ' ',
        21 => '▌',
        42 => '▐',
        63 => '█',
        n => {
            let index = u32::from(n) - 1 - u32::from(n > 21) - u32::from(n > 42);
            char::from_u32(0x1FB00 + index).unwrap_or(' ')
        }
    }
}

/// Sextants split the cell 2 across and 3 down; the rows are 3, 2 and 3 pixels tall.
fn sextant_glyphs() -> Vec<Glyph> {
    let band = |y: usize| match y {
        0..=2 => 0,
        3..=4 => 1,
        _ => 2,
    };
    (0u8..64)
        .map(|mask| Glyph {
            ch: sextant_char(mask),
            bitmap: bitmap(|x, y| mask >> (band(y) * 2 + x / 4) & 1 == 1),
        })
        .collect()
}

/// The 2 x 4 patterns (cells numbered 1 2 / 3 4 / 5 6 / 7 8, as in the characters' names) that already have a
/// character of their own elsewhere in Unicode, so the octant block has none.
const NOT_OCTANTS: [&str; 26] = [
    "", "1", "2", "12", "13", "24", "1234", "35", "46", "123456", "7", "57", "1357", "2457",
    "123457", "8", "68", "1368", "2468", "123468", "78", "5678", "135678", "245678", "345678",
    "12345678",
];

/// The octants: U+1CD00.. in ascending order of their cell mask (cell k = bit k-1), skipping [`NOT_OCTANTS`].
/// Returns each character with its mask.
fn octants() -> Vec<(char, u8)> {
    let excluded: Vec<u8> = NOT_OCTANTS
        .iter()
        .map(|cells| {
            cells
                .chars()
                .filter_map(|d| d.to_digit(10))
                .fold(0u8, |mask, d| mask | 1 << (d - 1))
        })
        .collect();
    (0..=255u8)
        .filter(|mask| !excluded.contains(mask))
        .zip(0x1CD00u32..)
        .filter_map(|(mask, code)| char::from_u32(code).map(|ch| (ch, mask)))
        .collect()
}

/// Octants split the cell 2 across and 4 down; each part is 4 pixels wide and 2 tall.
fn octant_glyphs() -> Vec<Glyph> {
    octants()
        .into_iter()
        .map(|(ch, mask)| Glyph {
            ch,
            bitmap: bitmap(|x, y| mask >> ((y / 2) * 2 + x / 4) & 1 == 1),
        })
        .collect()
}

fn glyphs(mode: ImgMode) -> &'static [Glyph] {
    static SETS: [OnceLock<Vec<Glyph>>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    match mode {
        ImgMode::Blocks => SETS[0].get_or_init(block_glyphs),
        ImgMode::Sextant => SETS[1].get_or_init(|| union(block_glyphs(), sextant_glyphs())),
        ImgMode::Octant => {
            SETS[2].get_or_init(|| union(union(block_glyphs(), sextant_glyphs()), octant_glyphs()))
        }
        ImgMode::Ascii | ImgMode::Off => &[],
    }
}

// ---------------------------------------------------------------------------
// The fit
// ---------------------------------------------------------------------------

/// Perceptual channel weights for the fit error (green matters most, blue least).
const W: [f32; 3] = [1.414, 2.0, 1.732];

/// For each cell, the glyph (with free foreground and background colours) that minimises the weighted squared error.
/// `px` must be `cols * 8` x `rows * 8`.
fn render_fit(px: &RgbImage, set: &[Glyph], cols: usize, rows: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for col in 0..cols {
            // The cell's pixels.
            let mut p = [[0f32; 3]; CELL * CELL];
            let mut total = [0f32; 3];
            for y in 0..CELL {
                for x in 0..CELL {
                    let v = px
                        .get_pixel((col * CELL + x) as u32, (row * CELL + y) as u32)
                        .0;
                    for c in 0..3 {
                        p[y * CELL + x][c] = f32::from(v[c]);
                        total[c] += p[y * CELL + x][c];
                    }
                }
            }
            let sq_total: f32 = p
                .iter()
                .map(|q| (0..3).map(|c| (q[c] * W[c]).powi(2)).sum::<f32>())
                .sum();
            let weighted_sq = |s: [f32; 3], k: u32| -> f32 {
                if k == 0 {
                    0.0
                } else {
                    (0..3).map(|c| (s[c] * W[c]).powi(2)).sum::<f32>() / k as f32
                }
            };

            // Best two-colour fit: error = sum |p|^2 - |S_fg|^2/n_fg - |S_bg|^2/n_bg.
            let mut best = (f32::MAX, 0usize, [0f32; 3]);
            for (gi, glyph) in set.iter().enumerate() {
                let mut s_fg = [0f32; 3];
                let mut bits = glyph.bitmap;
                while bits != 0 {
                    let i = bits.leading_zeros() as usize;
                    bits &= !(1u64 << (63 - i));
                    for c in 0..3 {
                        s_fg[c] += p[i][c];
                    }
                }
                let n_fg = glyph.bitmap.count_ones();
                let s_bg = [total[0] - s_fg[0], total[1] - s_fg[1], total[2] - s_fg[2]];
                let err = sq_total - weighted_sq(s_fg, n_fg) - weighted_sq(s_bg, 64 - n_fg);
                if err < best.0 - 1e-3 {
                    best = (err, gi, s_fg);
                }
            }

            let (_, gi, s_fg) = best;
            let glyph = set[gi];
            let n_fg = glyph.bitmap.count_ones();
            let s_bg = [total[0] - s_fg[0], total[1] - s_fg[1], total[2] - s_fg[2]];
            let mean = |s: [f32; 3], k: u32| -> [f32; 3] {
                if k == 0 {
                    [0.0; 3]
                } else {
                    [s[0] / k as f32, s[1] / k as f32, s[2] / k as f32]
                }
            };
            let (fg, bg) = (mean(s_fg, n_fg), mean(s_bg, 64 - n_fg));
            let style = if glyph.bitmap == 0 {
                // Single colour: make fg == bg so neighbouring empty cells merge into one span.
                Style::new().fg(rgb(bg)).bg(rgb(bg))
            } else {
                Style::new().fg(rgb(fg)).bg(rgb(bg))
            };
            push(&mut spans, glyph.ch, style);
        }
        lines.push(Line::from(spans));
    }
    lines
}

// ---------------------------------------------------------------------------
// Classic ASCII ramp, true colour
// ---------------------------------------------------------------------------

/// Dark -> light. Ordered by how much ink each glyph puts on the cell.
const RAMP: &[u8] = b" .'`^\",:;Il!i><~+_-?][}{1)(|\\/tfjrxnuvczXYUJCLQ0OZmwqpdbkhao*#MW&8%B@$";

fn render_ascii(px: &RgbImage, cols: usize, rows: usize) -> Vec<Line<'static>> {
    (0..rows)
        .map(|y| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for x in 0..cols {
                let p = px.get_pixel(x as u32, y as u32).0;
                let l = (luma(p) / 255.0).powf(0.85);
                let ch = RAMP[((l * (RAMP.len() - 1) as f32).round() as usize).min(RAMP.len() - 1)]
                    as char;
                // Brighten dim colours so thin glyphs stay readable on a dark background.
                let peak = p.iter().copied().max().unwrap_or(1).max(1);
                let boost = (150.0 / f32::from(peak)).clamp(1.0, 3.0);
                let c = [
                    f32::from(p[0]) * boost,
                    f32::from(p[1]) * boost,
                    f32::from(p[2]) * boost,
                ];
                push(&mut spans, ch, Style::new().fg(rgb(c)));
            }
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    const FIT_MODES: [ImgMode; 3] = [ImgMode::Octant, ImgMode::Sextant, ImgMode::Blocks];

    fn solid(w: u32, h: u32, c: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(w, h, Rgb(c))
    }

    fn width_of(line: &Line<'_>) -> usize {
        line.spans.iter().map(|s| s.content.chars().count()).sum()
    }

    fn text_of(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The bitmap of `ch` in `mode`'s glyph set.
    fn bitmap_of(mode: ImgMode, ch: char) -> u64 {
        glyphs(mode)
            .iter()
            .find(|g| g.ch == ch)
            .unwrap_or_else(|| panic!("{ch} is not in the {mode:?} set"))
            .bitmap
    }

    #[test]
    fn fit_never_exceeds_the_bounds() {
        for (w, h) in [
            (800, 600),
            (600, 800),
            (1000, 100),
            (100, 1000),
            (64, 64),
            (4000, 3000),
            (48, 4000),
        ] {
            for (mc, mr) in [(10, 5), (40, 12), (80, 24), (200, 60), (1, 1)] {
                let (c, r) = fit(w, h, mc, mr);
                assert!(c >= 1 && r >= 1);
                assert!(
                    c <= mc.max(1) && r <= mr.max(1),
                    "{w}x{h} in {mc}x{mr} -> {c}x{r}"
                );
                assert!(c <= MAX_ART_COLS);
            }
        }
    }

    /// The cells `budget` leaves a picture of this size on a wide pane.
    fn cells(w: u32, h: u32) -> (usize, usize) {
        let img = solid(w, h, [90, 90, 90]);
        let (cols, rows) = budget(&img, 200);
        fit(w, h, cols, rows)
    }

    #[test]
    fn art_cells_is_exactly_the_rectangle_the_art_fills() {
        for (w, h, pane) in [
            (800, 600, 80),
            (600, 800, 40),
            (3000, 100, 200),
            (30, 20, 80),
            (200, 6000, 60),
        ] {
            let img = solid(w, h, [10, 120, 200]);
            let (cols, rows) = art_cells(&img, pane);
            let (max_cols, max_rows) = budget(&img, pane);
            let art = render(&img, ImgMode::Blocks, max_cols, max_rows);
            assert_eq!(art.len(), rows, "{w}x{h} in {pane}");
            assert!(art.iter().all(|l| width_of(l) == cols), "{w}x{h} in {pane}");
        }
    }

    #[test]
    fn pictures_of_every_shape_take_about_the_same_number_of_cells() {
        for (w, h) in [
            (1000, 1000),
            (628, 800),
            (1066, 1356),
            (2000, 439),
            (800, 800),
            (600, 1000),
            (1000, 500),
        ] {
            let (c, r) = cells(w, h);
            assert!(
                (600..=1300).contains(&(c * r)),
                "{w}x{h} -> {c}x{r} = {} cells",
                c * r
            );
        }
        // Extremes are held to the limits rather than to the area.
        let (c, r) = cells(3000, 100);
        assert_eq!(c, MAX_ART_COLS);
        assert!((1..=4).contains(&r), "{r}");
        let (c, r) = cells(200, 6000);
        assert_eq!(r, MAX_ART_ROWS);
        assert!((1..=4).contains(&c), "{c}");
    }

    #[test]
    fn the_shape_of_the_picture_decides_how_the_cells_are_spent() {
        let (wide_c, wide_r) = cells(2000, 439);
        let (square_c, square_r) = cells(1000, 1000);
        let (tall_c, tall_r) = cells(628, 800);
        assert!(
            wide_c > square_c && wide_r < square_r,
            "wide {wide_c}x{wide_r}"
        );
        assert!(
            tall_c <= square_c && tall_r >= square_r,
            "tall {tall_c}x{tall_r}"
        );
        // The tweet screenshot from a chat: not tiny, not a wall.
        assert!(
            (35..=60).contains(&tall_c) && (18..=28).contains(&tall_r),
            "{tall_c}x{tall_r}"
        );
    }

    #[test]
    fn a_narrow_pane_shrinks_the_picture_and_never_stretches_it() {
        let img = solid(1000, 1000, [10, 10, 10]);
        let (cols, rows) = budget(&img, 30);
        let (c, r) = fit(1000, 1000, cols, rows);
        assert_eq!((c, r), (30, 15));
        let tiny = solid(20, 10, [10, 10, 10]);
        let (cols, rows) = budget(&tiny, 200);
        assert_eq!(
            fit(20, 10, cols, rows).0,
            20,
            "one column per pixel at most"
        );
    }
    #[test]
    fn square_pictures_get_half_as_many_rows_as_columns() {
        assert_eq!(fit(500, 500, 60, 60), (60, 30));
    }

    #[test]
    fn every_mode_produces_rectangular_output_within_bounds() {
        let mut img = RgbImage::new(200, 120);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = Rgb([(x % 256) as u8, (y * 2 % 256) as u8, ((x + y) % 256) as u8]);
        }
        for mode in [
            ImgMode::Octant,
            ImgMode::Sextant,
            ImgMode::Blocks,
            ImgMode::Ascii,
        ] {
            let lines = render(&img, mode, 50, 14);
            assert!(!lines.is_empty() && lines.len() <= 14, "{mode:?}");
            let w = width_of(&lines[0]);
            assert!((1..=50).contains(&w), "{mode:?} width {w}");
            assert!(
                lines.iter().all(|l| width_of(l) == w),
                "{mode:?} has ragged rows"
            );
        }
        assert!(render(&img, ImgMode::Off, 50, 14).is_empty());
    }

    #[test]
    fn sextant_characters_follow_unicode() {
        assert_eq!(sextant_char(1), '\u{1FB00}');
        assert_eq!(sextant_char(20), '\u{1FB13}');
        assert_eq!(sextant_char(21), '▌');
        assert_eq!(sextant_char(42), '▐');
        assert_eq!(sextant_char(62), '\u{1FB3B}');
        assert_eq!(sextant_char(63), '█');
    }

    #[test]
    fn octants_follow_unicode_numbering() {
        let all = octants();
        assert_eq!(all.len(), 230, "U+1CD00..=U+1CDE5");
        // OCTANT-3 is the first character; the last is every cell but the first (OCTANT-2345678).
        assert_eq!(all.first(), Some(&('\u{1CD00}', 0b0000_0100)));
        assert_eq!(all.last(), Some(&('\u{1CDE5}', 0b1111_1110)));
        // Names from the standard: OCTANT-23 follows OCTANT-3, and OCTANT-14 follows OCTANT-4.
        assert_eq!(all[1], ('\u{1CD01}', 0b0000_0110));
        assert_eq!(all[4], ('\u{1CD04}', 0b0000_1001));
        let masks: Vec<u8> = all.iter().map(|(_, m)| *m).collect();
        assert!(masks.windows(2).all(|w| w[0] < w[1]), "ascending");
        assert!(
            all.iter()
                .all(|(c, _)| ('\u{1CD00}'..='\u{1CDE5}').contains(c))
        );
    }

    #[test]
    fn glyph_bitmaps_have_the_shapes_their_characters_draw() {
        // One byte per row, the high bit is the leftmost pixel.
        assert_eq!(bitmap_of(ImgMode::Blocks, '▌'), 0xF0F0_F0F0_F0F0_F0F0);
        assert_eq!(bitmap_of(ImgMode::Blocks, '▄'), 0x0000_0000_FFFF_FFFF);
        assert_eq!(bitmap_of(ImgMode::Blocks, '▏'), 0x8080_8080_8080_8080);
        assert_eq!(bitmap_of(ImgMode::Blocks, '▘'), 0xF0F0_F0F0_0000_0000);
        assert_eq!(bitmap_of(ImgMode::Blocks, '─'), 0x0000_00FF_FF00_0000);
        assert_eq!(bitmap_of(ImgMode::Blocks, '│'), 0x1818_1818_1818_1818);
        // A sextant: cell 3 (middle left) is four pixels wide and two tall.
        assert_eq!(
            bitmap_of(ImgMode::Sextant, '\u{1FB03}'),
            0x0000_00F0_F000_0000
        );
        // An octant: cell 3 of 2 x 4 is the left half of the second row pair.
        assert_eq!(
            bitmap_of(ImgMode::Octant, '\u{1CD00}'),
            0x0000_F0F0_0000_0000
        );
    }

    #[test]
    fn glyph_sets_have_the_expected_sizes_and_nest() {
        let (blocks, sextants, octants) = (
            glyphs(ImgMode::Blocks),
            glyphs(ImgMode::Sextant),
            glyphs(ImgMode::Octant),
        );
        assert_eq!(
            (blocks.len(), sextants.len(), octants.len()),
            (43, 102, 332)
        );
        for g in blocks {
            assert!(sextants.iter().any(|s| s.ch == g.ch), "{}", g.ch);
        }
        for g in sextants {
            assert!(octants.iter().any(|o| o.ch == g.ch), "{}", g.ch);
        }
        // Simplest first: the empty pattern leads, and no character appears twice.
        assert_eq!(octants[0].bitmap, 0);
        let mut chars: Vec<char> = octants.iter().map(|g| g.ch).collect();
        chars.sort_unstable();
        chars.dedup();
        assert_eq!(chars.len(), octants.len());
    }

    #[test]
    fn no_two_characters_in_a_set_draw_the_same_pattern() {
        // Among other things this confirms the octant rule: octants exist only for patterns without another character.
        let mut seen = std::collections::HashMap::new();
        for g in glyphs(ImgMode::Octant) {
            if let Some(other) = seen.insert(g.bitmap, g.ch) {
                panic!("{} and {other} draw the same pattern", g.ch);
            }
        }
    }

    #[test]
    fn a_solid_picture_collapses_to_blank_cells_of_one_colour() {
        for mode in FIT_MODES {
            let lines = render(&solid(100, 100, [200, 30, 30]), mode, 20, 10);
            for l in &lines {
                assert_eq!(
                    l.spans.len(),
                    1,
                    "{mode:?}: solid colour should merge into one span"
                );
                assert_eq!(l.spans[0].style.bg, Some(Color::Rgb(200, 30, 30)));
            }
        }
    }

    #[test]
    fn a_vertical_split_is_reproduced_with_both_colours() {
        let mut img = solid(64, 64, [255, 0, 0]);
        for y in 0..64 {
            for x in 32..64 {
                img.put_pixel(x, y, Rgb([0, 0, 255]));
            }
        }
        for mode in FIT_MODES {
            // 4 columns: the boundary falls on a cell edge, so cells are solid red or solid blue.
            let lines = render(&img, mode, 4, 2);
            let colours: Vec<_> = lines[0].spans.iter().filter_map(|s| s.style.bg).collect();
            assert!(
                colours.contains(&Color::Rgb(255, 0, 0))
                    && colours.contains(&Color::Rgb(0, 0, 255)),
                "{mode:?} {colours:?}"
            );

            // 3 columns: the middle cell straddles the boundary and must use a half block with red + blue.
            let lines = render(&img, mode, 3, 2);
            let mid = &lines[0]
                .spans
                .iter()
                .find(|s| s.content.contains(['▌', '▐']))
                .unwrap_or_else(|| panic!("{mode:?}: a half-block cell"));
            let pair = (mid.style.fg, mid.style.bg);
            assert!(
                pair == (Some(Color::Rgb(255, 0, 0)), Some(Color::Rgb(0, 0, 255)))
                    || pair == (Some(Color::Rgb(0, 0, 255)), Some(Color::Rgb(255, 0, 0))),
                "{mode:?} {pair:?}"
            );
        }
    }

    #[test]
    fn thin_lines_survive() {
        // Left half red, right half blue, with a thin white vertical line through the middle of the left half.
        let mut img = solid(160, 160, [255, 0, 0]);
        for y in 0..160 {
            for x in 80..160 {
                img.put_pixel(x, y, Rgb([0, 0, 255]));
            }
            img.put_pixel(40, y, Rgb([255, 255, 255]));
        }
        for mode in FIT_MODES {
            let lines = render(&img, mode, 20, 10);
            let colours: Vec<Color> = lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .filter_map(|s| s.style.bg)
                .collect();
            assert!(
                colours
                    .iter()
                    .any(|c| matches!(c, Color::Rgb(r, _, b) if *r > 200 && *b < 60)),
                "{mode:?} lost red"
            );
            assert!(
                colours
                    .iter()
                    .any(|c| matches!(c, Color::Rgb(r, _, b) if *r < 60 && *b > 200)),
                "{mode:?} lost blue"
            );
            // The one-pixel line should survive as a bright foreground somewhere on the left.
            let bright = lines.iter().flat_map(|l| l.spans.iter()).any(|s| matches!(s.style.fg, Some(Color::Rgb(r, g, b)) if r > 180 && g > 120 && b > 120));
            assert!(bright, "{mode:?} dropped the thin line");
        }
    }

    #[test]
    fn finer_glyph_sets_fit_a_detailed_picture_at_least_as_well() {
        // A picture of thin diagonal and horizontal strokes: each richer set can only reduce the error, because it
        // contains every glyph of the one before it.
        let mut img = solid(240, 96, [20, 20, 30]);
        for i in 0..240u32 {
            for t in 0..2 {
                let y = (i / 3 + t) % 96;
                img.put_pixel(i, y, Rgb([240, 240, 120]));
            }
            img.put_pixel(i, 48, Rgb([120, 200, 255]));
        }
        let error = |mode: ImgMode| -> f64 {
            let lines = render(&img, mode, 30, 6);
            let (cols, rows) = (width_of(&lines[0]), lines.len());
            let reference = resize(
                &img,
                (cols * CELL) as u32,
                (rows * CELL) as u32,
                FilterType::Lanczos3,
            );
            let mut err = 0f64;
            for (row, line) in lines.iter().enumerate() {
                let mut col = 0;
                for span in &line.spans {
                    for ch in span.content.chars() {
                        let g = glyphs(mode).iter().find(|g| g.ch == ch).expect("glyph");
                        let (Some(Color::Rgb(fr, fg, fb)), Some(Color::Rgb(br, bg, bb))) =
                            (span.style.fg, span.style.bg)
                        else {
                            continue;
                        };
                        for y in 0..CELL {
                            for x in 0..CELL {
                                let on = g.bitmap >> (63 - (y * CELL + x)) & 1 == 1;
                                let shown = if on { [fr, fg, fb] } else { [br, bg, bb] };
                                let want = reference
                                    .get_pixel((col * CELL + x) as u32, (row * CELL + y) as u32)
                                    .0;
                                for c in 0..3 {
                                    err += (f64::from(shown[c]) - f64::from(want[c])).powi(2);
                                }
                            }
                        }
                        col += 1;
                    }
                }
            }
            err
        };
        let (blocks, sextant, octant) = (
            error(ImgMode::Blocks),
            error(ImgMode::Sextant),
            error(ImgMode::Octant),
        );
        assert!(
            sextant <= blocks * 1.02 && octant <= sextant * 1.02,
            "blocks {blocks:.0}, sextant {sextant:.0}, octant {octant:.0}"
        );
    }

    /// Rough timing, run with: cargo test --release -- --ignored --nocapture
    #[test]
    #[ignore]
    fn timing_of_each_renderer_on_a_large_picture() {
        let mut img = RgbImage::new(800, 600);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = Rgb([(x % 256) as u8, (y % 256) as u8, ((x * y) % 256) as u8]);
        }
        for mode in [
            ImgMode::Octant,
            ImgMode::Sextant,
            ImgMode::Blocks,
            ImgMode::Ascii,
        ] {
            let t = std::time::Instant::now();
            let lines = render(&img, mode, 100, 40);
            println!(
                "{mode:?}: {:?} for {}x{} cells",
                t.elapsed(),
                width_of(&lines[0]),
                lines.len()
            );
        }
    }

    #[test]
    fn ascii_maps_dark_to_sparse_and_light_to_dense() {
        let dark = text_of(&render(&solid(80, 80, [0, 0, 0]), ImgMode::Ascii, 10, 5));
        let light = text_of(&render(
            &solid(80, 80, [255, 255, 255]),
            ImgMode::Ascii,
            10,
            5,
        ));
        assert!(dark.chars().all(|c| c == ' ' || c == '\n'), "{dark:?}");
        assert!(light.contains('$') || light.contains('@'), "{light:?}");
    }

    #[test]
    fn auto_levels_stretches_flat_pictures_only() {
        let mut flat = RgbImage::new(10, 10);
        for (x, _, p) in flat.enumerate_pixels_mut() {
            let v = 100 + (x as u8) * 3; // 100..127
            *p = Rgb([v, v, v]);
        }
        auto_levels(&mut flat);
        let max = flat.pixels().map(|p| p.0[0]).max().unwrap();
        assert!(max > 240, "flat picture should be stretched, max = {max}");

        let mut full = RgbImage::new(10, 10);
        for (x, _, p) in full.enumerate_pixels_mut() {
            *p = Rgb([(x * 28) as u8; 3]);
        }
        let before = full.clone();
        auto_levels(&mut full);
        assert_eq!(
            full, before,
            "a picture that already spans the range is untouched"
        );
    }

    #[test]
    fn modes_parse_by_name_and_the_cycle_visits_everything_once() {
        for (name, mode) in [
            ("octant", ImgMode::Octant),
            ("Sextant", ImgMode::Sextant),
            ("blocks", ImgMode::Blocks),
            ("ascii", ImgMode::Ascii),
            ("off", ImgMode::Off),
        ] {
            assert_eq!(ImgMode::parse(name), Some(mode));
        }
        assert_eq!(ImgMode::parse("not-a-mode"), None);
        assert_eq!(
            ImgMode::DEFAULT,
            ImgMode::Blocks,
            "the set every font draws"
        );

        let mut m = ImgMode::DEFAULT;
        let mut seen = vec![m];
        for _ in 0..4 {
            m = m.next();
            seen.push(m);
        }
        assert_eq!(m.next(), ImgMode::DEFAULT);
        seen.sort_by_key(|m| format!("{m:?}"));
        seen.dedup();
        assert_eq!(seen.len(), 5);
    }

    /// Draws the art of the picture named by `VTTEAMS_PREVIEW_IMAGE` into a PNG (`VTTEAMS_PREVIEW_OUT`), the way a terminal
    /// with a cell twice as tall as wide would show it, so that a change to the fit can be looked at.
    /// Size: `VTTEAMS_PREVIEW_COLS` x `VTTEAMS_PREVIEW_ROWS`.
    #[test]
    #[ignore = "a viewing aid, run on demand"]
    fn preview_a_picture() {
        let var =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.into());
        let img = image::open(var("VTTEAMS_PREVIEW_IMAGE", ""))
            .unwrap()
            .to_rgb8();
        // Without a size, the size the app would choose in a pane VTTEAMS_PREVIEW_WIDTH columns wide.
        let (cols, rows) = match (
            std::env::var("VTTEAMS_PREVIEW_COLS"),
            std::env::var("VTTEAMS_PREVIEW_ROWS"),
        ) {
            (Ok(c), Ok(r)) => (c.parse().unwrap(), r.parse().unwrap()),
            _ => budget(&img, var("VTTEAMS_PREVIEW_WIDTH", "170").parse().unwrap()),
        };
        eprintln!("art up to {cols}x{rows}");
        let mode = ImgMode::parse(&var("VTTEAMS_PREVIEW_MODE", "blocks")).unwrap();
        let lines = render(&img, mode, cols, rows);
        let set = glyphs(mode);
        let (w, h) = (
            lines[0]
                .spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>(),
            lines.len(),
        );
        let mut out = RgbImage::new((w * CELL) as u32, (h * CELL * 2) as u32);
        for (row, line) in lines.iter().enumerate() {
            let mut col = 0;
            for span in &line.spans {
                let (Some(Color::Rgb(fr, fg, fb)), Some(Color::Rgb(br, bg, bb))) =
                    (span.style.fg, span.style.bg)
                else {
                    continue;
                };
                for ch in span.content.chars() {
                    let bits = set.iter().find(|g| g.ch == ch).map_or(0, |g| g.bitmap);
                    for y in 0..CELL {
                        for x in 0..CELL {
                            let on = bits >> (63 - (y * CELL + x)) & 1 == 1;
                            let c = if on { [fr, fg, fb] } else { [br, bg, bb] };
                            for dy in 0..2 {
                                out.put_pixel(
                                    (col * CELL + x) as u32,
                                    (row * CELL * 2 + y * 2 + dy) as u32,
                                    image::Rgb(c),
                                );
                            }
                        }
                    }
                    col += 1;
                }
            }
        }
        out.save(var("VTTEAMS_PREVIEW_OUT", "preview.png")).unwrap();
    }
}
