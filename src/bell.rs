//! When to ring the terminal's bell: a new message from someone else arrived. The terminal decides what a bell is
//! (a sound, a flashing tab or taskbar entry, a mark on the window), which is how it gets attention when the window is
//! behind others. Pure, so it is tested without a terminal or a clock.
//!
//! What counts as new is a message newer than anything seen so far in that chat (a watermark per chat), so the same
//! message seen twice, once by the open chat's poll and again by the chat list, rings once. What was there when the
//! program started, or when a chat was first opened, only sets the watermark.

use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::time::{Duration as Wait, Instant};

/// When the bell rings (the `bell` setting).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BellMode {
    /// Never.
    #[default]
    Off,
    /// For a new message while the terminal is behind other windows. Needs a terminal that reports focus; one that
    /// does not counts as always in front, and so never rings.
    Background,
    /// For every new message.
    Always,
}

impl BellMode {
    /// The mode a setting's value names.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "no" => Some(Self::Off),
            "background" | "unfocused" => Some(Self::Background),
            "always" | "on" | "yes" => Some(Self::Always),
            _ => None,
        }
    }

    /// The word for it in the settings.
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Background => "background",
            Self::Always => "always",
        }
    }
}

/// A chat the program had not seen before (someone started a conversation) rings only for a message this fresh: a chat
/// that merely entered the list because another dropped out of it has an old one.
const FRESH: Duration = Duration::minutes(2);
/// A burst of messages rings once.
const GAP: Wait = Wait::from_secs(3);

#[derive(Debug, Default)]
pub struct Bell {
    mode: BellMode,
    /// The newest message seen in each chat, whoever sent it.
    marks: HashMap<String, DateTime<Utc>>,
    /// The first look at the chat list has been taken (`baseline` is for that look, `observe` after it).
    started: bool,
    last_rung: Option<Instant>,
    pending: bool,
}

impl Bell {
    pub fn set_mode(&mut self, mode: BellMode) {
        self.mode = mode;
    }

    /// Whether the first look at the chat list has been taken.
    pub fn started(&self) -> bool {
        self.started
    }

    /// The first look at the chat list is over: what comes after it is news.
    pub fn start(&mut self) {
        self.started = true;
    }

    /// `created` is the newest message of `chat` as things stand: remembered, not rung for.
    pub fn baseline(&mut self, chat: &str, created: DateTime<Utc>) {
        let mark = self.marks.entry(chat.to_string()).or_insert(created);
        *mark = (*mark).max(created);
    }

    /// The newest message of `chat` was written at `created`; `foreign` says it is somebody else's (and a real
    /// message), `in_front` that the terminal is the window in front. `wall` is the time now.
    pub fn observe(
        &mut self,
        chat: &str,
        created: DateTime<Utc>,
        foreign: bool,
        in_front: bool,
        now: Instant,
        wall: DateTime<Utc>,
    ) {
        let news = match self.marks.get(chat) {
            Some(&seen) => created > seen,
            // A chat nobody had seen: news only if it is a new conversation, not an old one that has just come into
            // the list.
            None => self.started && wall - created < FRESH,
        };
        let mark = self.marks.entry(chat.to_string()).or_insert(created);
        *mark = (*mark).max(created);
        if !news || !foreign {
            return;
        }
        let wanted = match self.mode {
            BellMode::Off => false,
            BellMode::Background => !in_front,
            BellMode::Always => true,
        };
        if wanted
            && self
                .last_rung
                .is_none_or(|at| now.duration_since(at) >= GAP)
        {
            self.pending = true;
            self.last_rung = Some(now);
        }
    }

    /// Whether to ring now (once for each time it was due).
    pub fn take(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap() + Duration::minutes(minutes)
    }

    /// A bell in this mode that has taken its first look at one chat, whose newest message is from minute 0.
    fn started(mode: BellMode) -> Bell {
        let mut b = Bell::default();
        b.set_mode(mode);
        b.baseline("chat", at(0));
        b.start();
        b
    }

    fn observe(
        b: &mut Bell,
        minute: i64,
        foreign: bool,
        in_front: bool,
        secs_later: u64,
        base: Instant,
    ) {
        b.observe(
            "chat",
            at(minute),
            foreign,
            in_front,
            base + Wait::from_secs(secs_later),
            at(minute),
        );
    }

    #[test]
    fn the_messages_there_already_are_not_news_and_a_new_one_from_someone_else_is() {
        let base = Instant::now();
        let mut b = started(BellMode::Always);
        observe(&mut b, 0, true, true, 0, base);
        assert!(!b.take(), "the newest message was there at the start");
        observe(&mut b, 5, true, true, 10, base);
        assert!(b.take());
        assert!(!b.take(), "once for each");
        // The same message seen again (the list after the poll) is not news again.
        observe(&mut b, 5, true, true, 20, base);
        assert!(!b.take());
    }

    #[test]
    fn your_own_messages_and_events_move_the_mark_but_do_not_ring() {
        let base = Instant::now();
        let mut b = started(BellMode::Always);
        observe(&mut b, 5, false, true, 0, base);
        assert!(!b.take());
        // Someone's message from before yours is not news either: it is behind the mark.
        observe(&mut b, 4, true, true, 10, base);
        assert!(!b.take());
        observe(&mut b, 6, true, true, 20, base);
        assert!(b.take());
    }

    #[test]
    fn the_modes() {
        let base = Instant::now();
        for (mode, in_front, rings) in [
            (BellMode::Off, false, false),
            (BellMode::Background, false, true),
            (BellMode::Background, true, false),
            (BellMode::Always, true, true),
            (BellMode::Always, false, true),
        ] {
            let mut b = started(mode);
            observe(&mut b, 5, true, in_front, 0, base);
            assert_eq!(b.take(), rings, "{mode:?}, in front: {in_front}");
        }
    }

    #[test]
    fn a_burst_rings_once_and_later_messages_ring_again() {
        let base = Instant::now();
        let mut b = started(BellMode::Always);
        observe(&mut b, 1, true, true, 0, base);
        observe(&mut b, 2, true, true, 1, base);
        observe(&mut b, 3, true, true, 2, base);
        assert!(b.take());
        assert!(!b.take());
        observe(&mut b, 4, true, true, 10, base);
        assert!(b.take(), "a message well after the burst");
    }

    #[test]
    fn a_chat_not_seen_before_rings_only_for_a_fresh_message_and_only_once_started() {
        let base = Instant::now();
        let mut b = Bell::default();
        b.set_mode(BellMode::Always);
        // Before the first look at the list is over, everything is the starting point.
        b.observe("early", at(0), true, true, base, at(0));
        assert!(!b.take());
        b.start();
        // A conversation someone has just started.
        b.observe(
            "new",
            at(10),
            true,
            true,
            base + Wait::from_secs(10),
            at(10) + Duration::seconds(30),
        );
        assert!(b.take());
        // An old chat that has only now come into the list.
        b.observe(
            "old",
            at(10),
            true,
            true,
            base + Wait::from_secs(20),
            at(10) + Duration::hours(3),
        );
        assert!(!b.take());
        assert!(!b.take());
    }

    #[test]
    fn modes_and_settings() {
        for (text, mode) in [
            ("off", BellMode::Off),
            ("Background", BellMode::Background),
            (" always ", BellMode::Always),
            ("on", BellMode::Always),
        ] {
            assert_eq!(BellMode::parse(text), Some(mode), "{text}");
        }
        assert_eq!(BellMode::parse("loud"), None);
        assert_eq!(BellMode::default(), BellMode::Off);
        assert_eq!(BellMode::Background.label(), "background");
    }
}
