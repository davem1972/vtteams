//! Putting pictures on the screen, once, and taking them away when they no longer belong.
//!
//! The frame ratatui draws leaves the cells a picture covers alone (`CellDiffOption::Skip`); the [`Presenter`] then
//! gives the bytes of the pictures that go over them, to follow ratatui's cell updates in the same write
//! (`output::Display`). It remembers what it has put up, so a picture that is still where it was is not written again (a
//! Sixel is hundreds of kilobytes), and what it has to write for the others is only the difference:
//!
//! - a picture newly in the plan: the cells are cleared, the cursor goes to the rectangle and the picture is written,
//!   with the cursor saved and restored around it so the editor's cursor is where ratatui left it;
//! - a picture no longer in the plan: nothing for Sixel and iTerm2 (the cells it covered are no longer skipped, so
//!   ratatui repaints them over it), a delete for Kitty, whose pictures sit in a layer of their own;
//! - the screen has changed size: ratatui cleared it, so everything is written again.
//!
//! Kitty pictures are sent to the terminal once and then placed and moved by id.
//!
//! What the presenter remembers changes for good only when the bytes are known to have been written
//! ([`Presenter::stage`], then [`Presenter::commit`] or [`Presenter::discard`]), so that a failed write is not taken for a
//! picture on the screen and the next frame sends it again.

use super::Protocol;
use super::encode::{self, Encoded};
use super::plan::OverlayCell;
use ratatui::layout::Rect;
use std::collections::VecDeque;
use std::io::Write;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// One picture to show: where, and what.
#[derive(Clone, Debug)]
pub struct Placement {
    /// The cells it is drawn in: all of its rectangle, or the part of it that is inside the screen's free rows.
    pub rect: Rect,
    pub image: Arc<Encoded>,
    /// The rows of the picture (in its own pixels) that `rect` shows, as [`Encoded::visible_rows`] gives them: all of
    /// them for a picture that is wholly there.
    pub rows: Range<u32>,
    /// Cells of text to write again over the picture once it is written (the labels of the transcript that fall inside
    /// it), for the protocols whose pictures are pixels in the cells. Empty for Kitty, whose pictures are under the text.
    pub overlay: Vec<OverlayCell>,
}

/// The placement is what `shown` already has up: the same picture, in the same cells, showing the same rows, with the same
/// text written over it.
fn same(p: &Placement, s: &Shown) -> bool {
    s.id == p.image.id && s.rect == p.rect && s.rows == p.rows && s.overlay == p.overlay
}

/// Kitty pictures kept in the terminal at once; the oldest that is not showing is freed beyond this.
const KITTY_KEPT: usize = 24;

/// Whether this program has put a Kitty picture in the terminal that has not been deleted, for the panic hook.
static KITTY_IN_USE: AtomicBool = AtomicBool::new(false);

/// Ends a synchronized update (`CSI ? 2026 l`), the pairing of the one each frame begins: a program that stops between
/// the two leaves the terminal holding back everything that follows, its own error message included, until the
/// terminal's own timeout gives up on it. A terminal that has no such mode ignores it.
pub const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";

/// Leave nothing of ours behind in the terminal, after a crash or on the way out: end a synchronized update that may be
/// open (first, or what follows would be held back too) and take away every Kitty picture. Harmless when there was
/// neither, and nothing at all is needed for Sixel and iTerm2, whose pictures are cells of the screen that is left.
pub fn emergency_cleanup(out: &mut impl Write) {
    leave(&KITTY_IN_USE, out);
}

fn leave(in_use: &AtomicBool, out: &mut impl Write) {
    let _ = out.write_all(END_SYNCHRONIZED_UPDATE);
    cleanup_if_set(in_use, out);
    let _ = out.flush();
}

fn cleanup_if_set(in_use: &AtomicBool, out: &mut impl Write) {
    if in_use.swap(false, Ordering::Relaxed) {
        let _ = out.write_all(encode::KITTY_DELETE_ALL);
        let _ = out.flush();
    }
}

#[derive(Clone, Debug)]
struct Shown {
    id: u64,
    rect: Rect,
    rows: Range<u32>,
    overlay: Vec<OverlayCell>,
    kitty: Option<(u32, u32)>, // (image_id, placement_id)
}

