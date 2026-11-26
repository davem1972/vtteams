//! The standard keymap, driven the way a person drives it: by key presses.

use super::*;

/// Feeds keys: plain characters, and `<Name>` for others, with `C-`, `S-` and `A-` prefixes for modifiers
/// (`<S-Left>`, `<C-S-z>`). Names: CR, Esc, BS, Del, Tab, Left, Right, Up, Down, Home, End, PgUp, PgDn, Ins, lt.
/// Returns every action other than `None`, in order.
fn run(ed: &mut Editor, keys: &str) -> Vec<EditorAction> {
    let mut actions = Vec::new();
    let chars: Vec<char> = keys.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ev = if chars[i] == '<'
            && let Some(end) = chars[i..].iter().position(|c| *c == '>')
        {
            let name: String = chars[i + 1..i + end].iter().collect();
            i += end;
            parse(&name)
        } else {
            KeyEvent::new(KeyCode::Char(chars[i]), KeyModifiers::NONE)
        };
        i += 1;
        let action = ed.handle_key(ev);
        if action != EditorAction::None {
            actions.push(action);
        }
        check(ed);
    }
    actions
}

fn parse(name: &str) -> KeyEvent {
    let mut mods = KeyModifiers::NONE;
    let mut rest = name;
    loop {
        match rest.get(..2) {
            Some("C-") => mods |= KeyModifiers::CONTROL,
            Some("S-") => mods |= KeyModifiers::SHIFT,
            Some("A-") => mods |= KeyModifiers::ALT,
            _ => break,
        }
        rest = &rest[2..];
    }
    let code = match rest {
        "CR" => KeyCode::Enter,
        "Esc" => KeyCode::Esc,
        "BS" => KeyCode::Backspace,
        "Del" => KeyCode::Delete,
        "Tab" => KeyCode::Tab,
        "BackTab" => KeyCode::BackTab,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PgUp" => KeyCode::PageUp,
        "PgDn" => KeyCode::PageDown,
        "Ins" => KeyCode::Insert,
        "lt" => KeyCode::Char('<'),
        c if c.chars().count() == 1 => KeyCode::Char(c.chars().next().unwrap()),
        other => panic!("unknown key {other}"),
    };
    KeyEvent::new(code, mods)
}

fn check(ed: &Editor) {
    assert!(!ed.buf.is_empty() && ed.lines.len() == ed.buf.len());
    let (r, c) = ed.cursor();
    assert!(r < ed.buf.len(), "row {r} out of bounds");
    assert!(c <= ed.buf[r].len(), "col {c} past the end of row {r}");
    assert_eq!(ed.mode(), Mode::Insert, "the standard keymap has no modes");
    assert!(ed.pending().is_empty() && ed.command_line().is_none());
    if let Some(((sr, sc), (er, ec), linewise)) = ed.selection() {
        assert!(!linewise);
        assert!((sr, sc) <= (er, ec), "selection backwards");
        assert!(er < ed.buf.len() && ec <= ed.buf[er].len());
    }
}

fn editor(text: &str) -> Editor {
    let mut ed = Editor::with_keymap(Keymap::Standard);
    ed.set_text(text);
    ed
}

fn text_after(ed: &mut Editor, keys: &str) -> String {
    run(ed, keys);
    ed.text()
}

#[test]
fn keymap_names_parse_and_the_standard_one_has_no_modes() {
    assert_eq!(Keymap::parse("standard"), Some(Keymap::Standard));
    assert_eq!(Keymap::parse(" VIM "), Some(Keymap::Vim));
    assert_eq!(Keymap::parse("emacs"), None);
    let mut ed = Editor::with_keymap(Keymap::Standard);
    assert_eq!(ed.mode(), Mode::Insert);
    ed.set_mode(Mode::Normal);
    assert_eq!(ed.mode(), Mode::Insert, "asking for a mode is ignored");
    ed.set_keymap(Keymap::Vim);
    assert_eq!(ed.mode(), Mode::Normal);
}

