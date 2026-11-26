//! Buffer primitives: ranges, undo and redo, registers and pending-input state.

use super::*;
use unicode_segmentation::UnicodeSegmentation;

impl Editor {
    pub(super) fn sync(&mut self) {
        self.lines = self.buf.iter().map(|l| l.iter().collect()).collect();
        self.version += 1;
    }

    pub(super) fn len_of(&self, r: usize) -> usize {
        self.buf.get(r).map_or(0, Vec::len)
    }

    /// Character-index boundaries of each grapheme cluster on row `r`, plus one at the end of the line.
    fn cluster_bounds(&self, r: usize) -> Vec<usize> {
        let line = &self.buf[r];
        let text: String = line.iter().collect();
        let mut bounds = vec![0];
        let mut i = 0;
        for cluster in text.graphemes(true) {
            i += cluster.chars().count();
            bounds.push(i);
        }
        bounds
    }

    /// The character index just before `c`'s grapheme cluster: `0` if `c` is already at or before the first one.
    /// Moving, and deleting with Backspace, goes here rather than to `c - 1`, so a cluster a message routinely
    /// carries — an emoji with a skin-tone or gender modifier, a flag, a family joined with zero-width joiners —
    /// moves and is erased as the one glyph it is drawn as, not one invisible code point at a time.
    pub(super) fn cluster_before(&self, r: usize, c: usize) -> usize {
        self.cluster_bounds(r)
            .into_iter()
            .rev()
            .find(|&b| b < c)
            .unwrap_or(0)
    }

    /// The character index just after `c`'s grapheme cluster (the start of the next one, or the end of the line):
    /// the Right-moving, Delete-key counterpart of [`Self::cluster_before`].
    pub(super) fn cluster_after(&self, r: usize, c: usize) -> usize {
        self.cluster_bounds(r)
            .into_iter()
            .find(|&b| b > c)
            .unwrap_or_else(|| self.len_of(r))
    }

    pub(super) fn last_row(&self) -> usize {
        self.buf.len() - 1
    }

    pub(super) fn clamp_pos(&self, p: Pos) -> Pos {
        let r = p.0.min(self.last_row());
        (r, p.1.min(self.buf[r].len()))
    }

    /// Index of the first non-blank character (or the line length when there is none).
    pub(super) fn first_non_blank(&self, r: usize) -> usize {
        let l = &self.buf[r];
        l.iter().position(|c| !c.is_whitespace()).unwrap_or(l.len())
    }

    pub(super) fn total_chars(&self) -> usize {
        self.buf.iter().map(|l| l.len() + 1).sum()
    }

    /// Replaces `[s, e)` with `text` (which may contain newlines) and returns the position just after the
    /// inserted text. Every buffer edit goes through here.
    pub(super) fn replace_range(&mut self, s: Pos, e: Pos, text: &str) -> Pos {
        let (mut s, mut e) = (self.clamp_pos(s), self.clamp_pos(e));
        if s > e {
            std::mem::swap(&mut s, &mut e);
        }
        let tail: Vec<char> = self.buf[e.0][e.1..].to_vec();
        let mut cur: Vec<char> = self.buf[s.0][..s.1].to_vec();
        let mut parts = text.split('\n');
        cur.extend(parts.next().unwrap_or("").chars());
        let mut new_lines: Vec<Vec<char>> = Vec::new();
        for p in parts {
            new_lines.push(std::mem::replace(&mut cur, p.chars().collect()));
        }
        let end = (s.0 + new_lines.len(), cur.len());
        cur.extend(tail);
        new_lines.push(cur);
        self.buf.splice(s.0..=e.0, new_lines);
        self.sync();
        end
    }

    /// Replaces whole rows `r1..=r2` with `new` (an empty `new` leaves a single empty line if nothing remains).
    pub(super) fn replace_rows(&mut self, r1: usize, r2: usize, new: Vec<Vec<char>>) {
        self.buf.splice(r1..=r2, new);
        if self.buf.is_empty() {
            self.buf.push(Vec::new());
        }
        self.sync();
    }

    /// Text of `[s, e)`.
    pub(super) fn get_range(&self, s: Pos, e: Pos) -> String {
        let (s, e) = (self.clamp_pos(s), self.clamp_pos(e));
        if s >= e {
            return String::new();
        }
        let mut out = String::new();
        for (r, line) in self.buf.iter().enumerate().take(e.0 + 1).skip(s.0) {
            let a = if r == s.0 { s.1 } else { 0 };
            let b = if r == e.0 { e.1 } else { line.len() };
            out.extend(&line[a.min(b)..b]);
            if r != e.0 {
                out.push('\n');
            }
        }
        out
    }

