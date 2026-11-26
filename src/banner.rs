//! The banner above the message box: the status messages and out-of-office notices of the people in the open chat, as
//! the Teams client shows them in the header over the box. Pure (the people, the width and the clock come in), so it
//! is tested without a terminal.
//!
//! Everything here is text a person wrote, drawn as plain text: no Markdown, no links, nothing that can reach the
//! terminal as a sequence (the library has made the notes plain and one line; `inline_style::strip` and whitespace
//! normalising are applied again on the way in, and every cell drawn is a grapheme of that).

use crate::inline_style;
use crate::theme::{OUT_OF_OFFICE, SUBTLE};
use crate::view::MAX_BANNER_ROWS;
use chrono::{DateTime, TimeZone, Utc};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Which kind of note a person has up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Their calendar says they are out of office (the message of the reply is the note).
    OutOfOffice,
    /// An ordinary status message.
    Status,
}

/// One person's note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub name: String,
    /// The words of the note, if there are any.
    pub text: Option<String>,
    /// When an out-of-office reply ends, if it says.
    pub until: Option<DateTime<Utc>>,
}

/// A grapheme cluster of the banner, its width and how it is drawn.
#[derive(Clone, Debug)]
struct Cell {
    text: String,
    width: usize,
    style: Style,
}

impl Cell {
    fn is_space(&self) -> bool {
        self.text == " "
    }
}

/// `s` as one line of plain text: style markers and controls out, whitespace collapsed.
fn plain(s: &str) -> String {
    teams_client::clean(&inline_style::strip(s))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The time of an out-of-office reply's end, in `zone`: the clock time on `now`'s day, else with the day and date.
fn when<Tz>(t: DateTime<Utc>, now: DateTime<Utc>, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: std::fmt::Display,
{
    let (t, today) = (t.with_timezone(zone), now.with_timezone(zone));
    if t.date_naive() == today.date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%a %-d %b %H:%M").to_string()
    }
}

/// The colour a note of this kind is drawn in, as the Teams client draws it: an ordinary status message in grey, an
/// out-of-office notice in red.
fn colour(kind: Kind) -> Color {
    match kind {
        Kind::OutOfOffice => OUT_OF_OFFICE,
        Kind::Status => SUBTLE,
    }
}

/// What `entry` says, as styled pieces: `Name: words`, or `Name is out of office until <time>: words`.
fn sentence<Tz>(entry: &Entry, now: DateTime<Utc>, zone: &Tz) -> Vec<(String, Style)>
where
    Tz: TimeZone,
    Tz::Offset: std::fmt::Display,
{
    let line = Style::new().fg(colour(entry.kind));
    let name = line.bold();
    let said = line.add_modifier(Modifier::ITALIC);
    let mut out = vec![(plain(&entry.name), name)];
    match entry.kind {
        Kind::OutOfOffice => {
            let ooo = line;
            let mut label = " is out of office".to_string();
            if let Some(until) = entry.until {
                label.push_str(&format!(" until {}", when(until, now, zone)));
            }
            out.push((label, ooo));
            if entry.text.is_some() {
                out.push((": ".to_string(), said));
            }
        }
        Kind::Status => out.push((": ".to_string(), said)),
    }
    if let Some(text) = &entry.text {
        out.push((plain(text), said));
    }
    out
}

fn cells(pieces: &[(String, Style)]) -> Vec<Cell> {
    pieces
        .iter()
        .flat_map(|(text, style)| {
            text.graphemes(true).map(|g| Cell {
                text: g.to_string(),
                width: g.width(),
                style: *style,
            })
        })
        .collect()
}

/// `cells` wrapped at spaces to `width` columns in at most `max_rows` rows; what does not fit ends the last row with `…`.
/// A word wider than a row is broken where the row ends.
fn wrap(cells: &[Cell], width: usize, max_rows: usize) -> Vec<Vec<Cell>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Cell>> = vec![Vec::new()];
    let mut used = 0;
    let mut cut = false;
    'words: for word in cells.split(Cell::is_space).filter(|w| !w.is_empty()) {
        let word_width: usize = word.iter().map(|c| c.width).sum();
        let row = rows.last_mut().expect("a row");
        if used > 0 && used + 1 + word_width <= width {
            let style = row.last().map_or_else(Style::new, |c| c.style);
            row.push(Cell {
                text: " ".to_string(),
                width: 1,
                style,
            });
            row.extend(word.iter().cloned());
            used += 1 + word_width;
            continue;
        }
        if used > 0 {
            if rows.len() == max_rows {
                cut = true;
                break;
            }
            rows.push(Vec::new());
            used = 0;
        }
        // On an empty row now; a word wider than the row goes over as many rows as it needs.
        for cell in word {
            if used > 0 && used + cell.width > width {
                if rows.len() == max_rows {
                    cut = true;
                    break 'words;
                }
                rows.push(Vec::new());
                used = 0;
            }
            rows.last_mut().expect("a row").push(cell.clone());
            used += cell.width;
        }
    }
    if cut && let Some(last) = rows.last_mut() {
        ellipsize(last, width);
    }
    rows
}

