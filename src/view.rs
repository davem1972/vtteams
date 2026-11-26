//! Where everything is on screen, and how far each pane is scrolled.
//!
//! This is the state drawing needs but must not change: it is computed once per frame, before drawing (`ui::prepare`),
//! and drawing only reads it. The same rectangles are what mouse clicks are matched against, so what is clickable is
//! always exactly what was drawn. The layout itself is a pure function of the terminal size, so it is unit-tested
//! without a terminal.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;

/// Terminals narrower than this show one pane at a time.
pub const NARROW_BELOW: u16 = 80;
/// Rows assumed for the transcript and the chat list until the first frame has been laid out.
const NOMINAL_HEIGHT: usize = 20;

/// Screen rectangles for one frame. A rectangle is empty (`Rect::default()`) when its pane is not shown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Geometry {
    pub header: Rect,
    /// The whole chat-list pane (heading and rows).
    pub list: Rect,
    pub list_head: Rect,
    pub list_body: Rect,
    /// The conversation pane including its left divider.
    pub conversation: Rect,
    pub divider: Rect,
    /// The conversation pane without the divider.
    pub conversation_body: Rect,
    pub title: Rect,
    /// The transcript's text area.
    pub transcript: Rect,
    pub scrollbar: Rect,
    /// The people's status messages and out-of-office notices, directly above the message box (empty when there is
    /// none to show).
    pub banner: Rect,
    pub editor: Rect,
    /// The footer's message line, key hints and command panel, top to bottom.
    pub status: Rect,
    pub keys: Rect,
    pub panel: Rect,
}

impl Geometry {
    /// Whether `(column, row)` is inside `r`.
    pub fn contains(r: Rect, column: u16, row: u16) -> bool {
        column >= r.x
            && column < r.x.saturating_add(r.width)
            && row >= r.y
            && row < r.y.saturating_add(r.height)
    }
}

/// The most rows the banner of people's status messages and out-of-office notices takes.
pub const MAX_BANNER_ROWS: usize = 2;
/// The transcript keeps at least this many rows whatever the banner or the editor ask for.
const MIN_TRANSCRIPT_ROWS: u16 = 3;

/// Lay the screen out. `chats_focused` decides which pane a narrow terminal shows. The editor grows with its content,
/// so `editor_rows` is asked how many rows the editor needs for text `width` columns wide. `banner_rows` is given the
/// width of the conversation pane and the most rows the banner may have (at most [`MAX_BANNER_ROWS`], and what is left
/// once the title, the editor and the transcript's minimum have their rows, so a short terminal has none) and says how
/// many it takes.
pub fn layout(
    area: Rect,
    chats_focused: bool,
    banner_rows: impl FnOnce(usize, usize) -> usize,
    editor_rows: impl FnOnce(usize) -> usize,
) -> Geometry {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .areas(area);
    let list_w = (area.width * 28 / 100).clamp(28, 44);
    let (list, conversation) = if area.width < NARROW_BELOW {
        if chats_focused {
            (body, Rect::default())
        } else {
            (Rect::default(), body)
        }
    } else {
        let [list, right] =
            Layout::horizontal([Constraint::Length(list_w), Constraint::Min(20)]).areas(body);
        (list, right)
    };
    let (list_head, list_body) = if list == Rect::default() {
        (Rect::default(), Rect::default())
    } else {
        let [head, rows] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(list);
        (head, rows)
    };

    let [status, keys, panel] = Layout::vertical([Constraint::Length(1); 3]).areas(footer);
    let mut g = Geometry {
        header,
        list,
        list_head,
        list_body,
        conversation,
        status,
        keys,
        panel,
        ..Geometry::default()
    };
    if conversation != Rect::default() {
        let [divider, conversation_body] =
            Layout::horizontal([Constraint::Length(1), Constraint::Min(1)]).areas(conversation);
        // The editor draws one blank column of padding inside its border, and its text wraps one column earlier.
        let text_width = (conversation_body.width.saturating_sub(2) as usize)
            .saturating_sub(1)
            .max(1);
        let max_editor = (conversation_body.height / 3).max(3);
        let editor_h = (u16::try_from(editor_rows(text_width))
            .unwrap_or(u16::MAX)
            .saturating_add(2))
        .clamp(3, max_editor);
        // What the title, the editor and the transcript's minimum leave over is all the banner can have.
        let room = conversation_body
            .height
            .saturating_sub(2 + editor_h + MIN_TRANSCRIPT_ROWS);
        let most = usize::from(room).min(MAX_BANNER_ROWS);
        let banner_h = u16::try_from(banner_rows(conversation_body.width as usize, most).min(most))
            .unwrap_or(0);
        let [title, trans, banner, editor] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(MIN_TRANSCRIPT_ROWS),
            Constraint::Length(banner_h),
            Constraint::Length(editor_h),
        ])
        .areas(conversation_body);
        let [text_area, scrollbar] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(trans);
        g.divider = divider;
        g.conversation_body = conversation_body;
        g.title = title;
        g.transcript = text_area;
        g.scrollbar = scrollbar;
        g.banner = if banner_h > 0 {
            banner
        } else {
            Rect::default()
        };
        g.editor = editor;
    }
    g
}

