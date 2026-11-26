//! Motions and text objects: where a motion lands and what range an object covers.

use super::*;

impl Editor {
    pub(super) fn cls(&self, r: usize, c: usize, big: bool) -> u8 {
        self.buf[r].get(c).map_or(0, |&ch| class(ch, big))
    }

    /// Start of the next word. With `op`, the motion stops at the end of the line instead of running into the
    /// next one (so `dw` on the last word of a line does not join lines).
    pub(super) fn word_fwd(&self, p: Pos, big: bool, op: bool) -> Pos {
        let (mut r, mut c) = p;
        let started_inside = c < self.buf[r].len();
        let cls = self.cls(r, c, big);
        if cls != 0 {
            while c < self.buf[r].len() && self.cls(r, c, big) == cls {
                c += 1;
            }
        }
        loop {
            if c >= self.buf[r].len() {
                if r + 1 >= self.buf.len() || (op && started_inside) {
                    return (r, self.buf[r].len());
                }
                r += 1;
                c = 0;
                if self.buf[r].is_empty() {
                    return (r, 0);
                }
                continue;
            }
            if self.cls(r, c, big) != 0 {
                return (r, c);
            }
            c += 1;
        }
    }

    pub(super) fn word_end(&self, p: Pos, big: bool) -> Pos {
        let (mut r, mut c) = p;
        c += 1;
        loop {
            if c >= self.buf[r].len() {
                if r + 1 >= self.buf.len() {
                    return (r, self.buf[r].len().saturating_sub(1));
                }
                r += 1;
                c = 0;
                continue;
            }
            if self.cls(r, c, big) != 0 {
                break;
            }
            c += 1;
        }
        let cls = self.cls(r, c, big);
        while c + 1 < self.buf[r].len() && self.cls(r, c + 1, big) == cls {
            c += 1;
        }
        (r, c)
    }

    pub(super) fn word_back(&self, p: Pos, big: bool) -> Pos {
        let (mut r, mut c) = (p.0, p.1.min(self.buf[p.0].len()));
        let step = |r: &mut usize, c: &mut usize| -> Option<()> {
            if *c > 0 {
                *c -= 1;
                Some(())
            } else if *r > 0 {
                *r -= 1;
                if self.buf[*r].is_empty() {
                    return None; // an empty line is a word of its own
                }
                *c = self.buf[*r].len() - 1;
                Some(())
            } else {
                None
            }
        };
        if step(&mut r, &mut c).is_none() {
            return (r, 0);
        }
        while self.cls(r, c, big) == 0 {
            if step(&mut r, &mut c).is_none() {
                return (r, 0);
            }
        }
        let cls = self.cls(r, c, big);
        while c > 0 && self.cls(r, c - 1, big) == cls {
            c -= 1;
        }
        (r, c)
    }

    /// Steps one character through the buffer, skipping empty lines.
    pub(super) fn step(&self, p: Pos, fwd: bool) -> Option<Pos> {
        let (r, c) = p;
        if fwd {
            if c + 1 < self.buf[r].len() {
                return Some((r, c + 1));
            }
            (r + 1..self.buf.len())
                .find(|&rr| !self.buf[rr].is_empty())
                .map(|rr| (rr, 0))
        } else {
            if c > 0 {
                return Some((r, c - 1));
            }
            (0..r)
                .rev()
                .find(|&rr| !self.buf[rr].is_empty())
                .map(|rr| (rr, self.buf[rr].len() - 1))
        }
    }

    pub(super) fn at(&self, p: Pos) -> Option<char> {
        self.buf.get(p.0).and_then(|l| l.get(p.1)).copied()
    }

