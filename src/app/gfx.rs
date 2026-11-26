//! Pictures drawn with the terminal's graphics protocol: which are drawn this frame, their encoding on workers, and the
//! `:images` choices.
//!
//! The transcript always holds a picture's block art. When a protocol is in use and a picture is ready, wholly on screen
//! and nothing is drawn over it, [`App::plan_graphics`] (from `ui::prepare`, the only place drawing state changes) says
//! where, `ui::draw` leaves those cells alone, and `main::event_loop` has the [`Presenter`] write the pictures after the
//! frame is flushed. A picture not encoded yet is encoded on a worker and shows as block art until then.

use super::{App, Ev, Overlay};
use crate::gfx::encode::{self, Encoded};
use crate::gfx::plan::{self, Viewport};
use crate::gfx::present::{Placement, Presenter};
use crate::gfx::probe::Facts;
use crate::gfx::{self, CellPx, Graphics, ImageChoice, Protocol};
use crate::imgart::{self, ImgMode};
use crate::render::ImgState;
use crate::transcript::GUTTER;
use crate::view::Geometry;
use image::RgbImage;
use ratatui::layout::Rect;
use ratatui::style::Color;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Encodings made at the same time: a Sixel of a large picture is a few hundred milliseconds of one worker.
const MAX_ENCODES_AT_ONCE: usize = 2;
/// Encoded pictures kept, by count and by bytes; the least recently drawn go first.
const MAX_KEPT: usize = 40;
const MAX_KEPT_BYTES: usize = 32 * 1024 * 1024;

/// How long the loop waits for input while an encoding is being made, so that its answer is drawn within a few
/// milliseconds of arriving instead of at the next idle tick.
const BUSY_POLL: Duration = Duration::from_millis(10);
/// How long it waits otherwise.
const IDLE_POLL: Duration = Duration::from_millis(60);
/// The most pictures whose timeline is kept for the log.
const MAX_MARKS: usize = 64;

/// When the steps of getting one picture on the screen happened, for the log (times only, never content).
struct Marks {
    /// The picture was decoded, or else first wanted.
    first: Instant,
    last: Instant,
    planned: bool,
}

/// A picture the frame wants drawn: its whole rectangle in cells, and the part of it that is inside.
struct Wanted {
    url: String,
    cols: usize,
    art_rows: usize,
    slice: plan::Slice,
    image: Arc<RgbImage>,
}

/// Which encoding: the picture's URL, the protocol, and the size in pixels it was made for.
pub type EncodeKey = (String, Protocol, u32, u32);

enum Slot {
    /// Wanted by the frame being drawn and not started: dropped at the start of the next frame, wanted again or not.
    Queued {
        image: Arc<RgbImage>,
        id: u64,
    },
    Running,
    Ready {
        image: Arc<Encoded>,
        used: u64,
    },
    Failed,
}

/// What the app knows about drawing pictures with the terminal's graphics.
pub struct Gfx {
    choice: ImageChoice,
    /// What detection found (used when the choice is `auto`).
    detected: Option<Protocol>,
    cell: Option<CellPx>,
    /// The cell size came from the system and can be read again when the window is resized.
    cell_from_system: bool,
    cache: HashMap<EncodeKey, Slot>,
    /// What to draw this frame, decided by [`App::plan_graphics`].
    plan: Vec<Placement>,
    next_id: u64,
    running: usize,
    /// When each picture was decoded, by URL (bounded).
    decoded: HashMap<String, Instant>,
    /// The log's timeline of each picture being encoded, by encoding id.
    marks: HashMap<u64, Marks>,
    /// Counts frames, for "least recently drawn".
    frame: u64,
}

impl Default for Gfx {
    fn default() -> Self {
        Self {
            choice: ImageChoice::DEFAULT,
            detected: None,
            cell: None,
            cell_from_system: false,
            cache: HashMap::new(),
            plan: Vec::new(),
            next_id: 0,
            running: 0,
            decoded: HashMap::new(),
            marks: HashMap::new(),
            frame: 0,
        }
    }
}

impl Gfx {
    fn graphics(&self) -> Option<Graphics> {
        gfx::resolve(self.choice, self.detected, self.cell)
    }

