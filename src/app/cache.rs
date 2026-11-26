//! The start-up cache in the app: put the last session's lists on screen before the service has answered, and keep the
//! copy for the next start up to date (see `crate::cache` for what is kept and how it is sealed).
//!
//! What this does and does not do:
//! - The cached lists are shown **as cached** (`ChatList::is_cached`, `Teams::is_cached`: the header says so) until the
//!   first real answer replaces them; that answer replaces them whole, so a chat or team that is no longer there goes
//!   like any other that drops out of a list.
//! - Nothing is sent to the service because of it. The chat list is read at start-up as it always was, and the teams
//!   list is read the first time the tab is shown.
//! - It has no side effects of a real list. The bell sees only what the service said (`bell_for_list` runs for a real
//!   answer), so a cached unread chat never rings; nothing is selected from it, so a chat is not opened (and its
//!   messages not fetched) on the strength of a list that may be a week old; no read marker is sent.
//! - Writing is off the UI thread, at most once a minute, atomically, and a failure is a log line.

use super::{App, Ev};
use crate::cache::{Cache, Restored, Snapshot};
use chrono::Utc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The least time between two writes of the cache.
pub(super) const WRITE_GAP: Duration = Duration::from_secs(60);

impl App {
    /// Keep the lists in `cache`, and show what it holds from the last session, if that is usable. A cache that cannot
    /// be used is a line in the log and the normal first load; it is never an error.
    pub fn use_cache(&mut self, cache: Arc<Cache>) {
        let read = cache.read(Utc::now());
        self.cache = Some(cache);
        if let Ok(restored) = read {
            self.apply_cache(restored);
        }
    }

    /// Show a snapshot's lists, unless the service has already answered for them.
    fn apply_cache(&mut self, restored: Restored) {
        let Restored {
            account,
            chats,
            chats_at,
            teams,
            teams_at,
        } = restored;
        if let Some(at) = chats_at {
            self.chats.receive_cached(chats, at);
        }
        // A backend with no teams has nothing to show them for, whatever an old file says.
        if let Some(at) = teams_at.filter(|_| self.backend.can_list_teams()) {
            self.teams.receive_cached(teams, at);
        }
        // The file is sealed to this account, so whose it is can be said before `me` has come back: the person's own
        // name is left out of a chat's title from the first frame.
        self.account_hint = Some(account);
    }

    /// The lists were read from the service: the cache is to be brought up to date when it next may be.
    pub(super) fn mark_cache_dirty(&mut self) {
        if self.cache.is_some() {
            self.cache_dirty = true;
        }
    }

