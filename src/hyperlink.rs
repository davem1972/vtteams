//! Terminal hyperlinks (OSC 8): text on screen that the terminal opens, on Ctrl-click or a click, as an address that
//! need not be the text itself. A sender's client often shows a long address shortened ("…/objectscrip…") and a long
//! address wraps over several rows; a terminal guessing links from the text then opens only what one row shows. Marked
//! with OSC 8, every row of it opens the whole address.
//!
//! The escape sequence carries an address that came from the network, which everything else here keeps out of escape
//! sequences, so it is allowed under one rule, [`target`]: a web or mail address of printable ASCII only (no space, no
//! control character, so nothing can end the sequence early or start another), no longer than [`MAX_TARGET`], with a
//! mail address's parameters dropped exactly as [`crate::app`]'s `open_url` drops them. An address that fails is simply
//! not marked. The visible text inside the sequence is what the frame already holds in those cells.
//!
//! ratatui has no hyperlink type; its own tests show the way it supports one: the whole link (opening sequence, text,
//! closing sequence) in the first cell, given [`CellDiffOption::ForcedWidth`] for the columns the text covers, so the
//! diff writes it as one unit and leaves the covered cells alone. [`mark`] does that, one cell for each run of one style.
//! A cell the terminal still shows as part of a link after the link has gone (scrolled away, a popup opened) keeps the
//! link if the new frame happens to put the same character there, so [`mark`] has every cell of last frame's links
//! written again ([`CellDiffOption::AlwaysUpdate`]) unless it is part of a link again.

use ratatui::buffer::{Buffer, CellDiffOption};
use std::num::NonZeroU16;
use unicode_width::UnicodeWidthStr;

/// The longest address marked. Terminals keep each one in memory per link; Windows Terminal and VTE accept a few
/// thousand bytes.
pub const MAX_TARGET: usize = 2048;

const OPEN: &str = "\x1b]8;;";
const END: &str = "\x1b\\";

/// A link drawn this frame: columns `x..x + width` of row `y`, and the address to open (already passed by [`target`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spot {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub url: String,
}

/// The address to hand the terminal for a link to `url`, or `None` when it must not be marked.
pub fn target(url: &str) -> Option<String> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    let web = lower.starts_with("https://") || lower.starts_with("http://");
    let mail = lower.starts_with("mailto:");
    if !(web || mail) || url.len() > MAX_TARGET || !url.bytes().all(|b| (0x21..=0x7e).contains(&b))
    {
        return None;
    }
    if mail {
        let address = url["mailto:".len()..]
            .split(['?', '#'])
            .next()
            .unwrap_or("");
        return (!address.is_empty()).then(|| format!("mailto:{address}"));
    }
    Some(url.to_string())
}

/// What the cell starting a link holds: the opening sequence, the visible `text`, the closing sequence.
fn wrapped(url: &str, text: &str) -> String {
    format!("{OPEN}{url}{END}{text}{OPEN}{END}")
}

/// Whether a cell's text is a link [`mark`] made: one opening sequence whose address passes [`target`], text with no
/// control character, and one closing sequence. The frame's check for control characters lets exactly these through.
pub fn is_ours(symbol: &str) -> bool {
    let Some(rest) = symbol.strip_prefix(OPEN) else {
        return false;
    };
    let Some((url, rest)) = rest.split_once(END) else {
        return false;
    };
    let Some(text) = rest.strip_suffix(&format!("{OPEN}{END}")) else {
        return false;
    };
    target(url).as_deref() == Some(url) && !text.chars().any(char::is_control)
}