    /// Log a step of picture `id`'s way to the screen: how long since the one before it and since the picture was decoded.
    fn mark(&mut self, id: u64, what: &str) {
        let Some(m) = self.marks.get_mut(&id) else {
            return;
        };
        let now = Instant::now();
        log::debug!(
            "picture #{id}: {what} (+{} ms, {} ms since it was decoded or first wanted)",
            now.duration_since(m.last).as_millis(),
            now.duration_since(m.first).as_millis()
        );
        m.last = now;
    }

    /// Start a picture's timeline, bounded.
    fn start_marks(&mut self, id: u64, url: &str) {
        if self.marks.len() >= MAX_MARKS
            && let Some(oldest) = self.marks.keys().min().copied()
        {
            self.marks.remove(&oldest);
        }
        let now = Instant::now();
        let first = self.decoded.get(url).copied().unwrap_or(now);
        self.marks.insert(
            id,
            Marks {
                first,
                last: first,
                planned: false,
            },
        );
    }

    /// Forget encodings that can no longer be shown: the picture is gone from the store, or it is over the limits.
    fn evict(&mut self) {
        let frame = self.frame;
        let bytes = |cache: &HashMap<EncodeKey, Slot>| -> usize {
            cache
                .values()
                .map(|s| match s {
                    Slot::Ready { image, .. } => image.stream.len(),
                    _ => 0,
                })
                .sum()
        };
        loop {
            let ready = self
                .cache
                .values()
                .filter(|s| matches!(s, Slot::Ready { .. }))
                .count();
            if ready <= MAX_KEPT && bytes(&self.cache) <= MAX_KEPT_BYTES {
                return;
            }
            // Never one that is being drawn right now.
            let oldest = self
                .cache
                .iter()
                .filter_map(|(k, s)| match s {
                    Slot::Ready { used, .. } if *used < frame => Some((k.clone(), *used)),
                    _ => None,
                })
                .min_by_key(|(_, used)| *used);
            match oldest {
                Some((key, _)) => {
                    self.cache.remove(&key);
                }
                None => return,
            }
        }
    }
}

impl App {
    /// Set what the `images` setting asks for and what was learned about the terminal. Called once at start-up.
    pub fn set_images(&mut self, choice: ImageChoice, facts: Facts) {
        self.gfx.detected = facts.detected;
        self.gfx.cell = facts.cell;
        self.gfx.cell_from_system = facts.cell_from_system;
        self.set_image_choice(choice);
    }

    /// Choose how pictures are drawn: the glyph set for block art follows (blocks when the choice is graphics).
    pub fn set_image_choice(&mut self, choice: ImageChoice) {
        self.gfx.choice = choice;
        self.img_mode = choice.art_mode();
    }

    /// The protocol pictures are drawn with right now, if any.
    pub fn graphics(&self) -> Option<Graphics> {
        self.gfx.graphics()
    }

    /// The pictures to draw over this frame, as decided by `ui::prepare`.
    pub fn gfx_plan(&self) -> &[Placement] {
        &self.gfx.plan
    }

    /// The window changed size: if the cell size came from the system, read it again.
    pub fn on_resize(&mut self, system_cell: Option<CellPx>) {
        if self.gfx.cell_from_system
            && let Some(cell) = system_cell
            && self.gfx.cell != Some(cell)
        {
            log::debug!("cell size now {}", cell.label());
            self.gfx.cell = Some(cell);
        }
    }

    /// Write this frame's pictures (and take down the ones that are gone) through `presenter`; see `gfx::present`. The
    /// loop gathers them with the rest of the frame instead (`stage_graphics`); tests write them themselves.
    #[cfg(test)]
    pub fn present_graphics(
        &mut self,
        presenter: &mut Presenter,
        out: &mut impl std::io::Write,
    ) -> std::io::Result<usize> {
        let background = match crate::theme::BG {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => (0, 0, 0),
        };
        let started = Instant::now();
        let wrote = presenter.present(self.view.screen, &self.gfx.plan, background, out)?;
        self.graphics_written(presenter, wrote, started.elapsed());
        Ok(wrote)
    }

