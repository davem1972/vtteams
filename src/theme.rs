//! Palette (Catppuccin-flavoured, matches the news TUI) and small styling helpers.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

pub const BG: Color = Color::Rgb(17, 17, 27);
pub const PANEL_BG: Color = Color::Rgb(24, 24, 37);
pub const TEXT: Color = Color::Rgb(205, 214, 244);
pub const SUBTLE: Color = Color::Rgb(147, 153, 178);
pub const DIM: Color = Color::Rgb(88, 91, 112);
/// The selected row of the chat list.
pub const SEL_BG: Color = Color::Rgb(56, 60, 92);
/// The selected message.
pub const MSG_SEL_BG: Color = Color::Rgb(46, 50, 76);
/// The selected item in a popup (the emoji selector, the people picker), which sits on `PANEL_BG`.
pub const POPUP_SEL_BG: Color = Color::Rgb(78, 83, 128);
pub const OK: Color = Color::Rgb(166, 227, 161);
pub const ERR: Color = Color::Rgb(243, 139, 168);
pub const WARN: Color = Color::Rgb(249, 226, 175);
pub const LINK: Color = Color::Rgb(137, 180, 250);
pub const ACCENT: Color = Color::Rgb(203, 166, 247);
pub const CODE_BG: Color = Color::Rgb(30, 30, 46);
pub const CODE_FG: Color = Color::Rgb(205, 214, 244);
pub const CODE_BORDER: Color = Color::Rgb(69, 71, 90);
pub const CODE_LABEL: Color = Color::Rgb(148, 226, 213);
pub const CHIP_BG: Color = Color::Rgb(49, 50, 68);
/// A message that @mentions the signed-in user (the header badge, the top-bar counter, and the mention itself in
/// the body): the same red as `ERR`, by design — meant to stand out the way Teams' own client does for a mention
/// of *you* specifically, not because it is an error. Mirrored as `convert::MENTION_ME_COLOR`. A mention of
/// someone other than the signed-in user has no colour of its own here: it is `LINK`'s, matching Teams' own
/// link-styled mention of someone else (which also opens a hover profile card there; vtteams has no such popup).
pub const MENTION_ME: Color = Color::Rgb(243, 139, 168);

/// An out-of-office notice in the banner above the message box: the red the Teams client draws it in (an ordinary status
/// message there is grey, `SUBTLE` here). Its own constant, because it is what the notice means and not an error or a
/// mention of you.
pub const OUT_OF_OFFICE: Color = Color::Rgb(240, 100, 120);

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One accent colour per person, stable for a given name.
const PEOPLE: [(u8, u8, u8); 8] = [
    (137, 180, 250), // blue
    (245, 194, 231), // pink
    (250, 179, 135), // peach
    (148, 226, 213), // teal
    (203, 166, 247), // mauve
    (249, 226, 175), // yellow
    (166, 227, 161), // green
    (235, 160, 172), // maroon
];

pub fn person_color(key: &str) -> Color {
    let h = key
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    let (r, g, b) = PEOPLE[h as usize % PEOPLE.len()];
    Color::Rgb(r, g, b)
}

pub fn lerp(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let f = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

/// cyan -> violet -> pink, `t` in 0..=1.
pub fn gradient(t: f32) -> Color {
    const A: (u8, u8, u8) = (96, 226, 255);
    const B: (u8, u8, u8) = (166, 120, 255);
    const C: (u8, u8, u8) = (255, 105, 180);
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t < 0.5 {
        lerp(A, B, t * 2.0)
    } else {
        lerp(B, C, (t - 0.5) * 2.0)
    };
    Color::Rgb(r, g, b)
}

/// Gradient text that slowly drifts with `tick`. The drift moves in coarse steps: a smooth one changes the colour by
/// about one RGB level per frame, which nobody can see but still makes the terminal rewrite the cells (and the cursor
/// hop there and back) every frame while the app is otherwise idle.
pub fn gradient_spans(text: &str, tick: usize, style: Style) -> Vec<Span<'static>> {
    const STEPS: f32 = 40.0;
    let n = text.chars().count().max(1) as f32;
    let shift = ((tick as f32 * 0.012).sin() * 0.25 * STEPS).round() / STEPS;
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            Span::styled(
                c.to_string(),
                style.fg(gradient(i as f32 / n + shift + 0.15)),
            )
        })
        .collect()
}

