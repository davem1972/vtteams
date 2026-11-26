//! Key handling for Normal and Visual mode.

use super::*;

impl Editor {
    pub(super) fn escape(&mut self) -> EditorAction {
        if self.mode != Mode::Normal {
            self.mode = Mode::Normal;
            self.reset_pending();
            EditorAction::None
        } else if self.is_pending() {
            self.reset_pending();
            EditorAction::None
        } else {
            EditorAction::Escape
        }
    }

    pub(super) fn is_visual(&self) -> bool {
        matches!(self.mode, Mode::Visual | Mode::VisualLine)
    }

    pub(super) fn exec_motion(&mut self, m: Motion) -> EditorAction {
        let count = self.total_count();
        let op = self.op.take();
        let action = if let Some(op) = op {
            self.op_motion(op, m, count)
        } else {
            if let Some(mo) = self.calc_motion(m, count, false) {
                self.row = mo.pos.0;
                self.col = mo.pos.1;
                self.want = match mo.want {
                    Want::Reset => None,
                    Want::Set(w) => Some(w),
                };
            }
            EditorAction::None
        };
        self.reset_pending();
        action
    }

    pub(super) fn normal_key(&mut self, key: KeyEvent) -> EditorAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // A modified Enter sends, as it does in insert mode (and Ctrl-S does everywhere): the key someone who just
        // pressed Esc after typing reaches for. It must not fall into the Ctrl/Alt handling below, which would cancel it.
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
        {
            self.reset_pending();
            return EditorAction::Send;
        }
        if let Some(p) = self.prefix {
            return self.prefixed(p, key);
        }
        if (alt && !ctrl) || key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('[')) {
            return if alt && key.code != KeyCode::Esc {
                self.cancel()
            } else {
                self.escape()
            };
        }
        if ctrl {
            return match key.code {
                KeyCode::Char('r') if !self.is_visual() && self.op.is_none() => {
                    let n = self.total_count().unwrap_or(1).min(100);
                    for _ in 0..n {
                        self.redo();
                    }
                    self.reset_pending();
                    EditorAction::None
                }
                code => match motion_for(code, true) {
                    Some(m) => self.exec_motion(m),
                    None => self.cancel(),
                },
            };
        }
        match key.code {
            KeyCode::Enter => {
                if self.is_pending() || self.is_visual() {
                    self.cancel()
                } else {
                    EditorAction::Send
                }
            }
            KeyCode::Char(d) if d.is_ascii_digit() && (d != '0' || self.active_count() > 0) => {
                let slot = if self.op.is_some() {
                    &mut self.cnt2
                } else {
                    &mut self.cnt1
                };
                *slot = (*slot * 10 + d.to_digit(10).unwrap_or(0) as usize).min(MAX_COUNT);
                EditorAction::None
            }
            KeyCode::Char('"') => {
                self.prefix = Some('"');
                EditorAction::None
            }
            KeyCode::Delete => self.char_cmd('x'),
            KeyCode::Char(c) => {
                if let Some(m) = motion_for(key.code, false) {
                    return self.exec_motion(m);
                }
                self.char_cmd(c)
            }
            code => match motion_for(code, false) {
                Some(m) => self.exec_motion(m),
                None => self.cancel(),
            },
        }
    }

    pub(super) fn active_count(&self) -> usize {
        if self.op.is_some() {
            self.cnt2
        } else {
            self.cnt1
        }
    }

    pub(super) fn cancel(&mut self) -> EditorAction {
        self.reset_pending();
        EditorAction::None
    }

    /// The second key of a two-key command (`gg`, `f<c>`, `r<c>`, `"<r>`, `i<obj>`, `a<obj>`).
    pub(super) fn prefixed(&mut self, p: char, key: KeyEvent) -> EditorAction {
        self.prefix = None;
        let plain = !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        let KeyCode::Char(c) = key.code else {
            return self.cancel();
        };
        if !plain {
            return self.cancel();
        }
        match p {
            'g' => {
                if c == 'g' {
                    self.exec_motion(Motion::GotoFirst)
                } else {
                    self.cancel()
                }
            }
            'f' | 'F' | 't' | 'T' => {
                self.last_find = Some((p, c));
                self.exec_motion(Motion::Find(p, c))
            }
            '"' => {
                self.reg_sel = Some(c);
                EditorAction::None
            }
            'r' => self.replace_chars(c),
            'i' | 'a' => self.apply_text_object(p == 'a', c),
            _ => self.cancel(),
        }
    }

    pub(super) fn replace_chars(&mut self, c: char) -> EditorAction {
        if self.is_visual() {
            let (s, e) = self.selection_range();
            self.map_chars(s, e, |_| c.to_string());
            self.mode = Mode::Normal;
            self.reset_pending();
            return EditorAction::None;
        }
        let n = self.total_count().unwrap_or(1);
        self.reset_pending();
        let len = self.len_of(self.row);
        if self.col + n > len {
            return EditorAction::None;
        }
        self.save_undo();
        self.mark_change();
        let (r, col) = (self.row, self.col);
        self.replace_range((r, col), (r, col + n), &c.to_string().repeat(n));
        self.col = col + n - 1;
        EditorAction::None
    }

    pub(super) fn apply_text_object(&mut self, around: bool, obj: char) -> EditorAction {
        let found = self.text_object(around, obj);
        let op = self.op.take();
        self.reset_pending();
        let Some((s, e, linewise)) = found else {
            return EditorAction::None;
        };
        if self.is_visual() {
            if linewise {
                self.mode = Mode::VisualLine;
                self.anchor = (s.0, 0);
                self.row = e.0;
                self.col = 0;
            } else {
                self.anchor = s;
                self.row = e.0;
                self.col = e.1.saturating_sub(1);
            }
            return EditorAction::None;
        }
        let Some(op) = op else {
            return EditorAction::None;
        };
        if linewise {
            self.apply_op_lines(op, s.0, e.0)
        } else {
            self.apply_op_char(op, s, e)
        }
    }

    /// Visual selection as (start, end exclusive) character positions (whole rows when linewise).
    pub(super) fn selection_range(&self) -> (Pos, Pos) {
        let (s, e, lw) =
            self.selection()
                .unwrap_or(((self.row, self.col), (self.row, self.col), false));
        if lw {
            return ((s.0, 0), (e.0, self.len_of(e.0)));
        }
        let el = self.len_of(e.0);
        let end = if e.1 >= el && e.0 < self.last_row() {
            (e.0 + 1, 0)
        } else {
            (e.0, (e.1 + 1).min(el))
        };
        (s, end)
    }

    pub(super) fn char_cmd(&mut self, c: char) -> EditorAction {
        if self.is_visual() {
            return self.visual_cmd(c);
        }
        let count = self.total_count();
        let n = count.unwrap_or(1);
        // With an operator pending only doubling (`dd`), text objects and motions make sense.
        if let Some(op) = self.op {
            return match c {
                'i' | 'a' => {
                    self.prefix = Some(c);
                    EditorAction::None
                }
                _ if c == op => {
                    self.op = None;
                    let a = self.op_lines(op, count);
                    self.reset_pending();
                    a
                }
                'g' | 'f' | 'F' | 't' | 'T' => {
                    self.prefix = Some(c);
                    EditorAction::None
                }
                _ => self.cancel(),
            };
        }
        match c {
            'g' | 'f' | 'F' | 't' | 'T' | 'r' => {
                self.prefix = Some(c);
                EditorAction::None
            }
            'd' | 'c' | 'y' | '>' | '<' => {
                self.op = Some(c);
                EditorAction::None
            }
            'x' => self.shorthand('d', Motion::Right),
            'X' => self.shorthand('d', Motion::Left),
            'D' => self.shorthand('d', Motion::LineEnd),
            'C' => self.shorthand('c', Motion::LineEnd),
            's' => {
                if self.len_of(self.row) == 0 {
                    self.reset_pending();
                    self.save_undo();
                    self.mark_change();
                    self.enter_insert(1, "");
                    EditorAction::None
                } else {
                    self.shorthand('c', Motion::Right)
                }
            }
            'S' => {
                let a = self.op_lines('c', count);
                self.reset_pending();
                a
            }
            'Y' => {
                let a = self.op_lines('y', count);
                self.reset_pending();
                a
            }
            'i' | 'a' | 'I' | 'A' | 'o' | 'O' => {
                self.reset_pending();
                self.save_undo();
                self.mark_change();
                let r = self.row;
                match c {
                    'a' => self.col = self.cluster_after(r, self.col).min(self.len_of(r)),
                    'I' => self.col = self.first_non_blank(r),
                    'A' => self.col = self.len_of(r),
                    'o' | 'O' => {
                        let indent: String = self.buf[r]
                            .iter()
                            .take_while(|c| c.is_whitespace())
                            .collect();
                        if c == 'o' {
                            let l = self.len_of(r);
                            self.replace_range((r, l), (r, l), &format!("\n{indent}"));
                            self.row = r + 1;
                        } else {
                            self.replace_range((r, 0), (r, 0), &format!("{indent}\n"));
                        }
                        self.col = indent.chars().count();
                    }
                    _ => {}
                }
                self.enter_insert(n, if matches!(c, 'o' | 'O') { "\n" } else { "" });
                EditorAction::None
            }
            'p' | 'P' => {
                let reg = self.get_reg();
                self.reset_pending();
                if let Some(reg) = reg {
                    self.paste_reg(reg, c == 'p', n);
                }
                EditorAction::None
            }
            'u' => {
                for _ in 0..n.min(100) {
                    self.undo();
                }
                self.reset_pending();
                EditorAction::None
            }
            'J' => {
                self.reset_pending();
                self.join_lines(self.row, n);
                EditorAction::None
            }
            '~' => {
                self.reset_pending();
                let r = self.row;
                let len = self.len_of(r);
                if len > 0 {
                    self.save_undo();
                    self.mark_change();
                    let end = (self.col + n).min(len);
                    let (s, e) = ((r, self.col), (r, end));
                    self.map_chars(s, e, toggle_case);
                    self.col = end;
                }
                EditorAction::None
            }
            '.' => {
                self.reset_pending();
                self.dot_repeat();
                EditorAction::None
            }
            ':' => {
                self.reset_pending();
                self.cmd = Some(String::new());
                EditorAction::None
            }
            'v' => {
                self.reset_pending();
                self.anchor = (self.row, self.col);
                self.mode = Mode::Visual;
                EditorAction::None
            }
            'V' => {
                self.reset_pending();
                self.anchor = (self.row, self.col);
                self.mode = Mode::VisualLine;
                EditorAction::None
            }
            _ => self.cancel(),
        }
    }

    /// `x`, `D`, `C`, ... : an operator applied to a motion, with the typed count.
    pub(super) fn shorthand(&mut self, op: char, m: Motion) -> EditorAction {
        let count = self.total_count();
        let a = self.op_motion(op, m, count);
        self.reset_pending();
        a
    }

    pub(super) fn dot_repeat(&mut self) {
        if self.replaying || self.last_change.is_empty() {
            return;
        }
        let keys = self.last_change.clone();
        self.replaying = true;
        for k in keys {
            self.handle_key(k);
        }
        self.replaying = false;
        // The replayed keys marked themselves as a change; the `.` that triggered them must not replace the
        // recorded change.
        self.is_change = false;
        if self.mode == Mode::Insert {
            // The recorded change was cut short; do not leave the editor stuck in insert mode.
            self.insert_esc();
        }
    }

    pub(super) fn visual_cmd(&mut self, c: char) -> EditorAction {
        let lw = self.mode == Mode::VisualLine;
        let (s, e) = self.selection_range();
        let (sel_s, sel_e, _) =
            self.selection()
                .unwrap_or(((self.row, self.col), (self.row, self.col), lw));
        let rows = (sel_s.0, sel_e.0);
        let n = self.total_count().unwrap_or(1);
        let mut action = EditorAction::None;
        let mut leave = true;
        match c {
            'g' | 'f' | 'F' | 't' | 'T' | 'r' | 'i' | 'a' => {
                self.prefix = Some(c);
                return action;
            }
            'o' | 'O' => {
                let cur = (self.row, self.col);
                (self.row, self.col) = self.anchor;
                self.anchor = cur;
                leave = false;
            }
            'v' | 'V' => {
                let target = if c == 'v' {
                    Mode::Visual
                } else {
                    Mode::VisualLine
                };
                if self.mode == target {
                    self.mode = Mode::Normal;
                } else {
                    self.mode = target;
                }
                leave = false;
            }
            'd' | 'x' | 'y' | 'c' | 's' | '>' | '<' => {
                let op = match c {
                    'x' => 'd',
                    's' => 'c',
                    o => o,
                };
                action = if lw || matches!(c, '>' | '<') {
                    self.apply_op_lines(op, rows.0, rows.1)
                } else {
                    self.apply_op_char(op, s, e)
                };
                if op == 'y' {
                    (self.row, self.col) = sel_s;
                }
                leave = op != 'c';
            }
            'X' | 'D' | 'Y' | 'S' | 'C' => {
                let op = match c {
                    'X' | 'D' => 'd',
                    'Y' => 'y',
                    _ => 'c',
                };
                action = self.apply_op_lines(op, rows.0, rows.1);
                if op == 'y' {
                    (self.row, self.col) = sel_s;
                }
                leave = op != 'c';
            }
            'J' => self.join_lines(rows.0, rows.1 - rows.0 + 1),
            '~' => self.map_chars(s, e, toggle_case),
            'u' => self.map_chars(s, e, |c| c.to_lowercase().collect()),
            'U' => self.map_chars(s, e, |c| c.to_uppercase().collect()),
            'p' | 'P' => {
                let old = self.get_reg();
                if let Some(old) = old {
                    action = self.visual_paste(old, lw, s, e, rows, n);
                }
            }
            ':' => {
                self.cmd = Some(String::new());
            }
            _ => {
                self.reset_pending();
                return EditorAction::None;
            }
        }
        if leave && self.mode != Mode::Insert {
            self.mode = Mode::Normal;
        }
        self.reset_pending();
        action
    }

    /// `p` in Visual mode: the selection is replaced by the register contents.
    pub(super) fn visual_paste(
        &mut self,
        old: Reg,
        lw: bool,
        s: Pos,
        e: Pos,
        rows: (usize, usize),
        n: usize,
    ) -> EditorAction {
        let was_last = rows.1 >= self.last_row();
        let action = if lw {
            self.apply_op_lines('d', rows.0, rows.1)
        } else {
            self.apply_op_char('d', s, e)
        };
        self.mode = Mode::Normal;
        if lw {
            let buffer_empty_now = self.buf.len() == 1 && self.buf[0].is_empty();
            if old.linewise {
                if buffer_empty_now {
                    let t = old.text.clone();
                    self.paste_reg(
                        Reg {
                            text: t,
                            linewise: true,
                        },
                        false,
                        n,
                    );
                    // Drop the empty line that remained.
                    let last = self.last_row();
                    if self.buf[last].is_empty() && last > 0 {
                        self.replace_rows(last, last, Vec::new());
                    }
                } else {
                    self.paste_reg(old, was_last, n);
                }
            } else {
                let reg = Reg {
                    text: old.text,
                    linewise: true,
                };
                self.paste_reg(reg, was_last && !buffer_empty_now, n);
            }
        } else if old.linewise {
            let at = self.clamp_pos(s);
            self.replace_range(at, at, "\n");
            self.row = at.0;
            self.col = at.1.saturating_sub(1);
            self.paste_reg(old, true, n);
        } else {
            let after = s.1 >= self.len_of(s.0) && self.len_of(s.0) > 0;
            self.row = s.0;
            self.col = if after { self.len_of(s.0) - 1 } else { s.1 };
            self.paste_reg(old, after, n);
        }
        action
    }
}
