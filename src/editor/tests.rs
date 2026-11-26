//! Behaviour of the editor, driven the way a person drives it: by key presses.

use super::*;

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

/// Feeds keys written vim-style: plain characters plus `<Esc>`, `<CR>`, `<BS>`, `<Del>`, `<Tab>`, `<Left>`,
/// `<Right>`, `<Up>`, `<Down>`, `<Home>`, `<End>`, `<C-x>`, `<A-CR>` and `<lt>`. Returns the last
/// non-`None` action.
fn run(ed: &mut Editor, keys: &str) -> EditorAction {
    let mut last = EditorAction::None;
    let chars: Vec<char> = keys.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ev = if chars[i] == '<'
            && let Some(end) = chars[i..].iter().position(|c| *c == '>')
        {
            let name: String = chars[i + 1..i + end].iter().collect();
            i += end;
            match name.as_str() {
                "Esc" => key(KeyCode::Esc, KeyModifiers::NONE),
                "CR" => key(KeyCode::Enter, KeyModifiers::NONE),
                "A-CR" => key(KeyCode::Enter, KeyModifiers::ALT),
                "S-CR" => key(KeyCode::Enter, KeyModifiers::SHIFT),
                "C-CR" => key(KeyCode::Enter, KeyModifiers::CONTROL),
                "BS" => key(KeyCode::Backspace, KeyModifiers::NONE),
                "Del" => key(KeyCode::Delete, KeyModifiers::NONE),
                "Tab" => key(KeyCode::Tab, KeyModifiers::NONE),
                "Left" => key(KeyCode::Left, KeyModifiers::NONE),
                "Right" => key(KeyCode::Right, KeyModifiers::NONE),
                "Up" => key(KeyCode::Up, KeyModifiers::NONE),
                "Down" => key(KeyCode::Down, KeyModifiers::NONE),
                "Home" => key(KeyCode::Home, KeyModifiers::NONE),
                "End" => key(KeyCode::End, KeyModifiers::NONE),
                "lt" => key(KeyCode::Char('<'), KeyModifiers::NONE),
                n if n.starts_with("C-") => key(
                    KeyCode::Char(n.chars().nth(2).unwrap()),
                    KeyModifiers::CONTROL,
                ),
                n => panic!("unknown key {n}"),
            }
        } else {
            let c = chars[i];
            let mods = if c.is_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            key(KeyCode::Char(c), mods)
        };
        i += 1;
        let a = ed.handle_key(ev);
        if a != EditorAction::None {
            last = a;
        }
        check_invariants(ed);
    }
    last
}

fn check_invariants(ed: &Editor) {
    assert!(!ed.lines.is_empty() && ed.lines.len() == ed.buf.len());
    let (r, c) = ed.cursor();
    assert!(r < ed.buf.len(), "row {r} out of bounds");
    let len = ed.buf[r].len();
    match ed.mode {
        Mode::Insert => assert!(c <= len, "col {c} > len {len}"),
        _ => assert!(c < len.max(1), "col {c} >= len {len} in {:?}", ed.mode),
    }
    for (l, b) in ed.lines.iter().zip(&ed.buf) {
        assert_eq!(l.chars().count(), b.len());
    }
    if ed.is_visual() {
        assert!(ed.anchor.0 < ed.buf.len());
        assert!(ed.anchor.1 < ed.buf[ed.anchor.0].len().max(1));
    }
}

fn ed(text: &str) -> Editor {
    Editor::from_text(text)
}

/// An editor on `text` with the cursor at the start.
fn at_start(text: &str) -> Editor {
    let mut e = ed(text);
    run(&mut e, "gg0");
    e
}

fn t(e: &Editor) -> String {
    e.text()
}

// -- basics --------------------------------------------------------------

#[test]
fn starts_in_normal_mode_and_empty() {
    let e = Editor::new();
    assert_eq!(e.mode(), Mode::Normal);
    assert!(e.is_empty());
    assert_eq!(e.cursor(), (0, 0));
    assert_eq!(e.lines(), &[String::new()]);
}

#[test]
fn insert_typing_and_escape() {
    let mut e = Editor::new();
    run(&mut e, "ihello world<Esc>");
    assert_eq!(t(&e), "hello world");
    assert_eq!(e.mode(), Mode::Normal);
    assert_eq!(e.cursor(), (0, 10));
}

#[test]
fn set_text_places_cursor_at_end() {
    let mut e = ed("ab\ncd");
    assert_eq!(e.cursor(), (1, 1));
    e.set_text("one\r\ntwo");
    assert_eq!(t(&e), "one\ntwo");
    e.clear();
    assert!(e.is_empty());
    assert_eq!(e.lines().len(), 1);
}

#[test]
fn version_bumps_on_changes() {
    let mut e = Editor::new();
    let v = e.version();
    run(&mut e, "ia<Esc>");
    assert!(e.version() > v);
    let v = e.version();
    run(&mut e, "h");
    assert_eq!(e.version(), v);
}

#[test]
fn insert_editing_keys() {
    let mut e = Editor::new();
    run(
        &mut e,
        "iabc<BS>d<Left><Left>X<Right><Del><End>!<Home>^<Esc>",
    );
    assert_eq!(t(&e), "^aXb!");
    run(&mut e, "S");
    assert_eq!(t(&e), "");
}

#[test]
fn backspace_joins_lines_and_enter_splits() {
    let mut e = Editor::new();
    run(&mut e, "iabc<Left><CR><Esc>");
    assert_eq!(e.lines(), &["ab", "c"]);
    let mut e = ed("ab\ncd");
    run(&mut e, "0<BS>");
    assert_eq!(t(&e), "ab\ncd"); // Normal mode: <BS> only moves
    run(&mut e, "i<BS>");
    assert_eq!(t(&e), "abcd");
}

#[test]
fn ctrl_w_and_ctrl_u_in_insert() {
    let mut e = Editor::new();
    run(&mut e, "ifoo bar baz<C-w>X");
    assert_eq!(t(&e), "foo bar X");
    run(&mut e, "<C-w><C-w>");
    assert_eq!(t(&e), "foo ");
    run(&mut e, "<C-u>");
    assert_eq!(t(&e), "");
    run(&mut e, "a<CR>b<C-u><C-u>");
    assert_eq!(t(&e), "a");
}

#[test]
fn tab_inserts_two_spaces() {
    let mut e = Editor::new();
    run(&mut e, "i<Tab>x<Esc>");
    assert_eq!(t(&e), "  x");
}

// -- actions ---------------------------------------------------------------