pub fn chip(text: &str) -> Span<'static> {
    Span::styled(
        format!(" {text} "),
        Style::new().fg(TEXT).bg(CHIP_BG).bold(),
    )
}

pub fn pill(text: &str, color: Color) -> Span<'static> {
    Span::styled(format!(" {text} "), Style::new().fg(BG).bg(color).bold())
}

/// `lines` with `bg` applied to every span that has none, padded to `width` so the tint spans the full row.
pub fn tint(line: Line<'static>, bg: Color, width: usize) -> Line<'static> {
    let used: usize = line
        .spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let mut spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|mut s| {
            if s.style.bg.is_none() {
                s.style = s.style.bg(bg);
            }
            s
        })
        .collect();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), Style::new().bg(bg)));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG contrast ratio, via `inline_style`'s own formula (not a copy of it), so these tests check the formula
    /// actually used at runtime instead of one that could silently drift from it.
    fn contrast(a: Color, b: Color) -> f64 {
        let rgb = |c: Color| match c {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => panic!("not an RGB colour"),
        };
        f64::from(crate::inline_style::contrast(rgb(a), rgb(b)))
    }

    #[test]
    fn a_selection_stands_out_from_what_it_sits_on() {
        assert!(
            contrast(SEL_BG, BG) >= 1.6,
            "chat list row: {}",
            contrast(SEL_BG, BG)
        );
        assert!(
            contrast(MSG_SEL_BG, BG) >= 1.4,
            "message: {}",
            contrast(MSG_SEL_BG, BG)
        );
        assert!(
            contrast(POPUP_SEL_BG, PANEL_BG) >= 2.2,
            "popup item: {}",
            contrast(POPUP_SEL_BG, PANEL_BG)
        );
    }

    #[test]
    fn the_banner_colours_read_on_the_page_and_tell_the_two_kinds_apart() {
        for (name, colour) in [("out of office", OUT_OF_OFFICE), ("status message", SUBTLE)] {
            assert!(
                contrast(colour, BG) >= 4.5,
                "{name} on the page: {}",
                contrast(colour, BG)
            );
        }
        assert_ne!(OUT_OF_OFFICE, SUBTLE);
        assert_ne!(
            OUT_OF_OFFICE, MENTION_ME,
            "its own colour, not the mention's"
        );
        assert_ne!(OUT_OF_OFFICE, ERR);
    }

    #[test]
    fn text_on_a_selection_stays_readable() {
        for bg in [SEL_BG, MSG_SEL_BG, POPUP_SEL_BG] {
            assert!(contrast(TEXT, bg) >= 4.5, "text on {bg:?}");
        }
        // The muted text of a selected chat row is lifted from DIM to SUBTLE, because DIM would nearly vanish.
        assert!(
            contrast(SUBTLE, SEL_BG) >= 3.0,
            "{}",
            contrast(SUBTLE, SEL_BG)
        );
        assert!(
            contrast(DIM, SEL_BG) < 3.0,
            "DIM is not enough on a selection"
        );
    }

    #[test]
    fn the_details_in_a_popup_list_read_on_both_its_rows_and_its_highlight() {
        // The `:new` picker draws a person's email, phone, job and office in SUBTLE, and in TEXT on the highlight.
        assert!(
            contrast(SUBTLE, PANEL_BG) >= 4.5,
            "{}",
            contrast(SUBTLE, PANEL_BG)
        );
        assert!(
            contrast(TEXT, POPUP_SEL_BG) >= 4.5,
            "{}",
            contrast(TEXT, POPUP_SEL_BG)
        );
        assert!(
            contrast(DIM, POPUP_SEL_BG) < 2.0,
            "DIM, which they used to be drawn in, all but vanishes there: {}",
            contrast(DIM, POPUP_SEL_BG)
        );
    }
}
