//! The message box's colouring and rows: that what is kept between frames is what a fresh build would give, that it is
//! kept when nothing changed, and measurements.

use super::*;
use crate::app::App;
use crate::demo::DemoBackend;
use crate::editor::{Editor, EditorAction};
use crate::view::RowSpan;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn app_with(keymap: Keymap) -> App {
    let (mut app, _) = App::for_test(Arc::new(DemoBackend::instant()));
    app.set_keymap(keymap);
    app.focus = Focus::Editor;
    app
}

/// A draft of about `chars` characters: lines of prose with the inline formatting people type.
fn draft(chars: usize) -> String {
    let line =
        "a line of the draft with `code`, **bold**, _italic_ and a [link](https://example.com/x)\n";
    line.repeat(chars / line.len() + 1)[..chars].to_string()
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

/// A fixed pseudo-random sequence, so that a failure can be reproduced.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n
    }
}

// -- the colouring --------------------------------------------------------------------------------------------------

/// The colouring of a line as it was written before a failed search for a closer was remembered: every opener scans
/// ahead on its own. Slow on some text and the standard to match.
fn reference_md_cells(line: &str) -> Vec<Cell> {
    let chars: Vec<char> = line.chars().collect();
    let base = Style::new().fg(TEXT);
    let mut cells: Vec<Cell> = chars.iter().map(|&c| (c, base)).collect();
    let trimmed = line.trim_start();
    let indent = chars.len() - trimmed.chars().count();

    let paint = |cells: &mut Vec<Cell>, from: usize, to: usize, f: &dyn Fn(Style) -> Style| {
        for c in cells.iter_mut().take(to.min(chars.len())).skip(from) {
            c.1 = f(c.1);
        }
    };

    if trimmed.starts_with('#') && trimmed.trim_start_matches('#').starts_with(' ') {
        paint(&mut cells, 0, chars.len(), &|s| {
            s.fg(LINK).add_modifier(Modifier::BOLD)
        });
        return cells;
    }
    if trimmed.starts_with('>') {
        paint(&mut cells, 0, chars.len(), &|s| {
            s.fg(SUBTLE).add_modifier(Modifier::ITALIC)
        });
        paint(&mut cells, indent, indent + 1, &|s| s.fg(CODE_LABEL));
        return cells;
    }
    for marker in ["- ", "* ", "+ "] {
        if trimmed.starts_with(marker) {
            paint(&mut cells, indent, indent + 1, &|s| {
                s.fg(WARN).add_modifier(Modifier::BOLD)
            });
        }
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && trimmed[digits..].starts_with(". ") {
        paint(&mut cells, indent, indent + digits + 1, &|s| {
            s.fg(WARN).add_modifier(Modifier::BOLD)
        });
    }

    let mut i = 0;
    while i < chars.len() {
        let rest_starts = |pat: &str| {
            chars[i..]
                .iter()
                .take(pat.chars().count())
                .collect::<String>()
                == pat
        };
        let find_close = |from: usize, pat: &str| -> Option<usize> {
            let p: Vec<char> = pat.chars().collect();
            (from..chars.len().saturating_sub(p.len() - 1))
                .find(|&j| chars[j..j + p.len()] == p[..])
        };
        if chars[i] == '`'
            && let Some(j) = find_close(i + 1, "`")
        {
            paint(&mut cells, i, j + 1, &|s| s.fg(WARN).bg(CODE_BG));
            i = j + 1;
        } else if (rest_starts("**") || rest_starts("~~"))
            && let Some(j) = find_close(i + 2, &chars[i..i + 2].iter().collect::<String>())
        {
            let strike = chars[i] == '~';
            paint(&mut cells, i, j + 2, &|s| {
                if strike {
                    s.fg(DIM).add_modifier(Modifier::CROSSED_OUT)
                } else {
                    s.fg(WHITE).add_modifier(Modifier::BOLD)
                }
            });
            i = j + 2;
        } else if (chars[i] == '*' || chars[i] == '_')
            && chars.get(i + 1).is_some_and(|c| !c.is_whitespace())
            && (i == 0 || !chars[i - 1].is_alphanumeric())
            && let Some(j) = find_close(i + 1, &chars[i].to_string())
        {
            paint(&mut cells, i, j + 1, &|s| {
                s.fg(ACCENT).add_modifier(Modifier::ITALIC)
            });
            i = j + 1;
        } else if chars[i] == '['
            && let Some(j) = find_close(i + 1, "](")
            && let Some(k) = find_close(j + 2, ")")
        {
            paint(&mut cells, i, j + 1, &|s| {
                s.fg(LINK).add_modifier(Modifier::UNDERLINED)
            });
            paint(&mut cells, j + 1, k + 1, &|s| s.fg(DIM));
            i = k + 1;
        } else if chars[i] == '@' && (i == 0 || !chars[i - 1].is_alphanumeric()) {
            let end = (i + 1..chars.len())
                .find(|&j| !(chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '.'))
                .unwrap_or(chars.len());
            if end > i + 1 {
                paint(&mut cells, i, end, &|s| {
                    s.fg(CODE_LABEL).add_modifier(Modifier::BOLD)
                });
            }
            i = end.max(i + 1);
        } else if chars[i] == ':'
            && let Some(j) = find_close(i + 1, ":")
            && j > i + 1
            && j - i < 24
            && chars[i + 1..j]
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-'))
        {
            paint(&mut cells, i, j + 1, &|s| s.fg(WARN));
            i = j + 1;
        } else {
            i += 1;
        }
    }
    cells
}

fn same_colouring(line: &str) {
    assert_eq!(
        md_cells(line),
        reference_md_cells(line),
        "a different colouring for {line:?}"
    );
}

#[test]
fn a_remembered_closer_colours_every_hand_written_line_as_the_plain_scan_does() {
    for line in [
        "",
        "plain words",
        "# heading with [a](b) and `code`",
        "> a quote with **bold**",
        "- item with `code` and _it_",
        "12. numbered **bold** line",
        "see [the docs](https://example.com/a_b_c) and [more](https://example.com/(x))",
        "![an image](https://example.com/p.png) after",
        "[unclosed link and then [a](b) works",
        "[a](b [c](d) e) f",
        "[a](b",
        "[[[[ no closer ]( at all",
        "[ ]( [ ]( [ ](",
        "[a] (b) [c](d)",
        "](](](",
        "[x](y)) )) ))",
        "![ ![ ![ [ [ [",
        "`code` and `more` and `unclosed",
        "`` `` ` ``",
        "**bold** and **unclosed",
        "**a** ** b ** **",
        "~~strike~~ and ~~ unclosed and ~~two~~",
        "~~**both**~~ and **~~other~~**",
        "*one* and *two and _three_ and _four",
        "*a *b *c d* e*",
        "_a_b_ _c_ snake_case_name _",
        "* not italic * but *this* is",
        "**\\*escaped\\** and \\[x\\](y)",
        ":smile: and :not an emoji: and ::: and :a: :b:",
        ":averyveryveryveryveryverylongname: :ok:",
        "@ada and @bob.smith and mail@host and @",
        "@@a @_b @.c",
        "[\u{65e5}\u{672c}](x) and `\u{1f600}` and **\u{e9}** and _\u{1f600}_",
        "\u{1f600}[\u{1f600}](\u{1f600}) \u{4e2d}\u{6587}*\u{4e2d}*",
        "a\\`b` `c\\`",
        "  indented [a](b) and `c`",
        "\t[tab](x)",
        "[a](b)[c](d)[e](f)",
        "*[a](b)* and [*a*](b) and [`a`](b)",
        "`[a](b)` and **[a](b)** and [a](`b`)",
    ] {
        same_colouring(line);
    }
}

#[test]
fn a_remembered_closer_colours_random_lines_as_the_plain_scan_does() {
    let alphabet: Vec<char> = "[]()!\\`*_ab \n~:@>-#1.\t\u{e9}\u{4e2d}\u{1f600}"
        .chars()
        .collect();
    let mut rng = Lcg(7);
    for _ in 0..20_000 {
        let len = rng.below(48);
        let line: String = (0..len)
            .map(|_| alphabet[rng.below(alphabet.len())])
            .collect();
        // A line of the text holds no line break, but the colouring must not depend on that either.
        same_colouring(&line);
    }
}

#[test]
fn the_search_for_a_closer_is_made_once_for_all_the_openers_that_would_have_made_it() {
    let n = 32_000;
    for piece in [
        "[", "![", "[ ](", "](", "` ", "** ", "*a ", "_a ", "~~ ", ": ", "@a", "[a](b ", "[a]( ",
    ] {
        let line = piece.repeat(n / piece.chars().count());
        POSITIONS_LOOKED_AT.with(|c| c.set(0));
        let cells = md_cells(&line);
        let looked = POSITIONS_LOOKED_AT.with(std::cell::Cell::get);
        assert_eq!(cells.len(), line.chars().count());
        // Seven patterns, each searched at most once across the line (a few places twice where a search resumes).
        assert!(
            looked <= 8 * n,
            "{piece:?}: {looked} places looked at for {n} characters"
        );
    }
}

// -- the rows -------------------------------------------------------------------------------------------------------

/// The rows as they were made before neighbouring characters of one colour shared a span, with every line laid out on
/// each call: one span for each character, the cursor and selection read for every line. The standard to match.
fn reference_rows(ed: &Editor, cells: &[Vec<Cell>], width: usize) -> EditorRows {
    let (cr, cc) = ed.cursor();
    let sel = ed.selection();
    let sel_style = |row: usize, col: usize, len: usize| -> bool {
        match sel {
            Some(((sr, sc), (er, ec), linewise)) => {
                if row < sr || row > er {
                    false
                } else if linewise {
                    true
                } else {
                    (row > sr || col >= sc) && (row < er || col <= ec) && col < len.max(1)
                }
            }
            None => false,
        }
    };

    let width = width.max(4);
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row_spans: Vec<RowSpan> = Vec::new();
    let mut cursor = (0usize, 0usize);
    for (r, line_cells) in cells.iter().enumerate() {
        let len = line_cells.len();
        let col_widths =
            grapheme_col_widths(&line_cells.iter().map(|&(c, _)| c).collect::<Vec<char>>());
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        let mut first = 0usize;
        let mut cols: Vec<usize> = Vec::new();
        for col in 0..=len {
            let (ch, mut style) = line_cells
                .get(col)
                .copied()
                .unwrap_or((' ', Style::new().fg(TEXT)));
            if col == len && !(r == cr && cc >= len) && !sel_style(r, col, len) {
                break;
            }
            let cw = col_widths.get(col).copied().unwrap_or(1);
            if used + cw > width {
                rows.push(Line::from(std::mem::take(&mut spans)));
                row_spans.push(RowSpan {
                    line: r,
                    first,
                    cols: std::mem::take(&mut cols),
                    ends_line: false,
                });
                first = col;
                used = 0;
            }
            if r == cr && col == cc {
                cursor = (rows.len(), used);
            }
            if sel_style(r, col, len) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let text = if col == len {
                " ".to_string()
            } else {
                ch.to_string()
            };
            match spans.last_mut() {
                Some(last) if cw == 0 && col < len => last.content.to_mut().push_str(&text),
                _ => spans.push(Span::styled(text, style)),
            }
            if col < len {
                if cw == 0 && !cols.is_empty() {
                    cols.push(cols[cols.len() - 1]);
                } else {
                    cols.push(used);
                }
            }
            used += cw;
        }
        rows.push(Line::from(std::mem::take(&mut spans)));
        row_spans.push(RowSpan {
            line: r,
            first,
            cols,
            ends_line: true,
        });
    }
    if rows.is_empty() {
        rows.push(Line::default());
        row_spans.push(RowSpan {
            ends_line: true,
            ..RowSpan::default()
        });
    }
    EditorRows {
        lines: rows,
        cursor,
        spans: row_spans,
        version: ed.version(),
    }
}

/// What a row draws, with neighbouring spans of one style joined: how many spans a row is cut into is not what it shows.
fn drawn(rows: &EditorRows) -> Vec<Vec<(String, Style)>> {
    rows.lines
        .iter()
        .map(|line| {
            let mut joined: Vec<(String, Style)> = Vec::new();
            for span in &line.spans {
                match joined.last_mut() {
                    Some((text, style)) if *style == span.style => text.push_str(&span.content),
                    _ => joined.push((span.content.to_string(), span.style)),
                }
            }
            joined
        })
        .collect()
}

fn assert_same_rows(kept: &EditorRows, made_again: &EditorRows, what: &str) {
    assert_eq!(kept.lines, made_again.lines, "{what}: lines");
    assert_eq!(kept.cursor, made_again.cursor, "{what}: cursor");
    assert_eq!(kept.spans, made_again.spans, "{what}: what each row shows");
    assert_eq!(kept.version, made_again.version, "{what}: version");
}

fn assert_like_the_reference(kept: &EditorRows, reference: &EditorRows, what: &str) {
    assert_eq!(drawn(kept), drawn(reference), "{what}: what is drawn");
    assert_eq!(kept.cursor, reference.cursor, "{what}: cursor");
    assert_eq!(kept.spans, reference.spans, "{what}: what each row shows");
    assert_eq!(kept.version, reference.version, "{what}: version");
}

/// A key a person might press in either keymap.
fn random_key(rng: &mut Lcg) -> KeyEvent {
    const LETTERS: &[char] = &[
        'a',
        'b',
        'x',
        ' ',
        'h',
        'j',
        'k',
        'l',
        'w',
        'i',
        'o',
        'd',
        'u',
        'v',
        'V',
        '$',
        '0',
        'G',
        'p',
        'y',
        '[',
        ']',
        '(',
        ')',
        '`',
        '*',
        '_',
        '@',
        '\u{e9}',
        '\u{4e2d}',
        '\u{1f600}',
        '\u{2764}',
        '\u{fe0f}',
        '\u{200d}',
    ];
    const MOVES: &[KeyCode] = &[
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Backspace,
        KeyCode::Delete,
        KeyCode::Enter,
        KeyCode::Esc,
        KeyCode::Tab,
    ];
    match rng.below(10) {
        0..=3 => KeyEvent::new(
            KeyCode::Char(LETTERS[rng.below(LETTERS.len())]),
            KeyModifiers::NONE,
        ),
        4..=6 => {
            let mods = [
                KeyModifiers::NONE,
                KeyModifiers::SHIFT,
                KeyModifiers::CONTROL,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ];
            KeyEvent::new(MOVES[rng.below(MOVES.len())], mods[rng.below(mods.len())])
        }
        _ => KeyEvent::new(
            KeyCode::Char(['z', 'y', 'a', 'x', 'v', 'r'][rng.below(6)]),
            KeyModifiers::CONTROL,
        ),
    }
}

/// Random editing in either keymap, with the width changing now and then: after each key the rows kept from earlier
/// frames are the rows made from nothing, which in turn show what the one-span-a-character layout drew.
#[test]
fn kept_rows_are_the_rows_made_again_after_any_edit_move_selection_undo_or_resize() {
    let start = "fn main() {\n    let x = \"\u{e9}\u{1f600}\";\n}\n\n- item [a](b) `c`\n\u{65e5}\u{672c}\u{8a9e} **text** \u{2764}\u{fe0f} end\n```rust\nlet y = 1;\n```\nlast";
    let widths = [4, 5, 7, 12, 30, 80];
    for keymap in [Keymap::Standard, Keymap::Vim] {
        for seed in 0..6u64 {
            let mut rng = Lcg(seed * 7919 + 1);
            let mut ed = Editor::with_keymap(keymap);
            ed.set_text(start);
            let mut cells = (u64::MAX, Vec::new());
            let mut cache = view::RowsCache::default();
            let mut rows = EditorRows::default();
            let mut width = widths[rng.below(widths.len())];
            for step in 0..400 {
                let key = random_key(&mut rng);
                if ed.handle_key(key) == EditorAction::Paste {
                    ed.paste("pasted [x](y)\n`code`  and\ttext");
                }
                if ed.lines().iter().map(String::len).sum::<usize>() > 2_000 {
                    ed.set_text("short\ntext");
                }
                if rng.below(15) == 0 {
                    width = widths[rng.below(widths.len())];
                }
                // Frames the keys did not change at all, now and then.
                for _ in 0..=rng.below(2) {
                    refresh_editor_rows(
                        &ed,
                        &mut cells,
                        &mut cache,
                        &mut rows,
                        width,
                        &MentionPeople::default(),
                    );
                }
                let what = format!("{keymap:?} seed {seed} step {step} after {key:?} at {width}");
                let fresh = build_editor_rows(&ed, &mut (u64::MAX, Vec::new()), width);
                assert_same_rows(&rows, &fresh, &what);
                let reference = reference_rows(&ed, &cells.1, width);
                assert_like_the_reference(&rows, &reference, &what);
            }
        }
    }
}

// -- what a frame does and does not redo ----------------------------------------------------------------------------

fn counts(app: &App) -> (usize, usize) {
    (
        app.view.editor_cache.plain_builds,
        app.view.editor_cache.assemblies,
    )
}

fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    app.on_key(KeyEvent::new(code, mods));
}

