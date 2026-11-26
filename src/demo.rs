//! Offline demo backend: canned conversations in Teams' real HTML dialect, a synthetic image, and a little "live"
//! behaviour (people reply, new messages arrive) so the whole UI can be explored without a Microsoft account.

use chrono::{DateTime, Duration, Utc};
use image::{Rgb, RgbImage};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;
use teams_client::{
    Availability, Backend, Channel, ChannelActivity, ChannelKind, ChannelPost, Chat, ChatKind,
    Contact, Delivery, EmojiInfo, Error, Me, Member, Msg, OutMention, Page, Presence, Preview,
    PushHint, Reaction, ReplyTarget, Result, SearchHit, SearchPage, StatusNote, Team,
    html_to_plain, same_emoji,
};

pub const ME_ID: &str = "demo-me";

struct Person(&'static str, &'static str);
const ADA: Person = Person("u-ada", "Ada Lovelace");
const GRACE: Person = Person("u-grace", "Grace Hopper");
const LINUS: Person = Person("u-linus", "Linus Torvalds");
const MARGARET: Person = Person("u-margaret", "Margaret Hamilton");
const ALAN: Person = Person("u-alan", "Alan Turing");
const KATHERINE: Person = Person("u-katherine", "Katherine Johnson");
const CHARLES: Person = Person("u-charles", "Charles Muddy");
const MEPERSON: Person = Person(ME_ID, "John Titor");