#[test]
fn with_enter_adding_a_line_a_modified_enter_sends_and_the_fallbacks_still_work() {
    let mut ed = editor("");
    ed.set_enter_sends(false);
    assert_eq!(run(&mut ed, "one<CR>two"), []);
    assert_eq!(ed.text(), "one\ntwo", "a plain Enter adds a line");
    for send in ["<S-CR>", "<C-CR>", "<A-CR>"] {
        assert_eq!(run(&mut ed, send), [EditorAction::Send], "{send}");
        assert_eq!(ed.text(), "one\ntwo", "{send} adds nothing");
    }
    // A terminal that cannot tell the modified Enter from Enter still has both of these.
    assert_eq!(run(&mut ed, "<C-s>"), [EditorAction::Send]);
    assert_eq!(run(&mut ed, "<C-j>"), []);
    assert_eq!(ed.text(), "one\ntwo\n");
    // Inside an open fence nothing changes: Enter is a line.
    let mut ed = editor("");
    ed.set_enter_sends(false);
    run(&mut ed, "```rust<CR>x<CR>");
    assert_eq!(ed.text(), "```rust\nx\n");
    // And back to the default.
    ed.set_enter_sends(true);
    run(&mut ed, "```");
    assert_eq!(run(&mut ed, "<CR>"), [EditorAction::Send]);
}

#[test]
fn typing_inserts_and_enter_sends() {
    let mut ed = editor("");
    assert_eq!(run(&mut ed, "hello, wörld"), []);
    assert_eq!(ed.text(), "hello, wörld");
    assert_eq!(run(&mut ed, "<CR>"), [EditorAction::Send]);
    assert_eq!(
        ed.text(),
        "hello, wörld",
        "sending is the caller's business"
    );
    assert_eq!(run(&mut ed, "<C-s>"), [EditorAction::Send]);
}

#[test]
fn shift_ctrl_or_alt_enter_and_ctrl_j_add_a_line() {
    for newline in ["<S-CR>", "<C-CR>", "<A-CR>", "<C-j>"] {
        let mut ed = editor("");
        let actions = run(&mut ed, &format!("one{newline}two"));
        assert_eq!(actions, [], "{newline}");
        assert_eq!(ed.text(), "one\ntwo", "{newline}");
    }
}

#[test]
fn enter_inside_an_open_code_block_adds_a_line_and_after_it_sends() {
    let mut ed = editor("");
    assert_eq!(run(&mut ed, "```rust<CR>let x = 1;<CR>"), []);
    assert_eq!(ed.text(), "```rust\nlet x = 1;\n");
    assert_eq!(run(&mut ed, "```"), []);
    assert_eq!(
        run(&mut ed, "<CR>"),
        [EditorAction::Send],
        "the block is closed"
    );
    let mut ed = editor("");
    run(&mut ed, "see ``` inline");
    assert_eq!(
        run(&mut ed, "<CR>"),
        [EditorAction::Send],
        "backticks that do not start the line are not a fence"
    );
}

#[test]
fn a_fence_opened_and_closed_on_one_line_does_not_swallow_enter() {
    let mut ed = editor("");
    run(&mut ed, "```x``` ");
    assert_eq!(
        run(&mut ed, "<CR>"),
        [EditorAction::Send],
        "nothing is open"
    );
    let mut ed = editor("");
    run(&mut ed, "```rust<S-CR>fn main() {}");
    assert_eq!(run(&mut ed, "<CR>"), [], "an open block takes the newline");
    assert!(ed.text().ends_with("{}\n"));
}
#[test]
fn a_new_line_continues_a_list() {
    let mut ed = editor("");
    run(&mut ed, "- one<S-CR>two<S-CR>");
    assert_eq!(ed.text(), "- one\n- two\n- ");
    run(&mut ed, "<S-CR>");
    assert_eq!(ed.text(), "- one\n- two\n", "an empty item ends the list");
}

#[test]
fn shift_with_a_movement_key_selects_and_typing_replaces() {
    let mut ed = editor("hello world");
    run(
        &mut ed,
        "<Home><S-Right><S-Right><S-Right><S-Right><S-Right>",
    );
    assert_eq!(ed.selection(), Some(((0, 0), (0, 4), false)));
    run(&mut ed, "J");
    assert_eq!(ed.text(), "J world");
    assert_eq!(ed.selection(), None);

    let mut ed = editor("hello world");
    run(&mut ed, "<S-Left><S-Left>");
    assert_eq!(ed.selection(), Some(((0, 9), (0, 10), false)));
    run(&mut ed, "<BS>");
    assert_eq!(ed.text(), "hello wor");
}

