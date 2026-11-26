//! A multi-line text editor for composing chat messages (Markdown), with two ways of being driven.
//!
//! * [`Keymap::Standard`] (the default) is modeless, as in a text box or an ordinary editor: type, select with Shift,
//!   Ctrl-C / X / V / Z / Y, Enter to send. See `standard.rs`.
//! * [`Keymap::Vim`] is modal and vim-like, described below.
//!
//! This module is pure logic: it consumes crossterm key events and exposes the buffer, cursor, mode and
//! selection so the UI layer can draw it however it likes. Clipboard access is deliberately left to the caller:
//! yanks and deletes are reported through [`EditorAction::Yanked`], and the caller can seed the unnamed register
//! with [`Editor::set_register`] before a `p`.
//!
//! Implemented (vim semantics, as faithfully as is reasonable):
//!
//! * Modes: Normal, Insert, Visual (`v`), Visual-line (`V`), and a `:` command line.
//! * Motions with counts: `h j k l w b e W B E 0 ^ $ gg G f F t T ; , { } %`, arrows, Ctrl-d/Ctrl-u.
//! * Operators `d c y > <` combined with motions and text objects (`iw aw iW aW i" a" i' a' i` a` ib ab i( a(
//!   i[ a[ i{ a{ i< a< ip ap`), plus `dd cc yy >> <<`, `D C Y x X s S J ~ r p P`, `u`, Ctrl-r and `.`.
//! * Insert entry with counts: `i a I A o O`; Markdown list / quote continuation on Enter.
//! * Named registers (`"a`) and the black hole register (`"_`).
//!
//! Deliberately not implemented: macros (`q`), marks, search (`/`), `gu`/`gU`, `ge`, `H M L`, counts for text
//! objects (`2iw`), block-wise visual mode, dot-repeat of visual-mode operators, and `Ctrl-o` in insert mode.

use std::collections::HashMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

mod buffer;
mod insert;
mod motion;
mod normal;
mod operators;
mod standard;
#[cfg(test)]
mod standard_tests;
#[cfg(test)]
mod tests;

/// (row, column) where the column is a `char` index into the line.
type Pos = (usize, usize);
/// Normalised (start, end inclusive, linewise).
pub type Selection = (Pos, Pos, bool);

const MAX_COUNT: usize = 9_999;
/// Repeating operations (paste, insert counts) stop growing the buffer beyond this many characters.
const MAX_CHARS: usize = 200_000;
const UNDO_LIMIT: usize = 200;
const HALF_PAGE: usize = 5;
const INDENT: &str = "  ";

/// How keys are interpreted.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Keymap {
    /// Modal editing as in vim.
    Vim,
    /// Modeless editing as in a text box: Shift selects, Ctrl-C / X / V copy, cut and paste, Enter sends.
    #[default]
    Standard,
}