#[test]
fn a_modified_enter_in_insert_mode_sends_and_a_plain_one_adds_a_line() {
    for send in ["<S-CR>", "<C-CR>", "<A-CR>", "<C-s>"] {
        let mut e = ed("");
        run(&mut e, "ione");
        assert_eq!(run(&mut e, send), EditorAction::Send, "{send}");
        assert_eq!(e.mode(), Mode::Insert, "{send} leaves insert mode alone");
        assert_eq!(e.text(), "one", "{send} adds nothing");
    }
    let mut e = ed("");
    assert_eq!(run(&mut e, "ione<CR>two"), EditorAction::None);
    assert_eq!(e.text(), "one\ntwo");
}

#[test]
fn a_modified_enter_sends_in_normal_and_visual_mode_too_and_leaves_nothing_pending() {
    for send in ["<S-CR>", "<C-CR>", "<A-CR>"] {
        let mut e = ed("hi there");
        run(&mut e, "i<Esc>0");
        assert_eq!(e.mode(), Mode::Normal);
        assert_eq!(
            run(&mut e, send),
            EditorAction::Send,
            "{send} in normal mode"
        );
        assert_eq!(e.text(), "hi there", "{send} changes nothing");
        // In visual mode, where a plain Enter only cancels.
        run(&mut e, "v");
        assert_eq!(
            run(&mut e, send),
            EditorAction::Send,
            "{send} in visual mode"
        );
        run(&mut e, "<Esc><Esc>");
        // And in the middle of an operator, which it must not leave waiting for a motion: `w` is then a plain motion.
        run(&mut e, "d");
        assert_eq!(
            run(&mut e, send),
            EditorAction::Send,
            "{send} with d pending"
        );
        run(&mut e, "w");
        assert_eq!(
            e.text(),
            "hi there",
            "{send}: nothing was left pending, so w deleted nothing"
        );
    }
    // A plain Enter still only cancels what is pending, and sends when nothing is.
    let mut e = ed("hi");
    run(&mut e, "<Esc>d");
    assert_eq!(run(&mut e, "<CR>"), EditorAction::None);
    assert_eq!(run(&mut e, "<CR>"), EditorAction::Send);
}
#[test]
fn send_actions() {
    let mut e = ed("hi");
    assert_eq!(run(&mut e, "<CR>"), EditorAction::Send);
    assert_eq!(run(&mut e, "i<C-s>"), EditorAction::Send);
    assert_eq!(run(&mut e, "<A-CR>"), EditorAction::Send);
    assert_eq!(e.mode(), Mode::Insert);
    run(&mut e, "<Esc>");
    assert_eq!(run(&mut e, "<C-s>"), EditorAction::Send);
    run(&mut e, "v");
    assert_eq!(run(&mut e, "<C-s>"), EditorAction::Send);
}

#[test]
fn escape_action_only_when_idle() {
    let mut e = ed("hi");
    assert_eq!(run(&mut e, "<Esc>"), EditorAction::Escape);
    assert_eq!(run(&mut e, "d<Esc>"), EditorAction::None);
    assert_eq!(run(&mut e, "2<Esc>"), EditorAction::None);
    assert_eq!(e.pending(), "");
    assert_eq!(run(&mut e, "v<Esc>"), EditorAction::None);
    assert_eq!(e.mode(), Mode::Normal);
    assert_eq!(run(&mut e, "i<Esc>"), EditorAction::None);
    assert_eq!(run(&mut e, "<Esc>"), EditorAction::Escape);
}

#[test]
fn command_line() {
    let mut e = ed("hi");
    run(&mut e, ":");
    assert_eq!(e.command_line(), Some(""));
    run(&mut e, "wq");
    assert_eq!(e.command_line(), Some("wq"));
    run(&mut e, "<BS><BS>");
    assert_eq!(e.command_line(), Some(""));
    run(&mut e, "<BS>");
    assert_eq!(e.command_line(), None);
    run(&mut e, ":send");
    assert_eq!(run(&mut e, "<CR>"), EditorAction::Command("send".into()));
    assert_eq!(e.command_line(), None);
    run(&mut e, ":abc<Esc>");
    assert_eq!(e.command_line(), None);
    assert_eq!(t(&e), "hi");
}

#[test]
fn yank_and_delete_report_text() {
    let mut e = ed("foo bar");
    run(&mut e, "0");
    assert_eq!(run(&mut e, "yw"), EditorAction::Yanked("foo ".into()));
    assert_eq!(run(&mut e, "dd"), EditorAction::Yanked("foo bar\n".into()));
    assert!(e.is_empty());
}

// -- motions ---------------------------------------------------------------

#[test]
fn word_motions() {
    let mut e = at_start("foo.bar baz\nqux");
    run(&mut e, "w");
    assert_eq!(e.cursor(), (0, 3));
    run(&mut e, "w");
    assert_eq!(e.cursor(), (0, 4));
    run(&mut e, "w");
    assert_eq!(e.cursor(), (0, 8));
    run(&mut e, "w");
    assert_eq!(e.cursor(), (1, 0));
    run(&mut e, "b");
    assert_eq!(e.cursor(), (0, 8));
    run(&mut e, "0e");
    assert_eq!(e.cursor(), (0, 2));
    run(&mut e, "e");
    assert_eq!(e.cursor(), (0, 3));
    run(&mut e, "0W");
    assert_eq!(e.cursor(), (0, 8));
    run(&mut e, "0E");
    assert_eq!(e.cursor(), (0, 6));
    run(&mut e, "$B");
    assert_eq!(e.cursor(), (0, 8));
}

#[test]
fn counts_on_motions() {
    let mut e = at_start("a b c d e f");
    run(&mut e, "3w");
    assert_eq!(e.cursor(), (0, 6));
    run(&mut e, "2b");
    assert_eq!(e.cursor(), (0, 2));
    let mut e = at_start("1\n2\n3\n4\n5");
    run(&mut e, "3j");
    assert_eq!(e.cursor(), (3, 0));
    run(&mut e, "10k");
    assert_eq!(e.cursor(), (0, 0));
    run(&mut e, "10l");
    assert_eq!(e.cursor(), (0, 0));
    run(&mut e, "3G");
    assert_eq!(e.cursor().0, 2);
    run(&mut e, "G");
    assert_eq!(e.cursor().0, 4);
    run(&mut e, "2gg");
    assert_eq!(e.cursor().0, 1);
    run(&mut e, "gg");
    assert_eq!(e.cursor().0, 0);
}