/// `row` cut so that it and an ellipsis fit in `width` columns.
fn ellipsize(row: &mut Vec<Cell>, width: usize) {
    let style = row.last().map_or_else(Style::new, |c| c.style);
    let mut used: usize = row.iter().map(|c| c.width).sum();
    while used + 1 > width {
        let Some(cell) = row.pop() else { break };
        used -= cell.width;
    }
    while row.last().is_some_and(Cell::is_space) {
        row.pop();
    }
    row.push(Cell {
        text: "…".to_string(),
        width: 1,
        style,
    });
}

/// A row of cells as a line: one column of margin, then runs of the same style.
fn line(row: &[Cell]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    let mut run = String::new();
    let mut style = Style::new();
    for cell in row {
        if cell.style != style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        style = cell.style;
        run.push_str(&cell.text);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, style));
    }
    Line::from(spans)
}

/// `row` with ` +N more` after it, the count in the colour of the line it ends; where there is no room for both the
/// person and the count, the count alone.
fn with_count(row: &[Cell], hidden: usize, kind: Kind, room: usize) -> Vec<Cell> {
    let more = format!(" +{hidden} more");
    let style = Style::new().fg(colour(kind));
    if room > more.width() + 8 {
        let mut kept = wrap(row, room - more.width(), 1).remove(0);
        kept.extend(cells(&[(more, style)]));
        kept
    } else {
        wrap(&cells(&[(more.trim_start().to_string(), style)]), room, 1).remove(0)
    }
}