/// What is on a row of the chat list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRow {
    /// A row of the chat at this position (each chat takes two).
    Chat(usize),
    /// The thin rule between the pinned favourites and the other chats.
    Divider,
}

/// What is on `row` (counted from the top of the list) when it starts at chat position `offset` and the first `pinned`
/// chats are favourites. Each chat takes two rows; a one-row divider sits between the last favourite and the first of
/// the others, and is drawn only when both are on screen.
pub fn list_row(offset: usize, pinned: usize, row: usize) -> ListRow {
    match pinned_row(2, offset, pinned, row) {
        Some(pos) => ListRow::Chat(pos),
        None => ListRow::Divider,
    }
}

/// How many chats fit in `height` rows from position `offset`, counting the divider (at least one).
pub fn list_capacity(height: usize, offset: usize, pinned: usize) -> usize {
    pinned_capacity(2, height, offset, pinned)
}

/// What is on `row` of a list whose entries each take `per` rows, when it starts at entry `offset` and the first
/// `pinned` entries are favourites: the entry's position, or `None` for the one-row divider that sits between the last
/// favourite and the first of the others and is drawn only when both are on screen. The chat list and the teams list
/// are one definition of this.
fn pinned_row(per: usize, offset: usize, pinned: usize, row: usize) -> Option<usize> {
    let (mut at, mut pos) = (0, offset);
    loop {
        if pinned > 0 && pos == pinned && pos > offset {
            if row == at {
                return None;
            }
            at += 1;
        }
        if row < at + per {
            return Some(pos);
        }
        at += per;
        pos += 1;
    }
}

/// How many entries of `per` rows fit in `height` rows from entry `offset`, counting the divider (at least one).
fn pinned_capacity(per: usize, height: usize, offset: usize, pinned: usize) -> usize {
    let (mut used, mut shown, mut pos) = (0, 0, offset);
    loop {
        let divider = usize::from(pinned > 0 && pos == pinned && pos > offset);
        if used + divider + per > height || shown >= height {
            return shown.max(1);
        }
        used += divider + per;
        shown += 1;
        pos += 1;
    }
}

/// The two lists the left pane can show: the recent chats, or the teams with their channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Chats,
    Teams,
}

/// Where the tab labels sit in the list's heading row: for labels `widths` columns wide, the columns (counted from the
/// left edge of the heading) each takes, a space of padding either side of the label included, one blank column between
/// two. Drawing and clicking both use this, so what is clickable is what was drawn.
pub fn tab_cells(widths: &[usize]) -> Vec<std::ops::Range<usize>> {
    let mut at = 0;
    widths
        .iter()
        .map(|w| {
            let cell = at..at + w + 2;
            at = cell.end + 1;
            cell
        })
        .collect()
}

/// Which label (an index into `widths`) the heading's column `column` is over, if any.
pub fn tab_at(widths: &[usize], column: usize) -> Option<usize> {
    tab_cells(widths).iter().position(|c| c.contains(&column))
}

/// What is on a screen row of the teams list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamsLine {
    /// The list row at this position (one screen row each).
    Row(usize),
    /// The thin rule between the favourite channels and the teams.
    Divider,
}

/// How many rows of the teams list fit in `height` screen rows when it starts at row `offset` and the first `pinned`
/// rows are favourites (at least one). Every row takes exactly one screen row, and a divider takes one more where it is
/// drawn, as in the chat list.
pub fn teams_capacity(height: usize, offset: usize, pinned: usize) -> usize {
    pinned_capacity(1, height, offset, pinned)
}