#[test]
fn line_motions() {
    let mut e = at_start("   indented text");
    run(&mut e, "$");
    assert_eq!(e.cursor(), (0, 15));
    run(&mut e, "0");
    assert_eq!(e.cursor(), (0, 0));
    run(&mut e, "^");
    assert_eq!(e.cursor(), (0, 3));
    run(&mut e, "<End>");
    assert_eq!(e.cursor(), (0, 15));
    run(&mut e, "<Home>");
    assert_eq!(e.cursor(), (0, 0));
}

#[test]
fn vertical_motion_remembers_column() {
    let mut e = at_start("long line here\nab\nanother long one");
    run(&mut e, "$");
    run(&mut e, "j");
    assert_eq!(e.cursor(), (1, 1));
    run(&mut e, "j");
    assert_eq!(e.cursor(), (2, 15)); // `$` sticks to the end
    let mut e = at_start("long line here\nab\nanother long one");
    run(&mut e, "6l");
    run(&mut e, "jj");
    assert_eq!(e.cursor(), (2, 6));
    run(&mut e, "<Up><Up>");
    assert_eq!(e.cursor(), (0, 6));
}

#[test]
fn find_motions_and_repeat() {
    let mut e = at_start("a,b,c,d");
    run(&mut e, "f,");
    assert_eq!(e.cursor().1, 1);
    run(&mut e, ";");
    assert_eq!(e.cursor().1, 3);
    run(&mut e, ",");
    assert_eq!(e.cursor().1, 1);
    run(&mut e, "t,");
    assert_eq!(e.cursor().1, 2);
}

#[test]
fn till_and_backward_find() {
    let mut e = at_start("abc,def,ghi");
    run(&mut e, "t,");
    assert_eq!(e.cursor().1, 2);
    run(&mut e, ";");
    assert_eq!(e.cursor().1, 6);
    run(&mut e, "$F,");
    assert_eq!(e.cursor().1, 7);
    run(&mut e, "$2F,");
    assert_eq!(e.cursor().1, 3);
    run(&mut e, "$T,");
    assert_eq!(e.cursor().1, 8);
}

#[test]
fn paragraph_and_bracket_motions() {
    let mut e = at_start("a\nb\n\nc\nd\n\ne");
    run(&mut e, "}");
    assert_eq!(e.cursor().0, 2);
    run(&mut e, "}");
    assert_eq!(e.cursor().0, 5);
    run(&mut e, "{");
    assert_eq!(e.cursor().0, 2);
    run(&mut e, "{");
    assert_eq!(e.cursor().0, 0);
    let mut e = at_start("f(a, (b), c) x");
    run(&mut e, "%");
    assert_eq!(e.cursor(), (0, 11));
    run(&mut e, "%");
    assert_eq!(e.cursor(), (0, 1));
    let mut e = at_start("{\n  [x]\n}");
    run(&mut e, "%");
    assert_eq!(e.cursor(), (2, 0));
}

#[test]
fn half_page_scrolling() {
    let text: Vec<String> = (0..20).map(|i| i.to_string()).collect();
    let mut e = at_start(&text.join("\n"));
    run(&mut e, "<C-d>");
    assert_eq!(e.cursor().0, 5);
    run(&mut e, "<C-d><C-d><C-d><C-d><C-d>");
    assert_eq!(e.cursor().0, 19);
    run(&mut e, "<C-u>");
    assert_eq!(e.cursor().0, 14);
}

// -- operators ---------------------------------------------------------------

#[test]
fn delete_word_variants() {
    let mut e = at_start("foo bar baz");
    run(&mut e, "dw");
    assert_eq!(t(&e), "bar baz");
    run(&mut e, "2dw");
    assert_eq!(t(&e), "");
    let mut e = at_start("foo bar baz");
    run(&mut e, "wd$");
    assert_eq!(t(&e), "foo ");
    let mut e = at_start("foo bar");
    run(&mut e, "wd0");
    assert_eq!(t(&e), "bar");
    let mut e = at_start("foo bar");
    run(&mut e, "wdb");
    assert_eq!(t(&e), "bar");
    let mut e = at_start("foo bar");
    run(&mut e, "de");
    assert_eq!(t(&e), " bar");
}

#[test]
fn dw_on_last_word_does_not_join_lines() {
    let mut e = at_start("one two\nthree");
    run(&mut e, "wdw");
    assert_eq!(e.lines(), &["one ", "three"]);
    let mut e = at_start("one\n  two");
    run(&mut e, "dw");
    assert_eq!(e.lines(), &["", "  two"]);
}

#[test]
fn linewise_motions_with_operators() {
    let mut e = at_start("1\n2\n3\n4\n5");
    run(&mut e, "dj");
    assert_eq!(e.lines(), &["3", "4", "5"]);
    run(&mut e, "jdk");
    assert_eq!(e.lines(), &["5"]);
    run(&mut e, "dG");
    assert!(e.is_empty());
    let mut e = at_start("1\n2\n3\n4");
    run(&mut e, "jjdgg");
    assert_eq!(e.lines(), &["4"]);
    let mut e = at_start("1\n2\n3\n4");
    run(&mut e, "d2G");
    assert_eq!(e.lines(), &["3", "4"]);
    let mut e = at_start("1\n2");
    run(&mut e, "jdj");
    assert_eq!(t(&e), "1\n2"); // fails on the last line
}

#[test]
fn dd_cc_yy_and_counts() {
    let mut e = at_start("1\n2\n3\n4\n5");
    run(&mut e, "2dd");
    assert_eq!(e.lines(), &["3", "4", "5"]);
    run(&mut e, "d2d");
    assert_eq!(e.lines(), &["5"]);
    run(&mut e, "2d2d");
    assert!(e.is_empty());
    let mut e = at_start("  a\nb");
    run(&mut e, "ccx<Esc>");
    assert_eq!(e.lines(), &["  x", "b"]);
    let mut e = at_start("a\nb\nc");
    run(&mut e, "2yyGp");
    assert_eq!(e.lines(), &["a", "b", "c", "a", "b"]);
}

#[test]
fn x_variants() {
    let mut e = at_start("abcdef");
    run(&mut e, "x");
    assert_eq!(t(&e), "bcdef");
    run(&mut e, "2x");
    assert_eq!(t(&e), "def");
    run(&mut e, "$X");
    assert_eq!(t(&e), "df");
    run(&mut e, "10x");
    assert_eq!(t(&e), "d");
    run(&mut e, "x");
    assert_eq!(t(&e), "");
    run(&mut e, "x");
    assert_eq!(t(&e), "");
    let mut e = at_start("abc");
    run(&mut e, "<Del>");
    assert_eq!(t(&e), "bc");
}

