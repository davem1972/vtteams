//! Searching the messages of the open chat: the popup (`Overlay::Search`), what it lists, the request to the service,
//! going to a hit, and stepping through hits afterwards.
//!
//! While someone types, the list is the loaded messages that contain the text, at once and with nothing sent anywhere.
//! Enter asks the service (when the backend can) and the list then shows its hits, in its order, followed by loaded
//! messages the service did not give. Enter again opens the selected one. A hit older than what is loaded is reached by
//! fetching older pages (`ChatState::seek`, driven by [`App::drive_seek`]).
//!
//! In a channel whose team is known, when the backend can search several conversations at once, Tab switches the popup
//! between this channel and every channel of its team ([`SearchScope`]). A switch starts the list over (what was asked
//! of the other scope is not wanted any more, and an answer that arrives for it later is dropped); the loaded messages
//! typed against are always the open channel's. A hit in another channel is opened like a channel of the teams list
//! (`App::open_channel`) and then sought in like any other.

use super::chat::{Fetch, MAX_SEEK_PAGES, Seek};
use super::input::{ctrl_shortcut, types_text};
use super::{App, Ev, Focus, Overlay, describe_side};
use chrono::{DateTime, Duration, Local, Utc};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use std::collections::HashSet;
use std::ops::Range;
use std::time::Instant;
use teams_client::{
    Error, MAX_SEARCH_CONVERSATIONS, Msg, SearchHit, SearchPage, clean, html_to_plain, id_tag,
};

/// What a search covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchScope {
    /// The open conversation.
    #[default]
    Chat,
    /// Every channel of the open channel's team.
    Team,
}

/// The team of the open channel, as the popup knew it when it opened (the backend can search several conversations and
/// the teams list has the channel): what the title, the rows and the request need.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TeamScope {
    team: String,
    channel: String,
    /// The team's channels, the open one first and then the others in the list's order (the ones past the most a
    /// search covers are left out by the backend, so the open one must not be among them).
    channels: Vec<(String, String)>,
}

/// Longest text that can be searched for.
pub(super) const MAX_SEARCH_QUERY: usize = 200;
/// Most rows the popup keeps.
pub(super) const MAX_SEARCH_ROWS: usize = 500;
/// Longest preview or sender kept from a row's text, whatever the service sends.
const MAX_PREVIEW_CHARS: usize = 400;
const MAX_SENDER_CHARS: usize = 60;
/// How far before the first match a preview of a loaded message starts, so the match is not at the very edge.
const EXCERPT_LEAD: usize = 24;
/// A hit's time may be rounded by the service; a message this much older than the oldest one loaded has not been
/// passed yet, as far as the seek can tell.
const SEEK_SLACK_SECONDS: i64 = 2;
/// Most list rows the popup shows at once.
const MAX_LIST_ROWS: usize = 12;

/// How many rows of results fit in the popup on a terminal `height` rows high (the popup keeps its borders, the query,
/// the status and the hint, and a margin).
#[must_use]
pub fn search_list_rows(height: u16) -> usize {
    usize::from(height)
        .saturating_sub(7)
        .clamp(1, MAX_LIST_ROWS)
}

/// One line of text that came from the network: nothing a terminal would act on (no controls, no bidi marks, no style
/// markers), whitespace collapsed to single spaces.
pub(super) fn one_line(text: &str) -> String {
    crate::inline_style::strip(&clean(text))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`one_line`], cut to at most `max` characters.
fn short_line(text: &str, max: usize) -> String {
    one_line(text).chars().take(max).collect()
}

/// The character ranges of `text` where `query` occurs, ignoring case, in order and not overlapping. Both are compared
/// as lower-case characters, so a letter whose lower case is longer or shorter than itself still maps back to the
/// character it came from.
#[must_use]
pub fn find_matches(text: &str, query: &str) -> Vec<Range<usize>> {
    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut hay: Vec<char> = Vec::new();
    let mut origin: Vec<usize> = Vec::new();
    for (i, c) in text.chars().enumerate() {
        for lower in c.to_lowercase() {
            hay.push(lower);
            origin.push(i);
        }
    }
    let mut found: Vec<Range<usize>> = Vec::new();
    let mut at = 0;
    while at + needle.len() <= hay.len() {
        if hay[at..at + needle.len()] == needle[..] {
            let (start, end) = (origin[at], origin[at + needle.len() - 1] + 1);
            // One character whose lower case is several can match twice over itself.
            if found.last().is_none_or(|last| last.end <= start) {
                found.push(start..end);
            }
            at += needle.len();
        } else {
            at += 1;
        }
    }
    found
}

/// A row of the popup: a message, found by the service or only among the loaded ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchRow {
    pub message_id: String,
    /// The conversation the message is in: the open chat, or in a team search the channel it was found in.
    pub conversation: String,
    /// That channel's name, when it is one (empty for a chat).
    pub channel: String,
    pub sent: DateTime<Utc>,
    pub sender: String,
    /// One line of plain text.
    pub preview: String,
    /// Whether the service gave it (the others were found only among the loaded messages).
    pub from_server: bool,
}

/// The plain text of a message, on one line.
fn plain_text(m: &Msg) -> String {
    if m.html {
        one_line(&html_to_plain(&m.body))
    } else {
        one_line(&m.body)
    }
}

/// A piece of `plain` for a row: from a little before `first_match` (or the start), cut at a length a row can use.
fn excerpt(plain: &str, first_match: Option<usize>) -> String {
    const MAX: usize = 240;
    let start = first_match.map_or(0, |s| s.saturating_sub(EXCERPT_LEAD));
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let rest: Vec<char> = plain.chars().skip(start).collect();
    out.extend(rest.iter().take(MAX));
    if rest.len() > MAX {
        out.push('…');
    }
    out
}