/// Write the cells of text that go over a picture: each at its place, in its colours, joined into one run where they follow
/// one another in the same style. Ends with the attributes reset.
fn write_overlay(buf: &mut Vec<u8>, cells: &[OverlayCell]) {
    use unicode_width::UnicodeWidthStr;
    if cells.is_empty() {
        return;
    }
    let mut next: Option<(u16, u16)> = None;
    type Look = (Option<(u8, u8, u8)>, (u8, u8, u8), bool);
    let mut style: Option<Look> = None;
    for c in cells {
        if next != Some((c.x, c.y)) {
            let _ = write!(buf, "\x1b[{};{}H", c.y + 1, c.x + 1);
        }
        let now = (c.fg, c.bg, c.bold);
        if style != Some(now) {
            buf.extend_from_slice(b"\x1b[0");
            if c.bold {
                buf.extend_from_slice(b";1");
            }
            if let Some((r, g, b)) = c.fg {
                let _ = write!(buf, ";38;2;{r};{g};{b}");
            }
            let _ = write!(buf, ";48;2;{};{};{}m", c.bg.0, c.bg.1, c.bg.2);
            style = Some(now);
        }
        buf.extend_from_slice(c.symbol.as_bytes());
        next = Some((c.x + c.symbol.width().max(1) as u16, c.y));
    }
    buf.extend_from_slice(b"\x1b[0m");
}

/// What a [`Presenter`] thought was on the screen before it staged a change, kept until the bytes of the change are known
/// to have been written.
#[derive(Debug)]
struct Before {
    area: Rect,
    shown: Vec<Shown>,
    sent: VecDeque<u32>,
}

/// What is on the screen, and what it takes to make the screen match a plan.
#[derive(Debug, Default)]
pub struct Presenter {
    /// The size of the screen when the pictures in `shown` were written.
    area: Rect,
    shown: Vec<Shown>,
    /// The Kitty pictures the terminal holds, oldest first.
    sent: VecDeque<u32>,
    /// The pictures the last call wrote, which is what a log wants to say.
    fresh: Vec<u64>,
    /// What was thought up before the change that was staged and not yet written ([`Presenter::stage`]).
    undo: Option<Before>,
}

impl Presenter {
    /// Make the screen show `plan`, writing only what differs from the last call. `area` is the whole screen and
    /// `background` the colour its cells are cleared to. Writes nothing, and does not flush, when there is nothing to do.
    /// Returns how many bytes were written. What the presenter thinks is up changes only if the write succeeded. (The loop
    /// does not write by itself: it takes the bytes with [`stage`](Self::stage) and writes them with the rest of the frame.)
    #[cfg(test)]
    pub fn present(
        &mut self,
        area: Rect,
        plan: &[Placement],
        background: (u8, u8, u8),
        out: &mut impl Write,
    ) -> std::io::Result<usize> {
        let mut buf: Vec<u8> = Vec::new();
        let staged = self.stage(area, plan, background, &mut buf);
        if staged == 0 {
            return Ok(0);
        }
        match out.write_all(&buf).and_then(|()| out.flush()) {
            Ok(()) => {
                self.commit();
                Ok(staged)
            }
            Err(e) => {
                self.discard();
                Err(e)
            }
        }
    }

    /// Work out what it takes to make the screen show `plan` and append the bytes to `buf`, returning how many. The
    /// presenter then holds the new state as pending: [`commit`](Self::commit) it once the bytes have been written, or
    /// [`discard`](Self::discard) it if they were not, so that it never claims a picture is up that did not get there.
    /// With nothing to write (including a picture that went away and needs no bytes) the new state is simply taken.
    pub fn stage(
        &mut self,
        area: Rect,
        plan: &[Placement],
        background: (u8, u8, u8),
        buf: &mut Vec<u8>,
    ) -> usize {
        self.fresh.clear();
        // An earlier change that was neither committed nor discarded is taken as written.
        self.undo = None;
        let first = buf.len();
        let changes = area != self.area
            || self.sent.len() > KITTY_KEPT
            || !self.shown.iter().all(|s| plan.iter().any(|p| same(p, s)))
            || !plan.iter().all(|p| self.shown.iter().any(|s| same(p, s)));
        if !changes {
            return 0;
        }
        let before = Before {
            area: self.area,
            shown: self.shown.clone(),
            sent: self.sent.clone(),
        };
        self.change(area, plan, background, buf);
        let wrote = buf.len() - first;
        if wrote > 0 {
            self.undo = Some(before);
        }
        wrote
    }