/// What is drawn at screen row `row` (counted from the top of the list) when the list starts at row `offset`.
pub fn teams_row(offset: usize, pinned: usize, row: usize) -> TeamsLine {
    match pinned_row(1, offset, pinned, row) {
        Some(pos) => TeamsLine::Row(pos),
        None => TeamsLine::Divider,
    }
}

/// Where the teams list of `len` rows should start so that row `selected` is on screen, moving as little as it can from
/// `offset` and never leaving the list ending short of the bottom.
pub fn teams_scroll(
    offset: usize,
    selected: usize,
    height: usize,
    len: usize,
    pinned: usize,
) -> usize {
    let mut offset = offset;
    if selected < offset {
        offset = selected;
    }
    // The divider takes a screen row, so how many rows fit depends on where the list starts.
    while selected >= offset + teams_capacity(height, offset, pinned) {
        offset += 1;
    }
    while offset > 0 && offset + teams_capacity(height, offset - 1, pinned) > len {
        offset -= 1;
    }
    offset.min(len.saturating_sub(1))
}

/// One row of the editor as drawn: a line of the text, or the part of a long one that fits the width.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowSpan {
    /// The line of the text this row shows (a row of the editor's buffer).
    pub line: usize,
    /// The index of the first character of the line that this row shows.
    pub first: usize,
    /// The display column each character of the row starts at (a character that takes no room shares its
    /// predecessor's).
    pub cols: Vec<usize>,
    /// This is the last row of its line, so the cursor can stand after the last character.
    pub ends_line: bool,
}

/// The editor's text as drawn: wrapped, coloured rows and where the cursor falls among them.
#[derive(Debug, Clone, Default)]
pub struct EditorRows {
    pub lines: Vec<Line<'static>>,
    /// Row and column within `lines`.
    pub cursor: (usize, usize),
    /// What each of `lines` shows of the text, so a move up or down can go by a row of the screen and not of the text.
    pub spans: Vec<RowSpan>,
    /// The editor's `version()` the rows were made for: they describe what is on screen only while it still matches.
    pub version: u64,
}

/// Everything the editor's rows are made from: the rows for one key are the rows for any other equal to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowsKey {
    /// The editor's `version()`: the text.
    pub version: u64,
    /// The columns the text wraps at.
    pub width: usize,
    /// Where the cursor is, and what is selected: they decide the cell the cursor stands on, which lines carry the
    /// selection's colours and whether a line that exactly fills a row needs another for the cursor after its end.
    pub cursor: (usize, usize),
    pub selection: Option<crate::editor::Selection>,
}

/// The rows of one line of the text, wrapped, with neither a cursor nor a selection in it.
#[derive(Debug, Clone, Default)]
pub struct PlainRows {
    pub lines: Vec<Line<'static>>,
    pub spans: Vec<RowSpan>,
}

/// What `ui::prepare` keeps between frames so that the editor's rows are not made again when nothing they depend on has
/// changed (see `ui::refresh_editor_rows`).
#[derive(Debug, Default)]
pub struct RowsCache {
    /// The text version and width `plain` was made for.
    pub plain_for: Option<(u64, usize)>,
    /// The rows of each line of the text, without the cursor or the selection.
    pub plain: Vec<PlainRows>,
    /// What `View::editor_rows` was made from; `None` before the first frame.
    pub made_for: Option<RowsKey>,
    /// How many times `plain` was made again, and `View::editor_rows` made from it (for tests that say what was not
    /// redone).
    #[cfg(test)]
    pub plain_builds: usize,
    #[cfg(test)]
    pub assemblies: usize,
}

impl EditorRows {
    /// The place in the text (line, character) that display row `row` reaches at display column `x`: the character
    /// that starts at or before `x` (the first of a cluster), or the end of the row when `x` is past it, which on the
    /// last row of a line is after the last character and on any other the last one (the next character is on the
    /// next row).
    pub fn char_at(&self, row: usize, x: usize) -> Option<(usize, usize)> {
        let span = self.spans.get(row)?;
        let len = span.cols.len();
        // The first character of the cluster that starts at the nearest column at or before `x`.
        let index = span
            .cols
            .iter()
            .copied()
            .filter(|c| *c <= x)
            .max()
            .and_then(|col| span.cols.iter().position(|c| *c == col))
            .unwrap_or(0);
        let past_the_last = span.cols.last().is_none_or(|last| x > *last);
        let idx = if span.ends_line && past_the_last {
            span.first + len
        } else {
            span.first + index.min(len.saturating_sub(1))
        };
        Some((span.line, idx))
    }
}