struct State {
    chats: Vec<Chat>,
    messages: Vec<(String, Vec<Msg>)>,
    next_id: u64,
    pending_replies: Vec<(Instant, String, &'static Person, String)>,
    next_incoming: Instant,
    incoming_n: usize,
    /// Every request to mark a chat read so far: (chat, message id).
    read_marks: Vec<(String, String)>,
}

pub struct DemoBackend {
    state: Mutex<State>,
    /// Whether calls take a realistic moment, so spinners and loading states can be seen.
    latency: bool,
    /// Whether it answers message searches (a service that cannot is how the fallback to the loaded messages is seen).
    search: bool,
    /// Whether it has teams (a service that has none is how the chat list alone is seen).
    teams: bool,
    /// Whether it takes posts, replies and edits in channels (a service that does not is how the refusal is seen).
    posting: bool,
    /// Queued by `DemoBackend::queue_push` (tests only) for the next [`Backend::poll_pushes`] to return.
    pushes: Mutex<Vec<PushHint>>,
    /// Your own availability: what you chose (if you did), and every report of whether you were at the keyboard.
    own: Mutex<OwnStatus>,
}

#[derive(Default)]
struct OwnStatus {
    chosen: Option<Availability>,
    reports: Vec<bool>,
}

fn member(p: &Person) -> Member {
    Member {
        not_in_directory: false,
        id: p.0.into(),
        name: p.1.into(),
    }
}

fn msg(id: u64, ago_min: i64, who: &Person, html: &str) -> Msg {
    let t = Utc::now() - Duration::minutes(ago_min);
    Msg {
        root_id: None,
        id: id.to_string(),
        created: t,
        modified: t,
        edited: None,
        from_id: Some(who.0.into()),
        from_name: who.1.into(),
        system: false,
        deleted: false,
        html: true,
        body: html.into(),
        subject: None,
        attachments: vec![],
        reactions: vec![],
        mentions: vec![],
        delivery: Delivery::Sent,
    }
}

/// What a message that mentions people reads back as: each `<at id="N">` carries the mentioned person's own id, the
/// way the Teams web backend gives it.
fn read_back(html: &str, mentions: &[OutMention]) -> String {
    let mut out = html.to_string();
    for m in mentions {
        out = out.replace(
            &format!("<at id=\"{}\">", m.id),
            &format!("<at id=\"{}\">", m.user_id),
        );
    }
    out
}

/// Teams' own reply-quote markup (see `AGENTS.md`, "Messages, mentions and replies"), built by hand the same way the
/// rest of this file's canned messages are, so `--demo` exercises a real reply end to end.
fn reply_quote_html(target: &ReplyTarget) -> String {
    let id = escape_html(&target.id);
    let name = escape_html(&target.from_name);
    let who = match &target.from_id {
        Some(sender) => format!(
            "<strong itemprop=\"mri\" itemid=\"8:orgid:{sender}\">{name}</strong>",
            sender = escape_html(sender)
        ),
        None => format!("<strong>{name}</strong>"),
    };
    format!(
        "<blockquote itemscope=\"\" itemtype=\"http://schema.skype.com/Reply\" itemid=\"{id}\">{who}<span itemprop=\"time\" itemid=\"{id}\"></span><p itemprop=\"preview\">{preview}</p></blockquote>",
        preview = escape_html(&target.preview),
    )
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Marks `m` as @mentioning `who` (set directly, the way the demo backend builds every `Msg` by hand rather
/// than by parsing JSON; a real backend gets this from `properties.mentions` or Graph's `mentions` array).
fn mentioning(mut m: Msg, who: &Person) -> Msg {
    m.mentions.push(who.0.into());
    m
}

fn system(id: u64, ago_min: i64, text: &str) -> Msg {
    let mut m = msg(
        id,
        ago_min,
        &Person("", ""),
        &format!("<systemEventMessage/>{text}"),
    );
    m.system = true;
    m.from_id = None;
    m.from_name = String::new();
    m.body = text.into();
    m
}

/// A post in a channel: it may have a title.
fn post(id: u64, ago_min: i64, who: &Person, subject: Option<&str>, html: &str) -> Msg {
    let mut m = msg(id, ago_min, who, html);
    m.subject = subject.map(String::from);
    m
}

/// A reply in a channel to the post `root` (an id that need not be among the messages: a reply to a post older than
/// what was loaded names a post that is not there).
fn reply_in(id: u64, ago_min: i64, who: &Person, root: &str, html: &str) -> Msg {
    let mut m = msg(id, ago_min, who, html);
    m.root_id = Some(root.to_string());
    m
}

/// The newest message a person wrote in `list`, as a channel list shows it.
fn last_post(list: &[Msg]) -> Option<Preview> {
    let m = list.iter().rev().find(|m| !m.system && !m.deleted)?;
    Some(Preview {
        created: m.created,
        from_id: m.from_id.clone(),
        from_name: Some(m.from_name.clone()),
        text: m.subject.clone().unwrap_or_else(|| html_to_plain(&m.body)),
        automated: false,
    })
}

const ENG: &str = "demo-team-eng";
const RELEASES: &str = "demo-chan-releases";

fn channel(id: &str, name: &str) -> Channel {
    Channel {
        id: id.into(),
        name: name.into(),
        general: false,
        kind: ChannelKind::Standard,
        archived: false,
        shown: false,
        pinned: false,
        muted: false,
        followed: true,
        favorite: None,
        unread: false,
        last: None,
    }
}

/// Three teams with a channel of every kind the list draws: a team with General, a pinned channel, standard, private,
/// shared and archived ones that are shown and two that are hidden; a muted team; an archived one (the list does not
/// show it). Two channels are favourites, in different teams, one of them a General channel.
fn demo_teams() -> Vec<Team> {
    let general = |id: &str| Channel {
        general: true,
        shown: true,
        ..channel(id, "General")
    };
    let shown = |c: Channel| Channel { shown: true, ..c };
    vec![
        Team {
            id: ENG.into(),
            name: "Engineering".into(),
            archived: false,
            muted: false,
            members: Some(48),
            channels: vec![
                general(ENG),
                Channel {
                    pinned: true,
                    shown: true,
                    ..channel(RELEASES, "Releases")
                },
                Channel {
                    unread: true,
                    favorite: Some(1),
                    ..shown(channel("demo-chan-incidents", "Incidents"))
                },
                Channel {
                    kind: ChannelKind::Private,
                    ..shown(channel("demo-chan-design", "Design review"))
                },
                Channel {
                    kind: ChannelKind::Shared,
                    ..shown(channel("demo-chan-partners", "Partners"))
                },
                Channel {
                    archived: true,
                    ..shown(channel("demo-chan-old", "Old experiments"))
                },
                channel("demo-chan-random", "Random"),
                Channel {
                    archived: true,
                    ..channel("demo-chan-hack", "Hackathon 2023")
                },
            ],
        },
        Team {
            id: "demo-team-mkt".into(),
            name: "Marketing".into(),
            archived: false,
            muted: true,
            members: Some(12),
            channels: vec![
                Channel {
                    favorite: Some(2),
                    ..general("demo-team-mkt")
                },
                shown(channel("demo-chan-campaigns", "Campaigns")),
                channel("demo-chan-assets", "Assets"),
            ],
        },
        Team {
            id: "demo-team-legacy".into(),
            name: "Legacy Platform".into(),
            archived: true,
            muted: false,
            members: Some(5),
            channels: vec![Channel {
                archived: true,
                ..general("demo-team-legacy")
            }],
        },
    ]
}

/// The messages of the two channels that have any: posts with and without a title, replies, a reply whose post is not
/// among them, and a system event.
fn demo_channel_messages(id: &mut impl FnMut() -> u64) -> Vec<(String, Vec<Msg>)> {
    let welcome = id();
    let rota = id();
    let general = vec![
        system(
            id(),
            3000,
            "Margaret Hamilton added Ada Lovelace to the team.",
        ),
        post(
            welcome,
            600,
            &MARGARET,
            Some("Welcome to Engineering"),
            "<p>Please read the pinned guide before your first review.</p>",
        ),
        reply_in(
            id(),
            590,
            &ADA,
            &welcome.to_string(),
            "<p>Thanks, read it. One question about the on-call rota.</p>",
        ),
        reply_in(
            id(),
            585,
            &MARGARET,
            &welcome.to_string(),
            "<p>The rota is on the wiki; tell me if it is wrong.</p>",
        ),
        post(
            rota,
            120,
            &LINUS,
            None,
            "<p>Does anyone still have the flame graph from last week?</p>",
        ),
        reply_in(
            id(),
            110,
            &GRACE,
            &rota.to_string(),
            "<p>It is in the Platform Team chat.</p>",
        ),
        reply_in(
            id(),
            40,
            &KATHERINE,
            "an-older-post-that-is-not-loaded",
            "<p>Following up on the earlier thread: fixed in main.</p>",
        ),
    ];
    let checklist = id();
    let out = id();
    let releases = vec![
        post(
            checklist,
            300,
            &ADA,
            Some("Release 4.2 checklist"),
            "<p>Freeze on Thursday: tests, changelog, tag.</p><ul><li>tests green</li><li>changelog</li><li>tag</li></ul>",
        ),
        reply_in(
            id(),
            280,
            &LINUS,
            &checklist.to_string(),
            "<p>The changelog is ready.</p>",
        ),
        post(
            out,
            30,
            &MARGARET,
            Some("Release 4.2 is out"),
            "<p>Tagged and published. Thank you all \u{1F389}</p>",
        ),
        reply_in(
            id(),
            20,
            &MEPERSON,
            &out.to_string(),
            "<p>Great work, everyone.</p>",
        ),
        reply_in(
            id(),
            6,
            &ADA,
            &out.to_string(),
            "<p>The notes are on the wiki now.</p>",
        ),
    ];
    vec![(ENG.to_string(), general), (RELEASES.to_string(), releases)]
}

/// The demo accepts all but the last ten emoji of the shortcode table as reactions, as a real service accepts only some
/// (any emoji can still be written into a message).
fn reaction_set() -> Vec<EmojiInfo> {
    let all = crate::emoji::catalog();
    all.iter()
        .take(all.len().saturating_sub(10))
        .map(|e| EmojiInfo {
            emoji: e.emoji.clone(),
            name: e.name.clone(),
            aliases: e.aliases.clone(),
        })
        .collect()
}

fn reacted(mut m: Msg, r: &[(&str, usize)]) -> Msg {
    m.reactions = r
        .iter()
        .enumerate()
        .map(|(i, (e, c))| {
            // Who reacted: you on the first reaction of a message that has more than one person, then some of the
            // demo's people; a count beyond them is counted without names, as the service sometimes does.
            const PEOPLE: [(&str, &str); 4] = [
                ("u-grace", "Grace Hopper"),
                ("u-linus", "Linus Torvalds"),
                ("u-margaret", "Margaret Hamilton"),
                ("u-alan", "Alan Turing"),
            ];
            let mut people: Vec<(String, String)> = Vec::new();
            if i == 0 && *c > 1 {
                people.push((ME_ID.to_string(), String::new()));
            }
            for (id, name) in PEOPLE.iter().cycle().skip(i).take(PEOPLE.len()) {
                if people.len() >= (*c).min(4) {
                    break;
                }
                people.push(((*id).to_string(), (*name).to_string()));
            }
            let (users, names): (Vec<String>, Vec<String>) = people.into_iter().unzip();
            Reaction {
                emoji: (*e).into(),
                count: *c,
                users,
                names,
            }
        })
        .collect();
    m
}

fn chat(
    id: &str,
    kind: ChatKind,
    topic: Option<&str>,
    people: &[&Person],
    msgs: &[Msg],
    unread: bool,
) -> (Chat, Vec<Msg>) {
    let mut members = vec![member(&MEPERSON)];
    members.extend(people.iter().map(|p| member(p)));
    let last = msgs.iter().rev().find(|m| !m.system).or(msgs.last());
    let preview = last.map(|m| Preview {
        created: m.created,
        from_id: m.from_id.clone(),
        from_name: Some(m.from_name.clone()),
        text: if m.html {
            html_to_plain(&m.body)
        } else {
            m.body.clone()
        },
        automated: false,
    });
    let newest = last.map_or_else(Utc::now, |m| m.created);
    let c = Chat {
        id: id.into(),
        kind,
        topic: topic.map(String::from),
        members,
        updated: newest,
        preview,
        last_read: Some(if unread {
            newest - Duration::minutes(30)
        } else {
            newest
        }),
        web_url: Some(format!("https://teams.microsoft.com/l/chat/{id}/0")),
        favorite: None,
    };
    (c, msgs.to_vec())
}

fn plasma(w: u32, h: u32, seed: f32) -> RgbImage {
    RgbImage::from_fn(w, h, |x, y| {
        let (fx, fy) = (x as f32 / w as f32 * 6.0, y as f32 / h as f32 * 4.0);
        let v = (fx + seed).sin()
            + (fy * 1.3 - seed).sin()
            + ((fx + fy) * 0.8 + seed).sin()
            + ((fx * fx + fy * fy).sqrt() * 1.5).sin();
        let t = (v + 4.0) / 8.0;
        let ch = |p: f32| ((0.5 + 0.5 * (std::f32::consts::TAU * (t + p)).sin()) * 255.0) as u8;
        // A few bright "chart bars" so thin details show up in the art renderers.
        if y > h * 3 / 4 && (x / 24) % 2 == 0 && (h - y) < (((x / 24) * 13) % (h / 4)) + 4 {
            return Rgb([240, 240, 255]);
        }
        Rgb([ch(0.0), ch(0.33), ch(0.67)])
    })
}

impl DemoBackend {
    pub fn new() -> Self {
        let mut n = 1000u64;
        let mut id = || {
            n += 1;
            n
        };

        let rust_review = chat(
            "demo-ada",
            ChatKind::OneOnOne,
            None,
            &[&ADA],
            &[
                msg(
                    id(),
                    190,
                    &ADA,
                    "<p>Morning! Did you get a chance to look at the Bernoulli PR?</p>",
                ),
                msg(
                    id(),
                    186,
                    &MEPERSON,
                    "<p>Yes - nice work. One nit: <code>numbers()</code> allocates on every call.</p>",
                ),
                reacted(
                    msg(
                        id(),
                        183,
                        &ADA,
                        "<p>Fair point. How about this?</p><codeblock class=\"Rust\"><code>pub fn numbers(n: usize) -&gt; impl Iterator&lt;Item = f64&gt; {\n    let mut b = vec![0.0; n + 1];\n    (0..=n).map(move |m| {\n        b[m] = 1.0 / (m as f64 + 1.0);\n        for j in (1..=m).rev() {\n            b[j - 1] = j as f64 * (b[j - 1] - b[j]);\n        }\n        b[0] // B_m\n    })\n}</code></codeblock>",
                    ),
                    &[("👍", 1)],
                ),
                msg(
                    id(),
                    181,
                    &MEPERSON,
                    "<p>Much better. Ship it \u{1F680}</p>",
                ),
                msg(
                    id(),
                    12,
                    &ADA,
                    "<p>Merged. Also: the <b>engine</b> diagram you asked for is below.</p><p><img src=\"https://demo.invalid/plasma/1\" alt=\"engine diagram\"></p>",
                ),
                msg(
                    id(),
                    4,
                    &ADA,
                    "<p>Can you review the follow-up today? It's <i>small</i>, I promise. <a href=\"https://example.com/pr/42\">PR #42</a></p>",
                ),
            ],
            true,
        );

        let grace = chat(
            "demo-grace",
            ChatKind::OneOnOne,
            None,
            &[&GRACE],
            &[
                msg(
                    id(),
                    2900,
                    &GRACE,
                    "<p>Found the bug in the compiler. It was a literal bug \u{1F41B}</p>",
                ),
                msg(
                    id(),
                    2880,
                    &MEPERSON,
                    "<p>Of course it was. Moth in the relay?</p>",
                ),
                msg(
                    id(),
                    640,
                    &GRACE,
                    "<p>Here's the ObjectScript version of the fix for the IRIS side:</p><codeblock class=\"ObjectScript\"><code>ClassMethod Reconcile(pId As %String) As %Status\n{\n    Set tSC = $$$OK\n    Try {\n        Set tObj = ##class(Demo.Ledger).%OpenId(pId)\n        If '$IsObject(tObj) { Quit }\n        // walk the postings\n        For i = 1:1:tObj.Postings.Count() {\n            Set tP = tObj.Postings.GetAt(i)\n            Write tP.Amount, !\n        }\n    }\n    Catch ex { Set tSC = ex.AsStatus() }\n    Quit tSC\n}</code></codeblock>",
                ),
                msg(
                    id(),
                    630,
                    &MEPERSON,
                    "<p>Beautiful. Highlighting works in the terminal now, too.</p>",
                ),
            ],
            false,
        );

        let md_table = "<table><thead><tr><th>Service</th><th>p50</th><th>p99</th><th>Status</th></tr></thead><tbody><tr><td>gateway</td><td>12ms</td><td>80ms</td><td>✅</td></tr><tr><td>auth</td><td>30ms</td><td>240ms</td><td>⚠️</td></tr><tr><td>search</td><td>45ms</td><td>900ms</td><td>❌</td></tr></tbody></table>";
        let platform = chat(
            "demo-platform",
            ChatKind::Group,
            Some("Platform Team \u{1F6E0}"),
            &[&ADA, &LINUS, &MARGARET],
            &[
                system(id(), 5000, "Margaret Hamilton joined the chat."),
                msg(
                    id(),
                    300,
                    &LINUS,
                    "<p>Latency numbers from last night's soak test:</p>",
                ),
                msg(id(), 299, &LINUS, md_table),
                reacted(
                    msg(
                        id(),
                        290,
                        &MARGARET,
                        "<p><at id=\"0\">Linus Torvalds</at> search p99 is not <b>acceptable</b>. Can we get a flame graph?</p>",
                    ),
                    &[("👀", 2), ("👍", 1)],
                ),
                msg(
                    id(),
                    280,
                    &LINUS,
                    "<p>Attached. The hot spot is clear:</p><p><img src=\"https://demo.invalid/plasma/2\" alt=\"flame graph\"></p><p>Details in the <a href=\"https://example.com/wiki/search-perf\">wiki page</a> and the <a href=\"https://example.com/dash\">dashboard</a>.</p>",
                ),
                msg(
                    id(),
                    61,
                    &ADA,
                    "<p>Plan:</p><ul><li>cache the tokenizer</li><li>batch the <code>fsync</code>s</li><li>drop the O(n²) join</li></ul><p>and a quick Python check:</p><pre><code class=\"language-python\">def join(parts: list[str]) -&gt; str:\n    # O(n) instead of repeated concatenation\n    return \"\".join(parts)\n</code></pre>",
                ),
                msg(
                    id(),
                    9,
                    &MARGARET,
                    "<blockquote><strong>Ada Lovelace</strong><p>cache the tokenizer</p></blockquote><p>Agreed on that one - go.</p>",
                ),
                mentioning(
                    msg(
                        id(),
                        8,
                        &MARGARET,
                        &format!(
                            "<p><at id=\"{ME_ID}\">John Titor</at> can you take a look at the join fix before EOD?</p>"
                        ),
                    ),
                    &MEPERSON,
                ),
            ],
            true,
        );

        let linus = chat(
            "demo-linus",
            ChatKind::OneOnOne,
            None,
            &[&LINUS],
            &[
                msg(id(), 4000, &LINUS, "<p>Talk is cheap.</p>"),
                msg(id(), 3990, &MEPERSON, "<p>Show you the code?</p>"),
                msg(
                    id(),
                    3985,
                    &LINUS,
                    "<pre>$ git log --oneline | head -3\na1b2c3d fix: stop leaking fds\n9f8e7d6 feat: sextant art\n5c4b3a2 chore: bump deps</pre>",
                ),
            ],
            false,
        );

        let sprint = chat(
            "demo-sprint",
            ChatKind::Meeting,
            Some("Sprint planning"),
            &[&ADA, &ALAN, &KATHERINE, &CHARLES],
            &[
                system(id(), 1500, "Meeting started"),
                msg(
                    id(),
                    1480,
                    &KATHERINE,
                    "<p>Agenda:</p><ol><li>Last sprint review</li><li>Capacity</li><li>Risks</li></ol>",
                ),
                msg(
                    id(),
                    1475,
                    &CHARLES,
                    "<p>Risk from me: the release pipeline still needs a manual sign-off step.</p>",
                ),
                msg(
                    id(),
                    1470,
                    &ALAN,
                    "<p>Can the machine decide the order? \u{1F916}</p>",
                ),
                system(id(), 1400, "Meeting ended: 1h 2m"),
            ],
            false,
        );

        let moon = chat(
            "demo-moon",
            ChatKind::Group,
            None,
            &[&KATHERINE, &MARGARET, &ALAN],
            &[
                msg(
                    id(),
                    9000,
                    &KATHERINE,
                    "<p>Trajectory looks <b>right</b>. Verified by hand, too.</p>",
                ),
                msg(id(), 8900, &ALAN, "<p>I would expect nothing less. :-)</p>"),
            ],
            false,
        );

        let charles = chat(
            "demo-charles",
            ChatKind::OneOnOne,
            None,
            &[&CHARLES],
            &[
                msg(
                    id(),
                    9600,
                    &CHARLES,
                    "<p>The nightly build is green again. It was a stale cache on the runner.</p>",
                ),
                msg(
                    id(),
                    9590,
                    &MEPERSON,
                    "<p>Thanks for chasing it down \u{1F64F}</p>",
                ),
                msg(
                    id(),
                    9580,
                    &CHARLES,
                    "<p>Any time. I've added a step that clears it before each run.</p>",
                ),
            ],
            false,
        );

        let all = [rust_review, platform, grace, linus, sprint, moon, charles];
        // Two favourites, so the pinned block shows in the demo.
        let chats = all
            .iter()
            .map(|(c, _)| {
                let mut c = c.clone();
                c.favorite = match c.id.as_str() {
                    "demo-moon" => Some(1),
                    "demo-ada" => Some(2),
                    _ => None,
                };
                c
            })
            .collect();
        let mut messages: Vec<(String, Vec<Msg>)> =
            all.iter().map(|(c, m)| (c.id.clone(), m.clone())).collect();
        messages.extend(demo_channel_messages(&mut id));
        // Every other channel exists and has nothing in it yet.
        for team in demo_teams() {
            for ch in team.channels {
                if !messages.iter().any(|(known, _)| *known == ch.id) {
                    messages.push((ch.id, Vec::new()));
                }
            }
        }
        Self {
            latency: true,
            search: true,
            teams: true,
            posting: true,
            state: Mutex::new(State {
                chats,
                messages,
                next_id: 5000,
                pending_replies: Vec::new(),
                next_incoming: Instant::now() + std::time::Duration::from_secs(25),
                incoming_n: 0,
                read_marks: Vec::new(),
            }),
            pushes: Mutex::new(Vec::new()),
            own: Mutex::new(OwnStatus::default()),
        }
    }

    /// What was last chosen with `set_status`. Test-only.
    #[cfg(test)]
    pub fn chosen_status(&self) -> Option<Availability> {
        self.own
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .chosen
    }

    /// Every report of whether the person was at the keyboard, in order. Test-only.
    #[cfg(test)]
    pub fn activity_reports(&self) -> Vec<bool> {
        self.own
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reports
            .clone()
    }

    /// Queue a hint for the next [`Backend::poll_pushes`] to return. Test-only: the demo backend never produces
    /// one on its own.
    #[cfg(test)]
    pub fn queue_push(&self, hint: PushHint) {
        self.pushes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(hint);
    }

    /// The requests to mark a chat read so far, in order.
    #[cfg(test)]
    pub fn read_marks(&self) -> Vec<(String, String)> {
        self.state().read_marks.clone()
    }

    /// Deliver replies and incoming messages that have come due.
    fn tick(st: &mut State) {
        let now = Instant::now();
        let due: Vec<_> = st
            .pending_replies
            .iter()
            .filter(|r| r.0 <= now)
            .cloned()
            .collect();
        st.pending_replies.retain(|r| r.0 > now);
        for (_, chat_id, who, html) in due {
            st.next_id += 1;
            let m = msg(st.next_id, 0, who, &html);
            Self::push(st, &chat_id, m);
        }
        if now >= st.next_incoming {
            const LINES: [&str; 3] = [
                "<p>FYI: deploy finished \u{1F389} - <b>all green</b>.</p>",
                "<p>New benchmark:</p><pre><code class=\"language-rust\">let t = Instant::now();\nwork();\nprintln!(\"{:?}\", t.elapsed());</code></pre>",
                "<p>Anyone up for coffee? \u{2615}</p>",
            ];
            let html = LINES[st.incoming_n % LINES.len()];
            st.incoming_n += 1;
            st.next_id += 1;
            let m = msg(st.next_id, 0, &LINUS, html);
            Self::push(st, "demo-platform", m);
            st.next_incoming = now + std::time::Duration::from_secs(45);
        }
    }

    fn push(st: &mut State, chat_id: &str, m: Msg) {
        if let Some(c) = st.chats.iter_mut().find(|c| c.id == chat_id) {
            c.updated = m.created;
            c.preview = Some(Preview {
                created: m.created,
                from_id: m.from_id.clone(),
                from_name: Some(m.from_name.clone()),
                text: html_to_plain(&m.body),
                automated: false,
            });
        }
        if let Some((_, list)) = st.messages.iter_mut().find(|(id, _)| id == chat_id) {
            list.push(m);
        }
    }
}

impl DemoBackend {
    /// A demo whose calls answer at once (for tests).
    #[cfg(test)]
    pub fn instant() -> Self {
        Self {
            latency: false,
            ..Self::new()
        }
    }

    /// A demo whose calls answer at once and which cannot search messages (for tests).
    #[cfg(test)]
    pub fn without_search() -> Self {
        Self {
            search: false,
            ..Self::instant()
        }
    }

    /// A demo whose calls answer at once and which cannot post to channels (for tests).
    #[cfg(test)]
    pub fn without_posting() -> Self {
        Self {
            posting: false,
            ..Self::instant()
        }
    }

    /// A demo whose calls answer at once and which has no teams (for tests).
    #[cfg(test)]
    pub fn without_teams() -> Self {
        Self {
            teams: false,
            ..Self::instant()
        }
    }

    /// Matches by plain text over the messages of `ids`, best first: the earlier in the message the text is, the better,
    /// then the newest. So the order is not by date, as a real service's is not, and the answer comes five at a time so
    /// that paging is seen.
    fn search_ids(&self, ids: &[String], query: &str, from: usize) -> Result<SearchPage> {
        const PAGE: usize = 5;
        self.pause(150);
        if !self.search {
            return Err(Error::Unsupported {
                operation: "search messages",
                detail: "the demo is set not to search",
            });
        }
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Err(Error::InvalidInput {
                what: "search text",
                value: String::new(),
            });
        }
        let channels: Vec<(String, String)> = demo_teams()
            .into_iter()
            .flat_map(|t| t.channels)
            .map(|c| (c.id, c.name))
            .collect();
        let st = self.state();
        let known: Vec<(&String, &Vec<Msg>)> = ids
            .iter()
            .filter_map(|want| st.messages.iter().find(|(id, _)| id == want))
            .map(|(id, list)| (id, list))
            .collect();
        if known.is_empty() {
            return Err(Error::InvalidInput {
                what: "chat id",
                value: ids.first().cloned().unwrap_or_default(),
            });
        }
        let mut found: Vec<(usize, &String, &Msg, String)> = known
            .iter()
            .flat_map(|(id, list)| list.iter().map(move |m| (*id, m)))
            .filter(|(_, m)| !m.system && !m.deleted)
            .filter_map(|(id, m)| {
                let plain = html_to_plain(&m.body);
                let at = plain.to_lowercase().find(&query)?;
                Some((at, id, m, plain))
            })
            .collect();
        found.sort_by_key(|(at, _, m, _)| (*at, std::cmp::Reverse(m.created)));
        let total = found.len();
        let hits = found
            .into_iter()
            .skip(from)
            .take(PAGE)
            .map(|(_, id, m, plain)| SearchHit {
                message_id: m.id.clone(),
                sent: m.created,
                sender: m.from_name.clone(),
                preview: plain.chars().take(160).collect(),
                conversation: id.clone(),
                channel: channels
                    .iter()
                    .find(|(channel, _)| channel == id)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_default(),
            })
            .collect();
        Ok(SearchPage {
            hits,
            next: (from + PAGE < total).then_some(from + PAGE),
            total: Some(total as u64),
        })
    }

    /// Wait as a real service would.
    fn pause(&self, millis: u64) {
        if self.latency {
            std::thread::sleep(std::time::Duration::from_millis(millis));
        }
    }
}

impl Default for DemoBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoBackend {
    // The demo state is only ever mutated in whole steps, so it is valid even if a holder panicked.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Backend for DemoBackend {
    fn label(&self) -> String {
        "demo data".into()
    }

    fn me(&self) -> Result<Me> {
        Ok(Me {
            id: ME_ID.into(),
            name: MEPERSON.1.into(),
        })
    }

    fn chats(&self) -> Result<Vec<Chat>> {
        let mut st = self.state();
        Self::tick(&mut st);
        let mut chats = st.chats.clone();
        chats.sort_by_key(|c| std::cmp::Reverse(c.updated));
        Ok(chats)
    }

    fn find_people(&self, query: &str) -> Result<Vec<Contact>> {
        self.pause(200);
        const DIRECTORY: [(&str, &str, &str); 10] = [
            ("u-ada", "Ada Lovelace", "Analyst, Engines"),
            ("u-grace", "Grace Hopper", "Rear Admiral, Compilers"),
            ("u-linus", "Linus Torvalds", "Kernel, Maintainers"),
            (
                "u-margaret",
                "Margaret Hamilton",
                "Director, Software Engineering",
            ),
            ("u-alan", "Alan Turing", "Researcher, Computability"),
            (
                "u-katherine",
                "Katherine Johnson",
                "Mathematician, Trajectories",
            ),
            (
                "u-barbara",
                "Barbara Liskov",
                "Professor, Programming Methodology",
            ),
            ("u-edsger", "Edsger Dijkstra", "Fellow, Concurrency"),
            ("u-donald", "Donald Knuth", "Professor Emeritus, Algorithms"),
            ("u-charles", "Charles Muddy", "Engineer, Build and Release"),
        ];
        let q = query.trim().to_lowercase();
        Ok(DIRECTORY
            .iter()
            .map(|(id, name, detail)| Contact {
                id: (*id).into(),
                name: (*name).into(),
                email: format!("{}@example.com", name.to_lowercase().replace(' ', ".")),
                detail: (*detail).into(),
                phone: "+1 555 0100".into(),
                office: "Building 1".into(),
            })
            .filter(|c| {
                !q.is_empty() && (c.name.to_lowercase().contains(&q) || c.email.contains(&q))
            })
            .collect())
    }

    fn open_chat(&self, who: &Contact) -> Result<Chat> {
        let mut st = self.state();
        let id = format!("demo-dm-{}", who.id);
        if let Some(c) = st.chats.iter().find(|c| {
            c.id == id || (c.kind == ChatKind::OneOnOne && c.members.iter().any(|m| m.id == who.id))
        }) {
            return Ok(c.clone());
        }
        let chat = Chat {
            id: id.clone(),
            kind: ChatKind::OneOnOne,
            topic: None,
            members: vec![
                member(&MEPERSON),
                Member {
                    not_in_directory: false,
                    id: who.id.clone(),
                    name: who.name.clone(),
                },
            ],
            updated: Utc::now(),
            preview: None,
            last_read: None,
            web_url: None,
            favorite: None,
        };
        st.chats.push(chat.clone());
        st.messages.push((id, Vec::new()));
        Ok(chat)
    }

    fn messages(&self, chat_id: &str, older: Option<&str>) -> Result<Page> {
        self.pause(150);
        let mut st = self.state();
        Self::tick(&mut st);
        if older.is_some() {
            return Ok(Page::default());
        }
        let (_, list) = st
            .messages
            .iter()
            .find(|(id, _)| id == chat_id)
            .ok_or_else(|| Error::InvalidInput {
                what: "chat id",
                value: chat_id.to_string(),
            })?;
        Ok(Page {
            messages: list.clone(),
            older: None,
        })
    }

    fn send(
        &self,
        chat_id: &str,
        html: &str,
        _mentions: &[OutMention],
        reply_to: Option<&ReplyTarget>,
    ) -> Result<Msg> {
        self.pause(400);
        let mut st = self.state();
        st.next_id += 1;
        let content = match reply_to {
            Some(target) => format!("{}{html}", reply_quote_html(target)),
            None => html.to_string(),
        };
        let m = msg(st.next_id, 0, &MEPERSON, &content);
        Self::push(&mut st, chat_id, m.clone());
        // Someone answers a few seconds later.
        let responder: &'static Person = match chat_id {
            "demo-ada" => &ADA,
            "demo-grace" => &GRACE,
            "demo-linus" => &LINUS,
            "demo-platform" => &MARGARET,
            "demo-sprint" => &KATHERINE,
            _ => &ALAN,
        };
        st.pending_replies.push((
            Instant::now() + std::time::Duration::from_secs(4),
            chat_id.to_string(),
            responder,
            "<p>Got it, thanks! \u{1F44D}</p>".to_string(),
        ));
        Ok(m)
    }

    fn can_search_messages(&self) -> bool {
        self.search
    }

    /// Matches by plain text, best first: the earlier in the message the text is, the better, then the newest. So the
    /// order is not by date, as a real service's is not, and the answer comes five at a time so that paging is seen.
    fn search_messages(&self, chat_id: &str, query: &str, from: usize) -> Result<SearchPage> {
        self.search_ids(&[chat_id.to_string()], query, from)
    }

    fn can_search_conversations(&self) -> bool {
        self.search
    }

    /// The same search over several conversations (a team's channels): a conversation the demo does not know has
    /// nothing in it. Each hit says which conversation and channel it is in.
    fn search_conversations(
        &self,
        conversation_ids: &[String],
        query: &str,
        from: usize,
    ) -> Result<SearchPage> {
        self.search_ids(conversation_ids, query, from)
    }

    fn can_post_to_channels(&self) -> bool {
        self.posting
    }

    /// A post (with its title) or a reply (with the post it answers), appended to the channel's messages; nobody
    /// answers. A reply may name a post that is not loaded, as the demo's own do.
    fn post_to_channel(&self, channel: &str, post: &ChannelPost<'_>) -> Result<Msg> {
        self.pause(400);
        if !self.posting {
            return Err(Error::Unsupported {
                operation: "post to channel",
                detail: "the demo is set not to post",
            });
        }
        let invalid = |what: &'static str, reason: &str| Error::InvalidInput {
            what,
            value: reason.to_string(),
        };
        if post.html.trim().is_empty() {
            return Err(invalid("message", "empty"));
        }
        if post.subject.is_some() && post.thread.is_some() {
            return Err(invalid("post subject", "a reply has none"));
        }
        if !demo_teams()
            .iter()
            .flat_map(|t| &t.channels)
            .any(|c| c.id == channel)
        {
            return Err(invalid("channel id", "is not a channel's id"));
        }
        let mut st = self.state();
        st.next_id += 1;
        let mut m = msg(
            st.next_id,
            0,
            &MEPERSON,
            &read_back(post.html, post.mentions),
        );
        m.mentions = post.mentions.iter().map(|x| x.user_id.clone()).collect();
        m.subject = post
            .subject
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        m.root_id = post.thread.map(String::from);
        match st.messages.iter_mut().find(|(id, _)| id == channel) {
            Some((_, list)) => list.push(m.clone()),
            None => st.messages.push((channel.to_string(), vec![m.clone()])),
        }
        Ok(m)
    }