/// The loaded messages that contain `query` (ignoring case), newest first, at most [`MAX_SEARCH_ROWS`]. Events and
/// deleted messages are not searched.
pub(super) fn local_rows(msgs: &[Msg], query: &str) -> Vec<SearchRow> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut rows: Vec<SearchRow> = msgs
        .iter()
        .filter(|m| !m.system && !m.deleted)
        .filter_map(|m| {
            let plain = plain_text(m);
            let first = find_matches(&plain, query).first()?.start;
            Some(SearchRow {
                message_id: m.id.clone(),
                // The popup says which conversation these are (`Search::rebuild`).
                conversation: String::new(),
                channel: String::new(),
                sent: m.created,
                sender: short_line(&m.from_name, MAX_SENDER_CHARS),
                preview: excerpt(&plain, Some(first)),
                from_server: false,
            })
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.sent));
    rows.truncate(MAX_SEARCH_ROWS);
    rows
}

/// The service's rows in its order, then the loaded matches it did not give, newest first; no message twice (a message
/// id is an id within its conversation, so two channels can have one of the same number).
pub(super) fn combine(server: &[SearchRow], local: Vec<SearchRow>) -> Vec<SearchRow> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut rows: Vec<SearchRow> = Vec::new();
    for row in server.iter().cloned().chain(local) {
        if seen.insert((row.conversation.clone(), row.message_id.clone())) {
            rows.push(row);
        }
    }
    rows.truncate(MAX_SEARCH_ROWS);
    rows
}

/// A request to the service that has not been answered.
struct Pending {
    seq: u64,
    started: Instant,
    /// A following page, not the first.
    more: bool,
}

/// One hit to step to: which conversation it is in, its id there, and when it was sent.
#[derive(Clone)]
struct StepHit {
    conversation: String,
    message_id: String,
    sent: DateTime<Utc>,
}

/// The hits of the last list a hit was opened from, for `n` / `N` / F3.
struct Stepping {
    /// The conversation open now (the one the hit at `pos` is in): stepping is only offered while it is the open one.
    chat: String,
    hits: Vec<StepHit>,
    pos: usize,
}

/// What the popup holds. It is separate from `Overlay::Search` so that keys do not clone the rows.
#[derive(Default)]
pub(super) struct Search {
    /// The chat the popup was opened for.
    chat: Option<String>,
    /// What the list covers: this chat, or (only when `team` is known) its whole team.
    scope: SearchScope,
    /// The open channel's team, when this popup can search all of it. `None` is no toggle.
    team: Option<TeamScope>,
    /// The backend said it cannot search several conversations: no team search any more this session.
    team_unsupported: bool,
    query: String,
    sel: usize,
    /// Numbers the requests; an answer is wanted only if it is the one `pending` is waiting for.
    seq: u64,
    pending: Option<Pending>,
    /// The service's rows in its order, for `answered`.
    server_rows: Vec<SearchRow>,
    /// The (trimmed) text `server_rows` answer.
    answered: Option<String>,
    next: Option<usize>,
    total: Option<u64>,
    /// The text the service was asked about and refused or failed on, and why.
    failed: Option<(String, String)>,
    /// The service said it cannot search: not asked again this session.
    unsupported: bool,
    /// What the popup lists now.
    rows: Vec<SearchRow>,
    /// The version of the chat's messages `rows` were made from.
    rows_version: u64,
    stepping: Option<Stepping>,
}

impl Search {
    fn needle(&self) -> &str {
        self.query.trim()
    }

    /// A fresh popup for `chat`. What stays is what outlives a popup: the hits to step through and the knowledge that
    /// the service cannot search.
    fn begin(&mut self, chat: String, query: String, team: Option<TeamScope>) {
        *self = Search {
            chat: Some(chat),
            team: team.filter(|_| !self.team_unsupported),
            query,
            unsupported: self.unsupported,
            team_unsupported: self.team_unsupported,
            // Never started over: an answer to a request of an earlier popup must not pass for this one's.
            seq: self.seq,
            stepping: self.stepping.take(),
            ..Search::default()
        };
    }

    /// The popup is closed (or the chat left): nothing it asked for is wanted any more.
    fn close(&mut self) {
        self.pending = None;
    }

    /// The chat is left: everything about it goes.
    fn leave(&mut self) {
        let (unsupported, team_unsupported, seq) =
            (self.unsupported, self.team_unsupported, self.seq);
        *self = Search {
            unsupported,
            team_unsupported,
            seq,
            ..Search::default()
        };
    }

    /// Whether the popup can be switched between the channel and its team.
    fn can_toggle(&self) -> bool {
        self.team.is_some()
    }

    /// The scope's wording of "what ⏎ searches": the whole chat, the whole channel (when there is a team to widen to) or
    /// all of the team.
    fn whole(&self) -> String {
        match (&self.team, self.scope) {
            (Some(t), SearchScope::Team) => format!("all of {}", t.team),
            (Some(_), SearchScope::Chat) => "the whole channel".to_string(),
            (None, _) => "the whole chat".to_string(),
        }
    }