/// Per-frame state that drawing reads.
#[derive(Debug)]
pub struct View {
    /// The whole screen, as of the last frame laid out.
    pub screen: Rect,
    pub geometry: Geometry,
    /// The first chat shown in the list.
    pub list_offset: usize,
    /// The first row shown in the teams list.
    pub team_offset: usize,
    /// The list was scrolled with the wheel (or a trackpad) while this was open (the open chat's id, `None` for a
    /// channel): until the selection moves, the list stays where it was scrolled to instead of following it.
    pub list_wheeled: Option<Option<String>>,
    /// The same for the teams list, with the cursor's row.
    pub team_wheeled: Option<usize>,
    /// The first editor row shown.
    pub editor_top: usize,
    pub editor_rows: EditorRows,
    /// Coloured editor cells, cached by `Editor::version()`; the version starts out matching no editor.
    pub editor_cells: (u64, Vec<Vec<(char, Style)>>),
    /// What `editor_rows` was made from, and the rows of each line without the cursor and selection.
    pub editor_cache: RowsCache,
    /// The rows of the banner above the message box, as `ui::prepare` laid them out for the last frame (as many as
    /// `geometry.banner` has).
    pub banner: Vec<Line<'static>>,
    /// The links marked as terminal hyperlinks this frame, and last frame's (whose cells are written again where no link
    /// is now), as `ui::prepare` planned them.
    pub links: Vec<crate::hyperlink::Spot>,
    pub links_before: Vec<crate::hyperlink::Spot>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            screen: Rect::default(),
            geometry: Geometry::default(),
            list_offset: 0,
            team_offset: 0,
            list_wheeled: None,
            team_wheeled: None,
            editor_top: 0,
            editor_rows: EditorRows::default(),
            editor_cells: (u64::MAX, Vec::new()),
            editor_cache: RowsCache::default(),
            banner: Vec::new(),
            links: Vec::new(),
            links_before: Vec::new(),
        }
    }
}

impl View {
    /// Rows in the transcript viewport (a nominal 20 until the first frame is laid out).
    pub fn transcript_height(&self) -> usize {
        match self.geometry.transcript.height as usize {
            0 => NOMINAL_HEIGHT,
            h => h,
        }
    }

