//! Local unread overrides and service read-marker deduplication and retry hold-off.

use super::throttle_hold;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use teams_client::{Chat, Error, Preview};

#[derive(Default)]
pub(super) struct ReadState {
    local: HashMap<String, DateTime<Utc>>,
    /// The newest mark dispatched for each chat, including a request still in flight.
    marked: HashMap<String, DateTime<Utc>>,
    paused_until: Option<Instant>,
}

impl ReadState {
    pub(super) fn is_unread(&self, chat: &Chat, me: &str) -> bool {
        if !chat.is_unread(me) {
            return false;
        }
        match (self.local.get(&chat.id), chat.preview.as_ref()) {
            (Some(read), Some(p)) => p.created > *read,
            _ => true,
        }
    }

    /// Whether a channel is unread: the service's own word (`unread`, from the teams list or the conversation list)
    /// unless it has since been read here, past its newest message. A channel has no read marker of its own to compare
    /// with, so the local mark alone decides, and only a message newer than it makes the channel unread again.
    pub(super) fn channel_unread(&self, id: &str, unread: bool, last: Option<&Preview>) -> bool {
        if !unread {
            return false;
        }
        match (self.local.get(id), last) {
            (Some(read), Some(p)) => p.created > *read,
            (Some(_), None) => false,
            (None, _) => true,
        }
    }

    /// The most recent "read up to" marker for `chat`: the service's own, or a local override if it is later.
    /// `None` means the chat has never been read, so everything in it is new.
    pub(super) fn read_marker(&self, chat: &Chat) -> Option<DateTime<Utc>> {
        match (self.local.get(&chat.id).copied(), chat.last_read) {
            (Some(local), Some(server)) => Some(local.max(server)),
            (Some(local), None) => Some(local),
            (None, server) => server,
        }
    }

    pub(super) fn mark_local(&mut self, chat: String, arrived: DateTime<Utc>) {
        let read = self.local.entry(chat).or_insert(arrived);
        *read = (*read).max(arrived);
    }

    pub(super) fn begin_service_mark(
        &mut self,
        chat: &str,
        arrived: DateTime<Utc>,
        server_read: Option<DateTime<Utc>>,
        now: Instant,
    ) -> bool {
        if self.paused_until.is_some_and(|until| now < until)
            || self
                .marked
                .get(chat)
                .copied()
                .or(server_read)
                .is_some_and(|read| read >= arrived)
        {
            return false;
        }
        self.marked.insert(chat.to_string(), arrived);
        true
    }

    pub(super) fn fail_service_mark(&mut self, chat: &str, error: &Error, now: Instant) {
        self.marked.remove(chat);
        let wait = throttle_hold(error)
            .unwrap_or(Duration::from_secs(30))
            .min(crate::cadence::MAX_HOLD);
        self.paused_until = Some(now + wait);
    }

    #[cfg(test)]
    pub(super) fn set_paused_until(&mut self, until: Instant) {
        self.paused_until = Some(until);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(created: DateTime<Utc>) -> Preview {
        Preview {
            created,
            from_id: None,
            from_name: None,
            text: String::new(),
            automated: false,
        }
    }

    #[test]
    fn a_channel_is_unread_when_the_service_says_so_until_it_is_read_here_past_its_newest_message()
    {
        let mut read = ReadState::default();
        let at = Utc::now();
        let last = preview(at);
        assert!(read.channel_unread("c", true, Some(&last)));
        assert!(
            !read.channel_unread("c", false, Some(&last)),
            "the service's word"
        );
        read.mark_local("c".into(), at);
        assert!(!read.channel_unread("c", true, Some(&last)), "read here");
        assert!(
            !read.channel_unread("c", true, None),
            "read here, with nothing to date it by"
        );
        let newer = preview(at + chrono::Duration::seconds(1));
        assert!(
            read.channel_unread("c", true, Some(&newer)),
            "a newer message is unread again"
        );
        assert!(
            read.channel_unread("other", true, Some(&last)),
            "one channel's mark is its own"
        );
    }

    #[test]
    fn a_server_marker_or_a_dispatched_mark_prevents_duplicate_requests() {
        let mut read = ReadState::default();
        let now = Instant::now();
        let arrived = Utc::now();
        assert!(!read.begin_service_mark("chat", arrived, Some(arrived), now));
        assert!(read.begin_service_mark("chat", arrived, None, now));
        assert!(!read.begin_service_mark("chat", arrived, None, now));
        assert!(read.begin_service_mark("chat", arrived + chrono::Duration::seconds(1), None, now));
    }

    #[test]
    fn a_failed_mark_holds_all_chats_until_the_retry_interval_expires() {
        for (error, wait) in [
            (
                Error::Unexpected {
                    operation: "mark read",
                    detail: "failed",
                },
                30,
            ),
            (
                Error::Throttled {
                    operation: "mark read",
                    retry_after: None,
                },
                30,
            ),
            (
                Error::Throttled {
                    operation: "mark read",
                    retry_after: Some(Duration::from_secs(90)),
                },
                90,
            ),
        ] {
            let mut read = ReadState::default();
            let now = Instant::now();
            let arrived = Utc::now();
            assert!(read.begin_service_mark("chat", arrived, None, now));
            read.fail_service_mark("chat", &error, now);
            assert!(!read.begin_service_mark(
                "chat",
                arrived,
                None,
                now + Duration::from_secs(wait - 1)
            ));
            assert!(!read.begin_service_mark(
                "another",
                arrived,
                None,
                now + Duration::from_secs(wait - 1)
            ));
            assert!(read.begin_service_mark(
                "chat",
                arrived,
                None,
                now + Duration::from_secs(wait)
            ));
        }
    }

    #[test]
    fn unbounded_throttle_delay_does_not_panic() {
        let mut read = ReadState::default();
        let now = Instant::now();
        let arrived = Utc::now();
        read.fail_service_mark(
            "c",
            &Error::Throttled {
                operation: "mark read",
                retry_after: Some(Duration::from_secs(u64::MAX)),
            },
            now,
        );
        assert!(!read.begin_service_mark("c", arrived, None, now + Duration::from_secs(60)));
    }
}