#[test]
fn a_selection_can_cover_line_breaks() {
    let mut ed = editor("one\ntwo\nthree");
    run(&mut ed, "<C-Home><S-Down><S-Down><S-Right>");
    assert_eq!(ed.cursor(), (2, 1));
    run(&mut ed, "<Del>");
    assert_eq!(ed.text(), "hree");
}

#[test]
fn left_and_right_collapse_a_selection_to_its_edge() {
    let mut ed = editor("abcdef");
    run(&mut ed, "<Home><Right><S-Right><S-Right>");
    assert_eq!(ed.selection(), Some(((0, 1), (0, 2), false)));
    run(&mut ed, "<Left>");
    assert_eq!((ed.cursor(), ed.selection()), ((0, 1), None));
    run(&mut ed, "<S-Right><S-Right><Right>");
    assert_eq!((ed.cursor(), ed.selection()), ((0, 3), None));
}

#[test]
fn escape_clears_a_selection_first_and_then_leaves() {
    let mut ed = editor("abc");
    assert_eq!(run(&mut ed, "<S-Left><Esc>"), []);
    assert_eq!(ed.selection(), None);
    assert_eq!(run(&mut ed, "<Esc>"), [EditorAction::Escape]);
}

#[test]
fn select_all_copy_cut_and_paste() {
    let mut ed = editor("one\ntwo");
    let actions = run(&mut ed, "<C-a><C-c>");
    assert_eq!(actions, [EditorAction::Yanked("one\ntwo".into())]);
    assert_eq!(ed.text(), "one\ntwo", "copying leaves the text alone");
    assert!(ed.selection().is_some(), "and the selection");

    let actions = run(&mut ed, "<C-x>");
    assert_eq!(actions, [EditorAction::Yanked("one\ntwo".into())]);
    assert_eq!(ed.text(), "");

    assert_eq!(run(&mut ed, "<C-v>"), [EditorAction::Paste]);
    assert_eq!(run(&mut ed, "<S-Ins>"), [EditorAction::Paste]);
    ed.paste("one\ntwo");
    assert_eq!(ed.text(), "one\ntwo");
    assert_eq!(
        run(&mut ed, "<C-c>"),
        [],
        "nothing selected, nothing copied"
    );
    assert_eq!(run(&mut ed, "<C-x>"), []);
    assert_eq!(ed.text(), "one\ntwo");
}

#[test]
fn pasting_replaces_the_selection_and_is_one_step_of_undo() {
    let mut ed = editor("keep this and drop that");
    run(&mut ed, "<C-End>");
    for _ in 0..9 {
        run(&mut ed, "<S-Left>");
    }
    assert_eq!(ed.selection().map(|s| (s.0, s.1)), Some(((0, 14), (0, 22))));
    ed.paste("THAT\nTHING");
    assert_eq!(ed.text(), "keep this and THAT\nTHING");
    run(&mut ed, "<C-z>");
    assert_eq!(ed.text(), "keep this and drop that");
}

#[test]
fn undo_goes_back_a_word_at_a_time_and_redo_forward() {
    let mut ed = editor("");
    run(&mut ed, "one two three");
    assert_eq!(text_after(&mut ed, "<C-z>"), "one two ");
    assert_eq!(text_after(&mut ed, "<C-z>"), "one ");
    assert_eq!(text_after(&mut ed, "<C-z>"), "");
    assert_eq!(text_after(&mut ed, "<C-z>"), "", "nothing left to undo");
    assert_eq!(text_after(&mut ed, "<C-y>"), "one ");
    assert_eq!(text_after(&mut ed, "<C-S-z>"), "one two ");
    assert_eq!(text_after(&mut ed, "<C-y>"), "one two three");
    run(&mut ed, "<C-z>x");
    assert_eq!(ed.text(), "one two x");
    assert_eq!(
        text_after(&mut ed, "<C-y>"),
        "one two x",
        "a new edit ends the redo history"
    );
}

#[test]
fn a_run_of_erasing_is_one_step_of_undo() {
    let mut ed = editor("");
    run(&mut ed, "abcdef");
    run(&mut ed, "<BS><BS><BS>");
    assert_eq!(ed.text(), "abc");
    assert_eq!(text_after(&mut ed, "<C-z>"), "abcdef");
}

