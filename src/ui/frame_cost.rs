//! What one steady frame costs at the size of a real account, by part, and the reference implementations the cached
//! drawing paths are compared with.
//!
//! The synthetic catalogue (`Catalogue`) is made up: 300 chats (1:1, groups of 3-12, meetings; favourites, unread ones,
//! previews of 30-120 characters, presence for the people of the 1:1 chats), 30 teams of 1-32 channels (250 in all,
//! some archived, muted, pinned, six favourites and unread ones) and a conversation of 200 messages of mixed length with
//! code, links, reactions, mentions and three pictures. The measurements are `#[ignore]`d: run them by hand,
//!
//! ```text
//! cargo test --bin vtteams frame_cost_by_part -- --ignored --nocapture
//! cargo test --release --bin vtteams frame_cost_by_part -- --ignored --nocapture
//! ```

use super::*;
use crate::app::App;
use crate::demo::DemoBackend;
use crate::view::Tab;
use ratatui::Terminal;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use teams_client::{
    Attachment, Availability, Backend, Channel, ChannelKind, Chat, ChatKind, Contact, Delivery, Me,
    Member, Msg, OutMention, Page, Presence, Preview, Reaction, ReplyTarget, Team,
};

const ME: &str = "me-0";
const PEOPLE: usize = 90;
const FIRST: [&str; 18] = [
    "Ada",
    "Grace",
    "Linus",
    "Margaret",
    "Alan",
    "Katherine",
    "Dennis",
    "Barbara",
    "Ken",
    "Radia",
    "Edsger",
    "Frances",
    "Donald",
    "Hedy",
    "Brian",
    "Shafi",
    "Tim",
    "Anita",
];
const LAST: [&str; 15] = [
    "Lovelace",
    "Hopper",
    "Torvalds",
    "Hamilton",
    "Turing",
    "Johnson",
    "Ritchie",
    "Liskov",
    "Thompson",
    "Perlman",
    "Dijkstra",
    "Allen",
    "Knuth",
    "Lamarr",
    "Kernighan",
];

/// A fixed pseudo-random sequence, so that two runs measure the same data.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

fn person(n: usize) -> (String, String) {
    (
        format!("p-{n}"),
        format!(
            "{} {}",
            FIRST[n % FIRST.len()],
            LAST[(n / 3 + n) % LAST.len()]
        ),
    )
}

const WORDS: [&str; 24] = [
    "the", "build", "failed", "again", "because", "deploy", "review", "please", "merge", "ticket",
    "tomorrow", "meeting", "customer", "release", "notes", "agenda", "looks", "good", "to", "me",
    "thanks", "see", "attached", "quick",
];

fn sentence(r: &mut Lcg, words: usize) -> String {
    let mut s = String::new();
    for i in 0..words {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(WORDS[r.below(WORDS.len())]);
    }
    s
}

/// The 300 chats, newest first.
fn chats() -> Vec<Chat> {
    let mut r = Lcg(7);
    let now = Utc::now();
    (0..300)
        .map(|n| {
            let kind = match r.below(20) {
                0..=11 => ChatKind::OneOnOne,
                12..=16 => ChatKind::Group,
                _ => ChatKind::Meeting,
            };
            let count = match kind {
                ChatKind::OneOnOne => 1,
                _ => 2 + r.below(11),
            };
            let mut members = vec![Member {
                not_in_directory: false,
                id: ME.into(),
                name: "John Example".into(),
            }];
            for k in 0..count {
                let (id, name) = person((n * 7 + k * 13) % PEOPLE);
                members.push(Member {
                    not_in_directory: false,
                    id,
                    name,
                });
            }
            let topic = match kind {
                ChatKind::OneOnOne => None,
                ChatKind::Group if r.below(3) == 0 => None,
                _ => Some(format!("{} {}", sentence(&mut r, 2), n)),
            };
            let updated = now - chrono::Duration::minutes(n as i64 * 37 + 2);
            let from = if r.below(4) == 0 {
                (Some(ME.to_string()), Some("John Example".to_string()))
            } else {
                let (id, name) = person((n * 7) % PEOPLE);
                (Some(id), Some(name))
            };
            let unread = n % 15 == 3;
            let last_read = if unread {
                Some(updated - chrono::Duration::hours(2))
            } else {
                Some(updated + chrono::Duration::minutes(1))
            };
            let words = 5 + r.below(16);
            Chat {
                id: format!("chat-{n}"),
                kind,
                topic,
                members,
                updated,
                preview: Some(Preview {
                    created: updated,
                    from_id: if unread {
                        from.0.clone().filter(|id| id != ME).or(Some("p-1".into()))
                    } else {
                        from.0
                    },
                    from_name: from.1,
                    text: sentence(&mut r, words),
                    automated: false,
                }),
                last_read,
                web_url: None,
                favorite: (n > 0 && n % 41 == 0).then_some((n / 41) as u32),
            }
        })
        .collect()
}

