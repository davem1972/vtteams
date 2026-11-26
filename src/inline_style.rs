//! Inline formatting that Markdown has no way to say: underline, strikethrough, text colour and highlight.
//!
//! A message arrives as HTML (`<u>`, `<s>`, `<span style="color:#B6424C">`, `<span style="background-color:#9DD9DB">`),
//! is converted to Markdown, and that is laid out by termimad. To get these effects through those two steps they travel
//! in the text as private-use characters: one character opens a style (it is the style's number in a table) and
//! [`CLOSE`] ends the innermost one. After layout, [`apply_linked`] removes the characters and puts the styles on the spans.
//! A marker is one column wide, so a line with a styled run wraps one or two columns early at worst.
//!
//! Remote text cannot forge a marker: [`strip`] takes the whole range out of everything that comes from the network
//! before the converter adds its own.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::Mutex;

/// Ends the style opened last.
pub const CLOSE: char = '\u{E000}';
/// The first character that opens a style, and the last character reserved.
const FIRST: u32 = 0xE001;
const LAST: u32 = 0xF8FF;

/// What a styled run looks like. Colours are as the sender wrote them; [`apply_linked`] makes them readable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Format {
    pub underline: bool,
    pub strike: bool,
    pub bold: bool,
    pub italic: bool,
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
    /// The text is the label of link number `n` (1-based, as in `[n]`): it changes nothing about how the text looks,
    /// only where [`apply_linked`] says the link was laid out.
    pub link: Option<u16>,
}

impl Format {
    fn is_plain(&self) -> bool {
        *self == Self::default()
    }
}

/// Every format met so far, by number. Formats repeat (the same red again and again), so this stays small; at the
/// limit a new format is simply not applied.
static TABLE: Mutex<Vec<Format>> = Mutex::new(Vec::new());

fn table() -> std::sync::MutexGuard<'static, Vec<Format>> {
    TABLE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn is_marker(c: char) -> bool {
    (0xE000..=LAST).contains(&(c as u32))
}

/// The character that opens `format`, or `None` when the table is full.
fn opener(format: Format) -> Option<char> {
    let mut table = table();
    let index = match table.iter().position(|f| *f == format) {
        Some(i) => i,
        None => {
            if FIRST as usize + table.len() > LAST as usize {
                return None;
            }
            table.push(format);
            table.len() - 1
        }
    };
    char::from_u32(FIRST + u32::try_from(index).ok()?)
}

/// `text` with `format` applied. Whitespace at either end stays outside, so the markers cannot upset Markdown's rules
/// for where emphasis may start and end; nothing is added to text with nothing visible in it.
pub fn wrap(format: Format, text: &str) -> String {
    let core = text.trim();
    if format.is_plain() || core.is_empty() {
        return text.to_string();
    }
    let Some(open) = opener(format) else {
        return text.to_string();
    };
    let lead = &text[..text.len() - text.trim_start().len()];
    let trail = &text[text.trim_end().len()..];
    format!("{lead}{open}{core}{CLOSE}{trail}")
}

/// `s` without any marker: what comes from the network, or what goes to the clipboard.
pub fn strip(s: &str) -> String {
    if s.chars().any(is_marker) {
        s.chars().filter(|c| !is_marker(*c)).collect()
    } else {
        s.to_string()
    }
}