#[test]
fn backspace_and_delete_work_across_lines() {
    let mut ed = editor("ab\ncd");
    assert_eq!(text_after(&mut ed, "<Home><BS>"), "abcd");
    assert_eq!(ed.cursor(), (0, 2));
    assert_eq!(
        text_after(&mut ed, "<S-CR><Left><Del>"),
        "abcd",
        "Delete at the end of a line joins the next"
    );
    let mut ed = editor("ab");
    assert_eq!(
        text_after(&mut ed, "<Home><BS>"),
        "ab",
        "the very start has nothing before it"
    );
    assert_eq!(
        text_after(&mut ed, "<End><Del>"),
        "ab",
        "the very end has nothing after it"
    );
}

#[test]
fn left_and_right_cross_an_emoji_with_a_variation_selector_in_one_step() {
    // Heart + U+FE0F (request emoji presentation) is one glyph in any terminal that honours it, not two.
    let mut ed = editor("\u{2764}\u{FE0F}ab");
    assert_eq!(ed.cursor(), (0, 4), "set_text starts at the end");
    run(&mut ed, "<Left>");
    assert_eq!(ed.cursor(), (0, 3), "past 'b'");
    run(&mut ed, "<Left>");
    assert_eq!(ed.cursor(), (0, 2), "past 'a'");
    run(&mut ed, "<Left>");
    assert_eq!(
        ed.cursor(),
        (0, 0),
        "past the whole heart+selector cluster in one step, not onto the selector alone"
    );
    run(&mut ed, "<Right>");
    assert_eq!(ed.cursor(), (0, 2), "back across the cluster in one step");
}

#[test]
fn backspace_and_delete_remove_a_whole_emoji_cluster_not_half_of_it() {
    let mut ed = editor("\u{2764}\u{FE0F}ab");
    run(&mut ed, "<Home><Right>");
    assert_eq!(ed.cursor(), (0, 2), "right after the cluster");
    assert_eq!(
        text_after(&mut ed, "<BS>"),
        "ab",
        "the whole cluster is gone, not just the selector"
    );
    assert_eq!(ed.cursor(), (0, 0));

    let mut ed = editor("\u{2764}\u{FE0F}ab");
    assert_eq!(
        text_after(&mut ed, "<Home><Del>"),
        "ab",
        "Delete removes the whole cluster under the cursor too"
    );
    assert_eq!(ed.cursor(), (0, 0));
}

#[test]
fn control_backspace_and_delete_remove_a_word() {
    let mut ed = editor("one two three");
    assert_eq!(text_after(&mut ed, "<C-BS>"), "one two ");
    assert_eq!(text_after(&mut ed, "<C-h>"), "one ");
    assert_eq!(text_after(&mut ed, "<C-w>"), "");
    let mut ed = editor("one two three");
    assert_eq!(text_after(&mut ed, "<Home><C-Del>"), "two three");
    assert_eq!(text_after(&mut ed, "<C-Del>"), "three");
    let mut ed = editor("end.");
    assert_eq!(
        text_after(&mut ed, "<C-BS>"),
        "end",
        "punctuation is a word of its own"
    );
    let mut ed = editor("a\nb");
    assert_eq!(
        text_after(&mut ed, "<Down><Home><C-BS>"),
        "ab",
        "at the start of a line it joins"
    );
}

#[test]
fn control_arrows_move_by_word_and_home_end_by_line_or_message() {
    let mut ed = editor("one two\nthree");
    run(&mut ed, "<C-Home><C-Right>");
    assert_eq!(ed.cursor(), (0, 4));
    run(&mut ed, "<C-Right><C-Right>");
    assert_eq!(ed.cursor(), (1, 0), "past the end of a line, to the next");
    run(&mut ed, "<C-Left>");
    assert_eq!(ed.cursor(), (0, 7));
    run(&mut ed, "<C-Left>");
    assert_eq!(ed.cursor(), (0, 4));
    run(&mut ed, "<A-Left>");
    assert_eq!(ed.cursor(), (0, 0));
    run(&mut ed, "<End>");
    assert_eq!(ed.cursor(), (0, 7));
    run(&mut ed, "<C-End>");
    assert_eq!(ed.cursor(), (1, 5));
    run(&mut ed, "<Home>");
    assert_eq!(ed.cursor(), (1, 0));
    run(&mut ed, "<C-Home>");
    assert_eq!(ed.cursor(), (0, 0));
}