/// 30 teams, 250 channels.
fn teams() -> Vec<Team> {
    let sizes = [
        32, 26, 21, 17, 15, 13, 12, 10, 9, 9, 8, 8, 7, 7, 6, 6, 6, 5, 5, 5, 4, 4, 3, 3, 2, 2, 2, 1,
        1, 1,
    ];
    let mut r = Lcg(11);
    let now = Utc::now();
    let mut favourites = 0;
    sizes
        .iter()
        .enumerate()
        .map(|(t, &count)| {
            let channels = (0..count)
                .map(|c| {
                    let general = c == 0;
                    let favorite =
                        (!general && c == 1 && favourites < 6 && t % 3 == 0).then(|| {
                            favourites += 1;
                            favourites as u32
                        });
                    Channel {
                        id: if general {
                            format!("team-{t}")
                        } else {
                            format!("chan-{t}-{c}")
                        },
                        name: if general {
                            "General".into()
                        } else {
                            format!("{}-{c}", sentence(&mut r, 1))
                        },
                        general,
                        kind: match r.below(12) {
                            0 => ChannelKind::Private,
                            1 => ChannelKind::Shared,
                            _ => ChannelKind::Standard,
                        },
                        archived: r.below(30) == 0,
                        shown: general || r.below(3) > 0,
                        pinned: !general && r.below(8) == 0,
                        muted: r.below(15) == 0,
                        followed: r.below(2) == 0,
                        favorite,
                        unread: r.below(14) == 0,
                        last: Some(Preview {
                            created: now - chrono::Duration::hours((t * 5 + c) as i64),
                            from_id: Some("p-2".into()),
                            from_name: Some("Grace Hopper".into()),
                            text: sentence(&mut r, 8),
                            automated: false,
                        }),
                    }
                })
                .collect();
            Team {
                id: format!("team-{t}"),
                name: format!("{} {}", sentence(&mut r, 2), t),
                archived: t == 28 || t == 29,
                muted: t % 11 == 5,
                members: Some(5 + t as u32),
                channels,
            }
        })
        .collect()
}