    /// What the loaded messages are called: in a team search they are the open channel's alone.
    fn loaded_messages(&self) -> &'static str {
        if self.scope == SearchScope::Team {
            "this channel's loaded messages"
        } else {
            "the loaded messages"
        }
    }

    /// Start the list over for the other scope: nothing asked of the old one is wanted, nothing it answered is shown.
    fn switch_scope(&mut self) {
        if self.team.is_none() {
            return;
        }
        self.scope = match self.scope {
            SearchScope::Chat => SearchScope::Team,
            SearchScope::Team => SearchScope::Chat,
        };
        self.pending = None;
        self.failed = None;
        self.server_rows.clear();
        self.answered = None;
        self.next = None;
        self.total = None;
        self.sel = 0;
    }

    /// The conversation a hit is in, as far as this popup can use it, in the app's own spelling of its id. A chat
    /// search takes a hit that names the chat or none; a team search only one in a channel of the team (a hit naming
    /// anything else, or nothing, cannot be opened and is not listed).
    fn hit_conversation(&self, hit: &SearchHit) -> Option<String> {
        let chat = self.chat.as_deref()?;
        match self.scope {
            SearchScope::Chat => (hit.conversation.is_empty()
                || hit.conversation.eq_ignore_ascii_case(chat))
            .then(|| chat.to_string()),
            SearchScope::Team => self
                .team
                .as_ref()?
                .channels
                .iter()
                .find(|(id, _)| id.eq_ignore_ascii_case(&hit.conversation))
                .map(|(id, _)| id.clone()),
        }
    }

    /// The name of channel `id` as the teams list has it.
    fn channel_name(&self, id: &str) -> String {
        self.team
            .as_ref()
            .and_then(|t| t.channels.iter().find(|(c, _)| c == id))
            .map(|(_, name)| name.clone())
            .unwrap_or_default()
    }

    pub(super) fn is_busy(&self) -> bool {
        self.pending.is_some()
    }

    /// The text changed: what was asked about the old text is not wanted, and the selection starts over.
    fn changed(&mut self) {
        self.pending = None;
        self.failed = None;
        self.sel = 0;
    }

    fn rebuild(&mut self, msgs: &[Msg], version: u64) {
        let keep = self
            .rows
            .get(self.sel)
            .map(|r| (r.conversation.clone(), r.message_id.clone()));
        let query = self.needle();
        let mut local = local_rows(msgs, query);
        // The loaded messages are the open conversation's.
        let chat = self.chat.clone().unwrap_or_default();
        let channel = self.team.as_ref().map(|t| t.channel.clone());
        for row in &mut local {
            row.conversation.clone_from(&chat);
            row.channel = channel.clone().unwrap_or_default();
        }
        let served = self.answered.as_deref() == Some(query);
        let server: &[SearchRow] = if served { &self.server_rows } else { &[] };
        let rows = combine(server, local);
        self.sel = keep
            .and_then(|(conversation, id)| {
                rows.iter()
                    .position(|r| r.message_id == id && r.conversation == conversation)
            })
            .unwrap_or(self.sel)
            .min(rows.len().saturating_sub(1));
        self.rows = rows;
        self.rows_version = version;
    }

    /// Whether the service's answer for the text now in the box is in, so that Enter opens a row instead of asking.
    fn is_current(&self, can_server: bool) -> bool {
        let query = self.needle();
        !can_server
            || self.answered.as_deref() == Some(query)
            || self.failed.as_ref().is_some_and(|(q, _)| q == query)
    }

    /// Whether moving down past the last row would fetch another page.
    fn more_to_fetch(&self, can_server: bool) -> bool {
        can_server
            && self.pending.is_none()
            && self.next.is_some()
            && self.answered.as_deref() == Some(self.needle())
            && self.server_rows.len() < MAX_SEARCH_ROWS
    }

    /// Take a page of the service's answer for `query`; `from` 0 starts the list again.
    fn receive(&mut self, query: &str, from: usize, page: &SearchPage) -> usize {
        if from == 0 {
            self.server_rows.clear();
        }
        let before = self.server_rows.len();
        let mut seen: HashSet<(String, String)> = self
            .server_rows
            .iter()
            .map(|r| (r.conversation.clone(), r.message_id.clone()))
            .collect();
        for hit in &page.hits {
            let Some(conversation) = self.hit_conversation(hit) else {
                continue;
            };
            if hit.message_id.is_empty()
                || self.server_rows.len() >= MAX_SEARCH_ROWS
                || !seen.insert((conversation.clone(), hit.message_id.clone()))
            {
                continue;
            }
            // The service's name for the channel, else the one the teams list has; none for a chat.
            let channel = match short_line(&hit.channel, MAX_SENDER_CHARS) {
                named if !named.is_empty() => named,
                _ => short_line(&self.channel_name(&conversation), MAX_SENDER_CHARS),
            };
            self.server_rows.push(SearchRow {
                message_id: hit.message_id.clone(),
                conversation,
                channel,
                sent: hit.sent,
                sender: short_line(&hit.sender, MAX_SENDER_CHARS),
                preview: short_line(&hit.preview, MAX_PREVIEW_CHARS),
                from_server: true,
            });
        }
        self.answered = Some(query.to_string());
        self.failed = None;
        self.total = page.total;
        // `next` is the service's own offset, passed back unchanged; one that does not move forward ends the paging
        // (a page can hold no new rows, when the library dropped hits of other chats, and still have more behind it).
        self.next = page.next.filter(|&n| n > from);
        before
    }

    /// The words under the query line: what the list is and what Enter does.
    fn status(&self, can_server: bool, loaded: usize, spinner: &str) -> (String, bool) {
        let query = self.needle();
        let served = self.answered.as_deref() == Some(query);
        let server_count = self.rows.iter().filter(|r| r.from_server).count();
        let only_local = self.rows.len() - server_count;
        if let Some(p) = &self.pending {
            let what = if p.more {
                "loading more…"
            } else {
                "searching…"
            };
            return (format!("{spinner} {what}"), false);
        }
        if let Some((q, why)) = &self.failed
            && q == query
        {
            return (
                format!(
                    "search failed: {why}; showing the {} loaded message(s) that match",
                    self.rows.len()
                ),
                true,
            );
        }
        let local = self.loaded_messages();
        let whole = self.whole();
        if query.is_empty() {
            return (
                if can_server {
                    format!("type to search {local}; ⏎ searches {whole}")
                } else {
                    format!("type to search the {loaded} loaded messages")
                },
                false,
            );
        }
        if served && can_server {
            let about = self
                .total
                .map_or_else(String::new, |t| format!(" (about {t})"));
            let more = if self.next.is_some() {
                "; ↓ at the end loads more"
            } else {
                ""
            };
            // A team of more channels than a search covers is searched in part, which is said.
            let covered = match (&self.team, self.scope) {
                (Some(t), SearchScope::Team) if t.channels.len() > MAX_SEARCH_CONVERSATIONS => {
                    format!(
                        "; only {MAX_SEARCH_CONVERSATIONS} of its {} channels are searched",
                        t.channels.len()
                    )
                }
                _ => String::new(),
            };
            let line = if server_count == 0 && only_local == 0 {
                format!("nothing found on the server or in {local}{covered}")
            } else {
                let only = if self.scope == SearchScope::Team {
                    local
                } else {
                    "loaded messages"
                };
                format!(
                    "{server_count} on the server{about} + {only_local} only in {only}{more}{covered}"
                )
            };
            return (line, false);
        }
        if !can_server {
            return (
                format!("searched the {loaded} loaded messages; older ones are not searched"),
                false,
            );
        }
        let matching = if self.rows.is_empty() {
            "none".to_string()
        } else {
            self.rows.len().to_string()
        };
        (format!("{matching} in {local}; ⏎ searches {whole}"), false)
    }
}