#[test]
fn h_l_and_x_move_and_delete_by_whole_emoji_cluster_not_by_code_point() {
    // Heart + U+FE0F (request emoji presentation) is one glyph in any terminal that honours it, not two code points;
    // a single-codepoint emoji like 😀 (tested elsewhere) already moved correctly, but this one needs the fix.
    let mut e = at_start("\u{2764}\u{FE0F}ab");
    assert_eq!(e.cursor(), (0, 0));
    run(&mut e, "l");
    assert_eq!(
        e.cursor(),
        (0, 2),
        "l crosses the whole cluster in one step"
    );
    run(&mut e, "h");
    assert_eq!(e.cursor(), (0, 0), "h crosses back in one step");
    run(&mut e, "x");
    assert_eq!(
        t(&e),
        "ab",
        "x deletes the whole cluster, not just the selector"
    );

    // A count moves or deletes that many clusters, not that many code points.
    let mut e = at_start("\u{2764}\u{FE0F}\u{2764}\u{FE0F}ab");
    run(&mut e, "2x");
    assert_eq!(
        t(&e),
        "ab",
        "2x removed both emoji, not one emoji plus a stray selector"
    );
}

#[test]
fn insert_mode_arrows_backspace_and_delete_cross_a_whole_emoji_cluster() {
    let mut e = Editor::new();
    run(&mut e, "i\u{2764}\u{FE0F}ab<Home>");
    assert_eq!(e.cursor(), (0, 0));
    run(&mut e, "<Right>");
    assert_eq!(e.cursor(), (0, 2), "past the whole cluster in one step");
    run(&mut e, "<Left>");
    assert_eq!(e.cursor(), (0, 0), "back in one step");
    run(&mut e, "<Del>");
    assert_eq!(
        t(&e),
        "ab",
        "Delete removes the whole cluster under the cursor"
    );

    let mut e = Editor::new();
    run(&mut e, "i\u{2764}\u{FE0F}ab<Home><Right><BS>");
    assert_eq!(
        t(&e),
        "ab",
        "Backspace removes the whole cluster before the cursor"
    );
}

#[test]
fn capital_operators() {
    let mut e = at_start("hello world");
    run(&mut e, "wD");
    assert_eq!(t(&e), "hello ");
    let mut e = at_start("hello world");
    run(&mut e, "wCthere<Esc>");
    assert_eq!(t(&e), "hello there");
    let mut e = at_start("hello world");
    run(&mut e, "sj<Esc>");
    assert_eq!(t(&e), "jello world");
    let mut e = at_start("a\nb");
    run(&mut e, "YjP");
    assert_eq!(e.lines(), &["a", "a", "b"]);
    let mut e = at_start("a\nb");
    run(&mut e, "Sx<Esc>");
    assert_eq!(e.lines(), &["x", "b"]);
}

#[test]
fn s_on_empty_line_enters_insert() {
    let mut e = Editor::new();
    run(&mut e, "sabc<Esc>");
    assert_eq!(t(&e), "abc");
}

#[test]
fn change_word_behaves_like_ce() {
    let mut e = at_start("foo bar");
    run(&mut e, "cwxyz<Esc>");
    assert_eq!(t(&e), "xyz bar");
    let mut e = at_start("foo bar");
    run(&mut e, "$cwX<Esc>");
    assert_eq!(t(&e), "foo baX");
    let mut e = at_start("a b c d");
    run(&mut e, "2cwX<Esc>");
    assert_eq!(t(&e), "X c d");
    let mut e = at_start("ab cd");
    run(&mut e, "cWX<Esc>");
    assert_eq!(t(&e), "X cd");
    let mut e = at_start("a   b");
    run(&mut e, "lcwX<Esc>");
    assert_eq!(t(&e), "aXb");
}

#[test]
fn indent_and_dedent() {
    let mut e = at_start("a\nb\nc");
    run(&mut e, ">>");
    assert_eq!(e.lines(), &["  a", "b", "c"]);
    run(&mut e, "2>>");
    assert_eq!(e.lines(), &["    a", "  b", "c"]);
    run(&mut e, "<lt><lt>");
    assert_eq!(e.lines(), &["  a", "  b", "c"]);
    run(&mut e, ">j");
    assert_eq!(e.lines(), &["    a", "    b", "c"]);
    run(&mut e, "Vj<lt>");
    assert_eq!(e.lines(), &["  a", "  b", "c"]);
}

#[test]
fn join_lines() {
    let mut e = at_start("foo\n   bar\nbaz");
    run(&mut e, "J");
    assert_eq!(e.lines(), &["foo bar", "baz"]);
    assert_eq!(e.cursor(), (0, 3));
    let mut e = at_start("a\nb\nc\nd");
    run(&mut e, "3J");
    assert_eq!(e.lines(), &["a b c", "d"]);
    let mut e = at_start("f(\n)");
    run(&mut e, "J");
    assert_eq!(t(&e), "f()");
    let mut e = at_start("last");
    run(&mut e, "J");
    assert_eq!(t(&e), "last");
}

#[test]
fn toggle_case_and_replace() {
    let mut e = at_start("abC1");
    run(&mut e, "~");
    assert_eq!(t(&e), "AbC1");
    run(&mut e, "3~");
    assert_eq!(t(&e), "ABc1");
    let mut e = at_start("hello");
    run(&mut e, "rj");
    assert_eq!(t(&e), "jello");
    run(&mut e, "3rx");
    assert_eq!(t(&e), "xxxlo");
    assert_eq!(e.cursor(), (0, 2));
    run(&mut e, "9rz");
    assert_eq!(t(&e), "xxxlo");
}

// -- text objects ---------------------------------------------------------------

#[test]
fn word_text_objects() {
    let mut e = at_start("foo bar baz");
    run(&mut e, "wdiw");
    assert_eq!(t(&e), "foo  baz");
    let mut e = at_start("foo bar baz");
    run(&mut e, "wdaw");
    assert_eq!(t(&e), "foo baz");
    let mut e = at_start("foo bar");
    run(&mut e, "wdaw");
    assert_eq!(t(&e), "foo");
    let mut e = at_start("foo.bar baz");
    run(&mut e, "ciwX<Esc>");
    assert_eq!(t(&e), "X.bar baz");
    let mut e = at_start("foo.bar baz");
    run(&mut e, "ciWX<Esc>");
    assert_eq!(t(&e), "X baz");
    let mut e = at_start("foo.bar baz");
    run(&mut e, "daW");
    assert_eq!(t(&e), "baz");
}