    fn can_edit_posts(&self) -> bool {
        self.posting
    }

    /// Like the service: the post's title becomes what is given, so an edit that does not repeat it loses it.
    fn edit_post(
        &self,
        channel: &str,
        message_id: &str,
        subject: Option<&str>,
        html: &str,
        mentions: &[OutMention],
    ) -> Result<()> {
        self.pause(150);
        if !self.posting {
            return Err(Error::Unsupported {
                operation: "edit post",
                detail: "the demo is set not to post",
            });
        }
        let mut st = self.state();
        let bad = |what: &'static str, value: &str| Error::InvalidInput {
            what,
            value: value.to_string(),
        };
        let (_, list) = st
            .messages
            .iter_mut()
            .find(|(id, _)| id == channel)
            .ok_or_else(|| bad("channel id", channel))?;
        let m = list
            .iter_mut()
            .find(|m| m.id == message_id)
            .ok_or_else(|| bad("message id", message_id))?;
        let now = Utc::now();
        m.body = read_back(html, mentions);
        m.mentions = mentions.iter().map(|x| x.user_id.clone()).collect();
        m.subject = subject.map(String::from);
        m.edited = Some(now);
        m.modified = now;
        Ok(())
    }

    fn can_favorite(&self) -> bool {
        true
    }

    fn set_favorite(&self, chat_id: &str, on: bool) -> Result<()> {
        self.pause(100);
        let mut st = self.state();
        let next = st
            .chats
            .iter()
            .filter_map(|c| c.favorite)
            .max()
            .unwrap_or(0)
            + 1;
        let chat = st
            .chats
            .iter_mut()
            .find(|c| c.id == chat_id)
            .ok_or_else(|| Error::InvalidInput {
                what: "chat id",
                value: chat_id.to_string(),
            })?;
        chat.favorite = match (on, chat.favorite) {
            (true, None) => Some(next),
            (true, kept) => kept,
            (false, _) => None,
        };
        Ok(())
    }

    fn can_edit(&self) -> bool {
        true
    }

    fn edit(
        &self,
        chat_id: &str,
        message_id: &str,
        html: &str,
        _mentions: &[OutMention],
    ) -> Result<()> {
        self.pause(150);
        let mut st = self.state();
        let bad = |what: &'static str, value: &str| Error::InvalidInput {
            what,
            value: value.to_string(),
        };
        let (_, list) = st
            .messages
            .iter_mut()
            .find(|(id, _)| id == chat_id)
            .ok_or_else(|| bad("chat id", chat_id))?;
        let m = list
            .iter_mut()
            .find(|m| m.id == message_id)
            .ok_or_else(|| bad("message id", message_id))?;
        let now = Utc::now();
        m.body = html.to_string();
        m.edited = Some(now);
        m.modified = now;
        Ok(())
    }

    fn reaction_emoji(&self) -> Result<Vec<EmojiInfo>> {
        Ok(reaction_set())
    }

    fn can_presence(&self) -> bool {
        true
    }

    fn can_set_status(&self) -> bool {
        true
    }

    fn set_status(&self, status: Option<Availability>) -> Result<()> {
        self.pause(60);
        if status.is_some_and(|a| !a.is_choosable()) {
            return Err(Error::InvalidInput {
                what: "availability a person can choose",
                value: "that".to_string(),
            });
        }
        self.own
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .chosen = status;
        Ok(())
    }

    fn report_activity(&self, active: bool) -> Result<()> {
        self.pause(60);
        self.own
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reports
            .push(active);
        Ok(())
    }

    /// A fixed availability for each of the demo people, one of every look; anyone else is not said anything about.
    fn presence(&self, ids: &[String]) -> Result<std::collections::HashMap<String, Presence>> {
        self.pause(60);
        let known = [
            (ADA.0, Availability::Available, false),
            (GRACE.0, Availability::Busy, false),
            (LINUS.0, Availability::DoNotDisturb, false),
            (MARGARET.0, Availability::BeRightBack, false),
            (ALAN.0, Availability::Offline, true),
            (KATHERINE.0, Availability::Away, true),
            (CHARLES.0, Availability::Busy, false),
        ];
        // You: what you chose, else what your last report says (at the keyboard: available, otherwise away).
        let own = {
            let own = self.own.lock().unwrap_or_else(PoisonError::into_inner);
            own.chosen.or(match own.reports.last() {
                Some(true) => Some(Availability::Available),
                Some(false) => Some(Availability::Away),
                None => None,
            })
        };
        Ok(ids
            .iter()
            .filter_map(|id| {
                if id == ME_ID {
                    return own.map(|a| (id.clone(), Presence::new(a, false)));
                }
                let (_, a, ooo) = known.iter().find(|(who, ..)| who == id)?;
                let mut presence = Presence::new(*a, *ooo);
                // A status message with an expiry (with markup, as the service sends it), an out-of-office reply with a
                // message and an expiry, and one with neither (Alan).
                let later = |hours| Some(Utc::now() + chrono::Duration::hours(hours));
                if id == ADA.0 {
                    presence = presence.with_note(
                        StatusNote::new("Working on the <b>Analytical Engine</b>, ping me here")
                            .expect("a note")
                            .expiring(later(6)),
                    );
                }
                if id == KATHERINE.0 {
                    presence = presence.with_out_of_office_note(
                        StatusNote::new("Back on Monday; trajectories are being checked.")
                            .expect("a note")
                            .expiring(later(72)),
                    );
                }
                Some((id.clone(), presence))
            })
            .collect())
    }

    fn marks_read(&self) -> bool {
        true
    }

    fn mark_read(&self, chat_id: &str, message_id: &str, _arrived: DateTime<Utc>) -> Result<()> {
        self.pause(50);
        let mut st = self.state();
        st.read_marks
            .push((chat_id.to_string(), message_id.to_string()));
        Ok(())
    }

    fn react(&self, chat_id: &str, message_id: &str, emoji: &str, on: bool) -> Result<()> {
        self.pause(150);
        if !reaction_set().iter().any(|i| same_emoji(&i.emoji, emoji)) {
            return Err(Error::InvalidInput {
                what: "emoji the demo has a reaction for",
                value: emoji.to_string(),
            });
        }
        let mut st = self.state();
        let bad = |what: &'static str, value: &str| Error::InvalidInput {
            what,
            value: value.to_string(),
        };
        let (_, list) = st
            .messages
            .iter_mut()
            .find(|(id, _)| id == chat_id)
            .ok_or_else(|| bad("chat id", chat_id))?;
        let m = list
            .iter_mut()
            .find(|m| m.id == message_id)
            .ok_or_else(|| bad("message id", message_id))?;
        let at = m.reactions.iter().position(|r| same_emoji(&r.emoji, emoji));
        match (at, on) {
            (Some(i), true) => {
                let r = &mut m.reactions[i];
                if !r.users.iter().any(|u| u == ME_ID) {
                    r.users.push(ME_ID.into());
                    r.count += 1;
                }
            }
            (None, true) => m.reactions.push(Reaction {
                emoji: emoji.into(),
                count: 1,
                users: vec![ME_ID.into()],
                names: vec!["You".into()],
            }),
            (Some(i), false) => {
                let r = &mut m.reactions[i];
                if let Some(pos) = r.users.iter().position(|u| u == ME_ID) {
                    r.users.remove(pos);
                    if pos < r.names.len() {
                        r.names.remove(pos);
                    }
                    r.count = r.count.saturating_sub(1);
                }
                if r.count == 0 {
                    m.reactions.remove(i);
                }
            }
            (None, false) => {}
        }
        m.modified = Utc::now();
        Ok(())
    }

    fn can_list_teams(&self) -> bool {
        self.teams
    }

    fn teams(&self) -> Result<Vec<Team>> {
        self.pause(300);
        if !self.teams {
            return Err(Error::Unsupported {
                operation: "list teams",
                detail: "the demo is set to have no teams",
            });
        }
        let st = self.state();
        let mut teams = demo_teams();
        for ch in teams.iter_mut().flat_map(|t| t.channels.iter_mut()) {
            if let Some((_, list)) = st.messages.iter().find(|(id, _)| *id == ch.id) {
                ch.last = last_post(list);
            }
        }
        Ok(teams)
    }

    /// What the conversation list would say about the two channels that have anything in them: the releases channel is
    /// unread until it has been marked read.
    fn channel_activity(&self) -> Vec<ChannelActivity> {
        if !self.teams {
            return Vec::new();
        }
        let st = self.state();
        [ENG, RELEASES]
            .iter()
            .filter_map(|id| {
                let (_, list) = st.messages.iter().find(|(known, _)| known == id)?;
                let unread = *id == RELEASES && !st.read_marks.iter().any(|(chat, _)| chat == id);
                Some(ChannelActivity {
                    id: (*id).to_string(),
                    unread,
                    last: Some(last_post(list)?),
                })
            })
            .collect()
    }

    fn serves_image(&self, url: &str) -> bool {
        url.starts_with("https://demo.invalid/")
    }

    fn image(&self, url: &str) -> Result<RgbImage> {
        self.pause(500);
        let seed = url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse::<f32>().ok())
            .ok_or_else(|| Error::InvalidInput {
                what: "demo image url",
                value: url.to_string(),
            })?;
        Ok(plasma(480, 270, seed * 1.7))
    }

    fn poll_pushes(&self) -> Vec<PushHint> {
        std::mem::take(&mut *self.pushes.lock().unwrap_or_else(PoisonError::into_inner))
    }
}