#[test]
fn up_and_down_remember_the_column_and_the_ends_go_to_the_line_ends() {
    let mut ed = editor("a long line\nab\nanother long");
    run(
        &mut ed,
        "<C-Home><Right><Right><Right><Right><Right><Right>",
    );
    run(&mut ed, "<Down>");
    assert_eq!(ed.cursor(), (1, 2), "the short line");
    run(&mut ed, "<Down>");
    assert_eq!(ed.cursor(), (2, 6), "the column is remembered");
    run(&mut ed, "<Down>");
    assert_eq!(ed.cursor(), (2, 12), "the last line: to its end");
    run(&mut ed, "<C-Home><Up>");
    assert_eq!(ed.cursor(), (0, 0));
    run(&mut ed, "<PgDn>");
    assert_eq!(ed.cursor().0, 2);
    run(&mut ed, "<PgUp>");
    assert_eq!(ed.cursor().0, 0);
}

#[test]
fn tab_indents_and_shift_tab_unindents() {
    let mut ed = editor("");
    assert_eq!(text_after(&mut ed, "<Tab>x"), "  x");
    assert_eq!(text_after(&mut ed, "<BackTab>"), "x");
    assert_eq!(text_after(&mut ed, "<BackTab>"), "x", "nothing to remove");
    assert_eq!(ed.cursor(), (0, 1));
}

#[test]
fn undo_after_a_selection_replace_restores_the_selected_text() {
    let mut ed = editor("hello world");
    run(&mut ed, "<C-a>bye");
    assert_eq!(ed.text(), "bye");
    run(&mut ed, "<C-z>");
    assert_eq!(ed.text(), "hello world");
}

#[test]
fn set_text_and_clear_drop_the_selection() {
    let mut ed = editor("abc");
    run(&mut ed, "<C-a>");
    ed.set_text("new");
    assert_eq!(ed.selection(), None);
    run(&mut ed, "<C-a>");
    ed.clear();
    assert_eq!((ed.selection(), ed.mode()), (None, Mode::Insert));
}

#[test]
fn the_buffer_size_is_bounded() {
    let mut ed = editor("");
    ed.paste(&"x".repeat(MAX_CHARS * 2));
    assert!(ed.total_chars() <= MAX_CHARS + 1);
    let before = ed.text().len();
    run(&mut ed, "y<S-CR>z");
    assert_eq!(ed.text().len(), before, "full: further typing is ignored");
}

#[test]
fn any_sequence_of_keys_keeps_the_editor_consistent() {
    let codes = [
        KeyCode::Char('a'),
        KeyCode::Char(' '),
        KeyCode::Char('é'),
        KeyCode::Char('`'),
        KeyCode::Char('-'),
        KeyCode::Char('z'),
        KeyCode::Char('v'),
        KeyCode::Enter,
        KeyCode::Esc,
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Backspace,
        KeyCode::Delete,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Insert,
        KeyCode::F(5),
    ];
    let mods = [
        KeyModifiers::NONE,
        KeyModifiers::SHIFT,
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ];
    // A fixed pseudo-random sequence, so a failure can be reproduced.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..40 {
        let mut ed = editor(if round % 2 == 0 {
            ""
        } else {
            "- a list\n```\ncode\n```\nend"
        });
        for _ in 0..400 {
            let code = codes[(next() % codes.len() as u64) as usize];
            let m = mods[(next() % mods.len() as u64) as usize];
            if ed.handle_key(KeyEvent::new(code, m)) == EditorAction::Paste {
                ed.paste(if next() % 2 == 0 { "pasted\ntext" } else { "" });
            }
            check(&ed);
        }
    }
}

#[test]
fn move_to_snaps_to_cluster_boundary() {
    // Heart + U+FE0F (emoji selector) + 'x': trying to land on the selector (index 1) snaps to the heart (index 0).
    let mut ed = editor("\u{2764}\u{FE0F}x");
    ed.move_to((0, 1), false);
    assert_eq!(
        ed.cursor(),
        (0, 0),
        "moving to index 1 (inside the cluster) snaps to the start (index 0)"
    );
}