/// Replaces numeric character references that decode to markers (U+E000..=U+F8FF) with U+FFFD.
/// Called before conversion so that entities like `&#xE001;` cannot become markers after `strip` runs.
/// Replacement prevents a deleted reference from creating a new one from surrounding text.
pub fn strip_references(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    let chars: Vec<char> = s.chars().collect();

    while i < chars.len() {
        if chars[i] == '&' && i + 1 < chars.len() && chars[i + 1] == '#' {
            let start = i;
            i += 2;

            let is_hex = i < chars.len() && (chars[i] == 'x' || chars[i] == 'X');
            if is_hex {
                i += 1;
            }

            let digit_start = i;
            while i < chars.len() {
                let c = chars[i];
                let valid = if is_hex {
                    c.is_ascii_hexdigit()
                } else {
                    c.is_ascii_digit()
                };
                if valid {
                    i += 1;
                } else {
                    break;
                }
            }

            let has_digits = i > digit_start;
            let digit_end = i;

            if i < chars.len() && chars[i] == ';' {
                i += 1;
            }

            if has_digits {
                let radix = if is_hex { 16 } else { 10 };
                let digits: String = chars[digit_start..digit_end].iter().collect();
                if let Ok(value) = u32::from_str_radix(&digits, radix)
                    && (0xE000..=LAST).contains(&value)
                {
                    result.push('\u{FFFD}');
                    continue;
                }
            }

            result.extend(&chars[start..i]);
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Reading a style attribute
// ---------------------------------------------------------------------------

fn named_colour(name: &str) -> Option<(u8, u8, u8)> {
    Some(match name {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "lime" => (0, 255, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "orange" => (255, 165, 0),
        "purple" => (128, 0, 128),
        "pink" => (255, 192, 203),
        "gray" | "grey" => (128, 128, 128),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "brown" => (165, 42, 42),
        "navy" => (0, 0, 128),
        "teal" => (0, 128, 128),
        "maroon" => (128, 0, 0),
        "olive" => (128, 128, 0),
        "silver" => (192, 192, 192),
        _ => return None,
    })
}

/// A CSS colour: `#rgb`, `#rrggbb`, `rgb(r, g, b)` (and `rgba`, whose opacity is ignored) or a common name.
pub fn parse_colour(value: &str) -> Option<(u8, u8, u8)> {
    let v = value.trim().to_ascii_lowercase();
    if let Some(hex) = v.strip_prefix('#') {
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        return match hex.len() {
            3 => {
                let d = |i: usize| u8::from_str_radix(&hex[i..=i], 16).ok().map(|n| n * 17);
                Some((d(0)?, d(1)?, d(2)?))
            }
            6 => {
                let d = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
                Some((d(0)?, d(2)?, d(4)?))
            }
            _ => None,
        };
    }
    if let Some(args) = v
        .strip_prefix("rgba(")
        .or_else(|| v.strip_prefix("rgb("))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let mut parts = args.split([',', ' ', '/']).filter(|p| !p.is_empty());
        let mut channel = || -> Option<u8> {
            let p = parts.next()?;
            let n: f32 = p.strip_suffix('%').map_or_else(
                || p.parse().ok(),
                |pct| pct.parse::<f32>().ok().map(|x| x * 2.55),
            )?;
            Some(n.clamp(0.0, 255.0).round() as u8)
        };
        return Some((channel()?, channel()?, channel()?));
    }
    named_colour(&v)
}

/// The formatting a `style="..."` attribute asks for, as far as vtteams can draw it: colour, background, underline and
/// strike-through, bold and italic. Anything else in it is ignored.
pub fn parse_style(style: &str) -> Format {
    let mut f = Format::default();
    for declaration in style.split(';') {
        let Some((name, value)) = declaration.split_once(':') else {
            continue;
        };
        let (name, value) = (
            name.trim().to_ascii_lowercase(),
            value.trim().to_ascii_lowercase(),
        );
        match name.as_str() {
            "color" => f.fg = parse_colour(&value),
            "background-color" => f.bg = parse_colour(&value),
            // `background: #ff0 url(..)`: take the colour if it is the first word.
            "background" => f.bg = value.split_whitespace().next().and_then(parse_colour),
            "text-decoration" | "text-decoration-line" => {
                f.underline |= value.contains("underline");
                f.strike |= value.contains("line-through");
            }
            "font-weight" => {
                f.bold |= value == "bold" || value.parse::<u32>().is_ok_and(|w| w >= 600)
            }
            "font-style" => f.italic |= value == "italic" || value == "oblique",
            _ => {}
        }
    }
    f
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// WCAG relative luminance of an RGB colour. `pub(crate)` so `theme.rs`'s contrast tests exercise this formula
/// itself rather than a copy of their own, which would give false confidence if the two ever drifted apart.
pub(crate) fn luminance((r, g, b): (u8, u8, u8)) -> f32 {
    let f = |c: u8| {
        let x = f32::from(c) / 255.0;
        if x <= 0.03928 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

pub(crate) fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn rgb(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

const DARK_TEXT: (u8, u8, u8) = (20, 20, 30);
const LIGHT_TEXT: (u8, u8, u8) = (235, 235, 245);
/// Least contrast a sender's colour keeps against what it is drawn on; a colour chosen for a white page (Teams'
/// dark red) is lightened to this on the dark one.
const MIN_CONTRAST: f32 = 4.0;

/// Move `colour` towards white until it shows against `ground`.
fn lighten_until_readable(colour: (u8, u8, u8), ground: (u8, u8, u8)) -> (u8, u8, u8) {
    let mut c = colour;
    for _ in 0..12 {
        if contrast(c, ground) >= MIN_CONTRAST {
            break;
        }
        let up = |x: u8| x + ((255 - x) as f32 * 0.15).ceil() as u8;
        c = (up(c.0), up(c.1), up(c.2));
    }
    c
}

/// The style of a run of text: the style it already had (from the layout) with each active format on top.
fn styled(base: Style, active: &[Format]) -> Style {
    let mut style = base;
    let mut underline = false;
    let mut strike = false;
    let (mut bold, mut italic) = (false, false);
    let (mut fg, mut bg) = (None, None);
    for f in active {
        underline |= f.underline;
        strike |= f.strike;
        bold |= f.bold;
        italic |= f.italic;
        fg = f.fg.or(fg);
        bg = f.bg.or(bg);
    }
    if underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if strike {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    let page = rgb(crate::theme::BG).unwrap_or((17, 17, 27));
    match (fg, bg) {
        (None, None) => {}
        (Some(fg), None) => style = style.fg(rgb_color(lighten_until_readable(fg, page))),
        (fg, Some(bg)) => {
            // Text on a highlight: the sender's colour if it shows on it, else whichever of dark and light does.
            let text = fg.filter(|c| contrast(*c, bg) >= 3.0).unwrap_or(
                if contrast(DARK_TEXT, bg) >= contrast(LIGHT_TEXT, bg) {
                    DARK_TEXT
                } else {
                    LIGHT_TEXT
                },
            );
            style = style.fg(rgb_color(text)).bg(rgb_color(bg));
        }
    }
    style
}

fn rgb_color((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// Take the markers out of laid-out `lines` and put the styles they stand for on the text between them. A run may
/// continue on the next line, so the open styles carry over from line to line; whatever is still open at the end ends
/// there.
#[cfg(test)]
pub fn apply(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    apply_linked(lines).0
}

/// Where a link's label was laid out: line, the columns it covers on that line, and the link's number.
pub type LinkRun = (usize, std::ops::Range<usize>, u16);

/// Take the markers out of laid-out `lines` and put the styles they stand for on the text between them (a run may go on to the next line), also saying where the text of each link label ended up: one run per line it covers (a label that wraps
/// has one on each of its lines). Columns count what is drawn, so the markers taken out are not counted.
pub fn apply_linked(lines: Vec<Line<'static>>) -> (Vec<Line<'static>>, Vec<LinkRun>) {
    use unicode_width::UnicodeWidthChar;
    if !lines
        .iter()
        .any(|l| l.spans.iter().any(|s| s.content.chars().any(is_marker)))
    {
        return (lines, Vec::new());
    }
    let formats = table().clone();
    let mut active: Vec<Format> = Vec::new();
    let mut out = Vec::with_capacity(lines.len());
    let mut runs: Vec<LinkRun> = Vec::new();
    let link_of = |active: &[Format]| active.iter().rev().find_map(|f| f.link);
    for (row, line) in lines.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
        let mut col = 0;
        for span in line.spans {
            let mut text = String::new();
            let mut flush = |text: &mut String, active: &[Format]| {
                if !text.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(text),
                        styled(span.style, active),
                    ));
                }
            };
            for c in span.content.chars() {
                if c == CLOSE {
                    flush(&mut text, &active);
                    active.pop();
                } else if is_marker(c) {
                    flush(&mut text, &active);
                    active.push(
                        formats
                            .get((c as u32 - FIRST) as usize)
                            .copied()
                            .unwrap_or_default(),
                    );
                } else {
                    text.push(c);
                    let width = c.width().unwrap_or(0);
                    if let Some(n) = link_of(&active).filter(|_| width > 0) {
                        match runs.last_mut() {
                            Some((r, cols, m)) if *r == row && *m == n && cols.end == col => {
                                cols.end += width;
                            }
                            _ => runs.push((row, col..col + width, n)),
                        }
                    }
                    col += width;
                }
            }
            flush(&mut text, &active);
        }
        out.push(Line { spans, ..line });
    }
    (out, runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red() -> Format {
        Format {
            fg: Some((0xB6, 0x42, 0x4C)),
            ..Format::default()
        }
    }

    fn line(text: &str) -> Line<'static> {
        Line::from(Span::styled(
            text.to_string(),
            Style::new().fg(Color::Rgb(205, 214, 244)),
        ))
    }

    fn text(lines: &[Line<'_>]) -> String {
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

    #[test]
    fn colours_and_style_attributes_are_read_as_css_writes_them() {
        for (css, want) in [
            ("#B6424C", Some((0xB6, 0x42, 0x4C))),
            ("#f00", Some((255, 0, 0))),
            ("rgb(0, 128, 0)", Some((0, 128, 0))),
            ("rgba(10,20,30,0.5)", Some((10, 20, 30))),
            ("rgb(100%, 0%, 0%)", Some((255, 0, 0))),
            ("Teal", Some((0, 128, 128))),
            ("#12345", None),
            ("#gggggg", None),
            ("transparent", None),
            ("rgb(1,2)", None),
            ("url(javascript:1)", None),
        ] {
            assert_eq!(parse_colour(css), want, "{css}");
        }
        let f = parse_style(
            "color:#B6424C; text-decoration: underline line-through; font-weight:700; margin: 1px",
        );
        assert_eq!(f.fg, Some((0xB6, 0x42, 0x4C)));
        assert!(f.underline && f.strike && f.bold && !f.italic);
        assert_eq!(
            parse_style("background-color:#9DD9DB").bg,
            Some((0x9D, 0xD9, 0xDB))
        );
        assert_eq!(
            parse_style("background: #ff0 url(x.png) no-repeat").bg,
            Some((255, 255, 0))
        );
        assert!(parse_style("margin: 0; padding: 1px; nonsense").is_plain());
    }

    #[test]
    fn a_styled_run_goes_through_layout_and_comes_back_as_styled_spans() {
        let wrapped = wrap(
            Format {
                underline: true,
                ..Format::default()
            },
            "under",
        );
        let lines = apply(vec![line(&format!("see {wrapped} and more"))]);
        assert_eq!(text(&lines), "see under and more");
        let spans = &lines[0].spans;
        assert_eq!(spans.len(), 3);
        assert!(!spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(spans[1].style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(spans[1].content, "under");
        assert!(!spans[2].style.add_modifier.contains(Modifier::UNDERLINED));
        // The colour layout gave the text is kept where nothing overrides it.
        assert_eq!(spans[1].style.fg, Some(Color::Rgb(205, 214, 244)));
    }

    #[test]
    fn a_run_that_wraps_keeps_its_style_on_the_next_line_and_formats_nest() {
        let both = wrap(
            Format {
                strike: true,
                ..Format::default()
            },
            "gone soon",
        );
        let underlined = wrap(
            Format {
                underline: true,
                ..Format::default()
            },
            &format!("a {both} b"),
        );
        // Split the run across two lines the way a layout would.
        let (first, second) = underlined.split_at(underlined.find("soon").unwrap());
        let lines = apply(vec![line(first), line(second)]);
        assert_eq!(text(&lines), "a gone \nsoon b");
        let soon = &lines[1].spans[0];
        assert_eq!(soon.content, "soon");
        assert!(
            soon.style
                .add_modifier
                .contains(Modifier::CROSSED_OUT | Modifier::UNDERLINED)
        );
        let b = lines[1].spans.last().unwrap();
        assert!(
            b.style.add_modifier.contains(Modifier::UNDERLINED)
                && !b.style.add_modifier.contains(Modifier::CROSSED_OUT)
        );
    }

    #[test]
    fn what_is_left_open_ends_with_the_text_and_text_without_markers_is_untouched() {
        let open = wrap(red(), "x");
        let unclosed = open.trim_end_matches(CLOSE).to_string();
        let lines = apply(vec![line(&unclosed), line("next")]);
        assert_eq!(text(&lines), "x\nnext");
        // An unclosed run is drawn on, as the next line is the same paragraph; no stray marker survives.
        assert!(!text(&lines).chars().any(is_marker));
        let plain = vec![line("nothing special")];
        assert_eq!(apply(plain.clone()), plain);
    }

    #[test]
    fn remote_text_cannot_forge_a_marker_and_whitespace_stays_outside_the_markers() {
        let forged = format!("{}pwned{}", '\u{E001}', CLOSE);
        assert_eq!(strip(&forged), "pwned");
        assert_eq!(strip("plain text"), "plain text");
        let wrapped = wrap(red(), " red ");
        assert!(
            wrapped.starts_with(' ') && wrapped.ends_with(' '),
            "{wrapped:?}"
        );
        assert_eq!(strip(&wrapped), " red ");
        assert_eq!(wrap(red(), "   "), "   ", "nothing visible, nothing added");
        assert_eq!(wrap(Format::default(), "x"), "x");
    }

    #[test]
    fn colours_are_made_to_show_on_the_page_and_on_a_highlight() {
        let page = rgb(crate::theme::BG).unwrap();
        // Teams'' dark red, drawn for a white page, is lightened for the dark one.
        let dark_red = (0x80, 0x10, 0x20);
        let shown = rgb(styled(
            Style::new(),
            &[Format {
                fg: Some(dark_red),
                ..Format::default()
            }],
        )
        .fg
        .unwrap())
        .unwrap();
        assert!(contrast(shown, page) >= MIN_CONTRAST - 0.1, "{shown:?}");
        assert!(shown.0 > dark_red.0, "still reddish: {shown:?}");
        // A colour that already shows is left alone.
        let bright = (255, 120, 120);
        assert_eq!(
            styled(
                Style::new(),
                &[Format {
                    fg: Some(bright),
                    ..Format::default()
                }]
            )
            .fg,
            Some(Color::Rgb(255, 120, 120))
        );
        // Text on a light highlight turns dark, on a dark one light, whatever the layout had made it.
        let on_light = styled(
            Style::new().fg(Color::Rgb(205, 214, 244)),
            &[Format {
                bg: Some((0x9D, 0xD9, 0xDB)),
                ..Format::default()
            }],
        );
        assert_eq!(on_light.bg, Some(Color::Rgb(0x9D, 0xD9, 0xDB)));
        assert!(contrast(rgb(on_light.fg.unwrap()).unwrap(), (0x9D, 0xD9, 0xDB)) >= 7.0);
        let on_dark = styled(
            Style::new(),
            &[Format {
                bg: Some((10, 10, 40)),
                ..Format::default()
            }],
        );
        assert!(contrast(rgb(on_dark.fg.unwrap()).unwrap(), (10, 10, 40)) >= 7.0);
    }

    #[test]
    fn the_table_gives_the_same_format_the_same_marker() {
        let a = opener(red()).unwrap();
        assert_eq!(opener(red()), Some(a));
        assert_ne!(
            opener(Format {
                underline: true,
                ..Format::default()
            }),
            Some(a)
        );
        assert!(is_marker(a) && a != CLOSE);
    }
}