#[test]
fn quote_text_objects() {
    let mut e = at_start(r#"say "hello there" now"#);
    run(&mut e, "ci\"X<Esc>");
    assert_eq!(t(&e), r#"say "X" now"#);
    let mut e = at_start(r#"say "hello there" now"#);
    run(&mut e, "fhda\"");
    assert_eq!(t(&e), "say now");
    let mut e = at_start("a 'b c' d 'e'");
    run(&mut e, "di'");
    assert_eq!(t(&e), "a '' d 'e'");
    let mut e = at_start("x `code` y");
    run(&mut e, "fcci`Z<Esc>");
    assert_eq!(t(&e), "x `Z` y");
    let mut e = at_start("no quotes");
    run(&mut e, "di\"");
    assert_eq!(t(&e), "no quotes");
}

#[test]
fn bracket_text_objects() {
    let mut e = at_start("f(a, (b), c)");
    run(&mut e, "fbdi(");
    assert_eq!(t(&e), "f(a, (), c)");
    let mut e = at_start("f(a, (b), c)");
    run(&mut e, "fbda(");
    assert_eq!(t(&e), "f(a, , c)");
    let mut e = at_start("f(a, (b), c)");
    run(&mut e, "fadib");
    assert_eq!(t(&e), "f()");
    let mut e = at_start("x[1, 2]");
    run(&mut e, "f1ci]9<Esc>");
    assert_eq!(t(&e), "x[9]");
    let mut e = at_start("{a}");
    run(&mut e, "di{");
    assert_eq!(t(&e), "{}");
    let mut e = at_start("{ a }");
    run(&mut e, "$da{");
    assert_eq!(t(&e), "");
}

#[test]
fn multiline_brace_inner_is_linewise() {
    let mut e = at_start("fn x() {\n    a;\n    b;\n}");
    run(&mut e, "jdi{");
    assert_eq!(e.lines(), &["fn x() {", "}"]);
}

#[test]
fn paragraph_text_objects() {
    let mut e = at_start("a\nb\n\nc\nd");
    run(&mut e, "dip");
    assert_eq!(e.lines(), &["", "c", "d"]);
    let mut e = at_start("a\nb\n\nc\nd");
    run(&mut e, "dap");
    assert_eq!(e.lines(), &["c", "d"]);
    let mut e = at_start("a\n\nc\nd");
    run(&mut e, "Gdap");
    assert_eq!(e.lines(), &["a"]);
}

// -- registers, paste, undo ---------------------------------------------------------------

#[test]
fn charwise_paste() {
    let mut e = at_start("abc");
    run(&mut e, "ylp");
    assert_eq!(t(&e), "aabc");
    assert_eq!(e.cursor(), (0, 1));
    run(&mut e, "3p");
    assert_eq!(t(&e), "aaaaabc");
    let mut e = at_start("one two");
    run(&mut e, "dwP");
    assert_eq!(t(&e), "one two");
    let mut e = at_start("one two");
    run(&mut e, "dwwp");
    assert_eq!(t(&e), "twoone ");
}

#[test]
fn linewise_paste_and_counts() {
    let mut e = at_start("a\nb");
    run(&mut e, "yyjp");
    assert_eq!(e.lines(), &["a", "b", "a"]);
    run(&mut e, "ggP");
    assert_eq!(e.lines(), &["a", "a", "b", "a"]);
    run(&mut e, "2p");
    assert_eq!(e.lines(), &["a", "a", "a", "a", "b", "a"]);
}

#[test]
fn multiline_charwise_paste() {
    let mut e = at_start("ab");
    e.set_register("X\nY".into(), false);
    run(&mut e, "p");
    assert_eq!(e.lines(), &["aX", "Yb"]);
    assert_eq!(e.cursor(), (0, 1));
}

#[test]
fn set_register_linewise_strips_newline() {
    let mut e = at_start("x");
    e.set_register("line\n".into(), true);
    run(&mut e, "p");
    assert_eq!(e.lines(), &["x", "line"]);
}

#[test]
fn named_and_blackhole_registers() {
    let mut e = at_start("one two three");
    run(&mut e, "\"ayw");
    assert_eq!(e.pending(), "");
    run(&mut e, "w\"bdw");
    run(&mut e, "\"_dw");
    assert_eq!(t(&e), "one ");
    run(&mut e, "$\"ap");
    assert_eq!(t(&e), "one one ");
    run(&mut e, "\"bp");
    assert_eq!(t(&e), "one one two ");
    // the black hole left the unnamed register holding what `"bdw` deleted
    run(&mut e, "p");
    assert_eq!(t(&e), "one one two two ");
}

#[test]
fn pending_display() {
    let mut e = ed("hello");
    run(&mut e, "2d");
    assert_eq!(e.pending(), "2d");
    run(&mut e, "3");
    assert_eq!(e.pending(), "2d3");
    run(&mut e, "<Esc>");
    run(&mut e, "g");
    assert_eq!(e.pending(), "g");
    run(&mut e, "<Esc>");
    run(&mut e, "\"a");
    assert_eq!(e.pending(), "\"a");
    run(&mut e, "<Esc>");
    run(&mut e, "\"");
    assert_eq!(e.pending(), "\"");
    run(&mut e, "<Esc>");
    run(&mut e, "dt");
    assert_eq!(e.pending(), "dt");
}

#[test]
fn undo_and_redo() {
    let mut e = at_start("one two");
    run(&mut e, "dw");
    run(&mut e, "x");
    assert_eq!(t(&e), "wo");
    run(&mut e, "u");
    assert_eq!(t(&e), "two");
    run(&mut e, "u");
    assert_eq!(t(&e), "one two");
    run(&mut e, "u");
    assert_eq!(t(&e), "one two");
    run(&mut e, "<C-r>");
    assert_eq!(t(&e), "two");
    run(&mut e, "<C-r><C-r>");
    assert_eq!(t(&e), "wo");
}

#[test]
fn insert_session_is_one_undo_step() {
    let mut e = Editor::new();
    run(&mut e, "ihello<CR>world<Esc>");
    run(&mut e, "Aok<Esc>");
    assert_eq!(e.lines(), &["hello", "worldok"]);
    run(&mut e, "u");
    assert_eq!(e.lines(), &["hello", "world"]);
    run(&mut e, "u");
    assert!(e.is_empty());
    run(&mut e, "<C-r>");
    assert_eq!(e.lines(), &["hello", "world"]);
}

#[test]
fn new_change_clears_redo() {
    let mut e = at_start("abc");
    run(&mut e, "xu");
    run(&mut e, "x");
    run(&mut e, "<C-r>");
    assert_eq!(t(&e), "bc");
}

#[test]
fn counted_undo() {
    let mut e = at_start("abcd");
    run(&mut e, "xxx");
    run(&mut e, "2u");
    assert_eq!(t(&e), "bcd");
}

#[test]
fn no_op_operations_preserve_redo_history() {
    // Paste with an empty register must not wipe redo history.
    let mut e = at_start("");
    run(&mut e, "iabc<Esc>");
    run(&mut e, "u");
    assert_eq!(t(&e), "");
    e.set_register("".into(), false);
    run(&mut e, "p");
    assert_eq!(t(&e), "", "pasting empty text is a no-op");
    run(&mut e, "<C-r>");
    assert_eq!(t(&e), "abc", "redo works after a no-op paste");
}

#[test]
fn a_change_over_nothing_still_undoes_what_was_typed_as_one_step() {
    // `c0` at the start of a line changes nothing, then text is typed: undo takes the typed text away and nothing
    // more, and the change before it stays.
    let mut e = at_start("hello");
    run(&mut e, "Ax<Esc>");
    assert_eq!(t(&e), "hellox");
    run(&mut e, "0c0Y<Esc>");
    assert_eq!(t(&e), "Yhellox");
    run(&mut e, "u");
    assert_eq!(
        t(&e),
        "hellox",
        "only what was typed after the empty change is undone"
    );
    run(&mut e, "u");
    assert_eq!(t(&e), "hello");
}
// -- insert entry ---------------------------------------------------------------

#[test]
fn insert_entry_commands() {
    let mut e = at_start("  abc");
    run(&mut e, "Ax<Esc>");
    assert_eq!(t(&e), "  abcx");
    run(&mut e, "Iy<Esc>");
    assert_eq!(t(&e), "  yabcx");
    run(&mut e, "0az<Esc>");
    assert_eq!(t(&e), " z yabcx");
    run(&mut e, "ib<Esc>");
    assert_eq!(t(&e), " bz yabcx");
}

#[test]
fn open_line_with_autoindent() {
    let mut e = at_start("  a\nb");
    run(&mut e, "ox<Esc>");
    assert_eq!(e.lines(), &["  a", "  x", "b"]);
    run(&mut e, "Oy<Esc>");
    assert_eq!(e.lines(), &["  a", "  y", "  x", "b"]);
    assert_eq!(e.cursor(), (1, 2));
}

#[test]
fn insert_with_count() {
    let mut e = Editor::new();
    run(&mut e, "3ia<Esc>");
    assert_eq!(t(&e), "aaa");
    let mut e = at_start("x");
    run(&mut e, "2ofoo<Esc>");
    assert_eq!(e.lines(), &["x", "foo", "foo"]);
    let mut e = at_start("x");
    run(&mut e, "2A-<Esc>");
    assert_eq!(t(&e), "x--");
}

// -- list continuation ---------------------------------------------------------------

#[test]
fn list_continuation() {
    let mut e = Editor::new();
    run(&mut e, "i- one<CR>two<CR>three");
    assert_eq!(e.lines(), &["- one", "- two", "- three"]);
    let mut e = Editor::new();
    run(&mut e, "i1. a<CR>b<CR>c");
    assert_eq!(e.lines(), &["1. a", "2. b", "3. c"]);
    let mut e = Editor::new();
    run(&mut e, "i> quote<CR>more");
    assert_eq!(e.lines(), &["> quote", "> more"]);
    let mut e = Editor::new();
    run(&mut e, "i  * nested<CR>x");
    assert_eq!(e.lines(), &["  * nested", "  * x"]);
    let mut e = Editor::new();
    run(&mut e, "i- [ ] todo<CR>next");
    assert_eq!(e.lines(), &["- [ ] todo", "- [ ] next"]);
}

#[test]
fn enter_inside_a_code_fence_does_not_continue_lists_or_quotes() {
    // Code often has lines that look like list items, numbered lines, comments or quotes.
    let mut e = Editor::new();
    run(
        &mut e,
        "i```py<CR>- not a list<CR>1. nor this<CR>> or this<CR>x",
    );
    assert_eq!(
        e.lines(),
        &["```py", "- not a list", "1. nor this", "> or this", "x"]
    );
    // An empty "list item" in code stays as typed instead of being deleted.
    let mut e = Editor::new();
    run(&mut e, "i```<CR>-<CR>-<CR>");
    assert_eq!(e.lines(), &["```", "-", "-", ""]);
    // Indentation still carries over, as in any code editor.
    let mut e = Editor::new();
    run(&mut e, "i```<CR>    - a<CR>b");
    assert_eq!(e.lines(), &["```", "    - a", "    b"]);
    // After the fence closes, lists continue again.
    let mut e = Editor::new();
    run(&mut e, "i```<CR>x<CR>```<CR>- a<CR>b");
    assert_eq!(e.lines(), &["```", "x", "```", "- a", "- b"]);
}

#[test]
fn enter_on_empty_list_item_ends_list() {
    let mut e = Editor::new();
    run(&mut e, "i- a<CR><CR>plain");
    assert_eq!(e.lines(), &["- a", "plain"]);
    let mut e = Editor::new();
    run(&mut e, "i1. a<CR><CR>");
    assert_eq!(e.lines(), &["1. a", ""]);
}

#[test]
fn enter_mid_list_item_splits_with_marker() {
    let mut e = Editor::new();
    run(&mut e, "i- foobar<Left><Left><Left><CR>");
    assert_eq!(e.lines(), &["- foo", "- bar"]);
    assert_eq!(e.cursor(), (1, 2));
    let mut e = Editor::new();
    run(&mut e, "i- x<Home><CR>");
    assert_eq!(e.lines(), &["", "- x"]); // cursor before the marker: plain newline
}

#[test]
fn plain_autoindent_on_enter() {
    let mut e = Editor::new();
    run(&mut e, "i    code<CR>more");
    assert_eq!(e.lines(), &["    code", "    more"]);
}

// -- dot repeat ---------------------------------------------------------------

#[test]
fn dot_repeats_delete_word() {
    let mut e = at_start("a b c d");
    run(&mut e, "dw..");
    assert_eq!(t(&e), "d");
}

#[test]
fn dot_repeats_counted_x() {
    let mut e = at_start("abcdefgh");
    run(&mut e, "3x.");
    assert_eq!(t(&e), "gh");
}

#[test]
fn dot_repeats_change_with_typed_text() {
    let mut e = at_start("foo bar baz");
    run(&mut e, "ciwX<Esc>w.w.");
    assert_eq!(t(&e), "X X X");
}

#[test]
fn dot_repeats_append_and_open() {
    let mut e = at_start("a\nb");
    run(&mut e, "A!<Esc>j.");
    assert_eq!(e.lines(), &["a!", "b!"]);
    let mut e = at_start("a");
    run(&mut e, "ox<Esc>.");
    assert_eq!(e.lines(), &["a", "x", "x"]);
}

#[test]
fn dot_repeats_dd_p_and_join() {
    let mut e = at_start("1\n2\n3\n4\n5\n6");
    run(&mut e, "dd.");
    assert_eq!(e.lines(), &["3", "4", "5", "6"]);
    run(&mut e, "J.");
    assert_eq!(e.lines(), &["3 4 5", "6"]);
    let mut e = at_start("a");
    run(&mut e, "yyp.");
    assert_eq!(e.lines(), &["a", "a", "a"]);
}

#[test]
fn dot_is_one_undo_step_and_ignores_motions() {
    let mut e = at_start("a b c d");
    run(&mut e, "dww.");
    assert_eq!(t(&e), "b d");
    run(&mut e, "u");
    assert_eq!(t(&e), "b c d");
}

#[test]
fn dot_with_nothing_recorded_is_harmless() {
    let mut e = at_start("abc");
    run(&mut e, ".");
    assert_eq!(t(&e), "abc");
}

#[test]
fn dot_repeat_survives_failed_motion() {
    let mut e = at_start("a\nb");
    run(&mut e, "x");
    run(&mut e, "jdj"); // fails: not a change
    run(&mut e, "gg.");
    assert_eq!(e.lines(), &["", "b"]);
}

// -- visual ---------------------------------------------------------------

#[test]
fn visual_selection_reporting() {
    let mut e = at_start("hello world");
    run(&mut e, "vee");
    assert_eq!(e.mode(), Mode::Visual);
    assert_eq!(e.selection(), Some(((0, 0), (0, 10), false)));
    run(&mut e, "o");
    assert_eq!(e.cursor(), (0, 0));
    assert_eq!(e.selection(), Some(((0, 0), (0, 10), false)));
    run(&mut e, "<Esc>");
    assert_eq!(e.selection(), None);
    let mut e = at_start("ab\ncd");
    run(&mut e, "Vj");
    assert_eq!(e.mode(), Mode::VisualLine);
    assert_eq!(e.selection(), Some(((0, 0), (1, 0), true)));
    run(&mut e, "V");
    assert_eq!(e.mode(), Mode::Normal);
}

#[test]
fn visual_delete_yank_change() {
    let mut e = at_start("hello world");
    run(&mut e, "vlld");
    assert_eq!(t(&e), "lo world");
    assert_eq!(e.mode(), Mode::Normal);
    let mut e = at_start("hello world");
    assert_eq!(run(&mut e, "veyP"), EditorAction::Yanked("hello".into()));
    assert_eq!(t(&e), "hellohello world");
    let mut e = at_start("hello world");
    run(&mut e, "vecbye<Esc>");
    assert_eq!(t(&e), "bye world");
    let mut e = at_start("hello world");
    run(&mut e, "wvex");
    assert_eq!(t(&e), "hello ");
}

#[test]
fn visual_across_lines_and_linewise() {
    let mut e = at_start("abc\ndef\nghi");
    run(&mut e, "lvjd");
    assert_eq!(e.lines(), &["af", "ghi"]);
    let mut e = at_start("abc\ndef\nghi");
    run(&mut e, "Vjd");
    assert_eq!(e.lines(), &["ghi"]);
    let mut e = at_start("abc\ndef\nghi");
    run(&mut e, "jVy");
    assert_eq!(e.cursor(), (1, 0));
    run(&mut e, "Gp");
    assert_eq!(e.lines(), &["abc", "def", "ghi", "def"]);
    let mut e = at_start("a\nb\nc");
    run(&mut e, "VjjcX<Esc>");
    assert_eq!(e.lines(), &["X"]);
}

#[test]
fn visual_indent_join_case() {
    let mut e = at_start("a\nb\nc");
    run(&mut e, "Vj>");
    assert_eq!(e.lines(), &["  a", "  b", "c"]);
    let mut e = at_start("a\nb\nc");
    run(&mut e, "VjJ");
    assert_eq!(e.lines(), &["a b", "c"]);
    let mut e = at_start("Hello World");
    run(&mut e, "veU");
    assert_eq!(t(&e), "HELLO World");
    run(&mut e, "veu");
    assert_eq!(t(&e), "hello World");
    run(&mut e, "ve~");
    assert_eq!(t(&e), "HELLO World");
    let mut e = at_start("abcd");
    run(&mut e, "vllrx");
    assert_eq!(t(&e), "xxxd");
}

#[test]
fn visual_text_objects() {
    let mut e = at_start("foo bar baz");
    run(&mut e, "wviwd");
    assert_eq!(t(&e), "foo  baz");
    let mut e = at_start("a(b c)d");
    run(&mut e, "fbvi(y");
    assert_eq!(e.cursor(), (0, 2));
    run(&mut e, "$p");
    assert_eq!(t(&e), "a(b c)db c");
    let mut e = at_start("a\nb\n\nc");
    run(&mut e, "vipd");
    assert_eq!(e.lines(), &["", "c"]);
}

#[test]
fn visual_paste_replaces_selection() {
    let mut e = at_start("foo bar");
    run(&mut e, "yiwwviwp");
    assert_eq!(t(&e), "foo foo");
    // the replaced text is now in the register
    run(&mut e, "0P");
    assert_eq!(t(&e), "barfoo foo");
}

#[test]
fn visual_empty_line_selection() {
    let mut e = at_start("a\n\nb");
    run(&mut e, "jvd");
    assert_eq!(e.lines(), &["a", "b"]);
}

#[test]
fn visual_escape_and_toggle() {
    let mut e = at_start("abc");
    run(&mut e, "vVv");
    assert_eq!(e.mode(), Mode::Visual);
    run(&mut e, "v");
    assert_eq!(e.mode(), Mode::Normal);
    assert_eq!(e.selection(), None);
}

// -- unicode ---------------------------------------------------------------

#[test]
fn multibyte_text() {
    let mut e = at_start("héllo wörld 😀 日本語");
    run(&mut e, "wdw");
    assert_eq!(t(&e), "héllo 😀 日本語");
    run(&mut e, "wx");
    assert_eq!(t(&e), "héllo 😀 本語");
    let mut e = at_start("日本語 text");
    run(&mut e, "lx");
    assert_eq!(t(&e), "日語 text");
    assert_eq!(e.cursor(), (0, 1));
    let mut e = at_start("😀😀😀");
    run(&mut e, "$ylp");
    assert_eq!(t(&e), "😀😀😀😀");
    let mut e = Editor::new();
    run(&mut e, "içà<BS>é<Esc>");
    assert_eq!(t(&e), "çé");
    let mut e = at_start("naïve café");
    run(&mut e, "fé~");
    assert_eq!(t(&e), "naïve cafÉ");
    let mut e = at_start("a😀b");
    run(&mut e, "lrX");
    assert_eq!(t(&e), "aXb");
    let mut e = at_start("«quoted»");
    run(&mut e, "$vbd");
    assert_eq!(t(&e), "«");
}

#[test]
fn multibyte_text_objects_and_find() {
    let mut e = at_start("say \"héllo wörld\" ok");
    run(&mut e, "fwci\"X<Esc>");
    assert_eq!(t(&e), "say \"X\" ok");
    let mut e = at_start("日本語, テスト");
    run(&mut e, "f,x");
    assert_eq!(t(&e), "日本語 テスト");
}

// -- paste API ---------------------------------------------------------------

#[test]
fn bracketed_paste() {
    let mut e = Editor::new();
    e.paste("hello\r\nworld");
    assert_eq!(e.lines(), &["hello", "world"]);
    let mut e = Editor::new();
    run(&mut e, "ia");
    e.paste("XY\nZ");
    run(&mut e, "b<Esc>");
    assert_eq!(e.lines(), &["aXY", "Zb"]);
    run(&mut e, "u");
    assert!(e.is_empty());
}

#[test]
fn paste_in_normal_and_visual_mode_and_command_line() {
    let mut e = at_start("ab");
    e.paste("X");
    assert_eq!(t(&e), "Xab");
    run(&mut e, "u");
    assert_eq!(t(&e), "ab");
    run(&mut e, "v");
    e.paste("Y");
    assert_eq!(e.mode(), Mode::Normal);
    run(&mut e, ":");
    e.paste("a\nb");
    assert_eq!(e.command_line(), Some("ab"));
}

// -- API misc ---------------------------------------------------------------

#[test]
fn set_mode_and_is_empty() {
    let mut e = Editor::new();
    e.set_mode(Mode::Insert);
    run(&mut e, "  ");
    assert!(e.is_empty());
    run(&mut e, "x");
    assert!(!e.is_empty());
    e.set_mode(Mode::Normal);
    assert_eq!(e.mode(), Mode::Normal);
    e.set_mode(Mode::Visual);
    assert!(e.selection().is_some());
}

#[test]
fn release_events_are_ignored() {
    let mut e = Editor::new();
    let mut ev = key(KeyCode::Char('i'), KeyModifiers::NONE);
    ev.kind = KeyEventKind::Release;
    e.handle_key(ev);
    assert_eq!(e.mode(), Mode::Normal);
}

#[test]
fn markdown_composition_session() {
    let mut e = Editor::new();
    run(
        &mut e,
        "i**Release notes**<CR><CR>- fixed bug<CR>added `foo`<CR><CR><Esc>",
    );
    assert_eq!(
        e.lines(),
        &["**Release notes**", "", "- fixed bug", "- added `foo`", ""]
    );
    run(&mut e, "ki```rust<CR>fn main() {}<CR>```<Esc>");
    assert!(t(&e).contains("```rust\nfn main() {}\n```"));
}

// -- robustness ---------------------------------------------------------------

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

#[test]
fn random_keys_never_panic_and_keep_invariants() {
    const ALPHABET: &[char] = &[
        'h', 'j', 'k', 'l', 'w', 'b', 'e', 'W', 'B', 'E', '0', '^', '$', 'g', 'G', 'f', 'F', 't',
        'T', ';', ',', '{', '}', '%', 'd', 'c', 'y', '>', '<', 'x', 'X', 's', 'S', 'D', 'C', 'Y',
        'i', 'a', 'I', 'A', 'o', 'O', 'p', 'P', 'u', 'J', '~', 'r', '.', ':', 'v', 'V', '"', '1',
        '2', '3', 'U', ' ', '(', ')', '[', ']', '{', '}', '`', '\'', 'é', '😀', '日', '-', '*',
        '#', 'q', 'z', 'n', 'm',
    ];
    let specials = [
        KeyCode::Esc,
        KeyCode::Enter,
        KeyCode::Backspace,
        KeyCode::Delete,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Tab,
    ];
    for seed in 0..12u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9e37_79b9) + 1);
        let mut e =
            Editor::from_text("fn main() {\n    println!(\"hi\");\n}\n\n- item é😀\n日本語 text");
        for _ in 0..1000 {
            let r = rng.next();
            let ev = match r % 10 {
                0 => key(
                    specials[(rng.next() as usize) % specials.len()],
                    KeyModifiers::NONE,
                ),
                1 => {
                    let c = ['r', 'd', 'u', 'w', 'h', '[', 'o', 's'][(rng.next() as usize) % 8];
                    key(KeyCode::Char(c), KeyModifiers::CONTROL)
                }
                _ => key(
                    KeyCode::Char(ALPHABET[(rng.next() as usize) % ALPHABET.len()]),
                    KeyModifiers::NONE,
                ),
            };
            e.handle_key(ev);
            if e.buf.iter().map(Vec::len).sum::<usize>() > 5_000 {
                e.set_text("x\ny");
            }
            check_invariants(&e);
        }
    }
}