    /// Put the bytes that this frame's pictures need (and for taking down the ones that are gone) at the end of `buf`,
    /// returning how many. Nothing is written: the caller writes `buf` and then says so with
    /// [`graphics_written`](Self::graphics_written), or tells the presenter it was not (`Presenter::discard`).
    pub fn stage_graphics(&mut self, presenter: &mut Presenter, buf: &mut Vec<u8>) -> usize {
        let background = match crate::theme::BG {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => (0, 0, 0),
        };
        presenter.stage(self.view.screen, &self.gfx.plan, background, buf)
    }

    /// The bytes [`stage_graphics`](Self::stage_graphics) gave (`wrote` of them) are on their way to the terminal and
    /// took `took` to hand over: the presenter now counts those pictures as up, and the log has their timelines.
    pub fn graphics_written(&mut self, presenter: &mut Presenter, wrote: usize, took: Duration) {
        presenter.commit();
        for id in presenter.take_new() {
            self.gfx.mark(
                id,
                &format!(
                    "written to the terminal: {wrote} bytes took {} ms to hand over",
                    took.as_millis()
                ),
            );
            self.gfx.marks.remove(&id);
        }
    }

    /// Decide which pictures are drawn with graphics this frame, and ask for the encodings that are missing. The rules:
    /// a protocol is in use; no popup, overlay or suggestion list is open (they are drawn over the transcript, and a
    /// picture would show through them); the picture is decoded, at least partly inside the transcript, and its
    /// encoding is ready. The two labels the transcript draws over its first and last row do not cut it: the picture is
    /// drawn under them, and the cells of their text are written again over it (Kitty draws pictures under text). A picture partly outside is cut: only the cells inside are drawn, and
    /// only the rows of the picture that belong to them (a Sixel in whole bands, Kitty by a source rectangle, with
    /// nothing sent again). iTerm2 cannot crop, so a partly visible picture stays block art there. Anything else is
    /// block art.
    pub fn plan_graphics(&mut self, g: &Geometry) {
        self.gfx.plan.clear();
        self.gfx.frame += 1;
        // What was wanted last frame and not started is wanted again below, or not at all any more.
        self.gfx
            .cache
            .retain(|_, s| !matches!(s, Slot::Queued { .. }));
        let store = self.images.store();
        self.gfx
            .cache
            .retain(|(url, ..), _| store.map.contains_key(url));
        let Some(active) = self.gfx.graphics() else {
            return;
        };
        if self.img_mode == ImgMode::Off
            || self.overlay != Overlay::None
            || g.transcript == Rect::default()
            || self.mention_suggestions().is_some()
        {
            return;
        }
        let wanted = self.pictures_in_view(g, u32::from(active.cell.h));
        let labels = crate::ui::hint_labels_of(self, g);
        let background = match crate::theme::BG {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => (0, 0, 0),
        };
        let frame = self.gfx.frame;
        for w in wanted {
            // The encoding is made for the picture's whole rectangle, whatever part of it is on screen, so scrolling
            // never asks for another.
            let size = (
                w.cols as u32 * u32::from(active.cell.w),
                w.art_rows as u32 * u32::from(active.cell.h),
            );
            let image = w.image;
            let key = (w.url, active.protocol, size.0, size.1);
            let mut first_plan = None;
            match self.gfx.cache.get_mut(&key) {
                Some(Slot::Ready { image, used }) => {
                    *used = frame;
                    if let Some(rows) = image.visible_rows(w.slice.from_px..w.slice.to_px) {
                        first_plan = Some(image.id);
                        // The labels the transcript draws over its rows that fall inside the picture are written again
                        // after it, so that they show on top. (A Kitty picture is under the text: nothing to write.)
                        let overlay = if image.protocol == Protocol::Kitty {
                            Vec::new()
                        } else {
                            labels
                                .iter()
                                .flat_map(|l| {
                                    plan::overlay_cells(
                                        (l.rect.x, l.rect.y),
                                        l.text,
                                        l.style,
                                        background,
                                        w.slice.rect,
                                    )
                                })
                                .collect()
                        };
                        self.gfx.plan.push(Placement {
                            rect: w.slice.rect,
                            image: Arc::clone(image),
                            rows,
                            overlay,
                        });
                    }
                }
                Some(_) => {}
                None => {
                    self.gfx.next_id += 1;
                    let id = self.gfx.next_id;
                    let (protocol, w, h) = (key.1, key.2, key.3);
                    let url = key.0.clone();
                    self.gfx.cache.insert(key, Slot::Queued { image, id });
                    self.gfx.start_marks(id, &url);
                    self.gfx.mark(
                        id,
                        &format!("wanted by the frame, as {} at {w}x{h} px", protocol.label()),
                    );
                }
            }
            if let Some(id) = first_plan
                && self.gfx.marks.get(&id).is_some_and(|m| !m.planned)
            {
                if let Some(m) = self.gfx.marks.get_mut(&id) {
                    m.planned = true;
                }
                self.gfx.mark(id, "in the plan of a frame");
            }
        }
        self.gfx.evict();
    }