    /// The staged bytes were written: what was staged is what is up.
    pub fn commit(&mut self) {
        self.undo = None;
    }

    /// The staged bytes were not written (or not all of them): the screen is as it was before they were staged.
    pub fn discard(&mut self) {
        if let Some(before) = self.undo.take() {
            self.area = before.area;
            self.shown = before.shown;
            self.sent = before.sent;
        }
        self.fresh.clear();
    }

    fn change(
        &mut self,
        area: Rect,
        plan: &[Placement],
        background: (u8, u8, u8),
        buf: &mut Vec<u8>,
    ) {
        if area != self.area {
            // ratatui cleared the screen when it changed size: what was shown is gone, and a Kitty picture is where
            // the old layout put it.
            for s in self.shown.drain(..) {
                if let Some((img_id, placement_id)) = s.kitty {
                    buf.extend(encode::kitty_delete(img_id, placement_id, false));
                }
            }
            self.area = area;
        }
        let wanted = |s: &Shown| plan.iter().any(|p| same(p, s));
        for s in self.shown.iter().filter(|s| !wanted(s)) {
            if let Some((img_id, placement_id)) = s.kitty {
                buf.extend(encode::kitty_delete(img_id, placement_id, false));
            }
        }
        self.shown.retain(wanted);
        for p in plan {
            if self.shown.iter().any(|s| same(p, s)) {
                continue;
            }
            let kitty = self.write_one(p, background, buf);
            self.fresh.push(p.image.id);
            self.shown.push(Shown {
                id: p.image.id,
                rect: p.rect,
                rows: p.rows.clone(),
                overlay: p.overlay.clone(),
                kitty,
            });
        }
        // Kitty pictures that are not showing and are past the limit are freed, oldest first.
        while self.sent.len() > KITTY_KEPT {
            let showing = |id: u32| {
                self.shown
                    .iter()
                    .any(|s| s.kitty.is_some_and(|(img, _)| img == id))
            };
            let Some(at) = self.sent.iter().position(|id| !showing(*id)) else {
                break;
            };
            if let Some(id) = self.sent.remove(at) {
                buf.extend(encode::kitty_delete(id, 1, true));
            }
        }
    }

    /// Write one picture into `buf`; the Kitty (image_id, placement_id) if it is one.
    fn write_one(
        &mut self,
        p: &Placement,
        background: (u8, u8, u8),
        buf: &mut Vec<u8>,
    ) -> Option<(u32, u32)> {
        let Rect {
            x,
            y,
            width,
            height,
        } = p.rect;
        // Save the cursor and its attributes; clear the cells the picture is about to cover (it may be smaller than its
        // rectangle, and what was under it is not wanted around it).
        buf.extend_from_slice(b"\x1b7");
        // Not for Kitty: its cells hold what the frame drew (blanks, and the labels over the picture), which is not
        // ours to erase.
        if p.image.protocol != Protocol::Kitty {
            let _ = write!(
                buf,
                "\x1b[0;48;2;{};{};{}m",
                background.0, background.1, background.2
            );
            for row in 0..height {
                let _ = write!(buf, "\x1b[{};{}H", y + row + 1, x + 1);
                buf.extend(std::iter::repeat_n(b' ', usize::from(width)));
            }
            buf.extend_from_slice(b"\x1b[0m");
        }
        let _ = write!(buf, "\x1b[{};{}H", y + 1, x + 1);
        let mut kitty = None;
        match p.image.protocol {
            Protocol::Sixel | Protocol::Iterm => {
                buf.extend_from_slice(&p.image.stream_for(&p.rows))
            }
            Protocol::Kitty => {
                let id = p.image.kitty_id();
                if !self.sent.contains(&id) {
                    buf.extend_from_slice(&p.image.stream);
                    self.sent.push_back(id);
                    KITTY_IN_USE.store(true, Ordering::Relaxed);
                }
                // Pick the smallest placement id >= 1 not in use by another placement of this image
                let used_placements: Vec<u32> = self
                    .shown
                    .iter()
                    .filter_map(|s| s.kitty.filter(|(img, _)| *img == id).map(|(_, p)| p))
                    .collect();
                let placement_id = (1u32..).find(|p| !used_placements.contains(p)).unwrap_or(1);
                let whole = p.rows.start == 0 && p.rows.end >= p.image.size.1;
                let crop = (!whole).then_some(encode::Crop {
                    x: 0,
                    y: p.rows.start,
                    width: p.image.size.0,
                    height: p.rows.end - p.rows.start,
                });
                buf.extend(encode::kitty_place(id, placement_id, true, crop));
                kitty = Some((id, placement_id));
            }
        }
        write_overlay(buf, &p.overlay);
        buf.extend_from_slice(b"\x1b8");
        kitty
    }