/// What drawing the popup needs.
pub struct SearchView<'a> {
    pub title: String,
    pub query: &'a str,
    pub rows: &'a [SearchRow],
    pub sel: usize,
    pub status: String,
    pub status_is_error: bool,
    /// What Enter does now: "search the whole chat", "search all of Team" or "open".
    pub enter: String,
    /// What Tab does, when the popup can be switched between a channel and its team.
    pub toggle: Option<String>,
    /// The rows say which channel they are in (a team search).
    pub show_channel: bool,
}

impl App {
    /// Whether the service can search what the popup covers now (the backend can, and has not said otherwise).
    fn search_on_server(&self) -> bool {
        match self.search.scope {
            SearchScope::Chat => self.backend.can_search_messages() && !self.search.unsupported,
            SearchScope::Team => {
                self.backend.can_search_conversations() && !self.search.team_unsupported
            }
        }
    }

    /// The open channel's team as a search could cover it: `None` for a chat, for a channel the teams list does not have
    /// (so its team is not known), and when the backend cannot search several conversations (or has said it cannot).
    fn team_scope_for(&self, chat: &str) -> Option<TeamScope> {
        if self.search.team_unsupported || !self.backend.can_search_conversations() {
            return None;
        }
        let open = self.current_channel().filter(|o| o.id == chat)?;
        let (team, channel) = self.teams.channel(&open.id)?;
        let mut channels = vec![(
            channel.id.clone(),
            short_line(&channel.name, MAX_SENDER_CHARS),
        )];
        channels.extend(
            team.channels
                .iter()
                .filter(|c| c.id != channel.id)
                .map(|c| (c.id.clone(), short_line(&c.name, MAX_SENDER_CHARS))),
        );
        Some(TeamScope {
            team: short_line(&team.name, MAX_SENDER_CHARS),
            channel: short_line(&channel.name, MAX_SENDER_CHARS),
            channels,
        })
    }