    /// The decoded pictures that are at least partly inside the free part of the transcript, with the part that is.
    fn pictures_in_view(&self, g: &Geometry, cell_h: u32) -> Vec<Wanted> {
        let Some(st) = self.state() else {
            return Vec::new();
        };
        if !st.loaded || st.messages().is_empty() {
            return Vec::new();
        }
        let height = usize::from(g.transcript.height);
        let view = Viewport {
            area: g.transcript,
            scroll: st.scroll,
        };
        let body_width = usize::from(g.transcript.width).saturating_sub(GUTTER + 1);
        let store = self.images.store();
        let mut out = Vec::new();
        for (rows, url) in &st.transcript.image_rows {
            if rows.end <= st.scroll || rows.start >= st.scroll + height {
                continue;
            }
            let Some(ImgState::Ready(img)) = store.map.get(url) else {
                continue;
            };
            let (cols, art_rows) = imgart::art_cells(img, body_width);
            // The picture's own rows only; a caption under it is text.
            let art_rows = art_rows.min(rows.len());
            if let Some(slice) =
                plan::visible_slice(&view, GUTTER as u16, rows, cols, art_rows, cell_h)
            {
                out.push(Wanted {
                    url: url.clone(),
                    cols,
                    art_rows,
                    slice,
                    image: Arc::clone(img),
                });
            }
        }
        out
    }

