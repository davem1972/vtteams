//! Where on the screen a picture may be drawn with graphics, and how much of it: pure rules about rectangles.
//!
//! A picture is drawn with graphics where it is inside the part of the transcript nothing else is drawn on. A picture
//! that is partly scrolled out of that part is cut at its edge: only the cells that are inside are drawn, and the rows of
//! the picture that belong to them (`Slice::from_px..to_px`). What a protocol can do with the cut (a Sixel in whole
//! bands of six rows, Kitty as a source rectangle, iTerm2 not at all) is decided where the picture is encoded
//! (`Encoded::visible_rows`).
//!
//! The labels the transcript draws over its own first and last row ("older messages", "latest") do not cut a picture:
//! the picture is drawn under them, and for a protocol that draws pixels into cells ([`overlay_cells`]) the few cells
//! that hold a label's text are written again after the picture, so that the text shows on top.

use ratatui::layout::Rect;
use std::ops::Range;

/// The transcript's viewport as the rules see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    /// The rectangle the transcript's text is drawn in.
    pub area: Rect,
    /// Which line of the transcript is at the top of it.
    pub scroll: usize,
}

/// The part of a picture that is inside the free rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slice {
    /// The cells of it that are inside, on the screen.
    pub rect: Rect,
    /// The pixel rows of the picture, counted from its top, that those cells show: where the first inside cell starts and
    /// where the last ends (not yet cut to the picture's own height, which a letterboxed picture falls short of).
    pub from_px: u32,
    pub to_px: u32,
}

/// One cell of text that is written again after a picture, because the picture's cells are where it belongs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayCell {
    pub x: u16,
    pub y: u16,
    pub symbol: String,
    pub fg: Option<(u8, u8, u8)>,
    pub bg: (u8, u8, u8),
    pub bold: bool,
}

/// The cells of `text`, which starts at (`x`, `y`) in `style`, that are inside `inside`: what to write over a picture so
/// that the text shows. A colour that is not an RGB one is left to the terminal's default (foreground) or to
/// `background` (background).
pub fn overlay_cells(
    (x, y): (u16, u16),
    text: &str,
    style: ratatui::style::Style,
    background: (u8, u8, u8),
    inside: Rect,
) -> Vec<OverlayCell> {
    use ratatui::style::{Color, Modifier};
    use unicode_width::UnicodeWidthChar;
    let rgb = |c: Option<Color>| match c {
        Some(Color::Rgb(r, g, b)) => Some((r, g, b)),
        _ => None,
    };
    let mut out = Vec::new();
    let mut at = x;
    for c in text.chars() {
        let width = c.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        let cell = Rect::new(at, y, width as u16, 1);
        if inside.contains((at, y).into()) && cell.right() <= inside.right() {
            out.push(OverlayCell {
                x: at,
                y,
                symbol: c.to_string(),
                fg: rgb(style.fg),
                bg: rgb(style.bg).unwrap_or(background),
                bold: style.add_modifier.contains(Modifier::BOLD),
            });
        }
        at = at.saturating_add(width as u16);
    }
    out
}