    /// Take everything away that is in the terminal's own layer, for leaving: Kitty pictures, placements and data.
    pub fn teardown(&mut self, out: &mut impl Write) {
        let mut buf = Vec::new();
        for id in self.sent.drain(..) {
            buf.extend(encode::kitty_delete(id, 1, true));
        }
        self.shown.clear();
        if !buf.is_empty() {
            let _ = out.write_all(&buf);
            let _ = out.flush();
        }
        KITTY_IN_USE.store(false, Ordering::Relaxed);
    }

    /// The ids of the pictures the last [`stage`](Self::stage) gave bytes for, taken (so a second call gives none).
    pub fn take_new(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.fresh)
    }

    /// How many pictures are up (for tests and the log).
    pub fn showing(&self) -> usize {
        self.shown.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(id: u64, protocol: Protocol) -> Arc<Encoded> {
        let stream = match protocol {
            Protocol::Sixel => format!("\x1bPq#{id}\x1b\\").into_bytes(),
            Protocol::Iterm => format!("\x1b]1337;File=inline=1:{id}\x1b\\").into_bytes(),
            Protocol::Kitty => format!("\x1b_Ga=t,i={id};AAAA\x1b\\").into_bytes(),
        };
        Arc::new(Encoded {
            id,
            protocol,
            size: (10, 10),
            stream,
            layout: None,
        })
    }

    fn at(rect: Rect, image: &Arc<Encoded>) -> Placement {
        Placement {
            rect,
            image: Arc::clone(image),
            rows: 0..image.size.1,
            overlay: Vec::new(),
        }
    }

    const SCREEN: Rect = Rect::new(0, 0, 100, 40);
    const BG: (u8, u8, u8) = (17, 17, 27);

    fn run(presenter: &mut Presenter, area: Rect, plan: &[Placement]) -> String {
        let mut out = Vec::new();
        let n = presenter.present(area, plan, BG, &mut out).unwrap();
        assert_eq!(n, out.len());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_new_picture_is_cleared_for_positioned_written_and_the_cursor_put_back() {
        let image = encoded(1, Protocol::Sixel);
        let mut p = Presenter::default();
        let out = run(&mut p, SCREEN, &[at(Rect::new(7, 5, 3, 2), &image)]);
        assert_eq!(
            out,
            "\x1b7\x1b[0;48;2;17;17;27m\x1b[6;8H   \x1b[7;8H   \x1b[0m\x1b[6;8H\x1bPq#1\x1b\\\x1b8"
        );
        assert_eq!(p.showing(), 1);
    }

    fn part(rect: Rect, image: &Arc<Encoded>, rows: std::ops::Range<u32>) -> Placement {
        Placement {
            rect,
            image: Arc::clone(image),
            rows,
            overlay: Vec::new(),
        }
    }

    fn text_at(x: u16, y: u16, text: &str) -> Vec<OverlayCell> {
        text.chars()
            .enumerate()
            .map(|(i, c)| OverlayCell {
                x: x + i as u16,
                y,
                symbol: c.to_string(),
                fg: Some((1, 2, 3)),
                bg: (4, 5, 6),
                bold: true,
            })
            .collect()
    }

    fn with_text(rect: Rect, image: &Arc<Encoded>, overlay: Vec<OverlayCell>) -> Placement {
        Placement {
            overlay,
            ..at(rect, image)
        }
    }

    #[test]
    fn text_over_a_sixel_is_written_after_it_in_its_colours_and_in_one_run() {
        let image = encoded(1, Protocol::Sixel);
        let rect = Rect::new(7, 5, 10, 3);
        let mut p = Presenter::default();
        let out = run(
            &mut p,
            SCREEN,
            &[with_text(rect, &image, text_at(9, 7, " ↓ G "))],
        );
        let picture = out.find("\x1bPq#1").expect("the picture");
        let text = out.find(" ↓ G ").expect("the text");
        assert!(picture < text, "the text goes after the picture: {out:?}");
        assert!(
            out.ends_with("\x1b[8;10H\x1b[0;1;38;2;1;2;3;48;2;4;5;6m ↓ G \x1b[0m\x1b8"),
            "positioned once, styled once, then the attributes and the cursor put back: {out:?}"
        );
        assert_eq!(
            out.matches("\x1b[8;10H").count(),
            1,
            "one run, not a position for each cell"
        );
    }

    #[test]
    fn a_cell_that_does_not_follow_the_last_gets_its_own_position() {
        let image = encoded(1, Protocol::Sixel);
        let rect = Rect::new(0, 0, 20, 9);
        let mut cells = text_at(2, 1, "ab");
        cells.extend(text_at(8, 1, "c"));
        cells.extend(text_at(8, 2, "d"));
        let mut p = Presenter::default();
        let out = run(&mut p, SCREEN, &[with_text(rect, &image, cells)]);
        let tail = &out[out.find("\x1bPq#1").unwrap()..];
        assert!(
            tail.contains(
                "\x1b[2;3H\x1b[0;1;38;2;1;2;3;48;2;4;5;6mab\x1b[2;9Hc\x1b[3;9Hd\x1b[0m\x1b8"
            ),
            "{tail:?}"
        );
    }

    #[test]
    fn text_that_comes_or_goes_makes_a_sixel_be_written_again_and_text_that_stays_does_not() {
        let image = encoded(1, Protocol::Sixel);
        let rect = Rect::new(7, 5, 10, 3);
        let label = text_at(9, 7, " ↓ G ");
        let mut p = Presenter::default();
        assert!(run(&mut p, SCREEN, &[with_text(rect, &image, Vec::new())]).contains("\x1bPq#1"));
        assert_eq!(
            run(&mut p, SCREEN, &[with_text(rect, &image, Vec::new())]),
            "",
            "nothing over it, nothing to write"
        );
        // The label appears over a picture that has not moved: the picture is written again, with the label on it.
        let appears = run(&mut p, SCREEN, &[with_text(rect, &image, label.clone())]);
        assert!(
            appears.contains("\x1bPq#1") && appears.contains(" ↓ G "),
            "{appears:?}"
        );
        assert_eq!(
            run(&mut p, SCREEN, &[with_text(rect, &image, label.clone())]),
            "",
            "the same label is left alone"
        );
        // It goes: the cells are the picture's again, so the picture is written again without it.
        let goes = run(&mut p, SCREEN, &[with_text(rect, &image, Vec::new())]);
        assert!(goes.contains("\x1bPq#1") && !goes.contains("G"), "{goes:?}");
        // A different label counts too.
        run(&mut p, SCREEN, &[with_text(rect, &image, label)]);
        let other = run(
            &mut p,
            SCREEN,
            &[with_text(rect, &image, text_at(9, 7, " x "))],
        );
        assert!(other.contains("\x1bPq#1") && other.contains(" x ") && !other.contains("G"));
    }

    #[test]
    fn a_changed_slice_is_written_again_and_an_unchanged_one_is_not() {
        let image = encoded(1, Protocol::Sixel);
        let rect = Rect::new(7, 5, 3, 2);
        let mut p = Presenter::default();
        assert!(!run(&mut p, SCREEN, &[part(rect, &image, 0..10)]).is_empty());
        assert_eq!(run(&mut p, SCREEN, &[part(rect, &image, 0..10)]), "");
        // The same cells showing other rows of the picture: written again, cleared first.
        let again = run(&mut p, SCREEN, &[part(rect, &image, 2..10)]);
        assert!(
            again.starts_with("\x1b7\x1b[0;48;2;") && again.contains("\x1bPq#1"),
            "{again:?}"
        );
        assert_eq!(run(&mut p, SCREEN, &[part(rect, &image, 2..10)]), "");
        assert_eq!(p.showing(), 1);
    }

    #[test]
    fn a_kitty_crop_that_changes_is_placed_again_from_the_data_the_terminal_holds() {
        let image = encoded(9, Protocol::Kitty);
        let rect = Rect::new(7, 5, 3, 2);
        let mut p = Presenter::default();
        let first = run(&mut p, SCREEN, &[part(rect, &image, 0..10)]);
        assert!(
            first.contains("a=t") && first.contains("a=p,i=9,p=1,q=2,C=1,z=-1\x1b\\"),
            "whole: no crop keys"
        );
        let cut = run(&mut p, SCREEN, &[part(rect, &image, 4..10)]);
        assert!(!cut.contains("a=t"), "no data sent again");
        assert!(
            cut.contains("a=d,d=i,i=9"),
            "the old placement is taken away"
        );
        assert!(
            cut.contains("a=p,i=9,p=1,q=2,C=1,x=0,y=4,w=10,h=6,z=-1\x1b\\"),
            "{cut:?}"
        );
        assert_eq!(run(&mut p, SCREEN, &[part(rect, &image, 4..10)]), "");
    }

    #[test]
    fn the_pictures_just_written_can_be_asked_for_once() {
        let (a, b) = (encoded(1, Protocol::Sixel), encoded(2, Protocol::Sixel));
        let mut p = Presenter::default();
        run(
            &mut p,
            SCREEN,
            &[at(Rect::new(0, 0, 2, 2), &a), at(Rect::new(0, 5, 2, 2), &b)],
        );
        assert_eq!(p.take_new(), [1, 2]);
        assert!(p.take_new().is_empty(), "taken");
        run(
            &mut p,
            SCREEN,
            &[at(Rect::new(0, 0, 2, 2), &a), at(Rect::new(0, 5, 2, 2), &b)],
        );
        assert!(
            p.take_new().is_empty(),
            "nothing was written the second time"
        );
    }

    #[test]
    fn a_picture_that_has_not_moved_is_not_written_again() {
        let image = encoded(1, Protocol::Sixel);
        let plan = [at(Rect::new(7, 5, 3, 2), &image)];
        let mut p = Presenter::default();
        assert!(!run(&mut p, SCREEN, &plan).is_empty());
        for _ in 0..5 {
            let mut out = Vec::new();
            assert_eq!(p.present(SCREEN, &plan, BG, &mut out).unwrap(), 0);
            assert!(out.is_empty(), "no bytes at all, not even a flush's worth");
        }
    }

    /// A pipe that refuses everything, as a link that has gone away does.
    struct Refusing;

    impl Write for Refusing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("the link went away"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_staged_change_counts_only_once_it_is_committed_and_not_if_it_is_discarded() {
        let image = encoded(1, Protocol::Sixel);
        let plan = [at(Rect::new(7, 5, 3, 2), &image)];
        let mut p = Presenter::default();
        let mut first = Vec::new();
        let n = p.stage(SCREEN, &plan, BG, &mut first);
        assert!(n > 0 && n == first.len());
        // The bytes did not get there.
        p.discard();
        assert_eq!(p.showing(), 0, "not up");
        assert!(p.take_new().is_empty(), "and nothing to say was written");
        let mut again = Vec::new();
        assert_eq!(p.stage(SCREEN, &plan, BG, &mut again), n);
        assert_eq!(again, first, "the same bytes, sent again");
        p.commit();
        assert_eq!(p.showing(), 1);
        assert_eq!(p.take_new(), [1]);
        let mut none = Vec::new();
        assert_eq!(
            p.stage(SCREEN, &plan, BG, &mut none),
            0,
            "up now: nothing to send"
        );
        assert!(none.is_empty());
        // A discard with nothing staged changes nothing.
        p.discard();
        assert_eq!(p.showing(), 1);
    }

    #[test]
    fn what_a_discarded_stage_took_down_is_up_again_and_what_it_sent_to_kitty_is_not_held() {
        let a = encoded(9, Protocol::Kitty);
        let mut p = Presenter::default();
        let mut buf = Vec::new();
        p.stage(SCREEN, &[at(Rect::new(7, 5, 3, 2), &a)], BG, &mut buf);
        p.commit();
        // The picture moves; the bytes for it are lost.
        let mut moved = Vec::new();
        assert!(p.stage(SCREEN, &[at(Rect::new(7, 4, 3, 2), &a)], BG, &mut moved) > 0);
        p.discard();
        assert_eq!(
            p.showing(),
            1,
            "the old placement is still the one thought up"
        );
        let mut retry = Vec::new();
        p.stage(SCREEN, &[at(Rect::new(7, 4, 3, 2), &a)], BG, &mut retry);
        assert_eq!(retry, moved, "the move is staged again whole");
        // A Kitty picture whose data never got there is sent again with it.
        let b = encoded(10, Protocol::Kitty);
        let mut lost = Vec::new();
        p.stage(SCREEN, &[at(Rect::new(0, 0, 2, 2), &b)], BG, &mut lost);
        assert!(String::from_utf8_lossy(&lost).contains("a=t"));
        p.discard();
        let mut again = Vec::new();
        p.stage(SCREEN, &[at(Rect::new(0, 0, 2, 2), &b)], BG, &mut again);
        assert!(
            String::from_utf8_lossy(&again).contains("a=t"),
            "the terminal does not hold it, so the data goes again"
        );
    }

    #[test]
    fn a_write_that_fails_is_not_taken_for_a_picture_that_is_up() {
        let image = encoded(1, Protocol::Sixel);
        let plan = [at(Rect::new(7, 5, 3, 2), &image)];
        let mut p = Presenter::default();
        assert!(p.present(SCREEN, &plan, BG, &mut Refusing).is_err());
        assert_eq!(p.showing(), 0);
        assert!(
            !run(&mut p, SCREEN, &plan).is_empty(),
            "sent again when it can be"
        );
        assert_eq!(p.showing(), 1);
    }

    #[test]
    fn a_picture_that_moved_or_changed_is_written_where_it_now_is() {
        let a = encoded(1, Protocol::Sixel);
        let b = encoded(2, Protocol::Sixel);
        let mut p = Presenter::default();
        run(&mut p, SCREEN, &[at(Rect::new(7, 5, 3, 2), &a)]);
        let moved = run(&mut p, SCREEN, &[at(Rect::new(7, 4, 3, 2), &a)]);
        assert!(moved.contains("\x1b[5;8H\x1bPq#1"), "{moved:?}");
        let changed = run(&mut p, SCREEN, &[at(Rect::new(7, 4, 3, 2), &b)]);
        assert!(changed.contains("\x1bPq#2") && !changed.contains("\x1bPq#1"));
        assert_eq!(p.showing(), 1);
    }

    #[test]
    fn a_sixel_taken_out_of_the_plan_is_left_to_ratatui_to_paint_over() {
        let a = encoded(1, Protocol::Sixel);
        let mut p = Presenter::default();
        run(&mut p, SCREEN, &[at(Rect::new(7, 5, 3, 2), &a)]);
        assert_eq!(run(&mut p, SCREEN, &[]), "", "nothing to write");
        assert_eq!(p.showing(), 0, "but it is no longer thought to be showing");
        assert!(
            !run(&mut p, SCREEN, &[at(Rect::new(7, 5, 3, 2), &a)]).is_empty(),
            "so it is written again when it returns"
        );
    }

    #[test]
    fn a_resized_screen_has_everything_written_again() {
        let a = encoded(1, Protocol::Sixel);
        let plan = [at(Rect::new(7, 5, 3, 2), &a)];
        let mut p = Presenter::default();
        run(&mut p, SCREEN, &plan);
        assert_eq!(run(&mut p, SCREEN, &plan), "");
        let again = run(&mut p, Rect::new(0, 0, 90, 40), &plan);
        assert!(again.contains("\x1bPq#1"), "{again:?}");
    }

    #[test]
    fn a_kitty_picture_is_sent_once_then_placed_moved_and_deleted_by_id() {
        let a = encoded(9, Protocol::Kitty);
        let mut p = Presenter::default();
        let first = run(&mut p, SCREEN, &[at(Rect::new(7, 5, 3, 2), &a)]);
        assert!(first.contains("\x1b_Ga=t,i=9;AAAA\x1b\\"), "sent");
        assert_eq!(
            first,
            "\x1b7\x1b[6;8H\x1b_Ga=t,i=9;AAAA\x1b\\\x1b_Ga=p,i=9,p=1,q=2,C=1,z=-1\x1b\\\x1b8",
            "no clearing for Kitty: its cells are the frame's, with the text over the picture"
        );
        let moved = run(&mut p, SCREEN, &[at(Rect::new(7, 4, 3, 2), &a)]);
        assert!(
            !moved.contains("a=t"),
            "the data is not sent again: {moved:?}"
        );
        let (delete, place) = (
            moved
                .find("a=d,d=i,i=9,p=1,q=2")
                .expect("the old placement is deleted"),
            moved.find("a=p,i=9").expect("and placed again"),
        );
        assert!(delete < place);
        let gone = run(&mut p, SCREEN, &[]);
        assert_eq!(
            gone, "\x1b_Ga=d,d=i,i=9,p=1,q=2\x1b\\",
            "only the placement goes"
        );
        let back = run(&mut p, SCREEN, &[at(Rect::new(7, 4, 3, 2), &a)]);
        assert!(!back.contains("a=t"), "still held by the terminal");
        let mut out = Vec::new();
        p.teardown(&mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b_Ga=d,d=I,i=9,p=1,q=2\x1b\\",
            "leaving frees the data"
        );
        let mut out = Vec::new();
        p.teardown(&mut out);
        assert!(out.is_empty(), "and only once");
    }

    #[test]
    fn the_kitty_pictures_kept_in_the_terminal_are_bounded() {
        let mut p = Presenter::default();
        let mut freed = 0;
        for id in 1..=(KITTY_KEPT as u64 + 5) {
            let image = encoded(id, Protocol::Kitty);
            let out = run(&mut p, SCREEN, &[at(Rect::new(0, 0, 2, 2), &image)]);
            freed += out.matches("d=I").count();
        }
        assert_eq!(freed, 5, "the oldest pictures that were not showing");
        assert_eq!(p.sent.len(), KITTY_KEPT);
    }

    #[test]
    fn several_pictures_are_written_and_dropped_independently() {
        let (a, b) = (encoded(1, Protocol::Sixel), encoded(2, Protocol::Sixel));
        let ra = Rect::new(2, 2, 4, 3);
        let rb = Rect::new(2, 10, 4, 3);
        let mut p = Presenter::default();
        let first = run(&mut p, SCREEN, &[at(ra, &a), at(rb, &b)]);
        assert!(first.contains("#1") && first.contains("#2"));
        let only_b = run(&mut p, SCREEN, &[at(rb, &b)]);
        assert_eq!(only_b, "");
        assert_eq!(p.showing(), 1);
        let a_again = run(&mut p, SCREEN, &[at(ra, &a), at(rb, &b)]);
        assert!(a_again.contains("#1") && !a_again.contains("#2"));
    }

    #[test]
    fn duplicate_kitty_placements_use_distinct_ids() {
        let image = encoded(9, Protocol::Kitty);
        let mut p = Presenter::default();
        let r1 = Rect::new(2, 2, 4, 3);
        let r2 = Rect::new(8, 2, 4, 3);
        let first = run(&mut p, SCREEN, &[at(r1, &image), at(r2, &image)]);
        assert!(first.contains(",p=1,"), "first placement uses p=1");
        assert!(first.contains(",p=2,"), "second placement uses p=2");
        let only_second = run(&mut p, SCREEN, &[at(r2, &image)]);
        assert!(
            only_second.contains("a=d,d=i,i=9,p=1,q=2"),
            "deletes only p=1"
        );
        assert!(!only_second.contains("p=2"), "p=2 is not deleted");
    }

    #[test]
    fn the_panic_cleanup_deletes_only_what_was_put_up_and_only_once() {
        // A flag of its own: the real one is shared by every test that shows a Kitty picture.
        let flag = AtomicBool::new(false);
        let mut out = Vec::new();
        cleanup_if_set(&flag, &mut out);
        assert!(out.is_empty(), "nothing was shown");
        flag.store(true, Ordering::Relaxed);
        cleanup_if_set(&flag, &mut out);
        assert_eq!(out, encode::KITTY_DELETE_ALL);
        out.clear();
        cleanup_if_set(&flag, &mut out);
        assert!(out.is_empty(), "and only once");
    }

    /// What leaving writes, to the sink the restore functions are given: a synchronized update that may be open is
    /// ended first, so that nothing after it is held back, and then the Kitty pictures are taken away if there are any.
    #[test]
    fn leaving_ends_a_synchronized_update_first_and_deletes_kitty_pictures_if_there_are_any() {
        assert_eq!(END_SYNCHRONIZED_UPDATE, b"\x1b[?2026l");
        let flag = AtomicBool::new(false);
        let mut out = Vec::new();
        leave(&flag, &mut out);
        assert_eq!(out, END_SYNCHRONIZED_UPDATE, "no picture: only the update");
        flag.store(true, Ordering::Relaxed);
        let mut out = Vec::new();
        leave(&flag, &mut out);
        let mut expected = END_SYNCHRONIZED_UPDATE.to_vec();
        expected.extend_from_slice(encode::KITTY_DELETE_ALL);
        assert_eq!(out, expected, "the update is ended before the pictures go");
        // The one the panic hook and the normal exit call writes at least the end of the update.
        let mut out = Vec::new();
        emergency_cleanup(&mut out);
        assert!(out.starts_with(END_SYNCHRONIZED_UPDATE));
    }
}