/// The rows of the banner for `entries` in a pane `width` columns wide, at most `max_rows` (and never more than
/// [`MAX_BANNER_ROWS`]); none when there is nothing to say. Out-of-office notices come first. One person's note may take
/// every row there is (and ends in `…` if it still does not fit); with more people each has a row (cut with `…` when too
/// long) and those that do not fit are a count, `+N more`, at the end of the last row.
pub fn lines<Tz>(
    entries: &[Entry],
    width: usize,
    max_rows: usize,
    now: DateTime<Utc>,
    zone: &Tz,
) -> Vec<Line<'static>>
where
    Tz: TimeZone,
    Tz::Offset: std::fmt::Display,
{
    let room = width.saturating_sub(1);
    let max_rows = max_rows.min(MAX_BANNER_ROWS);
    if entries.is_empty() || room == 0 || max_rows == 0 {
        return Vec::new();
    }
    let mut ordered: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.kind == Kind::OutOfOffice)
        .collect();
    ordered.extend(entries.iter().filter(|e| e.kind == Kind::Status));
    let cells_of = |e: &Entry| cells(&sentence(e, now, zone));
    let people = ordered.len();
    let shown = people.min(max_rows);
    let hidden = people - shown;
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    if people == 1 {
        rows = wrap(&cells_of(ordered[0]), room, max_rows);
    } else {
        for (i, entry) in ordered.iter().take(shown).enumerate() {
            let own = cells_of(entry);
            if i + 1 == shown && hidden > 0 {
                rows.push(with_count(&own, hidden, entry.kind, room));
            } else {
                rows.extend(wrap(&own, room, 1));
            }
        }
    }
    rows.iter().map(|r| line(r)).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn zone() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// 12:00 on the 21st in UTC, 14:00 in the zone above.
    fn now() -> DateTime<Utc> {
        at("2026-11-21T12:00:00Z")
    }

    fn status(name: &str, text: &str) -> Entry {
        Entry {
            kind: Kind::Status,
            name: name.into(),
            text: Some(text.into()),
            until: None,
        }
    }

    fn away(name: &str, text: Option<&str>, until: Option<&str>) -> Entry {
        Entry {
            kind: Kind::OutOfOffice,
            name: name.into(),
            text: text.map(Into::into),
            until: until.map(at),
        }
    }

    fn texts(entries: &[Entry], width: usize) -> Vec<String> {
        lines(entries, width, 2, now(), &zone())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn nothing_to_say_is_no_rows() {
        assert!(texts(&[], 80).is_empty());
        assert!(texts(&[status("Ada", "hi")], 0).is_empty());
        assert!(texts(&[status("Ada", "hi")], 1).is_empty());
    }

    #[test]
    fn a_status_message_reads_name_colon_words() {
        assert_eq!(
            texts(&[status("Ada Lovelace", "In the lab")], 80),
            [" Ada Lovelace: In the lab"]
        );
    }

    #[test]
    fn out_of_office_says_until_when_and_what_the_reply_says() {
        // 15:00 UTC is 17:00 in the zone, on the day shown.
        let t = |until, text| texts(&[away("Bob", text, until)], 120);
        assert_eq!(
            t(Some("2026-11-21T15:00:00Z"), Some("Back soon.")),
            [" Bob is out of office until 17:00: Back soon."],
            "today: the time alone, in the local zone"
        );
        assert_eq!(
            t(
                Some("2026-12-05T13:00:00Z"),
                Some("Out of office until 3:00 PM, 12/05.")
            ),
            [" Bob is out of office until Sat 5 Dec 15:00: Out of office until 3:00 PM, 12/05."],
            "another day: with the date"
        );
        assert_eq!(
            t(Some("2026-12-05T13:00:00Z"), None),
            [" Bob is out of office until Sat 5 Dec 15:00"]
        );
        assert_eq!(
            t(None, Some("Gone fishing")),
            [" Bob is out of office: Gone fishing"]
        );
        assert_eq!(t(None, None), [" Bob is out of office"]);
        // The day is the local one: 23:30 UTC on the 21st is already the 22nd at +2.
        assert_eq!(
            t(Some("2026-11-21T23:30:00Z"), None),
            [" Bob is out of office until Sun 22 Nov 01:30"]
        );
    }

    #[test]
    fn out_of_office_comes_before_status_messages() {
        let rows = texts(&[status("Ada", "busy bee"), away("Bob", None, None)], 80);
        assert_eq!(rows, [" Bob is out of office", " Ada: busy bee"]);
    }

    #[test]
    fn one_person_may_use_both_rows_and_a_longer_note_ends_in_an_ellipsis() {
        let words = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let rows = texts(&[status("Ada", words)], 30);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows.iter().all(|r| r.width() <= 30), "{rows:?}");
        assert!(rows[1].ends_with('…'), "{rows:?}");
        assert!(!rows[0].ends_with('…'));
        // Short enough for two rows: no ellipsis, nothing lost.
        let rows = texts(&[status("Ada", "alpha beta gamma delta epsilon")], 30);
        assert_eq!(rows.len(), 2);
        let joined = rows.join(" ").replace("  ", " ");
        assert!(
            joined.contains("epsilon") && !joined.contains('…'),
            "{rows:?}"
        );
    }

    #[test]
    fn two_people_have_a_row_each_and_a_long_note_is_cut_to_it() {
        let rows = texts(
            &[
                status(
                    "Ada",
                    "a very long message that cannot possibly fit on one row of thirty",
                ),
                away("Bob", Some("Back Monday"), None),
            ],
            30,
        );
        assert_eq!(rows.len(), 2);
        assert!(rows[0].starts_with(" Bob is out of office"), "{rows:?}");
        assert!(rows[0].ends_with('…') || rows[0].width() <= 30);
        assert!(rows[1].ends_with('…'), "{rows:?}");
        assert!(rows.iter().all(|r| r.width() <= 30), "{rows:?}");
    }

    #[test]
    fn more_than_two_people_end_the_second_row_with_how_many_more() {
        let entries: Vec<Entry> = ["Ada", "Bob", "Cy", "Di", "Ed"]
            .iter()
            .map(|n| status(n, "here"))
            .collect();
        let rows = texts(&entries, 60);
        assert_eq!(rows, [" Ada: here", " Bob: here +3 more"]);
        let rows = texts(&entries[..3], 60);
        assert_eq!(rows, [" Ada: here", " Bob: here +1 more"]);
        // Too narrow for a name and the count: the count stays.
        let rows = texts(&entries, 12);
        assert_eq!(rows.len(), 2);
        assert!(
            rows[1].contains("+3 more") && rows[1].width() <= 12,
            "{rows:?}"
        );
    }

    #[test]
    fn with_room_for_one_row_a_single_note_is_cut_and_several_people_are_a_count() {
        let one = |entries: &[Entry], rows| -> Vec<String> {
            lines(entries, 40, rows, now(), &zone())
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        let long = status(
            "Ada",
            "alpha beta gamma delta epsilon zeta eta theta iota kappa",
        );
        let rows = one(std::slice::from_ref(&long), 1);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ends_with('…') && rows[0].width() <= 40, "{rows:?}");
        let people = [
            away("Bob", None, None),
            status("Ada", "here"),
            status("Cy", "there"),
        ];
        assert_eq!(one(&people, 1), [" Bob is out of office +2 more"]);
        assert_eq!(
            one(&people, 2),
            [" Bob is out of office", " Ada: here +1 more"]
        );
        assert!(one(&people, 0).is_empty(), "no room, no banner");
        assert_eq!(one(&people, 7).len(), 2, "never more than two rows");
    }

    #[test]
    fn an_out_of_office_line_is_red_a_status_line_grey_and_the_count_takes_the_colour_of_its_line()
    {
        use std::collections::HashSet;
        let colours = |line: &Line<'_>| -> HashSet<Color> {
            line.spans
                .iter()
                .skip(1) // the margin
                .filter_map(|s| s.style.fg)
                .collect()
        };
        let rows = lines(
            &[
                status("Ada", "here"),
                away("Bob", Some("gone"), None),
                status("Cy", "there"),
            ],
            80,
            2,
            now(),
            &zone(),
        );
        // Out of office first, then the status message, which ends in the count.
        assert_eq!(colours(&rows[0]), HashSet::from([OUT_OF_OFFICE]));
        assert_eq!(colours(&rows[1]), HashSet::from([SUBTLE]));
        assert!(rows[1].to_string().ends_with("+1 more"));
        // Two out-of-office notices and a status message: the count ends a red line, so it is red.
        let rows = lines(
            &[
                away("Bob", None, None),
                away("Dee", Some("gone"), None),
                status("Ada", "here"),
            ],
            80,
            2,
            now(),
            &zone(),
        );
        assert!(rows[1].to_string().ends_with("+1 more"), "{rows:?}");
        assert_eq!(colours(&rows[1]), HashSet::from([OUT_OF_OFFICE]));
        // The text of a note is italic and the name is bold, in either colour.
        let row = &lines(&[status("Ada", "here")], 80, 2, now(), &zone())[0];
        assert!(row.spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert!(row.spans[2].style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn nothing_is_wider_than_the_pane_at_any_width_and_the_rows_are_at_most_two() {
        let entries = [
            away(
                "Bob \u{4F60}\u{597D} Longname",
                Some("x".repeat(500).as_str()),
                Some("2026-12-05T13:00:00Z"),
            ),
            status("Ada", "one \u{2764}\u{FE0F} two \u{4F60}\u{597D} three"),
            status("Cy", "three"),
        ];
        for width in 0..90 {
            for take in 1..=3 {
                let rows = texts(&entries[..take], width);
                assert!(rows.len() <= MAX_BANNER_ROWS, "{width} {take}");
                if width >= 4 {
                    for r in &rows {
                        assert!(r.width() <= width, "{r:?} at {width}");
                    }
                }
            }
        }
    }

    #[test]
    fn hostile_text_leaves_no_control_marker_or_bidi_character() {
        let nasty =
            "\u{1b}]52;c;AAAA\u{7} \u{202E}evil\u{2066} \u{E001}\u{E002}x\ty\r\nz \u{85}\u{9b}31m";
        let rows = texts(&[status(nasty, nasty), away(nasty, Some(nasty), None)], 100);
        for r in &rows {
            for c in r.chars() {
                assert!(
                    !c.is_control()
                        && !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{E000}'..='\u{F8FF}'),
                    "{c:?} in {r:?}"
                );
            }
        }
        assert!(!rows.is_empty());
    }

    #[test]
    fn a_word_wider_than_the_row_is_broken_rather_than_lost_or_looping() {
        let rows = texts(&[status("A", &"x".repeat(100))], 20);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.width() <= 20), "{rows:?}");
        assert!(rows[1].ends_with('…'));
    }
}