/// The part inside the viewport's rows of a picture that has `cols` x `art_rows` cells, starts at transcript line
/// `rows.start` and has `indent` columns in front of it, drawn in cells `cell_h` pixels high. `None` when none of it is
/// inside, or it does not fit across.
pub fn visible_slice(
    view: &Viewport,
    indent: u16,
    rows: &Range<usize>,
    cols: usize,
    art_rows: usize,
    cell_h: u32,
) -> Option<Slice> {
    if cols == 0 || art_rows == 0 || cell_h == 0 {
        return None;
    }
    if usize::from(indent) + cols > usize::from(view.area.width) {
        return None;
    }
    // Rows from the top of the viewport; negative above it.
    let top = i64::try_from(rows.start).ok()? - i64::try_from(view.scroll).ok()?;
    let bottom = top.checked_add(i64::try_from(art_rows).ok()?)?;
    let (inside_top, inside_bottom) = (top.max(0), bottom.min(i64::from(view.area.height)));
    if inside_bottom <= inside_top {
        return None;
    }
    let pixels = |n: i64| u32::try_from(n).ok()?.checked_mul(cell_h);
    Some(Slice {
        rect: Rect::new(
            view.area.x + indent,
            view.area.y + u16::try_from(inside_top).ok()?,
            u16::try_from(cols).ok()?,
            u16::try_from(inside_bottom - inside_top).ok()?,
        ),
        from_px: pixels(inside_top - top)?,
        to_px: pixels(inside_bottom - top)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(scroll: usize) -> Viewport {
        Viewport {
            area: Rect::new(40, 3, 80, 20),
            scroll,
        }
    }

    fn slice(scroll: usize, rows: Range<usize>, art_rows: usize) -> Option<Slice> {
        visible_slice(&view(scroll), 2, &rows, 30, art_rows, 20)
    }

    #[test]
    fn a_picture_wholly_inside_the_viewport_is_whole() {
        let s = slice(10, 14..24, 8).unwrap();
        assert_eq!(s.rect, Rect::new(42, 7, 30, 8));
        assert_eq!((s.from_px, s.to_px), (0, 160));
        // Exactly filling it still fits.
        let s = visible_slice(&view(0), 2, &(0..20), 78, 20, 20).unwrap();
        assert_eq!(s.rect, Rect::new(42, 3, 78, 20));
        assert_eq!((s.from_px, s.to_px), (0, 400));
    }

    #[test]
    fn a_picture_cut_at_the_top_keeps_its_lower_rows() {
        // 8 rows starting 2 above the viewport: 6 inside, from row 2 of the picture.
        let s = slice(10, 8..16, 8).unwrap();
        assert_eq!(s.rect, Rect::new(42, 3, 30, 6));
        assert_eq!((s.from_px, s.to_px), (40, 160));
        // Only its last row inside.
        let s = slice(10, 3..11, 8).unwrap();
        assert_eq!(
            (s.rect.y, s.rect.height, s.from_px, s.to_px),
            (3, 1, 140, 160)
        );
    }

    #[test]
    fn a_picture_cut_at_the_bottom_keeps_its_upper_rows() {
        // 8 rows starting 4 above the bottom: 4 inside.
        let s = slice(0, 16..24, 8).unwrap();
        assert_eq!(s.rect, Rect::new(42, 19, 30, 4));
        assert_eq!((s.from_px, s.to_px), (0, 80));
        let s = slice(0, 19..27, 8).unwrap();
        assert_eq!(
            (s.rect.y, s.rect.height, s.from_px, s.to_px),
            (22, 1, 0, 20)
        );
    }

    #[test]
    fn a_picture_taller_than_the_viewport_is_cut_at_both_ends() {
        // 30 rows, 5 above and 5 below the 20 of the viewport.
        let s = slice(5, 0..30, 30).unwrap();
        assert_eq!(s.rect, Rect::new(42, 3, 30, 20));
        assert_eq!((s.from_px, s.to_px), (100, 500));
    }

    #[test]
    fn a_picture_wholly_outside_is_nothing() {
        assert_eq!(slice(10, 0..8, 8), None, "above");
        assert_eq!(slice(0, 20..28, 8), None, "below");
        assert_eq!(slice(0, 5..5, 0), None);
        assert_eq!(visible_slice(&view(0), 2, &(0..5), 0, 5, 20), None);
        assert_eq!(visible_slice(&view(0), 2, &(0..5), 5, 5, 0), None);
        assert_eq!(
            visible_slice(&view(0), 2, &(0..5), 79, 5, 20),
            None,
            "wider than the viewport"
        );
    }

    #[test]
    fn the_labels_the_transcript_draws_do_not_cut_a_picture() {
        // A picture on the first and the last row: all of it is inside, whatever is written over those rows.
        let s = visible_slice(&view(0), 0, &(0..20), 10, 20, 20).unwrap();
        assert_eq!(
            (s.rect.y, s.rect.height, s.from_px, s.to_px),
            (3, 20, 0, 400)
        );
    }

    #[test]
    fn the_cells_of_a_label_inside_a_picture_are_rewritten_with_their_style() {
        use ratatui::style::{Color, Style};
        let style = Style::new()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Rgb(4, 5, 6))
            .bold();
        let cells = overlay_cells((10, 7), " ↓ G ", style, (9, 9, 9), Rect::new(0, 0, 100, 40));
        assert_eq!(
            cells.iter().map(|c| c.symbol.as_str()).collect::<String>(),
            " ↓ G "
        );
        assert_eq!(
            cells.iter().map(|c| c.x).collect::<Vec<_>>(),
            [10, 11, 12, 13, 14]
        );
        assert!(
            cells
                .iter()
                .all(|c| c.y == 7 && c.fg == Some((1, 2, 3)) && c.bg == (4, 5, 6) && c.bold)
        );
        // Only what is inside the picture's rectangle; the rest of a label is the frame's.
        let part = overlay_cells((10, 7), " ↓ G ", style, (9, 9, 9), Rect::new(12, 7, 2, 3));
        assert_eq!(part.iter().map(|c| c.x).collect::<Vec<_>>(), [12, 13]);
        assert!(
            overlay_cells((10, 7), " ↓ G ", style, (9, 9, 9), Rect::new(0, 8, 100, 3)).is_empty()
        );
        // A style without RGB colours falls back to the terminal's foreground and the given background.
        let plain = overlay_cells((0, 0), "x", Style::new(), (9, 8, 7), Rect::new(0, 0, 5, 5));
        assert_eq!(
            (plain[0].fg, plain[0].bg, plain[0].bold),
            (None, (9, 8, 7), false)
        );
        // A wide character takes two columns and a zero-width one none.
        let wide = overlay_cells(
            (0, 0),
            "a\u{4e2d}b\u{200b}c",
            Style::new(),
            (0, 0, 0),
            Rect::new(0, 0, 9, 1),
        );
        assert_eq!(
            wide.iter()
                .map(|c| (c.symbol.as_str(), c.x))
                .collect::<Vec<_>>(),
            [("a", 0), ("\u{4e2d}", 1), ("b", 3), ("c", 4)]
        );
    }
    #[test]
    fn huge_numbers_are_a_refusal_not_an_overflow() {
        let big = usize::MAX;
        assert_eq!(
            visible_slice(&view(0), 2, &(big - 1..big), 5, big, 20),
            None
        );
        assert_eq!(visible_slice(&view(0), u16::MAX, &(0..5), 5, 5, 20), None);
        assert_eq!(visible_slice(&view(big), 2, &(0..5), 5, 5, 20), None);
        assert_eq!(visible_slice(&view(0), 2, &(0..5), 5, 5, u32::MAX), None);
    }
}