    /// What the popup shows, if it is open.
    #[must_use]
    pub fn search_view(&self) -> Option<SearchView<'_>> {
        if self.overlay != Overlay::Search {
            return None;
        }
        let can_server = self.search_on_server();
        let loaded = self
            .search
            .chat
            .as_ref()
            .and_then(|c| self.states.get(c))
            .map_or(0, |st| st.messages().iter().filter(|m| !m.system).count());
        let spinner = crate::theme::SPINNER[(self.tick / 2) % crate::theme::SPINNER.len()];
        let (status, status_is_error) = self.search.status(can_server, loaded, spinner);
        let team = self.search.team.as_ref();
        let in_team = self.search.scope == SearchScope::Team;
        Some(SearchView {
            title: match team {
                Some(t) if in_team => format!("Search all of {}", t.team),
                Some(t) => format!("Search {} › {}", t.team, t.channel),
                None => "Search this chat".to_string(),
            },
            query: &self.search.query,
            rows: &self.search.rows,
            sel: self.search.sel,
            status,
            status_is_error,
            enter: if self.search.is_current(can_server) {
                "open".to_string()
            } else {
                format!("search {}", self.search.whole())
            },
            toggle: team.map(|t| {
                if in_team {
                    "tab: only this channel".to_string()
                } else {
                    format!("tab: all of {}", t.team)
                }
            }),
            show_channel: in_team,
        })
    }

    /// How many of the service's hits the popup is holding, whatever the text in the box now.
    #[cfg(test)]
    pub(super) fn search_server_rows_held(&self) -> usize {
        self.search.server_rows.len()
    }

    /// Whether the service has been asked to search and has not answered yet.
    #[must_use]
    pub fn search_pending(&self) -> bool {
        self.search.is_busy()
    }

    /// Whether a hit has been opened and `n` / `N` / F3 have somewhere to go.
    #[must_use]
    pub fn has_search_hits(&self) -> bool {
        self.search
            .stepping
            .as_ref()
            .is_some_and(|s| Some(&s.chat) == self.current.as_ref() && !s.hits.is_empty())
    }

    /// Switch the open popup between this channel and its team (Tab). Nothing happens where there is no team to widen to.
    pub(super) fn toggle_search_scope(&mut self) {
        if !self.search.can_toggle() {
            return;
        }
        self.search.switch_scope();
        self.rebuild_search_rows();
        self.search.sel = 0;
    }

    /// Open the popup for the open chat, with `text` in the box (and, if there is some, asked of the service).
    pub fn open_search(&mut self, text: Option<&str>) {
        let Some(chat) = self.current.clone() else {
            self.set_status("open a chat to search its messages", true);
            return;
        };
        if self.overlay != Overlay::None {
            return;
        }
        let query: String = text
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_SEARCH_QUERY)
            .collect();
        let team = self.team_scope_for(&chat);
        self.search.begin(chat, query, team);
        self.overlay = Overlay::Search;
        self.rebuild_search_rows();
        if !self.search.needle().is_empty() && self.search_on_server() {
            self.start_search_request(0);
        }
    }

    fn rebuild_search_rows(&mut self) {
        let Some(chat) = self.search.chat.clone() else {
            return;
        };
        let (msgs, version) = match self.states.get(&chat) {
            Some(st) => (st.messages(), st.version()),
            None => (&[][..], 0),
        };
        self.search.rebuild(msgs, version);
    }

    /// The open chat's messages changed while the popup is open (a poll): list what matches now.
    pub(super) fn keep_search_rows_current(&mut self) {
        if self.overlay != Overlay::Search {
            return;
        }
        let version = self
            .search
            .chat
            .as_ref()
            .and_then(|c| self.states.get(c))
            .map_or(0, |st| st.version());
        if version != self.search.rows_version {
            self.rebuild_search_rows();
        }
    }

    fn close_search(&mut self) {
        self.search.close();
        if self.overlay == Overlay::Search {
            self.overlay = Overlay::None;
        }
    }

    /// The chat is being left: close the popup, forget what was found and stop looking for a message in it.
    pub(super) fn leave_chat_search(&mut self) {
        if let Some(st) = self.current.as_ref().and_then(|c| self.states.get_mut(c)) {
            st.seek = None;
        }
        self.search.leave();
        if self.overlay == Overlay::Search {
            self.overlay = Overlay::None;
        }
    }

    /// Text pasted into the popup goes to its query line.
    pub(super) fn search_paste(&mut self, text: &str) {
        let before = self.search.query.clone();
        for c in text.chars().filter(|c| !c.is_control()) {
            if self.search.query.chars().count() < MAX_SEARCH_QUERY {
                self.search.query.push(c);
            }
        }
        if self.search.query != before {
            self.search_text_changed();
        }
    }

    fn search_text_changed(&mut self) {
        self.search.changed();
        self.rebuild_search_rows();
        // Moving to the top of a new list is the selection starting over, not the old row following the text.
        self.search.sel = 0;
    }

    pub(super) fn on_search_key(&mut self, key: KeyEvent) {
        let page = search_list_rows(self.view.geometry.panel.bottom()) as isize;
        match key.code {
            KeyCode::Esc => self.close_search(),
            KeyCode::Enter => self.search_enter(),
            KeyCode::Tab | KeyCode::BackTab => self.toggle_search_scope(),
            KeyCode::Up => self.move_search_selection(-1),
            KeyCode::Down => self.move_search_selection(1),
            KeyCode::PageUp => self.move_search_selection(-page),
            KeyCode::PageDown => self.move_search_selection(page),
            KeyCode::Home => self.search.sel = 0,
            KeyCode::End => self.search.sel = self.search.rows.len().saturating_sub(1),
            KeyCode::Backspace => {
                if self.search.query.pop().is_some() {
                    self.search_text_changed();
                }
            }
            KeyCode::Char('u') if ctrl_shortcut(key.modifiers) => {
                if !self.search.query.is_empty() {
                    self.search.query.clear();
                    self.search_text_changed();
                }
            }
            KeyCode::Char(c)
                if types_text(key.modifiers)
                    && !c.is_control()
                    && self.search.query.chars().count() < MAX_SEARCH_QUERY =>
            {
                self.search.query.push(c);
                self.search_text_changed();
            }
            _ => {}
        }
    }

    /// Move the selection; going down past the last row asks for the next page when the service has one.
    fn move_search_selection(&mut self, delta: isize) {
        let rows = self.search.rows.len();
        if rows == 0 {
            return;
        }
        let last = rows - 1;
        let wanted = self.search.sel as isize + delta;
        if delta > 0 && wanted > last as isize && self.search.more_to_fetch(self.search_on_server())
        {
            self.search.sel = last;
            if let Some(from) = self.search.next {
                self.start_search_request(from);
            }
            return;
        }
        self.search.sel = wanted.clamp(0, last as isize) as usize;
    }

    /// Enter: ask the service about text it has not been asked about, otherwise open the selected row.
    fn search_enter(&mut self) {
        if self.search.needle().is_empty() || self.search.pending.is_some() {
            return;
        }
        if self.search.is_current(self.search_on_server()) {
            self.open_selected_hit();
        } else {
            self.start_search_request(0);
        }
    }

    /// Send the query to the service: the first page (`from` 0, which also forgets what was stepped through) or the next.
    fn start_search_request(&mut self, from: usize) {
        let (Some(chat), query) = (self.search.chat.clone(), self.search.needle().to_string())
        else {
            return;
        };
        if !self.sign_in.is_online() {
            self.search.failed = Some((
                query,
                "the sign-in has expired: press F6 to sign in again".into(),
            ));
            return;
        }
        if from == 0 {
            // A new search replaces the hits to step through, and any hunt for an earlier one.
            self.search.stepping = None;
            if let Some(st) = self.states.get_mut(&chat) {
                st.seek = None;
            }
        }
        self.search.failed = None;
        self.search.seq += 1;
        let seq = self.search.seq;
        self.search.pending = Some(Pending {
            seq,
            started: Instant::now(),
            more: from > 0,
        });
        let scope = self.search.scope;
        log::debug!(
            "chat {}: search requested ({}, from {from}, {} character(s))",
            id_tag(&chat),
            match scope {
                SearchScope::Chat => "this chat".to_string(),
                SearchScope::Team => format!(
                    "{} channel(s) of its team",
                    self.search.team.as_ref().map_or(0, |t| t.channels.len())
                ),
            },
            query.chars().count()
        );
        let asked = query.clone();
        match scope {
            SearchScope::Chat => {
                let asked_chat = chat.clone();
                self.spawn_action(
                    move |b| b.search_messages(&asked_chat, &asked, from),
                    move |result| Ev::Searched {
                        chat,
                        scope,
                        query,
                        from,
                        seq,
                        result,
                    },
                );
            }
            SearchScope::Team => {
                // The open channel first: a team with more channels than a search covers still searches this one.
                let ids: Vec<String> = self
                    .search
                    .team
                    .as_ref()
                    .map(|t| t.channels.iter().map(|(id, _)| id.clone()).collect())
                    .unwrap_or_default();
                self.spawn_action(
                    move |b| b.search_conversations(&ids, &asked, from),
                    move |result| Ev::Searched {
                        chat,
                        scope,
                        query,
                        from,
                        seq,
                        result,
                    },
                );
            }
        }
    }

    /// The service answered a search. It is used only if it is the answer the popup is still waiting for: the text may
    /// have changed, the popup been closed or the chat switched since it was asked.
    pub(super) fn on_searched(
        &mut self,
        chat: &str,
        scope: SearchScope,
        query: &str,
        from: usize,
        seq: u64,
        result: Result<SearchPage, Error>,
    ) {
        // The scope is part of what was asked: an answer for the other one (switched since) is not this popup's.
        let wanted = self.overlay == Overlay::Search
            && self.current.as_deref() == Some(chat)
            && self.search.chat.as_deref() == Some(chat)
            && self.search.scope == scope
            && self.search.pending.as_ref().is_some_and(|p| p.seq == seq);
        if !wanted {
            log::debug!(
                "chat {}: a search answer arrived after it was wanted and was dropped",
                id_tag(chat)
            );
            return;
        }
        let elapsed = self
            .search
            .pending
            .take()
            .map_or(0, |p| p.started.elapsed().as_millis());
        match result {
            Ok(page) => {
                log::debug!(
                    "chat {}: search returned {} hit(s) from {from} in {elapsed} ms (more: {})",
                    id_tag(chat),
                    page.hits.len(),
                    page.next.is_some()
                );
                let old_server = self.search.receive(query, from, &page);
                self.rebuild_search_rows();
                // The first page starts at the top; a later one puts the selection on its first new row.
                self.search.sel = if from == 0 {
                    0
                } else {
                    old_server.min(self.search.rows.len().saturating_sub(1))
                };
            }
            Err(e) => {
                log::warn!(
                    "chat {}: search failed after {elapsed} ms: {}",
                    id_tag(chat),
                    e.for_log()
                );
                if matches!(e, Error::Unsupported { .. }) {
                    // Nothing more of this kind is asked this session; what is loaded is searched.
                    if scope == SearchScope::Team {
                        self.search.team_unsupported = true;
                        self.search.team = None;
                        self.search.scope = SearchScope::Chat;
                        self.set_status("searching a whole team is not supported here", true);
                    } else {
                        self.search.unsupported = true;
                    }
                    self.search.answered = None;
                    self.search.server_rows.clear();
                    self.search.next = None;
                    self.rebuild_search_rows();
                } else {
                    self.search.failed = Some((
                        query.to_string(),
                        short_line(&describe_side(&e), MAX_PREVIEW_CHARS),
                    ));
                }
            }
        }
    }

    /// Open the selected row: close the popup, remember the list for stepping, and go to the message.
    fn open_selected_hit(&mut self) {
        let Some(row) = self.search.rows.get(self.search.sel).cloned() else {
            return;
        };
        let hits: Vec<StepHit> = self
            .search
            .rows
            .iter()
            .map(|r| StepHit {
                conversation: r.conversation.clone(),
                message_id: r.message_id.clone(),
                sent: r.sent,
            })
            .collect();
        let note = format!("hit {} of {}", self.search.sel + 1, hits.len());
        self.search.stepping = Some(Stepping {
            chat: self.search.chat.clone().unwrap_or_default(),
            hits,
            pos: self.search.sel,
        });
        self.close_search();
        // A hit in another channel of the team: that channel is opened first, as the teams list opens one.
        if !self.show_conversation_of_hit(&row.conversation) {
            return;
        }
        self.focus_pane(Focus::Messages);
        self.go_to_message(&row.message_id, row.sent, note);
    }

    /// Make `conversation`, the one a hit is in, the open one: nothing to do when it is, otherwise it is a channel of
    /// the teams list, opened like one (the tab stays, the keyboard goes to the messages). Opening a channel leaves the
    /// chat the hits were found from, which forgets them, so the hits to step through are kept across it. False, with
    /// the reason said, when the list does not have that channel.
    fn show_conversation_of_hit(&mut self, conversation: &str) -> bool {
        if self.current.as_deref() == Some(conversation) {
            return true;
        }
        let kept = self.search.stepping.take();
        self.open_channel(conversation, false);
        let arrived = self.current.as_deref() == Some(conversation);
        self.search.stepping = kept.map(|mut s| {
            if arrived {
                s.chat = conversation.to_string();
            }
            s
        });
        if !arrived {
            self.set_status(
                "that channel is not in the teams list, so its message cannot be shown",
                true,
            );
        }
        arrived
    }

    /// `n` / F3 (`forward`) or `N` / Shift-F3: the next or previous hit of the last list a hit was opened from,
    /// wrapping round. Nothing without one.
    pub(super) fn step_hit(&mut self, forward: bool) {
        let Some(chat) = self.current.clone() else {
            return;
        };
        let Some(step) = self.search.stepping.as_mut().filter(|s| s.chat == chat) else {
            return;
        };
        let len = step.hits.len();
        if len == 0 {
            return;
        }
        step.pos = if forward {
            (step.pos + 1) % len
        } else {
            (step.pos + len - 1) % len
        };
        let hit = step.hits[step.pos].clone();
        let note = format!("hit {} of {len}", step.pos + 1);
        // A hit of a team search can be in another channel, which is opened first.
        if self.show_conversation_of_hit(&hit.conversation) {
            self.go_to_message(&hit.message_id, hit.sent, note);
        }
    }

    /// Select the message `id` (sent at `sent`) in the open chat and bring it into view, fetching older pages first if it
    /// is further back than what is loaded. `found` is what the status line says once it is there.
    fn go_to_message(&mut self, id: &str, sent: DateTime<Utc>, found: String) {
        let Some(chat) = self.current.clone() else {
            return;
        };
        let st = self
            .states
            .entry(chat.clone())
            .or_insert_with(super::ChatState::new);
        st.seek = Some(Seek {
            message_id: id.to_string(),
            sent,
            pages: 0,
            found,
        });
        self.drive_seek(&chat, false);
    }

    /// Take the next step of a hunt for a message (`ChatState::seek`): select it if it is loaded; give up if no older
    /// page can hold it or too many pages were fetched; otherwise ask for one more older page, when the request slot is
    /// free. `request_failed` says the page it was waiting for did not come.
    pub(super) fn drive_seek(&mut self, chat: &str, request_failed: bool) {
        let Some(st) = self.states.get_mut(chat) else {
            return;
        };
        let Some(seek) = st.seek.clone() else {
            return;
        };
        if request_failed {
            let why = st.error.clone().unwrap_or_default();
            st.seek = None;
            self.set_status(format!("could not reach that message: {why}"), true);
            return;
        }
        if !st.loaded {
            return;
        }
        if let Some(index) = st.position_of(&seek.message_id) {
            st.select_at(index);
            st.seek = None;
            self.set_status(seek.found, false);
            return;
        }
        let passed = st
            .oldest_loaded()
            .is_some_and(|oldest| oldest + Duration::seconds(SEEK_SLACK_SECONDS) < seek.sent);
        if passed || st.older.is_none() || seek.pages >= MAX_SEEK_PAGES {
            st.seek = None;
            self.set_status(
                "could not reach that message; scroll up to load older ones",
                true,
            );
            return;
        }
        if st.is_fetching() {
            return;
        }
        let held = st.sched.held_for().is_some();
        if self.start_message_fetch(chat, Fetch::Seek) {
            if let Some(seek) = self.states.get_mut(chat).and_then(|st| st.seek.as_mut()) {
                seek.pages += 1;
                let text = format!("looking for the message… (page {})", seek.pages);
                self.set_status(text, false);
            }
        } else {
            // Signed out, or the service asked us to wait (which `start_message_fetch` has said).
            if let Some(st) = self.states.get_mut(chat) {
                st.seek = None;
            }
            if !held {
                self.set_status(
                    "could not reach that message; scroll up to load older ones",
                    true,
                );
            }
        }
    }
}