/// Mark `spots` as links in `buffer`, and have every cell of `before` (last frame's links) that is not part of one now
/// written again. A spot is cut where the style changes (one cell holds one style) and where a cell is left alone for
/// a picture.
pub fn mark(buffer: &mut Buffer, spots: &[Spot], before: &[Spot]) {
    let area = buffer.area;
    let mut linked: Vec<(u16, std::ops::Range<u16>)> = Vec::new();
    for spot in spots {
        if spot.y < area.y || spot.y >= area.bottom() {
            continue;
        }
        let end = spot.x.saturating_add(spot.width).min(area.right());
        let mut x = spot.x.max(area.x);
        while x < end {
            // One run: cells of one style, none of them left alone for a picture.
            let Some(first) = buffer.cell((x, spot.y)) else {
                break;
            };
            if first.diff_option == CellDiffOption::Skip {
                x += 1;
                continue;
            }
            let style = first.style();
            let mut text = String::new();
            let mut at = x;
            while at < end {
                let Some(cell) = buffer.cell((at, spot.y)) else {
                    break;
                };
                if cell.diff_option == CellDiffOption::Skip || cell.style() != style {
                    break;
                }
                let width = cell.symbol().width().max(1) as u16;
                if at + width > end {
                    break;
                }
                text.push_str(cell.symbol());
                at += width;
            }
            if at == x {
                // A wide character cut by the end of the spot: left as it is.
                x += 1;
                continue;
            }
            if let (Some(cell), Some(width)) =
                (buffer.cell_mut((x, spot.y)), NonZeroU16::new(at - x))
            {
                cell.set_symbol(&wrapped(&spot.url, &text));
                cell.set_diff_option(CellDiffOption::ForcedWidth(width));
            }
            linked.push((spot.y, x..at));
            x = at;
        }
    }
    for spot in before {
        for x in spot.x..spot.x.saturating_add(spot.width) {
            if linked
                .iter()
                .any(|(y, cols)| *y == spot.y && cols.contains(&x))
            {
                continue;
            }
            if let Some(cell) = buffer.cell_mut((x, spot.y))
                && cell.diff_option == CellDiffOption::None
            {
                cell.set_diff_option(CellDiffOption::AlwaysUpdate);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};

    fn spot(x: u16, y: u16, width: u16, url: &str) -> Spot {
        Spot {
            x,
            y,
            width,
            url: url.into(),
        }
    }

    #[test]
    fn only_web_and_mail_addresses_of_printable_ascii_are_marked() {
        let long = format!("https://e.example/{}", "a".repeat(MAX_TARGET));
        assert_eq!(
            target("https://e.example/a?b=1#L72").as_deref(),
            Some("https://e.example/a?b=1#L72")
        );
        assert_eq!(
            target("HTTP://E.EXAMPLE/").as_deref(),
            Some("HTTP://E.EXAMPLE/")
        );
        for refused in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://e.example/\x1b\\\x1b]0;title",
            "https://e.example/\x07",
            "https://e.example/a b",
            "https://e.example/caf\u{e9}",
            "https://e.example/\u{9c}",
            "mailto:",
            long.as_str(),
        ] {
            assert_eq!(target(refused), None, "{refused:?}");
        }
        assert_eq!(
            target("mailto:ada@e.example?subject=x&attach=y").as_deref(),
            Some("mailto:ada@e.example"),
            "a mail link's parameters are not the sender's to choose"
        );
    }

    #[test]
    fn a_link_becomes_one_cell_per_run_of_one_style_covering_its_columns() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
        buffer.set_string(0, 0, "see ", Style::new());
        buffer.set_string(4, 0, "abc", Style::new().fg(Color::Blue));
        buffer.set_string(7, 0, "de", Style::new().fg(Color::Red));
        mark(&mut buffer, &[spot(4, 0, 5, "https://e.example/x")], &[]);
        let first = buffer.cell((4, 0)).unwrap();
        assert_eq!(
            first.symbol(),
            "\x1b]8;;https://e.example/x\x1b\\abc\x1b]8;;\x1b\\"
        );
        assert_eq!(
            first.diff_option,
            CellDiffOption::ForcedWidth(NonZeroU16::new(3).unwrap())
        );
        assert!(is_ours(first.symbol()));
        let second = buffer.cell((7, 0)).unwrap();
        assert_eq!(
            second.symbol(),
            "\x1b]8;;https://e.example/x\x1b\\de\x1b]8;;\x1b\\"
        );
        assert_eq!(
            buffer.cell((3, 0)).unwrap().symbol(),
            " ",
            "outside the link nothing changes"
        );
        assert_eq!(
            buffer.cell((9, 0)).unwrap().diff_option,
            CellDiffOption::None
        );
    }

    #[test]
    fn the_diff_writes_the_link_as_one_unit_and_skips_the_cells_it_covers() {
        let before = Buffer::empty(Rect::new(0, 0, 10, 1));
        let mut next = Buffer::empty(Rect::new(0, 0, 10, 1));
        next.set_string(2, 0, "link", Style::new());
        mark(&mut next, &[spot(2, 0, 4, "https://e.example/")], &[]);
        let diff = before.diff(&next);
        assert_eq!(diff.len(), 1, "{diff:?}");
        assert_eq!((diff[0].0, diff[0].1), (2, 0));
        assert!(diff[0].2.symbol().contains("link"));
    }

    #[test]
    fn cells_of_a_link_that_has_gone_are_written_again_even_when_unchanged() {
        let area = Rect::new(0, 0, 10, 1);
        let mut shown = Buffer::empty(area);
        shown.set_string(0, 0, "ab cd", Style::new());
        let was = [spot(0, 0, 5, "https://e.example/")];
        mark(&mut shown, &was, &[]);
        // The same text, no longer a link: every cell the link covered is sent again, so none keeps it.
        let mut next = Buffer::empty(area);
        next.set_string(0, 0, "ab cd", Style::new());
        mark(&mut next, &[], &was);
        let diff = shown.diff(&next);
        let columns: Vec<u16> = diff.iter().map(|(x, ..)| *x).collect();
        assert_eq!(columns, [0, 1, 2, 3, 4], "{diff:?}");
        // A link still there in the next frame is not written again.
        let mut again = Buffer::empty(area);
        again.set_string(0, 0, "ab cd", Style::new());
        mark(&mut again, &was, &was);
        assert!(shown.diff(&again).is_empty());
    }

    #[test]
    fn a_cell_left_alone_for_a_picture_is_not_drawn_over() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        buffer.set_string(0, 0, "abcdef", Style::new());
        buffer
            .cell_mut((2, 0))
            .unwrap()
            .set_diff_option(CellDiffOption::Skip);
        mark(&mut buffer, &[spot(0, 0, 6, "https://e.example/")], &[]);
        assert_eq!(
            buffer.cell((2, 0)).unwrap().diff_option,
            CellDiffOption::Skip
        );
        assert!(
            buffer
                .cell((0, 0))
                .unwrap()
                .symbol()
                .contains("\x1b\\ab\x1b")
        );
        assert!(
            buffer
                .cell((3, 0))
                .unwrap()
                .symbol()
                .contains("\x1b\\def\x1b")
        );
    }

    #[test]
    fn only_a_well_formed_link_with_a_safe_address_is_taken_for_ours() {
        assert!(is_ours(
            "\x1b]8;;https://e.example/\x1b\\text\x1b]8;;\x1b\\"
        ));
        for not in [
            "\x1b]0;title\x07",
            "\x1b]8;;https://e.example/\x1b\\text",
            "\x1b]8;;file:///x\x1b\\text\x1b]8;;\x1b\\",
            "\x1b]8;;https://e.example/\x1b\\te\x1bxt\x1b]8;;\x1b\\",
            "\x1b]8;id=1;https://e.example/\x1b\\text\x1b]8;;\x1b\\",
            "plain",
        ] {
            assert!(!is_ours(not), "{not:?}");
        }
    }
}
