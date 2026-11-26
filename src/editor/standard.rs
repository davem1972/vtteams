//! Key handling for the standard keymap: modeless editing as in a text box, a chat app or an ordinary editor.
//!
//! The editor is always ready for typing. Shift with a movement key selects, typing replaces the selection, and the
//! usual shortcuts apply: Ctrl-A select all, Ctrl-C / Ctrl-X copy / cut, Ctrl-V paste (reported as
//! [`EditorAction::Paste`], because the clipboard is the caller's), Ctrl-Z undo, Ctrl-Y or Ctrl-Shift-Z redo, Ctrl or
//! Alt with the arrow keys moves by word, Ctrl-Backspace and Ctrl-Delete delete a word, Ctrl-Home / Ctrl-End go to the
//! start and end. Enter sends; Shift-Enter, Ctrl-Enter, Alt-Enter or Ctrl-J starts a new line (with [Editor::set_enter_sends] off it is the other way round, except that Ctrl-J still adds a line and Ctrl-S sends), and so does Enter inside
//! a fenced code block that is still open, so code can be typed. Esc clears the selection, then leaves the editor.
//!
//! The mode is [`Mode::Insert`] throughout, so what draws or reacts to the editor (the cursor shape, the caller's
//! pane switching) needs no special case.

use super::*;

/// What the last edit was, so that a run of typing (or of erasing) is one step of undo.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Run {
    Typing,
    Erasing,
}

/// Rows that PageUp and PageDown move.
const PAGE_ROWS: usize = 5;

