//! Operators (`d c y > <` and friends) applied to ranges and lines, plus pasting, joining and case mapping.

use super::*;

impl Editor {
    /// Applies `op` to the character range `[s, e)`.
    pub(super) fn apply_op_char(&mut self, op: char, s: Pos, e: Pos) -> EditorAction {
        let (mut s, mut e) = (self.clamp_pos(s), self.clamp_pos(e));
        if s > e {
            std::mem::swap(&mut s, &mut e);
        }
        match op {
            'y' => {
                let t = self.get_range(s, e);
                self.row = s.0;
                self.col = s.1;
                self.store_reg(t, false)
            }
            'd' => {
                if s == e {
                    return EditorAction::None;
                }
                self.save_undo();
                self.mark_change();
                let t = self.get_range(s, e);
                self.replace_range(s, e, "");
                self.row = s.0;
                self.col = s.1;
                self.store_reg(t, false)
            }
            'c' => {
                // Always one undo step, even over nothing: what is typed next belongs to this change, and undoing it
                // must not also take away the change before it.
                self.save_undo();
                self.mark_change();
                let t = self.get_range(s, e);
                self.replace_range(s, e, "");
                let action = self.store_reg(t, false);
                self.row = s.0;
                self.col = s.1;
                self.enter_insert(1, "");
                action
            }
            _ => self.apply_op_lines(op, s.0, e.0),
        }
    }

    /// Applies `op` to the rows `r1..=r2`.
    pub(super) fn apply_op_lines(&mut self, op: char, r1: usize, r2: usize) -> EditorAction {
        let r2 = r2.min(self.last_row());
        let r1 = r1.min(r2);
        match op {
            'y' => {
                let t = self.rows_text(r1, r2);
                self.row = r1;
                self.store_reg(t, true)
            }
            'd' => {
                self.save_undo();
                self.mark_change();
                let t = self.rows_text(r1, r2);
                self.replace_rows(r1, r2, Vec::new());
                self.row = r1.min(self.last_row());
                self.col = self.first_non_blank(self.row);
                self.store_reg(t, true)
            }
            'c' => {
                self.save_undo();
                self.mark_change();
                let t = self.rows_text(r1, r2);
                let indent: Vec<char> = self.buf[r1]
                    .iter()
                    .take_while(|c| c.is_whitespace())
                    .copied()
                    .collect();
                self.row = r1;
                self.col = indent.len();
                self.replace_rows(r1, r2, vec![indent]);
                self.enter_insert(1, "");
                self.store_reg(t, true)
            }
            '>' | '<' => {
                self.save_undo();
                self.mark_change();
                for r in r1..=r2 {
                    if op == '>' {
                        if !self.buf[r].is_empty() {
                            self.replace_range((r, 0), (r, 0), INDENT);
                        }
                    } else {
                        let l = &self.buf[r];
                        let n = if l.first() == Some(&'\t') {
                            1
                        } else {
                            l.iter()
                                .take(INDENT.len())
                                .take_while(|c| **c == ' ')
                                .count()
                        };
                        if n > 0 {
                            self.replace_range((r, 0), (r, n), "");
                        }
                    }
                }
                self.row = r1;
                self.col = self.first_non_blank(r1);
                EditorAction::None
            }
            _ => EditorAction::None,
        }
    }