#[test]
fn vertical_move_snaps_to_cluster_boundary() {
    // Heart + U+FE0F + 'x' on line 0, "ab" on line 1: Up from (1,1) should land on a cluster boundary, not inside it.
    let mut ed = editor("\u{2764}\u{FE0F}x\nab");
    ed.move_to((1, 1), false);
    run(&mut ed, "<Up>");
    let col = ed.cursor().1;
    assert!(
        col == 0 || col == 2,
        "Up from (1,1) lands on a cluster boundary (0 or 2), not 1: got {col}"
    );
}

#[test]
fn no_op_edits_preserve_redo_history() {
    // A word delete that deletes nothing must not wipe the redo history. An empty Backspace keeps it as the control.
    let mut ed = editor("");
    run(&mut ed, "abc");
    run(&mut ed, "<C-z>");
    assert_eq!(ed.text(), "");
    run(&mut ed, "<Home><C-BS>");
    assert_eq!(ed.text(), "", "word delete back at start is a no-op");
    run(&mut ed, "<C-y>");
    assert_eq!(
        ed.text(),
        "abc",
        "redo works after a no-op word delete back"
    );

    // Word delete forward at end.
    let mut ed = editor("");
    run(&mut ed, "abc");
    run(&mut ed, "<C-z>");
    run(&mut ed, "<End><C-Del>");
    assert_eq!(ed.text(), "", "word delete forward at end is a no-op");
    run(&mut ed, "<C-y>");
    assert_eq!(
        ed.text(),
        "abc",
        "redo works after a no-op word delete forward"
    );

    // Empty paste.
    let mut ed = editor("");
    run(&mut ed, "abc");
    run(&mut ed, "<C-z>");
    ed.paste("");
    assert_eq!(ed.text(), "", "pasting nothing is a no-op");
    run(&mut ed, "<C-y>");
    assert_eq!(ed.text(), "abc", "redo works after a no-op paste");

    // Cut with no selection.
    let mut ed = editor("");
    run(&mut ed, "abc");
    run(&mut ed, "<C-z>");
    run(&mut ed, "<C-x>");
    assert_eq!(ed.text(), "", "cut with no selection is a no-op");
    run(&mut ed, "<C-y>");
    assert_eq!(ed.text(), "abc", "redo works after a no-op cut");

    // Control: an empty Backspace at the start is already a no-op that keeps redo.
    let mut ed = editor("");
    run(&mut ed, "abc");
    run(&mut ed, "<C-z>");
    run(&mut ed, "<Home><BS>");
    assert_eq!(ed.text(), "", "backspace at start is a no-op");
    run(&mut ed, "<C-y>");
    assert_eq!(
        ed.text(),
        "abc",
        "redo works after a no-op backspace (control)"
    );

    // Word delete forward at end.
    let mut ed = editor("abc");
    run(&mut ed, "<C-z>");
    run(&mut ed, "<C-y>");
    run(&mut ed, "<End><C-Del>");
    assert_eq!(ed.text(), "abc", "word delete forward at end is a no-op");
    run(&mut ed, "<C-y>");
    assert_eq!(
        ed.text(),
        "abc",
        "redo works after a no-op word delete forward"
    );

    // Empty paste.
    let mut ed = editor("abc");
    run(&mut ed, "<C-z>");
    run(&mut ed, "<C-y>");
    ed.paste("");
    assert_eq!(ed.text(), "abc", "pasting nothing is a no-op");
    run(&mut ed, "<C-y>");
    assert_eq!(ed.text(), "abc", "redo works after a no-op paste");

    // Cut with no selection.
    let mut ed = editor("abc");
    run(&mut ed, "<C-z>");
    run(&mut ed, "<C-y>");
    run(&mut ed, "<C-x>");
    assert_eq!(ed.text(), "abc", "cut with no selection is a no-op");
    run(&mut ed, "<C-y>");
    assert_eq!(ed.text(), "abc", "redo works after a no-op cut");

    // Control: an empty Backspace at the start is already a no-op that keeps redo.
    let mut ed = editor("abc");
    run(&mut ed, "<C-z>");
    run(&mut ed, "<C-y>");
    run(&mut ed, "<Home><BS>");
    assert_eq!(ed.text(), "abc", "backspace at start is a no-op");
    run(&mut ed, "<C-y>");
    assert_eq!(
        ed.text(),
        "abc",
        "redo works after a no-op backspace (control)"
    );
}