/// The rows the frame drawn last holds are those a fresh build gives for the editor as it is and the width it was laid
/// out for.
fn assert_fresh(app: &App, what: &str) {
    let width = usize::from(app.view.geometry.editor.width)
        .saturating_sub(3)
        .max(1);
    let fresh = build_editor_rows(&app.editor, &mut (u64::MAX, Vec::new()), width);
    assert_same_rows(&app.view.editor_rows, &fresh, what);
}

#[test]
fn a_frame_in_which_nothing_the_rows_depend_on_changed_does_not_make_them_again() {
    let mut app = app_with(Keymap::Standard);
    app.editor.set_text(&draft(4_000));
    crate::ui::render_to_string(&mut app, 120, 40);
    assert_eq!(counts(&app), (1, 1), "the first frame makes them");
    for _ in 0..5 {
        crate::ui::render_to_string(&mut app, 120, 40);
    }
    assert_eq!(
        counts(&app),
        (1, 1),
        "frames of an unchanged draft keep them"
    );
    assert_fresh(&app, "unchanged");

    // A tick of the app changes the header, not the draft.
    app.on_tick();
    crate::ui::render_to_string(&mut app, 120, 40);
    assert_eq!(counts(&app), (1, 1));
}

#[test]
fn a_change_of_text_width_cursor_selection_or_keymap_makes_the_rows_again() {
    let mut app = app_with(Keymap::Standard);
    app.editor.set_text(&draft(4_000));
    crate::ui::render_to_string(&mut app, 120, 40);
    let (plain, assembled) = counts(&app);

    // Text.
    key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    crate::ui::render_to_string(&mut app, 120, 40);
    assert_eq!(counts(&app), (plain + 1, assembled + 1), "typing");
    assert_fresh(&app, "typing");

    // Undo is a change of text like any other.
    key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
    crate::ui::render_to_string(&mut app, 120, 40);
    assert_eq!(counts(&app), (plain + 2, assembled + 2), "undo");
    assert_fresh(&app, "undo");

    // Width: a narrower terminal.
    crate::ui::render_to_string(&mut app, 90, 40);
    assert_eq!(
        counts(&app),
        (plain + 3, assembled + 3),
        "a narrower terminal"
    );
    assert_fresh(&app, "resized");

    // Cursor: the lines are not laid out again, the line it left and the one it is on are.
    key(&mut app, KeyCode::Up, KeyModifiers::NONE);
    crate::ui::render_to_string(&mut app, 90, 40);
    assert_eq!(counts(&app), (plain + 3, assembled + 4), "the cursor moved");
    assert_fresh(&app, "cursor");

    // Selection.
    key(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
    key(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
    crate::ui::render_to_string(&mut app, 90, 40);
    assert_eq!(counts(&app), (plain + 3, assembled + 5), "a selection");
    assert_fresh(&app, "selection");
    key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    crate::ui::render_to_string(&mut app, 90, 40);
    assert_eq!(counts(&app), (plain + 3, assembled + 6), "select all");
    assert_fresh(&app, "select all");

    // Text changed under a cursor that stays where it was: Delete at the very start.
    key(&mut app, KeyCode::Home, KeyModifiers::CONTROL);
    crate::ui::render_to_string(&mut app, 90, 40);
    let before = counts(&app);
    let first_line = app.editor.lines()[0].clone();
    key(&mut app, KeyCode::Delete, KeyModifiers::NONE);
    assert_ne!(app.editor.lines()[0], first_line);
    crate::ui::render_to_string(&mut app, 90, 40);
    assert_eq!(
        counts(&app),
        (before.0 + 1, before.1 + 1),
        "text changed, cursor did not"
    );
    assert_fresh(&app, "delete at the start");

    // Keymap: vim's normal mode keeps the cursor on a character and has no selection.
    key(&mut app, KeyCode::End, KeyModifiers::CONTROL);
    crate::ui::render_to_string(&mut app, 90, 40);
    let before = counts(&app);
    app.set_keymap(Keymap::Vim);
    crate::ui::render_to_string(&mut app, 90, 40);
    assert_eq!(
        counts(&app),
        (before.0, before.1 + 1),
        "the keymap's own cursor and no selection"
    );
    assert_fresh(&app, "keymap");
}

#[test]
fn rows_stay_the_rows_a_fresh_build_gives_through_typing_pasting_undo_a_resize_and_a_mention() {
    for keymap in [Keymap::Standard, Keymap::Vim] {
        let mut app = app_with(keymap);
        let vim = keymap == Keymap::Vim;
        crate::ui::render_to_string(&mut app, 100, 30);
        if vim {
            key(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
        }
        for c in "hello @a there [link](x) `code`".chars() {
            key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
            crate::ui::render_to_string(&mut app, 100, 30);
            assert_fresh(&app, "typing");
        }
        app.on_paste(&"pasted line with `ticks` and [brackets\nsecond ".repeat(20));
        crate::ui::render_to_string(&mut app, 100, 30);
        assert_fresh(&app, "paste");
        for width in [100, 61, 40, 100, 33] {
            crate::ui::render_to_string(&mut app, width, 30);
            assert_fresh(&app, "resize");
        }
        if vim {
            key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            key(&mut app, KeyCode::Char('u'), KeyModifiers::NONE);
        } else {
            key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
        }
        crate::ui::render_to_string(&mut app, 100, 30);
        assert_fresh(&app, "undo");
        for _ in 0..3 {
            key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
            crate::ui::render_to_string(&mut app, 100, 30);
            assert_fresh(&app, "erasing");
        }
    }
}

// -- measurements ---------------------------------------------------------------------------------------------------

#[test]
#[ignore = "a measurement, run on demand: cargo test --release editor_cost -- --ignored --nocapture"]
fn editor_cost_of_a_long_draft_and_of_an_unmatched_bracket_paste() {
    for size in [25_000, 100_000, 190_000] {
        let mut app = app_with(Keymap::Standard);
        app.editor.set_text(&draft(size));
        let started = Instant::now();
        crate::ui::render_to_string(&mut app, 160, 50);
        let first = started.elapsed();
        let steady = median(
            (0..15)
                .map(|_| {
                    let started = Instant::now();
                    crate::ui::render_to_string(&mut app, 160, 50);
                    started.elapsed()
                })
                .collect(),
        );
        key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        let started = Instant::now();
        crate::ui::render_to_string(&mut app, 160, 50);
        let moved = started.elapsed();
        key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        let started = Instant::now();
        crate::ui::render_to_string(&mut app, 160, 50);
        let typed = started.elapsed();
        println!(
            "{size:>7} characters: first frame {first:?}, median steady frame {steady:?}, \
             after a cursor move {moved:?}, after a typed character {typed:?}"
        );
    }
    for size in [16_000, 32_000] {
        for piece in [
            "[", "![", "[ ](", "` ", "** ", "*a ", "_a ", "~~ ", ": ", "@a",
        ] {
            let line = piece.repeat(size / piece.len());
            let started = Instant::now();
            let cells = editor_cells(&[line], &[]);
            let took = started.elapsed();
            assert!(!cells.is_empty());
            println!("colouring {size} characters of {piece:?}: {took:?}");
        }
    }
}

#[test]
fn zero_width_characters_share_their_predecessors_column() {
    // Heart + U+FE0F (emoji variation selector) + 'x': widths 2, 0, 1.
    let mut ed = Editor::with_keymap(Keymap::Standard);
    ed.set_text("\u{2764}\u{FE0F}x");
    let rows = build_editor_rows(&ed, &mut (u64::MAX, Vec::new()), 80);
    assert_eq!(
        rows.spans[0].cols,
        vec![0, 0, 2],
        "zero-width selector shares column 0 with the heart"
    );
    assert_eq!(
        rows.char_at(0, 2),
        Some((0, 2)),
        "column 2 maps to 'x' (index 2)"
    );
    assert_eq!(
        rows.char_at(0, 0),
        Some((0, 0)),
        "column 0 maps to the heart (index 0)"
    );
}

#[test]
fn the_people_of_the_chat_arriving_after_the_text_colours_its_mentions_again() {
    let mut ed = Editor::new();
    ed.paste("hi @Ada Lovelace");
    let (mut cells, mut cache, mut rows) = (
        (u64::MAX, Vec::new()),
        view::RowsCache::default(),
        EditorRows::default(),
    );
    let style_of = |rows: &EditorRows, text: &str| {
        let line = &rows.lines[0];
        let mut col = 0;
        let row: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let at = row[..row.find(text).unwrap()].chars().count();
        for span in &line.spans {
            let n = span.content.chars().count();
            if at < col + n {
                return span.style;
            }
            col += n;
        }
        unreachable!()
    };
    refresh_editor_rows(
        &ed,
        &mut cells,
        &mut cache,
        &mut rows,
        80,
        &MentionPeople::default(),
    );
    assert_ne!(
        style_of(&rows, "Lovelace"),
        style_of(&rows, "@Ada"),
        "nobody known: one word"
    );
    let people = MentionPeople::new(
        vec![teams_client::Member {
            id: "ada".into(),
            name: "Ada Lovelace".into(),
            not_in_directory: false,
        }],
        "me",
    );
    refresh_editor_rows(&ed, &mut cells, &mut cache, &mut rows, 80, &people);
    assert_eq!(
        style_of(&rows, "Lovelace"),
        style_of(&rows, "@Ada"),
        "the same text, coloured again once the chat's people are known"
    );
    assert_eq!(
        rows.version,
        ed.version(),
        "the rows still name the text they show"
    );
}