    /// Write the cache if the lists have changed since it was last written and at least [`WRITE_GAP`] has passed (called
    /// from the tick). Only what has been read from the service is written, with the time it was read, and a part that
    /// is still the cached one is carried forward with its own time, so it does not look younger than it is.
    pub(super) fn save_cache_if_due(&mut self) {
        if !self.cache_dirty {
            return;
        }
        let Some(cache) = self.cache.clone() else {
            return;
        };
        if self.read_only {
            // A diagnostic frame leaves nothing behind.
            self.cache_dirty = false;
            return;
        }
        if self
            .cache_last
            .is_some_and(|at| at.elapsed() < self.cache_gap)
        {
            return;
        }
        let chats = self
            .chats
            .read_at()
            .filter(|_| self.chats.loaded())
            .map(|at| (self.chats.items(), at));
        let teams = self
            .teams
            .read_at()
            .filter(|_| self.teams.loaded())
            .map(|at| (self.teams.items(), at));
        let snapshot = Snapshot::capture(cache.account(), Utc::now(), chats, teams);
        self.cache_dirty = false;
        self.cache_last = Some(Instant::now());
        if snapshot.is_empty() {
            return;
        }
        self.spawn_task(move || Ev::CacheWritten(cache.write(&snapshot)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Focus, Overlay};
    use crate::bell::BellMode;
    use crate::demo::DemoBackend;
    use crate::editor::Keymap;
    use crate::vault::MemoryKeys;
    use crate::view::Tab;
    use std::path::Path;
    use teams_client::{Backend, Chat, ChatKind, Error, Member, Preview, Team};

    /// A scratch directory that is removed when the test ends, even if it fails.
    fn temp(name: &str) -> crate::testdir::TestDir {
        crate::testdir::TestDir::new(&format!("appcache-{name}"))
    }

    /// The demo's own account, which the demo backend answers `me` with.
    const ME: &str = crate::demo::ME_ID;

    fn cache_in(dir: &Path, keys: &Arc<MemoryKeys>) -> Arc<Cache> {
        Arc::new(Cache::new(keys.clone(), dir, "teams", ME))
    }

    fn pump(app: &mut App, until: impl Fn(&App) -> bool) {
        for _ in 0..50 {
            app.on_tick();
            if until(app) {
                return;
            }
        }
        panic!("condition not reached");
    }

    fn cached_chat(id: &str, title: &str, unread: bool) -> Chat {
        cached_chat_sent(id, title, unread, chrono::Duration::minutes(3))
    }

    /// A chat whose newest message, from someone else, was sent `ago` before now.
    fn cached_chat_sent(id: &str, title: &str, unread: bool, ago: chrono::Duration) -> Chat {
        let sent = Utc::now() - ago;
        Chat {
            id: id.into(),
            kind: ChatKind::Group,
            topic: Some(title.into()),
            members: vec![Member {
                not_in_directory: false,
                id: "u-ada".into(),
                name: "Ada Lovelace".into(),
            }],
            updated: sent,
            preview: Some(Preview {
                created: sent,
                from_id: Some("u-ada".into()),
                from_name: Some("Ada Lovelace".into()),
                text: "the words of a message".into(),
                automated: false,
            }),
            last_read: Some(if unread {
                sent - chrono::Duration::hours(2)
            } else {
                sent
            }),
            web_url: None,
            favorite: None,
        }
    }

    /// A cache on disk holding `chats` and the demo's own teams, written by the code that writes it for real.
    fn written(dir: &Path, keys: &Arc<MemoryKeys>, chats: &[Chat], teams: &[Team]) -> Arc<Cache> {
        let cache = cache_in(dir, keys);
        let now = Utc::now();
        cache
            .write(&Snapshot::capture(
                ME,
                now,
                Some((chats, now)),
                (!teams.is_empty()).then_some((teams, now)),
            ))
            .unwrap();
        cache
    }

    fn demo_teams() -> Vec<Team> {
        DemoBackend::instant().teams().unwrap()
    }

    fn app_for(backend: Arc<dyn Backend>) -> App {
        let (mut app, _) = App::for_test(backend);
        app.set_keymap(Keymap::Vim);
        app
    }

    #[test]
    fn a_valid_cache_puts_the_chats_and_the_teams_on_screen_before_anything_is_asked() {
        let dir = temp("shown");
        let keys = MemoryKeys::new();
        let cache = written(
            &dir,
            &keys,
            &[cached_chat("cached-1", "Planning", false)],
            &demo_teams(),
        );
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(cache);
        assert!(app.chats.loaded() && app.chats.is_cached());
        assert_eq!(app.chats.items().len(), 1);
        assert_eq!(app.chats.items()[0].title(app.me_id()), "Planning");
        assert!(app.teams.loaded() && app.teams.is_cached());
        assert!(!app.teams.items().is_empty());
        assert!(
            app.me.is_none(),
            "nothing has been asked of the service yet"
        );
        assert_eq!(
            app.me_id(),
            ME,
            "but whose lists they are is known, from the sealed file"
        );
        assert_eq!(
            app.tab_labels(40).len(),
            2,
            "the Teams tab is there from the first frame"
        );
        assert!(app.current.is_none(), "no chat is opened from a cache");
        assert!(
            app.states.is_empty(),
            "so no conversation is fetched on its say-so"
        );
        assert!(
            !app.teams_pending(),
            "the teams list has not been asked for"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_real_answer_replaces_the_cached_list_and_a_chat_that_is_gone_goes_with_it() {
        let dir = temp("replace");
        let keys = MemoryKeys::new();
        let chats = [
            cached_chat("gone-1", "A chat I left", false),
            cached_chat("demo-platform", "Cached name", false),
        ];
        let cache = written(&dir, &keys, &chats, &[]);
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(cache);
        assert_eq!(app.chats.items().len(), 2);
        app.start();
        pump(&mut app, |a| !a.chats.is_cached());
        assert!(app.chats.loaded());
        assert!(
            app.chats.items().iter().all(|c| c.id != "gone-1"),
            "a cached chat the real list lacks is gone"
        );
        let real = app
            .chats
            .items()
            .iter()
            .find(|c| c.id == "demo-platform")
            .unwrap();
        assert_ne!(
            real.title(app.me_id()),
            "Cached name",
            "the service's own data replaced it"
        );
        assert!(
            app.current.is_some(),
            "and the first real list selects a chat as it always did"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cached_chat_that_is_open_and_missing_from_the_real_list_stays_while_a_draft_is_being_typed()
     {
        let dir = temp("draft");
        let keys = MemoryKeys::new();
        let cache = written(
            &dir,
            &keys,
            &[cached_chat("gone-1", "A chat I left", false)],
            &[],
        );
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(cache);
        app.select_chat("gone-1");
        app.focus = Focus::Editor;
        app.editor.set_text("half a message to someone");
        app.start();
        pump(&mut app, |a| !a.chats.is_cached());
        assert_eq!(
            app.current.as_deref(),
            Some("gone-1"),
            "a silent refresh does not retarget a send someone is typing"
        );
        assert_eq!(app.editor.text(), "half a message to someone");
        assert!(app.chats.items().iter().all(|c| c.id != "gone-1"));

        // With nothing typed and the box not in use, the same situation moves on.
        let (mut app2, _) = App::for_test(Arc::new(DemoBackend::instant()));
        app2.use_cache(written(
            &dir,
            &keys,
            &[cached_chat("gone-1", "A chat I left", false)],
            &[],
        ));
        app2.select_chat("gone-1");
        app2.start();
        pump(&mut app2, |a| !a.chats.is_cached());
        assert_ne!(app2.current.as_deref(), Some("gone-1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cached_unread_chat_does_not_ring_the_bell_and_neither_does_the_first_real_list() {
        let dir = temp("bell");
        let keys = MemoryKeys::new();
        // A message from someone else, unread, from a few minutes ago.
        let cache = written(
            &dir,
            &keys,
            &[cached_chat_sent(
                "demo-platform",
                "Platform",
                true,
                chrono::Duration::days(3),
            )],
            &[],
        );
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.set_bell_mode(BellMode::Always);
        app.use_cache(cache);
        assert!(
            app.is_unread(&app.chats.items()[0].clone()),
            "it is shown as unread"
        );
        assert!(!app.take_bell(), "showing a cached list rings nothing");
        app.start();
        pump(&mut app, |a| !a.chats.is_cached());
        assert!(
            !app.take_bell(),
            "the first list the service gives is the starting point, as it is without a cache"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_teams_tab_shows_the_cached_tree_at_once_and_asks_the_service_when_first_shown() {
        let dir = temp("teams");
        let keys = MemoryKeys::new();
        let cache = written(
            &dir,
            &keys,
            &[cached_chat("c1", "Chat", false)],
            &demo_teams(),
        );
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(cache);
        app.start();
        pump(&mut app, |a| !a.chats.is_cached());
        for _ in 0..5 {
            app.on_tick();
        }
        assert!(app.teams.is_cached(), "nothing has asked for the teams yet");
        assert!(!app.teams_pending());
        app.show_tab(Tab::Teams);
        assert!(app.on_teams_tab());
        assert!(
            !app.team_rows().is_empty(),
            "the cached tree is there while it is being read"
        );
        assert!(
            app.teams_pending(),
            "the first request happens on the first showing"
        );
        assert!(
            !app.is_busy() || !app.teams.shows_loading(),
            "quietly: the list is already showing"
        );
        pump(&mut app, |a| !a.teams.is_cached());
        assert!(!app.teams_pending());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_header_says_cached_until_the_real_list_arrives_and_no_text_is_made_up_for_a_preview() {
        let dir = temp("drawn");
        let keys = MemoryKeys::new();
        let cache = written(
            &dir,
            &keys,
            &[cached_chat("cached-1", "Planning", false)],
            &demo_teams(),
        );
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(cache);
        let shown = crate::ui::render_to_string(&mut app, 110, 30);
        assert!(shown.contains("cached"), "{shown}");
        assert!(shown.contains("Planning"), "{shown}");
        assert!(
            !shown.contains("the words of a message") && !shown.contains("Ada:"),
            "no preview text, and no 'Name: ' in front of nothing\n{shown}"
        );
        // The teams tab says it too.
        app.show_tab(Tab::Teams);
        let shown = crate::ui::render_to_string(&mut app, 110, 30);
        assert!(shown.contains("cached"), "{shown}");
        pump(&mut app, |a| !a.teams.is_cached());
        app.start();
        pump(&mut app, |a| !a.chats.is_cached());
        let shown = crate::ui::render_to_string(&mut app, 110, 30);
        assert!(!shown.contains("cached"), "{shown}");
        app.show_tab(Tab::Chats);
        let shown = crate::ui::render_to_string(&mut app, 110, 30);
        assert!(!shown.contains("cached"), "{shown}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cache_that_cannot_be_used_never_blocks_start_up() {
        let dir = temp("corrupt");
        let keys = MemoryKeys::new();
        let cache = cache_in(&dir, &keys);
        // Garbage, a file sealed with another key, and a key store that does not answer.
        std::fs::write(
            cache.path(),
            b"VTSE this is not a sealed file at all, just text that looks like one",
        )
        .unwrap();
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(cache.clone());
        assert!(!app.chats.loaded() && !app.chats.is_cached());
        assert!(
            !cache.path().exists(),
            "a file that can never open is removed"
        );
        app.start();
        pump(&mut app, |a| a.chats.loaded());
        assert!(!app.chats.is_cached());

        let broken = Arc::new(Cache::new(
            MemoryKeys::broken(crate::vault::KeyError::Failed("locked".into())),
            &dir,
            "teams",
            ME,
        ));
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(broken);
        app.start();
        pump(&mut app, |a| a.chats.loaded());
        assert!(app.chats.loaded() && !app.chats.is_cached());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cache_for_another_account_or_older_than_a_week_shows_nothing() {
        let dir = temp("other");
        let keys = MemoryKeys::new();
        let chats = [cached_chat("c1", "Chat", false)];
        written(&dir, &keys, &chats, &[]);
        let other = Arc::new(Cache::new(keys.clone(), &dir, "teams", "someone-else"));
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(other);
        assert!(!app.chats.loaded());

        let old = Utc::now() - chrono::Duration::days(8);
        let cache = cache_in(&dir, &keys);
        cache
            .write(&Snapshot::capture(ME, old, Some((&chats, old)), None))
            .unwrap();
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(cache);
        assert!(!app.chats.loaded(), "eight days is too old");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_service_without_teams_shows_no_cached_tree() {
        let dir = temp("noteams");
        let keys = MemoryKeys::new();
        let cache = written(
            &dir,
            &keys,
            &[cached_chat("c1", "Chat", false)],
            &demo_teams(),
        );
        let mut app = app_for(Arc::new(DemoBackend::without_teams()));
        app.use_cache(cache);
        assert!(app.chats.loaded());
        assert!(!app.teams.loaded());
        assert!(!app.teams_available());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn first_real_refresh(app: &mut App) {
        app.start();
        pump(app, |a| a.chats.loaded() && !a.chats.is_cached());
    }

    #[test]
    fn a_successful_refresh_writes_the_cache_once_a_minute_at_most() {
        let dir = temp("write");
        let keys = MemoryKeys::new();
        let cache = cache_in(&dir, &keys);
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(cache.clone());
        assert_eq!(
            cache.writes(),
            0,
            "nothing to write before anything was read"
        );
        first_real_refresh(&mut app);
        app.on_tick();
        assert_eq!(cache.writes(), 1, "the first answer is written at once");

        // More lists come in within the minute: remembered, not written yet.
        app.chats.sched.refresh_soon();
        pump(&mut app, |a| {
            !a.chats.is_fetching() && !a.chats.sched.due(Duration::from_secs(3600))
        });
        for _ in 0..5 {
            app.on_tick();
        }
        assert_eq!(
            cache.writes(),
            1,
            "a second write inside the minute is held back"
        );

        // Once the minute has passed the held-back change is written, without another refresh.
        app.cache_gap = Duration::ZERO;
        app.on_tick();
        assert_eq!(cache.writes(), 2);
        app.on_tick();
        assert_eq!(
            cache.writes(),
            2,
            "and only once: nothing has changed since"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn what_is_written_holds_the_lists_and_none_of_what_was_said() {
        let dir = temp("content");
        let keys = MemoryKeys::new();
        let cache = cache_in(&dir, &keys);
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(cache.clone());
        first_real_refresh(&mut app);
        app.show_tab(Tab::Teams);
        pump(&mut app, |a| a.teams.loaded() && !a.teams.is_cached());
        app.cache_gap = Duration::ZERO;
        app.on_tick();
        let read = cache.read(Utc::now()).unwrap();
        assert!(!read.chats.is_empty() && !read.teams.is_empty());
        assert!(
            read.chats.iter().all(|c| c
                .preview
                .as_ref()
                .is_none_or(|p| p.text.is_empty() && p.from_name.is_none())),
            "no preview text and no sender name"
        );
        assert!(read.chats.iter().all(|c| c.web_url.is_none()));
        assert!(
            read.teams
                .iter()
                .flat_map(|t| &t.channels)
                .all(|c| c.last.as_ref().is_none_or(|p| p.text.is_empty()))
        );
        // On disk, nothing of the demo's names is in the clear.
        let raw = std::fs::read(cache.path()).unwrap();
        let name = app.chats.items()[0].title(app.me_id());
        assert!(!raw.windows(name.len()).any(|w| w == name.as_bytes()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_read_only_app_writes_no_cache_and_one_with_none_writes_nothing() {
        let dir = temp("readonly");
        let keys = MemoryKeys::new();
        let cache = cache_in(&dir, &keys);
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.read_only = true;
        app.use_cache(cache.clone());
        first_real_refresh(&mut app);
        for _ in 0..5 {
            app.on_tick();
        }
        assert_eq!(cache.writes(), 0);
        assert!(!cache.path().exists());

        let mut plain = app_for(Arc::new(DemoBackend::instant()));
        first_real_refresh(&mut plain);
        plain.on_tick();
        assert!(
            !cache.path().exists(),
            "an app given no cache has nowhere to write"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_write_is_logged_and_never_shown() {
        let dir = temp("failwrite");
        let broken = Arc::new(Cache::new(
            MemoryKeys::broken(crate::vault::KeyError::Failed("locked".into())),
            &dir,
            "teams",
            ME,
        ));
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(broken.clone());
        first_real_refresh(&mut app);
        for _ in 0..5 {
            app.on_tick();
        }
        assert_eq!(broken.writes(), 0);
        assert!(
            app.status
                .as_ref()
                .is_none_or(|(text, _, _)| !text.contains("cache")),
            "{:?}",
            app.status
        );
        assert!(matches!(app.overlay, Overlay::None));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cached_part_that_is_never_refreshed_keeps_its_own_age() {
        let dir = temp("age");
        let keys = MemoryKeys::new();
        let old = Utc::now() - chrono::Duration::days(5);
        let cache = cache_in(&dir, &keys);
        cache
            .write(&Snapshot::capture(
                ME,
                old,
                Some((&[cached_chat("c1", "Chat", false)], old)),
                Some((&demo_teams(), old)),
            ))
            .unwrap();
        let mut app = app_for(Arc::new(DemoBackend::instant()));
        app.use_cache(cache.clone());
        first_real_refresh(&mut app);
        app.cache_gap = Duration::ZERO;
        app.on_tick();
        let back = cache.read(Utc::now()).unwrap();
        let teams_at = back.teams_at.unwrap();
        assert!(
            (Utc::now() - teams_at) > chrono::Duration::days(4),
            "the teams were not read this session, so they are still five days old"
        );
        assert!((Utc::now() - back.chats_at.unwrap()) < chrono::Duration::minutes(5));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_backend_error_while_the_cached_list_is_showing_keeps_it_and_says_so() {
        struct Failing;
        impl Backend for Failing {
            fn label(&self) -> String {
                "failing".into()
            }
            fn me(&self) -> Result<teams_client::Me, Error> {
                Err(Error::Unexpected {
                    operation: "me",
                    detail: "down",
                })
            }
            fn chats(&self) -> Result<Vec<Chat>, Error> {
                Err(Error::Unexpected {
                    operation: "list chats",
                    detail: "down",
                })
            }
            fn find_people(&self, _: &str) -> Result<Vec<teams_client::Contact>, Error> {
                Ok(vec![])
            }
            fn open_chat(&self, _: &teams_client::Contact) -> Result<Chat, Error> {
                Err(Error::Unexpected {
                    operation: "open",
                    detail: "down",
                })
            }
            fn messages(&self, _: &str, _: Option<&str>) -> Result<teams_client::Page, Error> {
                Err(Error::Unexpected {
                    operation: "messages",
                    detail: "down",
                })
            }
            fn send(
                &self,
                _: &str,
                _: &str,
                _: &[teams_client::OutMention],
                _: Option<&teams_client::ReplyTarget>,
            ) -> Result<teams_client::Msg, Error> {
                Err(Error::Unexpected {
                    operation: "send",
                    detail: "down",
                })
            }
            fn serves_image(&self, _: &str) -> bool {
                false
            }
            fn image(&self, _: &str) -> Result<image::RgbImage, Error> {
                Err(Error::Unexpected {
                    operation: "image",
                    detail: "down",
                })
            }
        }
        let dir = temp("offline");
        let keys = MemoryKeys::new();
        let cache = written(&dir, &keys, &[cached_chat("c1", "Chat", false)], &[]);
        let mut app = app_for(Arc::new(Failing));
        app.use_cache(cache);
        app.start();
        pump(&mut app, |a| a.chats.error().is_some());
        assert!(app.chats.is_cached(), "still the cached list");
        assert_eq!(app.chats.items().len(), 1);
        assert!(
            app.status
                .as_ref()
                .is_some_and(|(t, _, e)| *e && t.contains("cannot refresh"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