    pub(super) fn match_bracket(&self) -> Option<Pos> {
        let line = &self.buf[self.row];
        let c = (self.col..line.len()).find(|&i| "()[]{}".contains(line[i]))?;
        let ch = line[c];
        let (open, close, fwd) = match ch {
            '(' => ('(', ')', true),
            '[' => ('[', ']', true),
            '{' => ('{', '}', true),
            ')' => ('(', ')', false),
            ']' => ('[', ']', false),
            _ => ('{', '}', false),
        };
        let (same, other) = if fwd { (open, close) } else { (close, open) };
        let mut p = (self.row, c);
        let mut depth = 0usize;
        loop {
            p = self.step(p, fwd)?;
            match self.at(p) {
                Some(x) if x == same => depth += 1,
                Some(x) if x == other => {
                    if depth == 0 {
                        return Some(p);
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
    }

    /// The innermost `open`..`close` pair around the cursor.
    pub(super) fn enclosing(&self, open: char, close: char) -> Option<(Pos, Pos)> {
        let cur = (self.row, self.col);
        let mut p = cur;
        let mut depth = 0usize;
        let open_pos = loop {
            match self.at(p) {
                Some(ch) if ch == open => {
                    if depth == 0 {
                        break p;
                    }
                    depth -= 1;
                }
                Some(ch) if ch == close && p != cur => depth += 1,
                _ => {}
            }
            p = self.step(p, false)?;
        };
        let mut q = open_pos;
        depth = 0;
        let close_pos = loop {
            q = self.step(q, true)?;
            match self.at(q) {
                Some(ch) if ch == open => depth += 1,
                Some(ch) if ch == close => {
                    if depth == 0 {
                        break q;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        };
        Some((open_pos, close_pos))
    }

    pub(super) fn blank_row(&self, r: usize) -> bool {
        is_blank(&self.buf[r])
    }

    pub(super) fn find_char(
        &self,
        kind: char,
        ch: char,
        n: usize,
        skip_adjacent: bool,
    ) -> Option<Pos> {
        let line = &self.buf[self.row];
        let c = self.col;
        let mut cnt = 0;
        match kind {
            'f' | 't' => {
                let mut start = c + 1;
                if kind == 't' && skip_adjacent && line.get(c + 1) == Some(&ch) {
                    start = c + 2;
                }
                for (i, &lc) in line.iter().enumerate().skip(start) {
                    if lc == ch {
                        cnt += 1;
                        if cnt == n {
                            return Some((self.row, if kind == 'f' { i } else { i - 1 }));
                        }
                    }
                }
            }
            _ => {
                let mut end = c.min(line.len());
                if kind == 'T' && skip_adjacent && c >= 1 && line.get(c - 1) == Some(&ch) {
                    end = c - 1;
                }
                for i in (0..end).rev() {
                    if line[i] == ch {
                        cnt += 1;
                        if cnt == n {
                            return Some((self.row, if kind == 'F' { i } else { i + 1 }));
                        }
                    }
                }
            }
        }
        None
    }

    pub(super) fn calc_motion(&self, m: Motion, count: Option<usize>, op: bool) -> Option<Mo> {
        let n = count.unwrap_or(1).max(1);
        let (r, c) = (self.row, self.col);
        let last = self.last_row();
        let len = self.len_of(r);
        let mo = |pos, kind, want| Some(Mo { pos, kind, want });
        match m {
            Motion::Left => {
                if c == 0 {
                    return None;
                }
                // By whole grapheme cluster, not by `char`: an emoji with a skin-tone or gender modifier, a flag, a
                // family joined with zero-width joiners is one glyph, and `3h`/`d3h` moves or deletes three of them,
                // not three invisible code points into the middle of one.
                let mut pos = c;
                for _ in 0..n {
                    if pos == 0 {
                        break;
                    }
                    pos = self.cluster_before(r, pos);
                }
                mo((r, pos), Kind::Exclusive, Want::Reset)
            }
            Motion::Right => {
                let maxc = if op { len } else { self.cluster_before(r, len) };
                if c >= maxc {
                    return None;
                }
                let mut pos = c;
                for _ in 0..n {
                    if pos >= maxc {
                        break;
                    }
                    pos = self.cluster_after(r, pos).min(maxc);
                }
                mo((r, pos), Kind::Exclusive, Want::Reset)
            }
            Motion::Up | Motion::Down => {
                let nr = if m == Motion::Down {
                    if r >= last {
                        return None;
                    }
                    (r + n).min(last)
                } else {
                    if r == 0 {
                        return None;
                    }
                    r.saturating_sub(n)
                };
                let w = self.want.unwrap_or(c);
                let col = w.min(self.len_of(nr).saturating_sub(1));
                mo((nr, col), Kind::Linewise, Want::Set(w))
            }
            Motion::Page(down) => {
                if op {
                    return None;
                }
                self.calc_motion(
                    if down { Motion::Down } else { Motion::Up },
                    Some(HALF_PAGE),
                    false,
                )
            }
            Motion::WordFwd(big) => {
                let mut p = (r, c);
                for _ in 0..n {
                    let np = self.word_fwd(p, big, op);
                    if np == p {
                        break;
                    }
                    p = np;
                }
                (p != (r, c)).then_some(Mo {
                    pos: p,
                    kind: Kind::Exclusive,
                    want: Want::Reset,
                })
            }
            Motion::WordBack(big) => {
                let mut p = (r, c);
                for _ in 0..n {
                    let np = self.word_back(p, big);
                    if np == p {
                        break;
                    }
                    p = np;
                }
                (p != (r, c)).then_some(Mo {
                    pos: p,
                    kind: Kind::Exclusive,
                    want: Want::Reset,
                })
            }
            Motion::WordEnd(big) => {
                let mut p = (r, c);
                for _ in 0..n {
                    let np = self.word_end(p, big);
                    if np == p {
                        break;
                    }
                    p = np;
                }
                (p != (r, c)).then_some(Mo {
                    pos: p,
                    kind: Kind::Inclusive,
                    want: Want::Reset,
                })
            }
            Motion::LineStart => mo((r, 0), Kind::Exclusive, Want::Reset),
            Motion::FirstNonBlank => mo((r, self.first_non_blank(r)), Kind::Exclusive, Want::Reset),
            Motion::LineEnd => {
                let nr = (r + n - 1).min(last);
                let end_col = self.cluster_before(nr, self.len_of(nr));
                mo((nr, end_col), Kind::Inclusive, Want::Set(usize::MAX))
            }
            Motion::GotoLast | Motion::GotoFirst => {
                let default = if m == Motion::GotoLast { last } else { 0 };
                let nr = count.map_or(default, |c| c.saturating_sub(1)).min(last);
                mo((nr, self.first_non_blank(nr)), Kind::Linewise, Want::Reset)
            }
            Motion::Find(k, ch) => {
                let pos = self.find_char(k, ch, n, false)?;
                let kind = if matches!(k, 'f' | 't') {
                    Kind::Inclusive
                } else {
                    Kind::Exclusive
                };
                mo(pos, kind, Want::Reset)
            }
            Motion::RepeatFind(rev) => {
                let (k, ch) = self.last_find?;
                let k = if rev {
                    match k {
                        'f' => 'F',
                        'F' => 'f',
                        't' => 'T',
                        _ => 't',
                    }
                } else {
                    k
                };
                let pos = self.find_char(k, ch, n, true)?;
                let kind = if matches!(k, 'f' | 't') {
                    Kind::Inclusive
                } else {
                    Kind::Exclusive
                };
                mo(pos, kind, Want::Reset)
            }
            Motion::ParaFwd => {
                let mut rr = r;
                for _ in 0..n {
                    while rr < last && self.blank_row(rr) {
                        rr += 1;
                    }
                    while rr < last && !self.blank_row(rr) {
                        rr += 1;
                    }
                }
                let pos = if rr == last && !self.blank_row(last) {
                    (last, self.len_of(last))
                } else {
                    (rr, 0)
                };
                (pos != (r, c)).then_some(Mo {
                    pos,
                    kind: Kind::Exclusive,
                    want: Want::Reset,
                })
            }
            Motion::ParaBack => {
                let mut rr = r;
                for _ in 0..n {
                    while rr > 0 && self.blank_row(rr) {
                        rr -= 1;
                    }
                    while rr > 0 && !self.blank_row(rr) {
                        rr -= 1;
                    }
                }
                (rr != r || c != 0).then_some(Mo {
                    pos: (rr, 0),
                    kind: Kind::Exclusive,
                    want: Want::Reset,
                })
            }
            Motion::Match => {
                let pos = self.match_bracket()?;
                mo(pos, Kind::Inclusive, Want::Reset)
            }
        }
    }

    /// Range (start, end exclusive) of a text object, or rows for linewise ones.
    pub(super) fn text_object(&self, around: bool, obj: char) -> Option<(Pos, Pos, bool)> {
        let (r, c) = (self.row, self.col);
        let line = &self.buf[r];
        match obj {
            'w' | 'W' => {
                if line.is_empty() {
                    return None;
                }
                let big = obj == 'W';
                let c = c.min(line.len() - 1);
                let cls = class(line[c], big);
                let (mut s, mut e) = (c, c + 1);
                while s > 0 && class(line[s - 1], big) == cls {
                    s -= 1;
                }
                while e < line.len() && class(line[e], big) == cls {
                    e += 1;
                }
                if around {
                    if cls != 0 {
                        let mut e2 = e;
                        while e2 < line.len() && class(line[e2], big) == 0 {
                            e2 += 1;
                        }
                        if e2 > e {
                            e = e2;
                        } else {
                            while s > 0 && class(line[s - 1], big) == 0 {
                                s -= 1;
                            }
                        }
                    } else if e < line.len() {
                        let cls2 = class(line[e], big);
                        while e < line.len() && class(line[e], big) == cls2 {
                            e += 1;
                        }
                    }
                }
                Some(((r, s), (r, e), false))
            }
            '"' | '\'' | '`' => {
                let qs: Vec<usize> = (0..line.len())
                    .filter(|&i| line[i] == obj && (i == 0 || line[i - 1] != '\\'))
                    .collect();
                let (a, b) = qs
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| (p[0], p[1]))
                    .find(|&(_, b)| c <= b)?;
                if !around {
                    return Some(((r, a + 1), (r, b), false));
                }
                let (mut s, mut e) = (a, b + 1);
                while e < line.len() && line[e] == ' ' {
                    e += 1;
                }
                if e == b + 1 {
                    while s > 0 && line[s - 1] == ' ' {
                        s -= 1;
                    }
                }
                Some(((r, s), (r, e), false))
            }
            'p' => {
                let cur_blank = self.blank_row(r);
                let (mut s, mut e) = (r, r);
                while s > 0 && self.blank_row(s - 1) == cur_blank {
                    s -= 1;
                }
                while e + 1 < self.buf.len() && self.blank_row(e + 1) == cur_blank {
                    e += 1;
                }
                if around {
                    if e + 1 < self.buf.len() {
                        let b = self.blank_row(e + 1);
                        e += 1;
                        while e + 1 < self.buf.len() && self.blank_row(e + 1) == b {
                            e += 1;
                        }
                    } else if s > 0 {
                        let b = self.blank_row(s - 1);
                        s -= 1;
                        while s > 0 && self.blank_row(s - 1) == b {
                            s -= 1;
                        }
                    }
                }
                Some(((s, 0), (e, 0), true))
            }
            _ => {
                let (open, close) = bracket_pair(obj)?;
                let (op, cp) = self.enclosing(open, close)?;
                if around {
                    return Some((op, (cp.0, cp.1 + 1), false));
                }
                let open_at_eol = op.1 + 1 >= self.len_of(op.0);
                let close_after_indent = self.buf[cp.0][..cp.1].iter().all(|c| c.is_whitespace());
                if open_at_eol && close_after_indent && cp.0 > op.0 {
                    if cp.0 > op.0 + 1 {
                        return Some(((op.0 + 1, 0), (cp.0 - 1, 0), true));
                    }
                    return Some(((cp.0, 0), (cp.0, 0), false));
                }
                Some(((op.0, op.1 + 1), cp, false))
            }
        }
    }
}