#[test]
fn huge_counts_are_bounded() {
    let mut e = at_start("some text here");
    run(&mut e, "9999999yyp");
    run(&mut e, "9999p");
    run(&mut e, "9999999999ix<Esc>");
    run(&mut e, "9999J");
    run(&mut e, "99999x");
    assert!(e.buf.iter().map(Vec::len).sum::<usize>() <= MAX_CHARS + 10_000);
}

#[test]
fn x_deletes_whole_emoji_with_selector_at_end_of_insert() {
    let mut e = ed("");
    run(&mut e, "ia\u{2764}\u{FE0F}<Esc>x");
    assert_eq!(t(&e), "a");
}

#[test]
fn dollar_x_deletes_whole_emoji_with_selector() {
    let mut e = at_start("a\u{2764}\u{FE0F}");
    run(&mut e, "$x");
    assert_eq!(t(&e), "a");
}

#[test]
fn dollar_places_cursor_on_cluster_start_not_within_emoji() {
    let mut e = at_start("a\u{2764}\u{FE0F}");
    run(&mut e, "$");
    assert_eq!(e.cursor(), (0, 1));
}

#[test]
fn append_after_emoji_with_selector_places_insert_after_whole_cluster() {
    let mut e = at_start("\u{2764}\u{FE0F}b");
    run(&mut e, "ax<Esc>");
    assert_eq!(t(&e), "\u{2764}\u{FE0F}xb");
}