    pub(super) fn rows_text(&self, r1: usize, r2: usize) -> String {
        self.buf[r1..=r2]
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn clamp(&mut self) {
        if self.buf.is_empty() {
            self.buf.push(Vec::new());
            self.sync();
        }
        self.row = self.row.min(self.last_row());
        let len = self.buf[self.row].len();
        let max = if self.mode == Mode::Insert {
            len
        } else {
            self.cluster_before(self.row, len)
        };
        self.col = self.col.min(max);
        if let Some(a) = self.sel {
            self.sel = Some(self.clamp_pos(a));
        }
        self.anchor.0 = self.anchor.0.min(self.last_row());
        let alen = self.buf[self.anchor.0].len();
        self.anchor.1 = self.anchor.1.min(alen.saturating_sub(1));
    }

    // -- undo ----------------------------------------------------------------

    pub(super) fn save_undo(&mut self) {
        if self.undo.len() >= UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(Snap {
            buf: self.buf.clone(),
            cursor: (self.row, self.col),
        });
        self.redo.clear();
    }

    pub(super) fn undo(&mut self) {
        while let Some(s) = self.undo.pop() {
            if s.buf == self.buf {
                continue; // a snapshot taken by a command that changed nothing
            }
            let cur = Snap {
                buf: std::mem::replace(&mut self.buf, s.buf),
                cursor: (self.row, self.col),
            };
            self.redo.push(cur);
            self.restore(s.cursor);
            return;
        }
    }

    pub(super) fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            let cur = Snap {
                buf: std::mem::replace(&mut self.buf, s.buf),
                cursor: (self.row, self.col),
            };
            self.undo.push(cur);
            self.restore(s.cursor);
        }
    }

    pub(super) fn restore(&mut self, cursor: Pos) {
        self.row = cursor.0;
        self.col = cursor.1;
        self.want = None;
        self.sync();
        self.clamp();
    }

    // -- registers -------------------------------------------------------------

    /// Stores text in the selected register (and the unnamed one) and reports it for the system clipboard.
    pub(super) fn store_reg(&mut self, text: String, linewise: bool) -> EditorAction {
        let out = if linewise {
            format!("{text}\n")
        } else {
            text.clone()
        };
        let sel = self.reg_sel.take();
        if sel == Some('_') {
            return EditorAction::None;
        }
        let reg = Reg { text, linewise };
        if let Some(c) = sel.filter(|c| c.is_alphanumeric()) {
            self.regs.insert(c.to_ascii_lowercase(), reg.clone());
        }
        self.regs.insert('"', reg);
        EditorAction::Yanked(out)
    }

    pub(super) fn get_reg(&mut self) -> Option<Reg> {
        let sel = self.reg_sel.take().unwrap_or('"');
        self.regs
            .get(&sel.to_ascii_lowercase())
            .or_else(|| self.regs.get(&'"').filter(|_| sel == '"'))
            .cloned()
    }

    pub(super) fn reset_pending(&mut self) {
        self.cnt1 = 0;
        self.cnt2 = 0;
        self.op = None;
        self.prefix = None;
        self.reg_sel = None;
    }

    pub(super) fn is_pending(&self) -> bool {
        self.cnt1 > 0
            || self.cnt2 > 0
            || self.op.is_some()
            || self.prefix.is_some()
            || self.reg_sel.is_some()
    }

    pub(super) fn total_count(&self) -> Option<usize> {
        if self.cnt1 == 0 && self.cnt2 == 0 {
            None
        } else {
            Some((self.cnt1.max(1) * self.cnt2.max(1)).min(99_999))
        }
    }

    /// Marks the command in flight as a repeatable change (Normal-mode commands only).
    pub(super) fn mark_change(&mut self) {
        if self.mode == Mode::Normal {
            self.is_change = true;
        }
    }

    pub(super) fn finish_key(&mut self) {
        if !self.replaying && self.mode != Mode::Insert && !self.is_pending() {
            if self.is_change && !self.cur_keys.is_empty() {
                self.last_change = std::mem::take(&mut self.cur_keys);
            } else {
                self.cur_keys.clear();
            }
            self.is_change = false;
        }
        self.clamp();
    }

    pub(super) fn enter_insert(&mut self, count: usize, prefix: &str) {
        self.mode = Mode::Insert;
        self.ins_count = count.clamp(1, 1000);
        self.ins_prefix = prefix.to_string();
        self.ins_log.clear();
    }
}