/// The open conversation: 200 messages over three days.
fn conversation() -> Vec<Msg> {
    let mut r = Lcg(3);
    let now = Utc::now();
    (0..200)
        .map(|n| {
            let at = now - chrono::Duration::minutes((200 - n as i64) * 17);
            let who = if r.below(5) == 0 { (ME.to_string(), "John Example".to_string()) } else { person(1 + r.below(4)) };
            let body = match n {
                40 | 120 | 197 => format!(
                    "<p>look at this: {}</p><p><img src=\"https://demo.invalid/plasma/{n}\" alt=\"graph\"></p>",
                    sentence(&mut r, 4)
                ),
                _ => match r.below(10) {
                    0 => "<p>ok</p>".to_string(),
                    1 => format!("<p>{}</p>", sentence(&mut r, 3)),
                    2 => format!(
                        "<p>see <a href=\"https://example.com/docs/{n}\">the page</a> and <b>{}</b></p>",
                        sentence(&mut r, 6)
                    ),
                    3 => format!(
                        "<p>{}</p><codeblock class=\"Python\"><code>def f(x):<br>    return [i * x for i in range(10)]<br><br>print(f({n}))</code></codeblock>",
                        sentence(&mut r, 4)
                    ),
                    4 => format!(
                        "<p>{}</p><codeblock class=\"ObjectScript\"><code>Set x = ##class(Demo.Thing).%New()<br>Write x.Name, !<br>Quit $$$OK</code></codeblock>",
                        sentence(&mut r, 5)
                    ),
                    5 | 6 => format!("<p>{}. {}. {}.</p>", sentence(&mut r, 20), sentence(&mut r, 18), sentence(&mut r, 25)),
                    _ => {
                        let words = 8 + r.below(14);
                        format!("<p>{}</p>", sentence(&mut r, words))
                    }
                },
            };
            let reactions = if r.below(9) == 0 {
                vec![Reaction {
                    emoji: "👍".into(),
                    count: 1 + r.below(3),
                    users: vec!["p-2".into()],
                    names: vec!["Bob".into()],
                }]
            } else {
                vec![]
            };
            Msg {
                root_id: None,
                id: format!("{}", 1_700_000_000_000u64 + n as u64),
                created: at,
                modified: at,
                edited: None,
                from_id: Some(who.0),
                from_name: who.1,
                system: false,
                deleted: false,
                html: true,
                body,
                subject: None,
                attachments: Vec::<Attachment>::new(),
                reactions,
                mentions: if n % 23 == 7 { vec![ME.to_string()] } else { vec![] },
                delivery: Delivery::Sent,
            }
        })
        .collect()
}

/// The demo's pictures and everything else the app asks for, with the synthetic lists in place of its own.
struct Catalogue {
    inner: DemoBackend,
}

impl Backend for Catalogue {
    fn label(&self) -> String {
        "catalogue".into()
    }
    fn me(&self) -> teams_client::Result<Me> {
        Ok(Me {
            id: ME.into(),
            name: "John Example".into(),
        })
    }
    fn chats(&self) -> teams_client::Result<Vec<Chat>> {
        Ok(chats())
    }
    fn find_people(&self, q: &str) -> teams_client::Result<Vec<Contact>> {
        self.inner.find_people(q)
    }
    fn open_chat(&self, who: &Contact) -> teams_client::Result<Chat> {
        self.inner.open_chat(who)
    }
    fn messages(&self, chat_id: &str, _older: Option<&str>) -> teams_client::Result<Page> {
        let messages = if chat_id == "chat-0" {
            conversation()
        } else {
            conversation().into_iter().skip(160).collect()
        };
        Ok(Page {
            messages,
            older: None,
        })
    }
    fn send(
        &self,
        chat_id: &str,
        html: &str,
        m: &[OutMention],
        reply_to: Option<&ReplyTarget>,
    ) -> teams_client::Result<Msg> {
        self.inner.send(chat_id, html, m, reply_to)
    }
    fn can_list_teams(&self) -> bool {
        true
    }
    fn teams(&self) -> teams_client::Result<Vec<Team>> {
        Ok(teams())
    }
    fn can_presence(&self) -> bool {
        true
    }
    fn presence(
        &self,
        ids: &[String],
    ) -> teams_client::Result<std::collections::HashMap<String, Presence>> {
        let all = [
            Availability::Available,
            Availability::Busy,
            Availability::Away,
            Availability::Offline,
            Availability::DoNotDisturb,
        ];
        Ok(ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), Presence::new(all[i % all.len()], i % 17 == 0)))
            .collect())
    }
    fn can_search_messages(&self) -> bool {
        false
    }
    fn serves_image(&self, url: &str) -> bool {
        self.inner.serves_image(url)
    }
    fn image(&self, url: &str) -> teams_client::Result<image::RgbImage> {
        self.inner.image(url)
    }
}