/// `YYYY-MM-DD HH:MM` in the local time zone, as a row shows when a message was sent.
#[must_use]
pub fn search_time(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use teams_client::{Delivery, SearchHit};

    fn message(id: &str, mins_ago: i64, who: &str, body: &str) -> Msg {
        let t = Utc::now() - Duration::minutes(mins_ago);
        Msg {
            root_id: None,
            id: id.into(),
            created: t,
            modified: t,
            edited: None,
            from_id: Some(who.into()),
            from_name: who.into(),
            system: false,
            deleted: false,
            html: true,
            body: body.into(),
            subject: None,
            attachments: vec![],
            reactions: vec![],
            mentions: vec![],
            delivery: Delivery::Sent,
        }
    }

    fn row(id: &str, mins_ago: i64, server: bool) -> SearchRow {
        SearchRow {
            message_id: id.into(),
            conversation: "c".into(),
            channel: String::new(),
            sent: Utc::now() - Duration::minutes(mins_ago),
            sender: "S".into(),
            preview: id.into(),
            from_server: server,
        }
    }

    #[test]
    fn matching_ignores_case_and_reports_character_ranges() {
        assert_eq!(find_matches("Hello World, hello", "HELLO"), [0..5, 13..18]);
        assert_eq!(find_matches("abc", ""), Vec::<Range<usize>>::new());
        assert_eq!(find_matches("abc", "x"), Vec::<Range<usize>>::new());
        // Characters, not bytes: a multi-byte letter before the match does not shift it.
        assert_eq!(find_matches("é cat", "CAT").len(), 1);
        assert_eq!(find_matches("é cat", "CAT")[0], 2..5);
    }

    #[test]
    fn a_letter_whose_lower_case_is_longer_maps_back_to_its_own_character() {
        // 'İ' lower-cases to two characters ('i' and a combining dot); the range is still the one original character.
        let found = find_matches("İx", "i\u{307}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], 0..1);
        // And the text after it is not shifted by the extra character.
        assert_eq!(find_matches("İx", "x")[0], 1..2);
    }

    #[test]
    fn loaded_matches_are_newest_first_and_skip_events_and_deleted_messages() {
        let mut gone = message("c", 3, "Ada", "<p>the answer</p>");
        gone.deleted = true;
        let mut event = message("d", 2, "Ada", "the answer");
        event.system = true;
        let msgs = vec![
            message("a", 30, "Ada", "<p>The <b>answer</b> is 42</p>"),
            message("b", 10, "Grace", "<p>no</p>"),
            gone,
            event,
            message("e", 5, "Linus", "<p>my ANSWER</p>"),
        ];
        let rows = local_rows(&msgs, "answer");
        assert_eq!(
            rows.iter()
                .map(|r| r.message_id.as_str())
                .collect::<Vec<_>>(),
            ["e", "a"]
        );
        assert_eq!(rows[1].preview, "The answer is 42", "markup is not in it");
        assert_eq!(rows[1].sender, "Ada");
        assert!(rows.iter().all(|r| !r.from_server));
        assert!(local_rows(&msgs, "").is_empty());
    }

    #[test]
    fn a_match_far_into_a_long_message_is_in_its_preview() {
        let body = format!(
            "<p>{} needle {}</p>",
            "word ".repeat(80),
            "tail ".repeat(80)
        );
        let rows = local_rows(&[message("a", 1, "Ada", &body)], "needle");
        assert!(rows[0].preview.starts_with('…'), "{}", rows[0].preview);
        assert!(rows[0].preview.contains("needle"));
        assert!(rows[0].preview.chars().count() <= 242);
    }

    #[test]
    fn combining_keeps_the_services_order_then_the_rest_without_repeats() {
        let server = [row("s2", 50, true), row("s1", 5, true), row("s2", 50, true)];
        let local = vec![
            row("s1", 5, false),
            row("l1", 1, false),
            row("l0", 9, false),
        ];
        let rows = combine(&server, local);
        assert_eq!(
            rows.iter()
                .map(|r| r.message_id.as_str())
                .collect::<Vec<_>>(),
            ["s2", "s1", "l1", "l0"]
        );
        assert!(
            rows[1].from_server,
            "the service's copy of a message is kept"
        );
    }

    #[test]
    fn rows_are_limited() {
        let many: Vec<SearchRow> = (0..MAX_SEARCH_ROWS + 50)
            .map(|i| row(&format!("m{i}"), 1, false))
            .collect();
        assert_eq!(combine(&many, Vec::new()).len(), MAX_SEARCH_ROWS);
        let msgs: Vec<Msg> = (0..MAX_SEARCH_ROWS + 50)
            .map(|i| message(&format!("m{i}"), 1, "A", "x"))
            .collect();
        assert_eq!(local_rows(&msgs, "x").len(), MAX_SEARCH_ROWS);
    }

    #[test]
    fn nothing_from_the_network_reaches_a_row_with_a_control_or_a_bidi_mark() {
        let line = one_line("a\u{1b}[31mb\u{202e}c\u{7}\u{9b}d\u{200f}  \n e\u{e000}");
        assert_eq!(line, "a[31mbcd e");
        let mut search = Search {
            chat: Some("c".into()),
            ..Search::default()
        };
        search.receive(
            "x",
            0,
            &SearchPage {
                hits: vec![SearchHit {
                    message_id: "m".into(),
                    sent: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                    sender: "Ev\u{1b}]52;c;AAAA\u{7}il".into(),
                    preview: "\u{202e}hello\u{1b}[2J".into(),
                    conversation: String::new(),
                    channel: "Re\u{1b}[2Jleases\u{202e}".into(),
                }],
                next: None,
                total: None,
            },
        );
        let r = &search.server_rows[0];
        assert!(
            !r.sender
                .chars()
                .chain(r.preview.chars())
                .chain(r.channel.chars())
                .any(|c| c.is_control() || c == '\u{202e}'),
            "{r:?}"
        );
    }

    #[test]
    fn a_cursor_that_does_not_move_forward_ends_the_paging() {
        let mut search = Search {
            chat: Some("c".into()),
            ..Search::default()
        };
        let hit = |id: &str| SearchHit {
            message_id: id.into(),
            sent: Utc::now(),
            sender: String::new(),
            preview: String::new(),
            conversation: String::new(),
            channel: String::new(),
        };
        let page = |ids: &[&str], next| SearchPage {
            hits: ids.iter().map(|i| hit(i)).collect(),
            next,
            total: Some(9),
        };
        search.receive("q", 0, &page(&["a", "b"], Some(2)));
        assert_eq!(search.next, Some(2));
        // A page of nothing new (hits of other chats were dropped) with more behind it is not the end ...
        search.receive("q", 2, &page(&["a", "b"], Some(4)));
        assert_eq!(search.next, Some(4));
        assert_eq!(search.server_rows.len(), 2, "no row twice");
        // ... but a cursor that does not move forward is, or it would be asked for forever.
        search.receive("q", 4, &page(&["c"], Some(4)));
        assert_eq!(search.next, None);
        search.receive("q", 0, &page(&["a"], Some(0)));
        assert_eq!(search.next, None);
    }
}