impl Keymap {
    /// The keymap named `s` (`vim` or `standard`, in any case).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "vim" => Some(Self::Vim),
            "standard" => Some(Self::Standard),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Vim => "vim",
            Self::Standard => "standard",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    VisualLine,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EditorAction {
    None,
    Send,
    /// Esc pressed in Normal mode with nothing pending: the caller should leave the editor.
    Escape,
    /// Text just yanked or deleted (linewise text ends with `\n`), for mirroring to the system clipboard.
    Yanked(String),
    /// A `:cmd` line was submitted (without the colon).
    Command(String),
    /// The standard keymap asks for the clipboard's text: the caller answers with [`Editor::paste`].
    Paste,
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Exclusive,
    Inclusive,
    Linewise,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Want {
    Reset,
    Set(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Motion {
    Left,
    Right,
    Up,
    Down,
    Page(bool),
    WordFwd(bool),
    WordBack(bool),
    WordEnd(bool),
    LineStart,
    FirstNonBlank,
    LineEnd,
    GotoLast,
    GotoFirst,
    Find(char, char),
    RepeatFind(bool),
    ParaFwd,
    ParaBack,
    Match,
}

struct Mo {
    pos: Pos,
    kind: Kind,
    want: Want,
}

#[derive(Clone)]
struct Reg {
    /// Linewise registers store their lines joined with `\n` and *without* a trailing newline.
    text: String,
    linewise: bool,
}

struct Snap {
    buf: Vec<Vec<char>>,
    cursor: Pos,
}

pub struct Editor {
    keymap: Keymap,
    /// Whether a plain Enter sends in the standard keymap (the default; Shift/Ctrl/Alt-Enter then adds a line). When
    /// it does not, Enter adds a line and Shift/Ctrl/Alt-Enter sends.
    enter_sends: bool,
    /// The other end of the selection in the standard keymap (the cursor is one end).
    sel: Option<Pos>,
    /// What the last edit was in the standard keymap, so a run of typing is one step of undo.
    run: Option<standard::Run>,
    buf: Vec<Vec<char>>,
    lines: Vec<String>,
    row: usize,
    col: usize,
    /// Desired column for vertical motions (`usize::MAX` after `$`); `None` means "the current column".
    want: Option<usize>,
    mode: Mode,
    anchor: Pos,
    // Pending command state.
    cnt1: usize,
    cnt2: usize,
    op: Option<char>,
    prefix: Option<char>,
    reg_sel: Option<char>,
    regs: HashMap<char, Reg>,
    last_find: Option<(char, char)>,
    cmd: Option<String>,
    // History.
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    version: u64,
    // Dot repeat.
    cur_keys: Vec<KeyEvent>,
    is_change: bool,
    last_change: Vec<KeyEvent>,
    replaying: bool,
    // Current insert session (for `3ifoo<Esc>` style repeats).
    ins_count: usize,
    ins_prefix: String,
    ins_log: String,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

fn toggle_case(c: char) -> String {
    if c.is_lowercase() {
        c.to_uppercase().collect()
    } else if c.is_uppercase() {
        c.to_lowercase().collect()
    } else {
        c.to_string()
    }
}

fn is_blank(line: &[char]) -> bool {
    line.iter().all(|c| c.is_whitespace())
}

/// Detects a Markdown list item / quote marker at the start of `line`. Returns the index just past the marker
/// and the marker to start the next line with.
fn list_marker(line: &[char]) -> Option<(usize, String)> {
    let ind = line
        .iter()
        .take_while(|c| **c == ' ' || **c == '\t')
        .count();
    let indent: String = line[..ind].iter().collect();
    let rest = &line[ind..];
    if rest.len() >= 2 && matches!(rest[0], '-' | '*' | '+') && rest[1] == ' ' {
        let b = rest[0];
        if rest.len() >= 6
            && rest[2] == '['
            && matches!(rest[3], ' ' | 'x' | 'X')
            && rest[4] == ']'
            && rest[5] == ' '
        {
            return Some((ind + 6, format!("{indent}{b} [ ] ")));
        }
        return Some((ind + 2, format!("{indent}{b} ")));
    }
    if rest.len() >= 2 && rest[0] == '>' && rest[1] == ' ' {
        return Some((ind + 2, format!("{indent}> ")));
    }
    let d = rest.iter().take_while(|c| c.is_ascii_digit()).count();
    if (1..=9).contains(&d)
        && rest.len() > d + 1
        && matches!(rest[d], '.' | ')')
        && rest[d + 1] == ' '
    {
        let n: u64 = rest[..d].iter().collect::<String>().parse().ok()?;
        return Some((ind + d + 2, format!("{indent}{}{} ", n + 1, rest[d])));
    }
    None
}

fn bracket_pair(c: char) -> Option<(char, char)> {
    match c {
        '(' | ')' | 'b' => Some(('(', ')')),
        '[' | ']' => Some(('[', ']')),
        '{' | '}' | 'B' => Some(('{', '}')),
        '<' | '>' => Some(('<', '>')),
        _ => None,
    }
}

fn motion_for(code: KeyCode, ctrl: bool) -> Option<Motion> {
    if ctrl {
        return match code {
            KeyCode::Char('d') => Some(Motion::Page(true)),
            KeyCode::Char('u') => Some(Motion::Page(false)),
            KeyCode::Char('h') => Some(Motion::Left),
            _ => None,
        };
    }
    Some(match code {
        KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => Motion::Left,
        KeyCode::Right | KeyCode::Char('l' | ' ') => Motion::Right,
        KeyCode::Up | KeyCode::Char('k') => Motion::Up,
        KeyCode::Down | KeyCode::Char('j') => Motion::Down,
        KeyCode::Home => Motion::LineStart,
        KeyCode::End => Motion::LineEnd,
        KeyCode::Char('w') => Motion::WordFwd(false),
        KeyCode::Char('W') => Motion::WordFwd(true),
        KeyCode::Char('b') => Motion::WordBack(false),
        KeyCode::Char('B') => Motion::WordBack(true),
        KeyCode::Char('e') => Motion::WordEnd(false),
        KeyCode::Char('E') => Motion::WordEnd(true),
        KeyCode::Char('0') => Motion::LineStart,
        KeyCode::Char('^') => Motion::FirstNonBlank,
        KeyCode::Char('$') => Motion::LineEnd,
        KeyCode::Char('G') => Motion::GotoLast,
        KeyCode::Char('{') => Motion::ParaBack,
        KeyCode::Char('}') => Motion::ParaFwd,
        KeyCode::Char('%') => Motion::Match,
        KeyCode::Char(';') => Motion::RepeatFind(false),
        KeyCode::Char(',') => Motion::RepeatFind(true),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl Editor {
    /// A vim-keymap editor, which is what the editor's own tests exercise. The app builds its editor with
    /// [`Editor::with_keymap`] and the keymap the user chose (standard unless they asked for vim).
    pub fn new() -> Self {
        Self {
            keymap: Keymap::Vim,
            enter_sends: true,
            sel: None,
            run: None,
            buf: vec![Vec::new()],
            lines: vec![String::new()],
            row: 0,
            col: 0,
            want: None,
            mode: Mode::Normal,
            anchor: (0, 0),
            cnt1: 0,
            cnt2: 0,
            op: None,
            prefix: None,
            reg_sel: None,
            regs: HashMap::new(),
            last_find: None,
            cmd: None,
            undo: Vec::new(),
            redo: Vec::new(),
            version: 0,
            cur_keys: Vec::new(),
            is_change: false,
            last_change: Vec::new(),
            replaying: false,
            ins_count: 1,
            ins_prefix: String::new(),
            ins_log: String::new(),
        }
    }

    /// An editor driven by `keymap`.
    pub fn with_keymap(keymap: Keymap) -> Self {
        let mut e = Self::new();
        e.set_keymap(keymap);
        e
    }

    /// Choose what a plain Enter does in the standard keymap: send (`true`, the default) or add a line. The other
    /// way of pressing Enter (with Shift, Ctrl or Alt) does the opposite; Ctrl-S always sends and Ctrl-J always adds a
    /// line, which is what a terminal that cannot tell a modified Enter from Enter (SSH, mostly) leaves.
    pub fn set_enter_sends(&mut self, sends: bool) {
        self.enter_sends = sends;
    }

    /// Switch keymaps. The standard keymap has no modes, so it stays in insert mode; vim starts in normal mode.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
        self.cmd = None;
        self.sel = None;
        self.run = None;
        self.reset_pending();
        self.mode = match keymap {
            Keymap::Vim => Mode::Normal,
            Keymap::Standard => Mode::Insert,
        };
        self.clamp();
    }

    #[cfg(test)]
    pub fn from_text(s: &str) -> Self {
        let mut e = Self::new();
        e.set_text(s);
        e
    }

    /// The buffer, lines joined with `\n`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Replaces the buffer, puts the cursor at the end and clears the undo history. The mode is kept
    /// (except that Visual modes are left).
    pub fn set_text(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.buf = s.split('\n').map(|l| l.chars().collect()).collect();
        self.undo.clear();
        self.redo.clear();
        self.sel = None;
        self.run = None;
        self.reset_pending();
        self.cmd = None;
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            self.mode = Mode::Normal;
        }
        self.row = self.buf.len() - 1;
        self.col = self.buf[self.row].len();
        self.want = None;
        self.sync();
        self.clamp();
    }

    /// Empties the buffer, keeps the mode and clears the undo history.
    pub fn clear(&mut self) {
        let mode = self.mode;
        self.set_text("");
        self.mode = mode;
        self.clamp();
    }

    /// True when the buffer holds only whitespace.
    pub fn is_empty(&self) -> bool {
        self.buf.iter().all(|l| is_blank(l))
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, m: Mode) {
        if self.keymap == Keymap::Standard {
            // No modes here: asking for one only ends a selection.
            self.sel = None;
            self.run = None;
            return;
        }
        self.cmd = None;
        self.reset_pending();
        match m {
            Mode::Insert => self.enter_insert(1, ""),
            Mode::Visual | Mode::VisualLine => {
                if matches!(self.mode, Mode::Normal | Mode::Insert) {
                    self.anchor = (self.row, self.col);
                }
                self.mode = m;
            }
            Mode::Normal => self.mode = Mode::Normal,
        }
        self.clamp();
    }

    /// Cursor as (row, char column). In Normal/Visual mode the column is on a character (0 on an empty line);
    /// in Insert mode it may be one past the end.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// Move the cursor to (`row`, `col`), as an arrow key would: in the standard keymap with `extend` the selection grows
    /// from where the cursor was, otherwise any selection ends. The place is made valid (a line that is shorter, the
    /// end of a vim line in Normal mode). For callers that know where a screen row is, which the editor does not.
    pub fn move_to(&mut self, (row, col): (usize, usize), extend: bool) {
        self.run = None;
        let from = self.clamp_pos((self.row, self.col));
        if self.keymap == Keymap::Standard {
            if extend {
                if self.sel.is_none() {
                    self.sel = Some(from);
                }
            } else {
                self.sel = None;
            }
        }
        let mut to = self.clamp_pos((row, col));
        let line_len = self.len_of(to.0);
        if to.1 < line_len {
            to.1 = self.cluster_before(to.0, to.1 + 1);
        }
        (self.row, self.col) = to;
        if self.sel == Some(to) {
            self.sel = None;
        }
        self.want = None;
        self.clamp();
    }

    /// Normalised (start, end inclusive, linewise) while in a Visual mode.
    pub fn selection(&self) -> Option<Selection> {
        if self.keymap == Keymap::Standard {
            // The cursor sits between characters, so the last selected one is just before the later end.
            let (start, end) = self.sel_span()?;
            let last = if end.1 > 0 {
                (end.0, end.1 - 1)
            } else {
                (
                    end.0.saturating_sub(1),
                    self.len_of(end.0.saturating_sub(1)),
                )
            };
            return Some((start, last, false));
        }
        if !matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            return None;
        }
        let cur = (self.row, self.col);
        let (a, b) = if self.anchor <= cur {
            (self.anchor, cur)
        } else {
            (cur, self.anchor)
        };
        Some((a, b, self.mode == Mode::VisualLine))
    }

    /// Keys typed so far in an incomplete command, e.g. `2d`, `"a`, `g`.
    pub fn pending(&self) -> String {
        let mut s = String::new();
        if let Some(r) = self.reg_sel {
            s.push('"');
            s.push(r);
        } else if self.prefix == Some('"') {
            s.push('"');
        }
        if self.cnt1 > 0 {
            s.push_str(&self.cnt1.to_string());
        }
        if let Some(op) = self.op {
            s.push(op);
        }
        if self.cnt2 > 0 {
            s.push_str(&self.cnt2.to_string());
        }
        if let Some(p) = self.prefix.filter(|p| *p != '"') {
            s.push(p);
        }
        s
    }

    /// The text after the `:` while a command line is open.
    pub fn command_line(&self) -> Option<&str> {
        self.cmd.as_deref()
    }

    /// Increases whenever the buffer changes (useful for caching syntax highlighting).
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Seeds the unnamed register (e.g. from the system clipboard). For linewise text a single trailing
    /// newline is dropped.
    pub fn set_register(&mut self, text: String, linewise: bool) {
        let text = text.replace("\r\n", "\n");
        let text = if linewise {
            text.strip_suffix('\n').unwrap_or(&text).to_string()
        } else {
            text
        };
        self.regs.insert('"', Reg { text, linewise });
    }

    /// Inserts `text` at the cursor (bracketed paste), whatever the mode.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Some(cmd) = self.cmd.as_mut() {
            cmd.extend(text.chars().filter(|c| *c != '\n'));
            return;
        }
        if text.is_empty() {
            return;
        }
        if self.keymap == Keymap::Standard {
            // Pasting replaces the selection and is one step of undo.
            let room = MAX_CHARS.saturating_sub(self.total_chars());
            let text: String = text.chars().take(room).collect();
            self.run = None;
            self.save_undo();
            self.delete_selection();
            let at = self.clamp_pos((self.row, self.col));
            let end = self.replace_range(at, at, &text);
            self.row = end.0;
            self.col = end.1;
            self.clamp();
            return;
        }
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            self.mode = Mode::Normal;
        }
        self.reset_pending();
        if self.mode != Mode::Insert {
            self.save_undo();
        }
        let at = self.clamp_pos((self.row, self.col));
        let end = self.replace_range(at, at, &text);
        self.row = end.0;
        self.col = end.1;
        if self.mode == Mode::Insert {
            self.ins_log.push_str(&text);
        }
        self.clamp();
    }

    /// The `@name` being typed just before the cursor: the column of its `@` and the text after it. None unless the
    /// cursor is at the end of something that can be a mention: typing (standard keymap, or vim's Insert mode) with no
    /// selection, outside a fenced or inline code span, an `@` that starts a word (not an address such as `ada@x.org`),
    /// and a short name of at most three words with no other `@` in it.
    pub fn mention_at_cursor(&self) -> Option<(usize, String)> {
        const MAX_QUERY_CHARS: usize = 30;
        let typing = match self.keymap {
            Keymap::Standard => self.sel.is_none(),
            Keymap::Vim => self.mode == Mode::Insert,
        };
        if !typing || self.cmd.is_some() || self.in_open_fence() {
            return None;
        }
        let line = self.buf.get(self.row)?;
        let before = &line[..self.col.min(line.len())];
        let at = before.iter().rposition(|c| *c == '@')?;
        if at > 0 {
            let prev = before[at - 1];
            if prev.is_alphanumeric() || matches!(prev, '@' | '.' | '_' | '/' | ':' | '-' | '`') {
                return None;
            }
        }
        // Inside an inline code span: an odd number of backticks before it.
        if before[..at].iter().filter(|c| **c == '`').count() % 2 == 1 {
            return None;
        }
        let query: String = before[at + 1..].iter().collect();
        if query.chars().count() > MAX_QUERY_CHARS
            || query.split(' ').count() > 3
            || query.chars().any(|c| c.is_control() || c == '`')
        {
            return None;
        }
        Some((at, query))
    }

    /// Replaces what was typed from column `from` of the cursor's line up to the cursor with `text` (one step of undo).
    /// For a completion; the caller knows where it began.
    pub fn replace_before_cursor(&mut self, from: usize, text: &str) {
        let row = self.row.min(self.last_row());
        let col = self.col.min(self.buf[row].len());
        let from = from.min(col);
        self.run = None;
        self.save_undo();
        self.sel = None;
        let end = self.replace_range((row, from), (row, col), text);
        self.row = end.0;
        self.col = end.1;
        self.want = None;
        self.clamp();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EditorAction {
        if key.kind == KeyEventKind::Release {
            return EditorAction::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('s') {
            return EditorAction::Send;
        }
        if self.keymap == Keymap::Standard {
            return self.standard_key(key);
        }
        if self.cmd.is_some() {
            return self.cmd_key(key);
        }
        if !self.replaying {
            self.cur_keys.push(key);
        }
        let action = if self.mode == Mode::Insert {
            self.insert_key(key)
        } else {
            self.normal_key(key)
        };
        self.finish_key();
        action
    }
}