impl Editor {
    pub(super) fn standard_key(&mut self, key: KeyEvent) -> EditorAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let action = match key.code {
            KeyCode::Esc => {
                self.run = None;
                if self.sel.take().is_some() {
                    EditorAction::None
                } else {
                    EditorAction::Escape
                }
            }
            KeyCode::Enter => {
                let modified = shift || ctrl || alt;
                if modified == self.enter_sends || self.enter_sends && self.in_open_fence() {
                    self.type_newline();
                    EditorAction::None
                } else {
                    EditorAction::Send
                }
            }
            KeyCode::Char(c) if ctrl && !alt => self.control_key(c, shift),
            KeyCode::Char(c) if !ctrl && !alt || ctrl && alt => {
                self.type_str(&c.to_string());
                EditorAction::None
            }
            KeyCode::Tab => {
                self.type_str(INDENT);
                EditorAction::None
            }
            KeyCode::BackTab => {
                self.unindent();
                EditorAction::None
            }
            KeyCode::Backspace => {
                if ctrl || alt {
                    self.delete_word_back();
                } else {
                    self.erase_back();
                }
                EditorAction::None
            }
            KeyCode::Delete => {
                if ctrl || alt {
                    self.delete_word_forward();
                } else {
                    self.erase_forward();
                }
                EditorAction::None
            }
            KeyCode::Insert if shift => EditorAction::Paste,
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown => {
                self.move_standard(key.code, ctrl || alt, shift);
                EditorAction::None
            }
            _ => EditorAction::None,
        };
        // Nothing in this keymap uses the insert session's log, which only the vim keymap repeats.
        self.ins_log.clear();
        self.clamp();
        action
    }

    fn control_key(&mut self, c: char, shift: bool) -> EditorAction {
        match c.to_ascii_lowercase() {
            'a' => {
                self.run = None;
                let last = self.last_row();
                self.sel = Some((0, 0));
                self.row = last;
                self.col = self.len_of(last);
                EditorAction::None
            }
            'c' => self
                .copy_selection()
                .map_or(EditorAction::None, EditorAction::Yanked),
            'x' => match self.copy_selection() {
                Some(text) => {
                    self.begin_edit(Run::Erasing, true);
                    self.delete_selection();
                    EditorAction::Yanked(text)
                }
                None => EditorAction::None,
            },
            'v' => EditorAction::Paste,
            'z' if shift => {
                self.redo_standard();
                EditorAction::None
            }
            'z' => {
                self.undo_standard();
                EditorAction::None
            }
            'y' => {
                self.redo_standard();
                EditorAction::None
            }
            'j' => {
                self.type_newline();
                EditorAction::None
            }
            // Many terminals send Ctrl-H for Ctrl-Backspace, and Ctrl-W is the habit of anyone who uses a shell.
            'h' | 'w' => {
                self.delete_word_back();
                EditorAction::None
            }
            _ => EditorAction::None,
        }
    }

    // -- selection and clipboard ------------------------------------------------

    /// The selection as (start, end), end exclusive, if there is a non-empty one.
    pub(super) fn sel_span(&self) -> Option<(Pos, Pos)> {
        let a = self.clamp_pos(self.sel?);
        let cur = (self.row, self.col);
        match a.cmp(&cur) {
            std::cmp::Ordering::Less => Some((a, cur)),
            std::cmp::Ordering::Greater => Some((cur, a)),
            std::cmp::Ordering::Equal => None,
        }
    }

    /// The selected text, also kept in the unnamed register. The selection stays.
    fn copy_selection(&mut self) -> Option<String> {
        let (s, e) = self.sel_span()?;
        let text = self.get_range(s, e);
        self.regs.insert(
            '"',
            Reg {
                text: text.clone(),
                linewise: false,
            },
        );
        Some(text)
    }

    /// Remove the selection, if there is one, leaving the cursor where it was. True if text was removed.
    pub(super) fn delete_selection(&mut self) -> bool {
        let span = self.sel_span();
        self.sel = None;
        let Some((s, e)) = span else {
            return false;
        };
        self.replace_range(s, e, "");
        self.row = s.0;
        self.col = s.1;
        true
    }

    // -- undo --------------------------------------------------------------------

    /// Take an undo snapshot unless this edit continues the run before it.
    fn begin_edit(&mut self, run: Run, boundary: bool) {
        if self.sel.is_some() || self.run != Some(run) || boundary {
            self.save_undo();
        }
        self.run = Some(run);
    }

    fn undo_standard(&mut self) {
        self.run = None;
        self.sel = None;
        self.undo();
    }

    fn redo_standard(&mut self) {
        self.run = None;
        self.sel = None;
        self.redo();
    }

    // -- editing -------------------------------------------------------------------

    fn type_str(&mut self, s: &str) {
        if self.total_chars() + s.len() > MAX_CHARS {
            return;
        }
        // A new word starts a new step of undo, so undoing removes about a word at a time.
        let starts_word = s.chars().next().is_some_and(|c| !c.is_whitespace())
            && self.col > 0
            && self.buf[self.row]
                .get(self.col - 1)
                .is_some_and(|p| p.is_whitespace());
        self.begin_edit(Run::Typing, starts_word);
        self.delete_selection();
        self.insert_text(s);
    }

    fn type_newline(&mut self) {
        if self.total_chars() + 1 > MAX_CHARS {
            return;
        }
        self.run = None;
        self.save_undo();
        self.delete_selection();
        self.insert_newline();
    }

    fn erase_back(&mut self) {
        if self.sel.is_none() && self.row == 0 && self.col == 0 {
            return;
        }
        self.begin_edit(Run::Erasing, false);
        if !self.delete_selection() {
            self.backspace();
        }
    }

    fn erase_forward(&mut self) {
        let (r, len) = (self.row, self.len_of(self.row));
        let c = self.col.min(len);
        if self.sel.is_none() && c >= len && r >= self.last_row() {
            return;
        }
        self.begin_edit(Run::Erasing, false);
        if self.delete_selection() {
            return;
        }
        if c < len {
            self.replace_range((r, c), (r, self.cluster_after(r, c)), "");
        } else {
            self.replace_range((r, c), (r + 1, 0), "");
        }
    }

    fn delete_word_back(&mut self) {
        if self.sel.is_some() {
            self.begin_edit(Run::Erasing, true);
            self.delete_selection();
            return;
        }
        let at = (self.row, self.col.min(self.len_of(self.row)));
        let start = self.word_left(at);
        if start == at {
            return;
        }
        self.begin_edit(Run::Erasing, true);
        self.replace_range(start, at, "");
        self.row = start.0;
        self.col = start.1;
    }

    fn delete_word_forward(&mut self) {
        if self.sel.is_some() {
            self.begin_edit(Run::Erasing, true);
            self.delete_selection();
            return;
        }
        let at = (self.row, self.col.min(self.len_of(self.row)));
        let end = self.word_right(at);
        if at == end {
            return;
        }
        self.begin_edit(Run::Erasing, true);
        self.replace_range(at, end, "");
    }

    /// Take up to one indent's worth of spaces (or one tab) from the start of the line.
    fn unindent(&mut self) {
        let r = self.row;
        let line = &self.buf[r];
        let n = if line.first() == Some(&'\t') {
            1
        } else {
            line.iter()
                .take(INDENT.len())
                .take_while(|c| **c == ' ')
                .count()
        };
        if n == 0 {
            return;
        }
        self.run = None;
        self.save_undo();
        self.sel = None;
        self.replace_range((r, 0), (r, n), "");
        self.col = self.col.saturating_sub(n);
    }

    /// Whether the cursor is inside a fenced code block that has been opened and not yet closed.
    pub(super) fn in_open_fence(&self) -> bool {
        let mut open: Option<crate::fence::Fence> = None;
        for (r, line) in self.buf.iter().enumerate().take(self.row + 1) {
            let upto = if r == self.row {
                &line[..self.col.min(line.len())]
            } else {
                &line[..]
            };
            let text: String = upto.iter().collect();
            match open {
                Some(fence) if crate::fence::fence_closes(&text, fence) => open = None,
                None => {
                    if let Some((fence, _)) = crate::fence::fence_marker(&text) {
                        open = Some(fence);
                    }
                }
                Some(_) => {}
            }
        }
        open.is_some()
    }

    // -- movement ----------------------------------------------------------------------

    /// The start of the word before `at`, or the end of the previous line when `at` is at the start of one.
    fn word_left(&self, at: Pos) -> Pos {
        let (r, mut c) = (at.0, at.1.min(self.len_of(at.0)));
        if c == 0 {
            return if r == 0 {
                (0, 0)
            } else {
                (r - 1, self.len_of(r - 1))
            };
        }
        let line = &self.buf[r];
        while c > 0 && line[c - 1].is_whitespace() {
            c -= 1;
        }
        if c > 0 {
            let cls = class(line[c - 1], false);
            while c > 0 && class(line[c - 1], false) == cls {
                c -= 1;
            }
        }
        (r, c)
    }

    /// The start of the word after `at`, or the start of the next line when `at` is at the end of one.
    fn word_right(&self, at: Pos) -> Pos {
        let (r, mut c) = (at.0, at.1);
        let line = &self.buf[r];
        let len = line.len();
        if c >= len {
            return if r < self.last_row() {
                (r + 1, 0)
            } else {
                (r, len)
            };
        }
        let cls = class(line[c], false);
        if cls != 0 {
            while c < len && class(line[c], false) == cls {
                c += 1;
            }
        }
        while c < len && line[c].is_whitespace() {
            c += 1;
        }
        (r, c)
    }

    /// Move the cursor; with `extend` the selection grows (starting where the cursor was), otherwise it ends.
    fn move_standard(&mut self, code: KeyCode, word: bool, extend: bool) {
        self.run = None;
        let from = self.clamp_pos((self.row, self.col));
        if extend {
            if self.sel.is_none() {
                self.sel = Some(from);
            }
        } else if let Some(a) = self.sel.take() {
            // Left and Right with a selection go to its edge instead of moving on from the cursor.
            let a = self.clamp_pos(a);
            let (start, end) = if a <= from { (a, from) } else { (from, a) };
            match code {
                KeyCode::Left if !word => {
                    (self.row, self.col) = start;
                    self.want = None;
                    return;
                }
                KeyCode::Right if !word => {
                    (self.row, self.col) = end;
                    self.want = None;
                    return;
                }
                _ => {}
            }
        }
        let (r, c) = from;
        let len = self.len_of(r);
        let vertical = matches!(
            code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        );
        let want = self.want.unwrap_or(c);
        let to = match code {
            KeyCode::Left if word => self.word_left(from),
            KeyCode::Left if c > 0 => (r, self.cluster_before(r, c)),
            KeyCode::Left if r > 0 => (r - 1, self.len_of(r - 1)),
            KeyCode::Right if word => self.word_right(from),
            KeyCode::Right if c < len => (r, self.cluster_after(r, c)),
            KeyCode::Right if r < self.last_row() => (r + 1, 0),
            KeyCode::Up | KeyCode::PageUp => {
                let step = if code == KeyCode::PageUp {
                    PAGE_ROWS
                } else {
                    1
                };
                if r == 0 {
                    (0, 0)
                } else {
                    let nr = r.saturating_sub(step);
                    let col = want.min(self.len_of(nr));
                    let snapped = if col < self.len_of(nr) {
                        self.cluster_before(nr, col + 1)
                    } else {
                        col
                    };
                    (nr, snapped)
                }
            }
            KeyCode::Down | KeyCode::PageDown => {
                let step = if code == KeyCode::PageDown {
                    PAGE_ROWS
                } else {
                    1
                };
                if r == self.last_row() {
                    (r, len)
                } else {
                    let nr = (r + step).min(self.last_row());
                    let col = want.min(self.len_of(nr));
                    let snapped = if col < self.len_of(nr) {
                        self.cluster_before(nr, col + 1)
                    } else {
                        col
                    };
                    (nr, snapped)
                }
            }
            KeyCode::Home if word => (0, 0),
            KeyCode::Home => (r, 0),
            KeyCode::End if word => (self.last_row(), self.len_of(self.last_row())),
            KeyCode::End => (r, len),
            _ => from,
        };
        (self.row, self.col) = to;
        self.want = vertical.then_some(want);
        if self.sel == Some(to) {
            self.sel = None;
        }
    }
}