    /// Rows in the chat list (a nominal 20 until the first frame is laid out).
    pub fn list_height(&self) -> usize {
        match self.geometry.list_body.height as usize {
            0 => NOMINAL_HEIGHT,
            h => h,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    #[test]
    fn a_wide_terminal_shows_both_panes_side_by_side() {
        let g = layout(area(130, 46), true, |_, _| 0, |_| 1);
        assert!(g.list != Rect::default() && g.conversation != Rect::default());
        assert_eq!(g.list.x, 0);
        assert_eq!(
            g.conversation.x,
            g.list.x + g.list.width,
            "the conversation starts where the list ends"
        );
        assert_eq!(g.list.width, 36, "28% of 130, within 28..=44");
        assert_eq!(
            (
                g.header.height,
                g.status.height,
                g.keys.height,
                g.panel.height
            ),
            (2, 1, 1, 1)
        );
        assert_eq!(g.panel.y, 45, "the command panel is the bottom row");
    }

    #[test]
    fn a_narrow_terminal_shows_one_pane_chosen_by_focus() {
        let chats = layout(area(60, 30), true, |_, _| 0, |_| 1);
        assert!(
            chats.list != Rect::default()
                && chats.conversation == Rect::default()
                && chats.transcript == Rect::default()
        );
        let conversation = layout(area(60, 30), false, |_, _| 0, |_| 1);
        assert!(conversation.list == Rect::default() && conversation.list_body == Rect::default());
        assert!(conversation.transcript != Rect::default());
    }

    #[test]
    fn the_panes_tile_the_body_without_overlap_and_inside_the_screen() {
        for (w, h) in [(130, 46), (80, 24), (79, 24), (40, 12), (20, 6)] {
            for focus in [true, false] {
                let screen = area(w, h);
                let g = layout(screen, focus, |_, _| 0, |_| 3);
                for r in [
                    g.header,
                    g.list,
                    g.conversation,
                    g.transcript,
                    g.editor,
                    g.title,
                    g.panel,
                    g.status,
                    g.keys,
                    g.scrollbar,
                ] {
                    assert!(
                        r.right() <= screen.right() && r.bottom() <= screen.bottom(),
                        "{r:?} outside {screen:?} at {w}x{h}"
                    );
                }
                if g.list != Rect::default() && g.conversation != Rect::default() {
                    assert!(
                        g.list.right() <= g.conversation.x,
                        "list and conversation overlap at {w}x{h}"
                    );
                }
                if g.conversation != Rect::default() {
                    assert!(
                        g.title.bottom() <= g.transcript.y && g.transcript.bottom() <= g.editor.y,
                        "vertical order at {w}x{h}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_editor_grows_with_its_content_up_to_a_third_of_the_pane() {
        let height = |rows| {
            layout(area(130, 60), false, |_, _| 0, move |_| rows)
                .editor
                .height
        };
        assert_eq!(height(1), 3, "one row plus the border");
        assert_eq!(height(5), 7);
        assert_eq!(
            height(500),
            (60 - 5) / 3,
            "capped at a third of the conversation pane"
        );
        let g = layout(area(130, 60), false, |_, _| 0, |_| 500);
        assert!(g.transcript.height >= 3, "the transcript always keeps room");
    }

    #[test]
    fn the_banner_sits_between_the_transcript_and_the_message_box_and_takes_the_rows_from_the_transcript()
     {
        let without = layout(area(130, 46), false, |_, _| 0, |_| 1);
        assert_eq!(without.banner, Rect::default());
        for rows in 1..=2u16 {
            let g = layout(area(130, 46), false, move |_, _| rows as usize, |_| 1);
            assert_eq!(g.banner.height, rows);
            assert_eq!(
                g.transcript.bottom(),
                g.banner.y,
                "right under the transcript"
            );
            assert_eq!(g.banner.bottom(), g.editor.y, "right above the message box");
            assert_eq!(
                (g.banner.x, g.banner.width),
                (g.conversation_body.x, g.conversation_body.width),
                "across the whole pane"
            );
            assert_eq!(
                g.transcript.height + rows,
                without.transcript.height,
                "the transcript gives up exactly the rows the banner takes"
            );
            assert_eq!(
                (g.transcript.x, g.transcript.width, g.editor, g.title),
                (
                    without.transcript.x,
                    without.transcript.width,
                    without.editor,
                    without.title
                ),
                "nothing else moves, and the transcript keeps its width (so its wrapping is unchanged)"
            );
        }
        let asked = std::cell::Cell::new(0);
        let g = layout(
            area(130, 40),
            false,
            |w, _| {
                asked.set(w);
                1
            },
            |_| 1,
        );
        assert_eq!(asked.get(), g.conversation_body.width as usize);
    }

    #[test]
    fn more_than_two_rows_are_never_given_and_a_short_terminal_shows_none() {
        let g = layout(area(130, 46), false, |_, _| 9, |_| 1);
        assert_eq!(g.banner.height as usize, MAX_BANNER_ROWS);
        // Too short for the title, the editor and the transcript's three rows: no room for the banner.
        let g = layout(area(130, 12), false, |_, _| 2, |_| 1);
        assert_eq!(g.banner, Rect::default());
        assert!(g.transcript.height >= 3);
    }

    #[test]
    fn the_editor_is_asked_for_rows_at_the_width_it_will_wrap_to() {
        let mut asked = 0;
        let g = layout(
            area(130, 40),
            false,
            |_, _| 0,
            |w| {
                asked = w;
                1
            },
        );
        assert_eq!(
            asked,
            (g.conversation_body.width as usize - 2) - 1,
            "inside the border and one column of padding"
        );
    }

    #[test]
    fn the_divider_takes_one_row_between_the_favourites_and_the_rest() {
        // Three favourites: rows 0-5 are chats 0-2, row 6 the divider, then chat 3 from row 7.
        let rows: Vec<ListRow> = (0..10).map(|r| list_row(0, 3, r)).collect();
        assert_eq!(
            rows,
            [
                ListRow::Chat(0),
                ListRow::Chat(0),
                ListRow::Chat(1),
                ListRow::Chat(1),
                ListRow::Chat(2),
                ListRow::Chat(2),
                ListRow::Divider,
                ListRow::Chat(3),
                ListRow::Chat(3),
                ListRow::Chat(4)
            ]
        );
        // Scrolled to the first of the others there is nothing above it to divide it from; scrolled past, nor.
        assert_eq!(list_row(3, 3, 0), ListRow::Chat(3));
        assert_eq!(list_row(4, 3, 2), ListRow::Chat(5));
        // Scrolled to the last favourite the divider follows it at once.
        assert_eq!(list_row(2, 3, 2), ListRow::Divider);
        assert_eq!(list_row(2, 3, 3), ListRow::Chat(3));
        // No favourites, no divider.
        assert!((0..20).all(|r| list_row(0, 0, r) != ListRow::Divider));
        assert_eq!(list_row(5, 0, 3), ListRow::Chat(6));
    }

    #[test]
    fn the_capacity_counts_the_divider_and_is_never_zero() {
        assert_eq!(list_capacity(10, 0, 0), 5);
        assert_eq!(
            list_capacity(10, 0, 3),
            4,
            "6 rows, the divider, then one chat: 9 of 10"
        );
        assert_eq!(
            list_capacity(7, 0, 3),
            3,
            "the divider and a chat would need 9"
        );
        assert_eq!(
            list_capacity(10, 3, 3),
            5,
            "no divider from the first of the others on"
        );
        assert_eq!(list_capacity(1, 0, 3), 1);
        assert_eq!(list_capacity(0, 0, 3), 1);
        for height in 0..60 {
            for pinned in 0..6 {
                for offset in 0..8 {
                    let n = list_capacity(height, offset, pinned);
                    // What fits is exactly what list_row would put on screen.
                    let shown: std::collections::BTreeSet<usize> = (0..height)
                        .filter_map(|r| match list_row(offset, pinned, r) {
                            ListRow::Chat(p) => Some(p),
                            ListRow::Divider => None,
                        })
                        .collect();
                    let whole = shown
                        .iter()
                        .filter(|p| {
                            let rows: Vec<usize> = (0..height)
                                .filter(|r| list_row(offset, pinned, *r) == ListRow::Chat(**p))
                                .collect();
                            rows.len() == 2
                        })
                        .count();
                    assert!(
                        n == whole.max(1),
                        "height {height} pinned {pinned} offset {offset}: {n} vs {whole}"
                    );
                }
            }
        }
    }

    #[test]
    fn tab_cells_are_padded_labels_with_one_blank_column_between() {
        // "CHATS 6" (7 wide) and "TEAMS" (5): each with a space either side, a column apart.
        let cells = tab_cells(&[7, 5]);
        assert_eq!(cells, [0..9, 10..17]);
        assert_eq!(tab_at(&[7, 5], 0), Some(0));
        assert_eq!(tab_at(&[7, 5], 8), Some(0));
        assert_eq!(
            tab_at(&[7, 5], 9),
            None,
            "the blank column belongs to neither"
        );
        assert_eq!(tab_at(&[7, 5], 10), Some(1));
        assert_eq!(tab_at(&[7, 5], 16), Some(1));
        assert_eq!(tab_at(&[7, 5], 17), None);
        assert!(tab_cells(&[]).is_empty());
    }

    #[test]
    fn the_teams_list_scrolls_the_selection_into_view_and_what_fits_is_what_is_drawn() {
        assert_eq!(teams_capacity(0, 0, 0), 1);
        assert_eq!(teams_capacity(9, 0, 0), 9);
        assert_eq!(
            teams_capacity(9, 0, 3),
            8,
            "three favourites, the divider, then four more"
        );
        assert_eq!(teams_capacity(9, 3, 3), 9, "none from the first team on");
        for pinned in 0..6 {
            for height in 0..30 {
                for len in 0..40 {
                    for selected in 0..len.max(1) {
                        for offset in [0, 3, 17, 60] {
                            let at = teams_scroll(offset, selected, height, len, pinned);
                            let cap = teams_capacity(height, at, pinned);
                            assert!(
                                at <= selected && selected < at + cap,
                                "row {selected} of {len} ({pinned} favourites) is on screen from {at} with room for {cap}"
                            );
                            assert!(
                                at == 0 || at + teams_capacity(height, at - 1, pinned) <= len,
                                "the list does not end short of the bottom"
                            );
                            // The screen rows from `at` are exactly the rows `teams_row` names, in order, and the
                            // divider is on one only where it separates a favourite from a team that is shown.
                            let lines: Vec<TeamsLine> = (0..height.max(1))
                                .map(|r| teams_row(at, pinned, r))
                                .collect();
                            let rows: Vec<usize> = lines
                                .iter()
                                .filter_map(|l| match l {
                                    TeamsLine::Row(p) => Some(*p),
                                    TeamsLine::Divider => None,
                                })
                                .take(cap)
                                .collect();
                            assert_eq!(rows, (at..at + cap).collect::<Vec<_>>());
                            let drawn_divider = pinned > at && pinned < at + cap;
                            let before_team = lines
                                .iter()
                                .position(|l| *l == TeamsLine::Divider)
                                .filter(|_| drawn_divider);
                            assert_eq!(
                                before_team,
                                drawn_divider.then(|| pinned - at),
                                "the divider is the screen row just before the first team"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_teams_list_has_a_divider_row_after_the_favourites_only_where_both_show() {
        use TeamsLine::{Divider, Row};
        let screen = |offset, pinned, n| -> Vec<TeamsLine> {
            (0..n).map(|r| teams_row(offset, pinned, r)).collect()
        };
        assert_eq!(screen(0, 2, 5), [Row(0), Row(1), Divider, Row(2), Row(3)]);
        assert_eq!(
            screen(2, 2, 3),
            [Row(2), Row(3), Row(4)],
            "from the first team on"
        );
        assert_eq!(screen(1, 2, 3), [Row(1), Divider, Row(2)]);
        assert_eq!(
            screen(0, 0, 3),
            [Row(0), Row(1), Row(2)],
            "no favourites, no divider"
        );
    }

    #[test]
    fn hit_testing_matches_the_edges() {
        let r = Rect::new(5, 3, 10, 4);
        assert!(Geometry::contains(r, 5, 3) && Geometry::contains(r, 14, 6));
        assert!(
            !Geometry::contains(r, 15, 3)
                && !Geometry::contains(r, 5, 7)
                && !Geometry::contains(r, 4, 3)
        );
        assert!(
            !Geometry::contains(Rect::default(), 0, 0),
            "an empty rectangle contains nothing"
        );
    }

    #[test]
    fn heights_are_nominal_until_a_frame_has_been_laid_out() {
        let mut view = View::default();
        assert_eq!((view.transcript_height(), view.list_height()), (20, 20));
        view.geometry = layout(area(130, 46), true, |_, _| 0, |_| 1);
        assert_eq!(
            view.transcript_height(),
            view.geometry.transcript.height as usize
        );
        assert_eq!(view.list_height(), view.geometry.list_body.height as usize);
    }

    proptest::proptest! {
        #[test]
        fn a_banner_of_any_size_on_any_terminal_stays_inside_without_overlap_and_leaves_the_transcript_its_rows(
            w in 0u16..300, h in 0u16..120, focus in proptest::bool::ANY,
            banner in 0usize..6, rows in 0usize..100,
        ) {
            let screen = area(w, h);
            let g = layout(screen, focus, |_, _| banner, |_| rows);
            let plain = layout(screen, focus, |_, _| 0, |_| rows);
            for r in [g.header, g.list, g.conversation, g.title, g.transcript, g.banner, g.editor, g.panel] {
                proptest::prop_assert!(r.right() <= screen.right() && r.bottom() <= screen.bottom(), "{r:?} outside {screen:?}");
            }
            proptest::prop_assert!(g.banner.height as usize <= MAX_BANNER_ROWS);
            if g.banner != Rect::default() {
                proptest::prop_assert!(g.transcript.height >= 3, "the transcript keeps its minimum: {g:?}");
                proptest::prop_assert!(
                    g.title.bottom() <= g.transcript.y && g.transcript.bottom() <= g.banner.y && g.banner.bottom() <= g.editor.y,
                    "vertical order: {g:?}"
                );
                proptest::prop_assert_eq!(g.transcript.height + g.banner.height, plain.transcript.height);
                proptest::prop_assert_eq!((g.editor, g.title), (plain.editor, plain.title));
            } else {
                proptest::prop_assert_eq!(&g, &plain, "no banner, no change");
            }
        }

        #[test]
        fn any_terminal_size_lays_out_inside_the_screen(w in 0u16..300, h in 0u16..120, focus in proptest::bool::ANY, rows in 0usize..100) {
            let screen = area(w, h);
            let g = layout(screen, focus, |_, _| 0, |_| rows);
            for r in [g.header, g.list, g.list_body, g.conversation, g.transcript, g.editor, g.panel, g.status, g.keys] {
                proptest::prop_assert!(r.right() <= screen.right() && r.bottom() <= screen.bottom(), "{r:?} outside {screen:?}");
            }
        }
    }
}