/// A byte sink that only counts.
struct Counter(Rc<Cell<usize>>);

impl std::io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.set(self.0.get() + buf.len());
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scene {
    /// Just started: the Chats tab, the newest chat opened by the app itself and no other visited.
    ChatsOnly,
    /// The same after a day's use: 30 other chats visited (each keeps its messages), the 200-message conversation open.
    Conversation,
    TeamsFolded,
    TeamsOpen,
    Search,
    Help,
    /// The conversation with a picture on screen, drawn as block art.
    BlockArt,
    /// The same, drawn with Sixel graphics.
    Sixel,
    /// The emoji selector, with every emoji listed.
    Emoji,
}

impl Scene {
    const ALL: [Scene; 9] = [
        Scene::ChatsOnly,
        Scene::Conversation,
        Scene::TeamsFolded,
        Scene::TeamsOpen,
        Scene::Search,
        Scene::Help,
        Scene::BlockArt,
        Scene::Sixel,
        Scene::Emoji,
    ];
}

/// An app in the state a long session leaves it in: everything loaded, the open chat settled, pictures decoded.
fn app_for(scene: Scene) -> App {
    let (mut app, _) = App::for_test(Arc::new(Catalogue {
        inner: DemoBackend::instant(),
    }));
    app.set_keymap(crate::editor::Keymap::Standard);
    app.start();
    for _ in 0..12 {
        app.on_tick();
    }
    if scene != Scene::ChatsOnly {
        app.select_chat("chat-0");
        // Visit a number of other chats too, as a day's use does: each keeps its messages.
        for n in 1..=30 {
            app.select_chat(&format!("chat-{n}"));
            for _ in 0..3 {
                app.on_tick();
            }
        }
        app.select_chat("chat-0");
    }
    for _ in 0..12 {
        crate::ui::prepare(&mut app, ratatui::layout::Rect::new(0, 0, 156, 40));
        app.on_tick();
    }
    match scene {
        Scene::TeamsFolded | Scene::TeamsOpen => {
            app.show_tab(Tab::Teams);
            for _ in 0..6 {
                app.on_tick();
            }
            if scene == Scene::TeamsOpen {
                app.unfold_every_team();
            }
        }
        Scene::Search => {
            app.focus = crate::app::Focus::Messages;
            app.on_key(key(KeyCode::Char('f'), KeyModifiers::CONTROL));
            for c in "the".chars() {
                app.on_key(key(KeyCode::Char(c), KeyModifiers::NONE));
            }
        }
        Scene::Help => app.on_key(key(KeyCode::F(1), KeyModifiers::NONE)),
        Scene::Emoji => {
            app.focus = crate::app::Focus::Editor;
            app.on_key(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        }
        Scene::Sixel => app.set_images(
            crate::gfx::ImageChoice::Protocol(crate::gfx::Protocol::Sixel),
            crate::gfx::probe::Facts {
                cell: Some(crate::gfx::CellPx { w: 10, h: 20 }),
                why: "a measurement",
                ..crate::gfx::probe::Facts::NONE
            },
        ),
        _ => {}
    }
    for _ in 0..30 {
        app.on_tick();
        crate::ui::prepare(&mut app, ratatui::layout::Rect::new(0, 0, 156, 40));
    }
    app
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> Duration {
    // One to warm up.
    std::hint::black_box(f());
    median(
        (0..runs)
            .map(|_| {
                let started = Instant::now();
                std::hint::black_box(f());
                started.elapsed()
            })
            .collect(),
    )
}

fn us(d: Duration) -> String {
    format!("{:>8.0}", d.as_secs_f64() * 1e6)
}

/// A drawing section's cost: draws `f` into a persistent test terminal (the diff with the previous frame included), less
/// what an empty draw costs.
fn section(app: &App, size: (u16, u16), mut f: impl FnMut(&mut Frame<'_>, &App)) -> Duration {
    let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).expect("terminal");
    let base = time(40, || {
        terminal.draw(|_| {}).expect("draw");
    });
    let full = time(40, || {
        terminal.draw(|frame| f(frame, app)).expect("draw");
    });
    full.saturating_sub(base)
}

/// Draw `frames` frames of an app nothing happens to, as the idle loop does (the header's animation moves, nothing
/// else), into one test terminal.
fn idle_frames(app: &mut App, terminal: &mut Terminal<TestBackend>, frames: usize) {
    for _ in 0..frames {
        app.tick += 1;
        app.animate();
        terminal.draw(|f| crate::ui::render(f, app)).expect("draw");
    }
}

#[test]
fn an_app_nothing_happens_to_lays_no_message_out_again_frame_after_frame() {
    let mut app = app_for(Scene::BlockArt);
    let mut terminal = Terminal::new(TestBackend::new(156, 40)).expect("terminal");
    // A first few frames settle whatever the last tick changed.
    idle_frames(&mut app, &mut terminal, 3);
    let settled = crate::transcript::laid_out();
    idle_frames(&mut app, &mut terminal, 20);
    assert_eq!(
        crate::transcript::laid_out(),
        settled,
        "twenty frames in which nothing changed laid out messages again"
    );

    // The transcript is built again when anything it is made from may have changed (a poll that brought a message), but
    // a message that is as it was is taken from what was made of it: none is laid out for it.
    app.invalidate_open_transcript();
    idle_frames(&mut app, &mut terminal, 1);
    assert_eq!(
        crate::transcript::laid_out(),
        settled,
        "a transcript built again from the same messages"
    );

    // What does make every message be laid out again is a change of width, and the counter sees it: the test above is
    // not passing for want of looking.
    let mut narrower = Terminal::new(TestBackend::new(150, 40)).expect("terminal");
    idle_frames(&mut app, &mut narrower, 1);
    assert!(
        crate::transcript::laid_out() >= settled + 100,
        "{} laid out for a new width",
        crate::transcript::laid_out() - settled
    );
}

#[test]
fn an_idle_frame_with_the_synthetic_catalogue_is_cheap() {
    // Loose on purpose (a debug build, a busy machine): a steady frame of 300 chats, 30 teams and 200 messages takes
    // about a millisecond; this fails only when something per frame has become a hundred times that.
    let mut app = app_for(Scene::Conversation);
    let mut terminal = Terminal::new(TestBackend::new(156, 40)).expect("terminal");
    idle_frames(&mut app, &mut terminal, 3);
    let steady = time(20, || idle_frames(&mut app, &mut terminal, 1));
    assert!(
        steady < Duration::from_millis(100),
        "a steady frame took {steady:?}"
    );
}

#[test]
#[ignore = "a measurement, run on demand"]
fn frame_cost_by_part() {
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    for size in [(156u16, 40u16), (200, 60), (100, 30)] {
        for scene in Scene::ALL {
            let mut app = app_for(scene);
            let area = ratatui::layout::Rect::new(0, 0, size.0, size.1);
            crate::ui::prepare(&mut app, area);
            app.tick += 1;
            app.animate();
            crate::ui::prepare(&mut app, area);
            println!("\n== {build} {}x{} {scene:?}", size.0, size.1);
            if std::env::var_os("VTTEAMS_SHOW_FRAME").is_some() {
                println!("{}", crate::ui::render_to_string(&mut app, size.0, size.1));
            }

            // The whole frame as the event loop draws it: a test terminal, the header's animation changing a few cells.
            let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).expect("terminal");
            let mut presenter = crate::gfx::present::Presenter::default();
            let mut pictures = Counter(Rc::new(Cell::new(0)));
            terminal
                .draw(|f| crate::ui::render(f, &mut app))
                .expect("draw");
            app.present_graphics(&mut presenter, &mut pictures)
                .expect("pictures");
            println!("{} pictures planned", app.gfx_plan().len());
            let whole = time(40, || {
                app.tick += 1;
                app.animate();
                terminal
                    .draw(|f| crate::ui::render(f, &mut app))
                    .expect("draw");
                app.present_graphics(&mut presenter, &mut pictures)
                    .expect("pictures");
            });
            // The same into a byte sink, for what an unchanged frame sends.
            let written = Rc::new(Cell::new(0usize));
            let sink = CrosstermBackend::new(Counter(Rc::clone(&written)));
            let mut wire = Terminal::with_options(
                sink,
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(area),
                },
            )
            .expect("terminal");
            wire.draw(|f| crate::ui::render(f, &mut app)).expect("draw");
            written.set(0);
            app.tick += 1;
            app.animate();
            wire.draw(|f| crate::ui::render(f, &mut app)).expect("draw");
            let bytes = written.get();
            println!(
                "steady frame (prepare + draw + diff) {} us; {bytes} bytes written",
                us(whole)
            );

            // prepare, by part (each as `prepare` calls it)
            println!(
                "  prepare {} us",
                us(time(40, || crate::ui::prepare(&mut app, area)))
            );
            let now = Utc::now();
            println!(
                "    banner_entries      {} us",
                us(time(200, || app.banner_entries(now)))
            );
            println!(
                "    visible_chats       {} us",
                us(time(200, || app.visible_chats()))
            );
            println!(
                "    team_rows           {} us",
                us(time(200, || app.team_rows()))
            );
            let rows = app.team_rows();
            println!(
                "    team_cursor_index   {} us",
                us(time(200, || app.team_cursor_index(&rows)))
            );
            println!(
                "    outstanding_mentions{} us",
                us(time(200, || app.outstanding_mentions()))
            );
            println!(
                "    tab_labels          {} us",
                us(time(200, || app.tab_labels(40)))
            );
            let geometry = app.view.geometry.clone();
            println!(
                "    plan_graphics       {} us",
                us(time(200, || app.plan_graphics(&geometry)))
            );
            println!(
                "    layout_transcript   {} us",
                us(time(100, || app.layout_transcript(
                    geometry.transcript.width as usize,
                    geometry.transcript.height as usize
                )))
            );

            if app.current.is_some() {
                println!(
                    "  transcript rebuilt, message cache warm {} us",
                    us(time(20, || {
                        app.invalidate_open_transcript();
                        crate::ui::prepare(&mut app, area);
                    }))
                );
                let mut narrow = false;
                println!(
                    "  transcript rebuilt, every message laid out again {} us",
                    us(time(5, || {
                        narrow = !narrow;
                        let a =
                            ratatui::layout::Rect::new(0, 0, size.0 - u16::from(narrow), size.1);
                        crate::ui::prepare(&mut app, a);
                    }))
                );
                crate::ui::prepare(&mut app, area);
            }

            // draw, by part
            let g = app.view.geometry.clone();
            let part = |name: &str, d: Duration| println!("  draw {name:<20} {} us", us(d));
            part(
                "header",
                section(&app, size, |f, a| draw_header(f, a, g.header)),
            );
            if g.list != Rect::default() {
                part(
                    "chat_list",
                    section(&app, size, |f, a| draw_chat_list(f, a, &g)),
                );
            }
            if g.conversation != Rect::default() {
                part(
                    "conversation",
                    section(&app, size, |f, a| draw_conversation(f, a, &g)),
                );
                part(
                    "transcript",
                    section(&app, size, |f, a| draw_transcript(f, a, &g)),
                );
                part(
                    "editor",
                    section(&app, size, |f, a| draw_editor(f, a, g.editor)),
                );
                part(
                    "banner",
                    section(&app, size, |f, a| draw_banner(f, a, g.banner)),
                );
            }
            part("footer", section(&app, size, |f, a| draw_footer(f, a, &g)));
            part(
                "hint_labels_of",
                time(200, || hint_labels_of(&app, &g)).saturating_sub(Duration::ZERO),
            );
            part("all of draw", section(&app, size, draw));
        }
    }
}