    /// Runs `op` over the motion `m`.
    pub(super) fn op_motion(&mut self, op: char, m: Motion, count: Option<usize>) -> EditorAction {
        let start = (self.row, self.col);
        let mo = match m {
            // `cw` on a word behaves like `ce` (but never skips to the next word).
            Motion::WordFwd(big) if op == 'c' && self.cls(start.0, start.1, big) != 0 => {
                let n = count.unwrap_or(1).max(1);
                let cls = self.cls(start.0, start.1, big);
                let mut c = start.1;
                while self.cls(start.0, c + 1, big) == cls {
                    c += 1;
                }
                let mut p = (start.0, c);
                for _ in 1..n {
                    let np = self.word_end(p, big);
                    if np == p {
                        break;
                    }
                    p = np;
                }
                Some(Mo {
                    pos: p,
                    kind: Kind::Inclusive,
                    want: Want::Reset,
                })
            }
            _ => self.calc_motion(m, count, true),
        };
        let Some(mo) = mo else {
            return EditorAction::None;
        };
        if mo.kind == Kind::Linewise {
            return self.apply_op_lines(op, start.0.min(mo.pos.0), start.0.max(mo.pos.0));
        }
        let (a, mut b) = if mo.pos < start {
            (mo.pos, start)
        } else {
            (start, mo.pos)
        };
        if mo.kind == Kind::Inclusive {
            b.1 += 1;
        } else if b.1 == 0 && b.0 > a.0 {
            // An exclusive motion ending in column 0 stops at the end of the previous line, and becomes
            // linewise when it started at or before the first non-blank.
            let pr = b.0 - 1;
            if a.1 <= self.first_non_blank(a.0) {
                return self.apply_op_lines(op, a.0, pr);
            }
            b = (pr, self.len_of(pr));
        }
        self.apply_op_char(op, a, b)
    }

    /// `dd`, `cc`, `yy`, `>>`, `<<` with a count.
    pub(super) fn op_lines(&mut self, op: char, count: Option<usize>) -> EditorAction {
        let n = count.unwrap_or(1).max(1);
        self.apply_op_lines(op, self.row, self.row + n - 1)
    }

    pub(super) fn paste_reg(&mut self, reg: Reg, after: bool, count: usize) {
        if reg.text.is_empty() {
            return;
        }
        let room = MAX_CHARS.saturating_sub(self.total_chars()) / (reg.text.len() + 1);
        let n = count.clamp(1, room.max(1));
        self.save_undo();
        self.mark_change();
        if reg.linewise {
            let text = vec![reg.text.as_str(); n].join("\n");
            let start = if after {
                let l = self.len_of(self.row);
                self.replace_range((self.row, l), (self.row, l), &format!("\n{text}"));
                self.row + 1
            } else {
                self.replace_range((self.row, 0), (self.row, 0), &format!("{text}\n"));
                self.row
            };
            self.row = start;
            self.col = self.first_non_blank(start);
        } else {
            let text = reg.text.repeat(n);
            let at = (
                self.row,
                if after && self.len_of(self.row) > 0 {
                    self.cluster_after(self.row, self.col)
                } else {
                    self.col
                },
            );
            let end = self.replace_range(at, at, &text);
            if text.contains('\n') {
                self.row = at.0;
                self.col = at.1;
            } else {
                self.row = end.0;
                self.col = end.1.saturating_sub(1);
            }
        }
    }

    pub(super) fn join_lines(&mut self, r: usize, count: usize) {
        let last = (r + count.max(2) - 1).min(self.last_row());
        if last == r {
            return;
        }
        self.save_undo();
        self.mark_change();
        let mut merged = self.buf[r].clone();
        let mut col = 0;
        for b in &self.buf[r + 1..=last] {
            let lead = b.iter().take_while(|c| c.is_whitespace()).count();
            let need_space = merged.last().is_some_and(|c| !c.is_whitespace())
                && lead < b.len()
                && b[lead] != ')';
            col = merged.len();
            if need_space {
                merged.push(' ');
            }
            merged.extend_from_slice(&b[lead..]);
        }
        self.replace_rows(r, last, vec![merged]);
        self.row = r;
        self.col = col;
    }

    /// Rewrites every character of `[s, e)` through `f` (newlines are kept).
    pub(super) fn map_chars(&mut self, s: Pos, e: Pos, f: impl Fn(char) -> String) {
        let text = self.get_range(s, e);
        let new: String = text
            .chars()
            .map(|c| if c == '\n' { "\n".to_string() } else { f(c) })
            .collect();
        if new != text {
            self.save_undo();
            self.replace_range(s, e, &new);
        }
        self.row = s.0;
        self.col = s.1;
    }
}
