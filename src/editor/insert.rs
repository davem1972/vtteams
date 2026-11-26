//! Key handling for Insert mode and the `:` command line.

use super::*;

impl Editor {
    pub(super) fn insert_text(&mut self, s: &str) {
        let at = self.clamp_pos((self.row, self.col));
        let end = self.replace_range(at, at, s);
        self.row = end.0;
        self.col = end.1;
        self.ins_log.push_str(s);
    }

    pub(super) fn backspace(&mut self) {
        let (r, c) = (self.row, self.col.min(self.len_of(self.row)));
        if c > 0 {
            let start = self.cluster_before(r, c);
            self.replace_range((r, start), (r, c), "");
            self.col = start;
            for _ in 0..c - start {
                self.ins_log.pop();
            }
        } else if r > 0 {
            let pl = self.len_of(r - 1);
            self.replace_range((r - 1, pl), (r, 0), "");
            self.row = r - 1;
            self.col = pl;
            self.ins_log.pop();
        }
    }

    pub(super) fn insert_newline(&mut self) {
        let r = self.row;
        let line = self.buf[r].clone();
        let c = self.col.min(line.len());
        let indent: String = line.iter().take_while(|ch| ch.is_whitespace()).collect();
        let mut prefix: String = indent.chars().take(c).collect();
        // Inside a code fence a line that looks like a list item or a quote is code, and keeps only its indentation.
        if !self.in_open_fence()
            && let Some((mend, next)) = list_marker(&line)
            && c >= mend
        {
            if is_blank(&line[mend..]) && c >= line.len() {
                // Enter on an empty list item ends the list.
                self.replace_range((r, 0), (r, line.len()), "");
                self.col = 0;
                self.ins_log.clear();
                return;
            }
            prefix = next;
        }
        let end = self.replace_range((r, c), (r, c), &format!("\n{prefix}"));
        self.row = end.0;
        self.col = end.1;
        self.ins_log.push('\n');
    }

    pub(super) fn insert_esc(&mut self) {
        if self.ins_count > 1 && !self.ins_log.is_empty() {
            let unit = format!("{}{}", self.ins_prefix, self.ins_log);
            let room = MAX_CHARS.saturating_sub(self.total_chars()) / (unit.len() + 1);
            let reps = (self.ins_count - 1).min(room);
            if reps > 0 {
                let at = self.clamp_pos((self.row, self.col));
                let end = self.replace_range(at, at, &unit.repeat(reps));
                self.row = end.0;
                self.col = end.1;
            }
        }
        self.ins_count = 1;
        self.ins_log.clear();
        self.mode = Mode::Normal;
        self.col = self.cluster_before(self.row, self.col);
        self.want = None;
    }

    pub(super) fn insert_key(&mut self, key: KeyEvent) -> EditorAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let (r, len) = (self.row, self.len_of(self.row));
        match key.code {
            KeyCode::Esc => self.insert_esc(),
            KeyCode::Char('[') if ctrl => self.insert_esc(),
            // Plain Enter adds a line, as in vim; a modified one sends (a terminal that cannot tell them apart has
            // Ctrl-S).
            KeyCode::Enter if alt || ctrl || key.modifiers.contains(KeyModifiers::SHIFT) => {
                return EditorAction::Send;
            }
            KeyCode::Enter => self.insert_newline(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Char('h') if ctrl && !alt => self.backspace(),
            KeyCode::Char('w') if ctrl && !alt => {
                let c = self.col.min(len);
                if c == 0 {
                    self.backspace();
                } else {
                    let mut s = c;
                    while s > 0 && self.buf[r][s - 1].is_whitespace() {
                        s -= 1;
                    }
                    if s > 0 {
                        let cls = class(self.buf[r][s - 1], false);
                        while s > 0 && class(self.buf[r][s - 1], false) == cls {
                            s -= 1;
                        }
                    }
                    self.replace_range((r, s), (r, c), "");
                    self.col = s;
                    for _ in 0..c - s {
                        self.ins_log.pop();
                    }
                }
            }
            KeyCode::Char('u') if ctrl && !alt => {
                let c = self.col.min(len);
                if c == 0 {
                    self.backspace();
                } else {
                    self.replace_range((r, 0), (r, c), "");
                    self.col = 0;
                    self.ins_log.clear();
                }
            }
            KeyCode::Delete => {
                let c = self.col.min(len);
                if c < len {
                    self.replace_range((r, c), (r, self.cluster_after(r, c)), "");
                } else if r < self.last_row() {
                    self.replace_range((r, c), (r + 1, 0), "");
                }
            }
            KeyCode::Left => {
                self.col = self.cluster_before(r, self.col.min(len));
                self.ins_log.clear();
            }
            KeyCode::Right => {
                self.col = self.cluster_after(r, self.col.min(len));
                self.ins_log.clear();
            }
            KeyCode::Home => {
                self.col = 0;
                self.ins_log.clear();
            }
            KeyCode::End => {
                self.col = len;
                self.ins_log.clear();
            }
            KeyCode::Up | KeyCode::Down => {
                let w = self.want.unwrap_or(self.col);
                let nr = if key.code == KeyCode::Up {
                    r.saturating_sub(1)
                } else {
                    (r + 1).min(self.last_row())
                };
                if nr != r {
                    self.row = nr;
                    self.col = w.min(self.len_of(nr));
                }
                self.want = Some(w);
                self.ins_log.clear();
                return EditorAction::None;
            }
            KeyCode::Tab => self.insert_text(INDENT),
            KeyCode::Char(c) if (!ctrl && !alt) || (ctrl && alt) => {
                self.insert_text(&c.to_string())
            }
            _ => {}
        }
        self.want = None;
        EditorAction::None
    }

    pub(super) fn cmd_key(&mut self, key: KeyEvent) -> EditorAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(cmd) = self.cmd.as_mut() else {
            return EditorAction::None;
        };
        match key.code {
            KeyCode::Esc => self.cmd = None,
            KeyCode::Char('c' | '[') if ctrl => self.cmd = None,
            KeyCode::Char('u') if ctrl => cmd.clear(),
            KeyCode::Enter => {
                let text = std::mem::take(cmd);
                self.cmd = None;
                return EditorAction::Command(text);
            }
            KeyCode::Backspace => {
                if cmd.pop().is_none() {
                    self.cmd = None;
                }
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => cmd.push(c),
            _ => {}
        }
        EditorAction::None
    }
}