    /// Start the encodings the last frame asked for, a few at a time, on workers.
    pub(super) fn dispatch_encodes(&mut self) {
        if self.gfx.running >= MAX_ENCODES_AT_ONCE {
            return;
        }
        let mut queued: Vec<EncodeKey> = self
            .gfx
            .cache
            .iter()
            .filter(|(_, s)| matches!(s, Slot::Queued { .. }))
            .map(|(k, _)| k.clone())
            .collect();
        queued.sort();
        for key in queued {
            if self.gfx.running >= MAX_ENCODES_AT_ONCE {
                break;
            }
            let Some(Slot::Queued { image, id }) =
                self.gfx.cache.insert(key.clone(), Slot::Running)
            else {
                continue;
            };
            self.gfx.running += 1;
            let (_, protocol, w, h) = key.clone();
            self.gfx.mark(id, "handed to a worker");
            let handed = Instant::now();
            self.spawn_cpu_task(move || {
                let started = Instant::now();
                log::debug!(
                    "picture #{id}: encoding began on its own thread, {} ms after it was handed over",
                    started.duration_since(handed).as_millis()
                );
                // A panic is an answer too, so that the slot is never left waiting for one.
                let encoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    encode::encode(&image, protocol, (w, h), id)
                }))
                .unwrap_or(Err(encode::EncodeError::Panicked));
                let result = encoded
                    .map(|e| {
                        log::debug!(
                            "{} picture encoded: {}x{} px, {} bytes, {} ms",
                            protocol.label(),
                            e.size.0,
                            e.size.1,
                            e.stream.len(),
                            started.elapsed().as_millis()
                        );
                        Arc::new(e)
                    })
                    .map_err(|e| e.to_string());
                Ev::Encoded { key, result }
            });
        }
    }

    pub(super) fn on_encoded(&mut self, key: EncodeKey, result: Result<Arc<Encoded>, String>) {
        self.gfx.running = self.gfx.running.saturating_sub(1);
        // The picture may have been replaced or evicted while it was being encoded.
        let Some(slot) = self.gfx.cache.get_mut(&key) else {
            return;
        };
        if !matches!(slot, Slot::Running) {
            return;
        }
        let done = result.as_ref().ok().map(|e| e.id);
        *slot = match result {
            Ok(image) => Slot::Ready {
                image,
                used: self.gfx.frame,
            },
            Err(why) => {
                log::warn!("a picture was not encoded for {}: {why}", key.1.label());
                Slot::Failed
            }
        };
        if let Some(id) = done {
            self.gfx.mark(id, "encoding result received by the app");
        }
    }

    /// Pictures whose way to the screen is being timed for the log (for tests).
    #[cfg(test)]
    pub(super) fn timelines(&self) -> usize {
        self.gfx.marks.len()
    }

    /// Encodings asked for and not started (for tests).
    #[cfg(test)]
    pub(super) fn queued_encodings(&self) -> usize {
        self.gfx
            .cache
            .values()
            .filter(|s| matches!(s, Slot::Queued { .. }))
            .count()
    }

    /// A picture was decoded and stored: where its timeline starts.
    pub(super) fn note_decoded(&mut self, url: &str) {
        if self.gfx.decoded.len() >= MAX_MARKS {
            self.gfx.decoded.clear();
        }
        self.gfx.decoded.insert(url.to_string(), Instant::now());
    }

    /// How long the loop may wait for a key before it draws again: short while an encoding is under way or about to
    /// start, so that its answer is on the screen within a few milliseconds of arriving.
    pub fn poll_wait(&self) -> Duration {
        let busy = self.gfx.running > 0
            || self
                .gfx
                .cache
                .values()
                .any(|s| matches!(s, Slot::Queued { .. }));
        if busy { BUSY_POLL } else { IDLE_POLL }
    }

    /// A picture was stored again under this URL: what was made from the old one is no use.
    pub(super) fn forget_encodings(&mut self, url: &str) {
        self.gfx.cache.retain(|(u, ..), _| u != url);
    }

    /// `:images <choice>`.
    pub(super) fn images_command(&mut self, arg: &str) {
        match ImageChoice::parse(arg) {
            Some(choice) => {
                self.set_image_choice(choice);
                let text = self.images_label();
                self.set_status(format!("images: {text}"), false);
            }
            None => self.set_status(
                "usage: :images auto|sixel|kitty|iterm|octant|sextant|blocks|ascii|off",
                true,
            ),
        }
    }

    /// The `I` key: the next way of drawing pictures. Graphics come after "off" when the terminal has them.
    pub(super) fn cycle_images(&mut self) {
        let can_use_graphics =
            gfx::resolve(ImageChoice::Auto, self.gfx.detected, self.gfx.cell).is_some();
        if matches!(self.gfx.choice, ImageChoice::Art(ImgMode::Off)) && can_use_graphics {
            self.set_image_choice(ImageChoice::Auto);
        } else if self.graphics().is_some() {
            // Out of graphics, into the finest art.
            self.set_image_choice(ImageChoice::Art(ImgMode::Octant));
        } else {
            let next = self.img_mode.next();
            self.set_image_choice(ImageChoice::Art(next));
        }
        let text = self.images_label();
        self.set_status(format!("images: {text}"), false);
    }

    /// How pictures are drawn, in words: what the status line says after a change.
    pub fn images_label(&self) -> String {
        let blocks = self.img_mode.label();
        match (self.gfx.choice, self.graphics()) {
            (ImageChoice::Art(m), _) => m.label().to_string(),
            (ImageChoice::Auto, Some(g)) => format!("auto ({})", g.protocol.label()),
            (ImageChoice::Auto, None) if self.gfx.detected.is_some() => format!(
                "auto: the terminal's cell size is not known (set cell_pixels), so {blocks}"
            ),
            (ImageChoice::Auto, None) => format!("auto: no graphics in this terminal, so {blocks}"),
            (ImageChoice::Protocol(p), Some(_)) => p.label().to_string(),
            (ImageChoice::Protocol(p), None) => format!(
                "{}: the terminal's cell size is not known (set cell_pixels), so {blocks}",
                p.label()
            ),
        }
    }
}
