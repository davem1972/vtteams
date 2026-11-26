//! Teams and their channels: the second list of the left pane (the `TEAMS` tab).
//!
//! The teams list is large and slow (a person can be in dozens of teams), so it is asked for the first time the tab is
//! shown, then at most every ten minutes while the tab is in front and every half hour otherwise (`Schedule` carries the
//! back-off after failures and the service's own hold-off). What changed since is cheap: each tick the backend's
//! `channel_activity` (what the chat list it already polls said about channels) is merged over the list. It is a side
//! call in every way that matters: its failure is shown in the tab, never read as a lost sign-in (see
//! `Ev::sign_in_error`), and `Error::Unsupported` ends it and hides the tab.
//!
//! A channel is opened like a chat: its id goes to `select_chat`, its messages are read with `Backend::messages` and it
//! is polled, marked read and searched by that id. What is not a chat about it (a title, no people, posting by its own call) is
//! decided by [`Opened`], the snapshot taken when it was opened.

use super::{App, Ev, Focus, describe_side, throttle_hold};
use crate::cadence::Schedule;
use crate::editor::Mode;
use crate::view::{self, Tab};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use teams_client::{Channel, ChannelActivity, ChannelKind, Error, Team};

/// How often the teams list is read again while the tab is in front, and while it is not.
const REFRESH_VISIBLE: Duration = Duration::from_secs(10 * 60);
const REFRESH_HIDDEN: Duration = Duration::from_secs(30 * 60);
/// How soon a first load that failed is tried again (the schedule doubles it for each failure in a row).
const RETRY_FIRST_LOAD: Duration = Duration::from_secs(30);

/// What the person is told when something asks for posting and the service cannot (see `app/post.rs` for when it can).
pub(super) const NO_POSTING: &str =
    "posting in channels is not available yet; nothing was sent, and what you typed is kept";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Load {
    Idle,
    /// Somebody asked (the first time the tab is shown, `:refresh`): the spinner shows.
    Visible,
    /// The scheduled refresh of a list already showing: nothing shows unless it fails.
    Quiet,
}

/// The channel that is open, as it was when it was opened (and kept up to date while the list has it): what the title
/// and the message box need, which must survive a refresh of the list that no longer mentions the channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub id: String,
    pub team: String,
    pub name: String,
    /// How many members the team has, when the service says.
    pub members: Option<u32>,
    pub kind: ChannelKind,
    pub archived: bool,
}

/// One row of the teams list. The favourites come first (at the top, as `Favorite` rows, under a divider that is not a
/// row but is drawn between the last of them and the first of the rest, as the chat list does), then the teams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeamRow {
    /// A favourite channel of the person's "Favorites" folder, shown as `Team › Channel`. It is not part of the team
    /// tree: it never folds, and the channel is listed under its team as well when that is open.
    Favorite {
        team: usize,
        channel: usize,
    },
    /// A team (`open`: its channels are listed under it).
    Team {
        team: usize,
        open: bool,
    },
    Channel {
        team: usize,
        channel: usize,
    },
    /// `+N more channels`: the ones the person has not chosen to see; `open` lists them too.
    More {
        team: usize,
        hidden: usize,
        open: bool,
    },
}

/// What the cursor of the teams list stands on, by id, so that it stays on it when the list is refreshed or folded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKey {
    /// The favourites row of the channel with this id (a different row from its place under its team).
    Favorite(String),
    Team(String),
    Channel(String),
    More(String),
}

/// How a favourite channel reads in the favourites section: the team, then the channel (the General channel of a team is
/// `Team › General`).
pub fn favorite_label(team: &Team, channel: &Channel) -> String {
    format!("{} › {}", team.name, channel.name)
}

/// How many of the first of `rows` are favourites (the rows above the divider).
pub fn favorite_count(rows: &[TeamRow]) -> usize {
    rows.iter()
        .take_while(|r| matches!(r, TeamRow::Favorite { .. }))
        .count()
}

pub struct Teams {
    items: Vec<Team>,
    /// Channel id -> (team, channel) positions in `items`.
    index: HashMap<String, (usize, usize)>,
    loaded: bool,
    load: Load,
    /// The tab has been shown: until then nothing is asked for.
    requested: bool,
    pub(super) sched: Schedule,
    error: Option<String>,
    unsupported: bool,
    /// Teams whose channels are listed, and teams whose hidden channels are listed too. Per session.
    open: HashSet<String>,
    more: HashSet<String>,
    opened: Option<Opened>,
    /// The list is what the last session left (the start-up cache); the service has not been asked yet, or has not
    /// answered.
    cached: bool,
    /// When the list was last read from the service (or, while `cached`, when the cache says it was).
    read_at: Option<DateTime<Utc>>,
}

impl Teams {
    pub(super) fn new() -> Self {
        Self {
            items: Vec::new(),
            index: HashMap::new(),
            loaded: false,
            load: Load::Idle,
            requested: false,
            sched: Schedule::new(),
            error: None,
            unsupported: false,
            open: HashSet::new(),
            more: HashSet::new(),
            opened: None,
            cached: false,
            read_at: None,
        }
    }

    pub fn items(&self) -> &[Team] {
        &self.items
    }

    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// Whether the list on screen is the cached one, not yet replaced by the service's own.
    pub fn is_cached(&self) -> bool {
        self.cached
    }

    /// When the list was read from the service, as far as is known.
    pub(super) fn read_at(&self) -> Option<DateTime<Utc>> {
        self.read_at
    }

    /// Show `items` from the start-up cache until the service answers. Nothing is asked of the service because of it:
    /// the first request still happens when the tab is first shown. `requested` stays as it is.
    pub(super) fn receive_cached(&mut self, items: Vec<Team>, read_at: DateTime<Utc>) {
        if self.loaded {
            return;
        }
        self.items = items;
        self.reindex();
        self.loaded = true;
        self.cached = true;
        self.read_at = Some(read_at);
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) fn is_fetching(&self) -> bool {
        self.load != Load::Idle
    }

    pub(super) fn shows_loading(&self) -> bool {
        self.load == Load::Visible
    }

    /// The channel with `id` and its team.
    pub fn channel(&self, id: &str) -> Option<(&Team, &Channel)> {
        let &(t, c) = self.index.get(id)?;
        let team = self.items.get(t)?;
        Some((team, team.channels.get(c)?))
    }

    fn reindex(&mut self) {
        self.index = self
            .items
            .iter()
            .enumerate()
            .flat_map(|(t, team)| {
                team.channels
                    .iter()
                    .enumerate()
                    .map(move |(c, ch)| (ch.id.clone(), (t, c)))
            })
            .collect();
    }

    pub(super) fn receive(&mut self, items: Vec<Team>) {
        self.items = items;
        self.reindex();
        self.loaded = true;
        self.cached = false;
        self.read_at = Some(Utc::now());
        self.load = Load::Idle;
        self.error = None;
        self.sched.succeeded();
        // The open channel's snapshot follows the list while the list still has it.
        if let Some(o) = self.opened.take() {
            self.opened = Some(
                self.channel(&o.id)
                    .map_or(o, |(team, ch)| snapshot(team, ch)),
            );
        }
    }

    /// What the conversation list said about channels replaces what the teams list said: it is read much more often.
    /// A newest message is replaced only by another one, so an answer with none does not wipe what the list knew.
    pub(super) fn merge_activity(&mut self, activity: Vec<ChannelActivity>) {
        for a in activity {
            let Some(&(t, c)) = self.index.get(&a.id) else {
                continue;
            };
            let Some(ch) = self
                .items
                .get_mut(t)
                .and_then(|team| team.channels.get_mut(c))
            else {
                continue;
            };
            ch.unread = a.unread;
            if a.last.is_some() {
                ch.last = a.last;
            }
        }
    }

    /// The channels of `team` in the order they are listed: its General channel, the pinned ones, then the others the
    /// person has chosen to see (what the service calls shown); and, separately, the rest.
    fn listed(team: &Team) -> (Vec<usize>, Vec<usize>) {
        let all = 0..team.channels.len();
        let mut shown: Vec<usize> = all.clone().filter(|&i| team.channels[i].general).collect();
        shown.extend(all.clone().filter(|&i| {
            let c = &team.channels[i];
            c.pinned && !c.general
        }));
        shown.extend(all.clone().filter(|&i| {
            let c = &team.channels[i];
            c.shown && !c.pinned && !c.general
        }));
        let hidden = all.filter(|i| !shown.contains(i)).collect();
        (shown, hidden)
    }

    /// The teams the list shows: archived ones are not (their data stays loaded, so a channel of one that is open keeps
    /// its title).
    pub fn visible(&self) -> impl Iterator<Item = (usize, &Team)> {
        self.items.iter().enumerate().filter(|(_, t)| !t.archived)
    }

    /// The rows of the list. With `pin` (the `favorites` setting), first the favourite channels in the order of the
    /// person's Favorites folder; then every team that is not archived, and under each open one its channels. With a
    /// `filter`, the favourites whose `Team › Channel` text contains it (any case, and none of them opens a team), and
    /// the teams whose name or any channel's name contains it, a team with matching channels being open and listing
    /// only those.
    pub fn rows(&self, filter: &str, pin: bool) -> Vec<TeamRow> {
        let q = filter.trim().to_lowercase();
        let has = |name: &str| name.to_lowercase().contains(&q);
        let mut rows = Vec::new();
        if pin {
            let mut favourites: Vec<(u32, usize, usize)> = self
                .visible()
                .flat_map(|(t, team)| {
                    team.channels
                        .iter()
                        .enumerate()
                        .filter_map(move |(c, ch)| Some((ch.favorite?, t, c)))
                })
                .filter(|&(_, t, c)| {
                    let team = &self.items[t];
                    has(&favorite_label(team, &team.channels[c]))
                })
                .collect();
            favourites.sort_unstable();
            rows.extend(
                favourites
                    .into_iter()
                    .map(|(_, team, channel)| TeamRow::Favorite { team, channel }),
            );
        }
        for (t, team) in self.visible() {
            let (shown, hidden) = Self::listed(team);
            if !q.is_empty() {
                let hits: Vec<usize> = shown
                    .iter()
                    .chain(&hidden)
                    .copied()
                    .filter(|&c| has(&team.channels[c].name))
                    .collect();
                if !hits.is_empty() {
                    rows.push(TeamRow::Team {
                        team: t,
                        open: true,
                    });
                    rows.extend(
                        hits.into_iter()
                            .map(|channel| TeamRow::Channel { team: t, channel }),
                    );
                    continue;
                }
                if !has(&team.name) {
                    continue;
                }
            }
            let open = self.open.contains(&team.id);
            rows.push(TeamRow::Team { team: t, open });
            if !open {
                continue;
            }
            rows.extend(
                shown
                    .iter()
                    .map(|&channel| TeamRow::Channel { team: t, channel }),
            );
            if !hidden.is_empty() {
                let more = self.more.contains(&team.id);
                rows.push(TeamRow::More {
                    team: t,
                    hidden: hidden.len(),
                    open: more,
                });
                if more {
                    rows.extend(
                        hidden
                            .iter()
                            .map(|&channel| TeamRow::Channel { team: t, channel }),
                    );
                }
            }
        }
        rows
    }
}

fn snapshot(team: &Team, ch: &Channel) -> Opened {
    Opened {
        id: ch.id.clone(),
        team: team.name.clone(),
        name: ch.name.clone(),
        members: team.members,
        kind: ch.kind,
        archived: ch.archived || team.archived,
    }
}

/// What a tab label says: its name and count, and its unread count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabText {
    pub tab: Tab,
    pub main: String,
    pub unread: String,
}

impl TabText {
    pub fn width(&self) -> usize {
        use unicode_width::UnicodeWidthStr;
        self.main.width() + self.unread.width()
    }
}

impl App {
    /// Whether the teams list has been asked for and has not come yet (a `--dump` waits for it between keys).
    pub fn teams_pending(&self) -> bool {
        self.teams.is_fetching()
    }

    /// Whether the left pane has a `TEAMS` tab: the service can list teams and has not said it cannot.
    pub fn teams_available(&self) -> bool {
        self.backend.can_list_teams() && !self.teams.unsupported
    }

    /// Whether the teams list is what the left pane shows.
    pub fn on_teams_tab(&self) -> bool {
        self.tab == Tab::Teams && self.teams_available()
    }

    /// The channel that is open, if what is open is a channel.
    pub fn current_channel(&self) -> Option<&Opened> {
        self.teams
            .opened
            .as_ref()
            .filter(|o| self.current.as_deref() == Some(o.id.as_str()))
    }

    /// Whether a channel is unread: the service's word, until it has been read here.
    pub fn channel_unread(&self, ch: &Channel) -> bool {
        self.read
            .channel_unread(&ch.id, ch.unread, ch.last.as_ref())
    }

    /// How many of a team's channels are unread.
    pub fn team_unread(&self, team: &Team) -> usize {
        team.channels
            .iter()
            .filter(|c| self.channel_unread(c))
            .count()
    }

    /// How many channels are unread in the teams the list shows (archived teams are not shown, so not counted).
    fn teams_unread_total(&self) -> usize {
        self.teams.visible().map(|(_, t)| self.team_unread(t)).sum()
    }

    /// The tab labels as they fit in `width` columns, in order: empty when the left pane has no tabs. The unread counts
    /// go first when there is no room, then the counts.
    pub fn tab_labels(&self, width: usize) -> Vec<TabText> {
        if !self.teams_available() {
            return Vec::new();
        }
        let chats = (
            self.visible_chats().len(),
            self.chats
                .items()
                .iter()
                .filter(|c| self.is_unread(c))
                .count(),
        );
        let teams = (
            self.teams.loaded.then(|| self.teams.visible().count()),
            self.teams_unread_total(),
        );
        let build = |counts: bool, unread: bool| -> Vec<TabText> {
            let label = |tab: Tab, name: &str, count: Option<usize>, n: usize| TabText {
                tab,
                main: match count.filter(|_| counts) {
                    Some(count) => format!("{name} {count}"),
                    None => name.to_string(),
                },
                unread: if unread && n > 0 {
                    format!(" ●{n}")
                } else {
                    String::new()
                },
            };
            vec![
                label(Tab::Chats, "CHATS", Some(chats.0), chats.1),
                label(Tab::Teams, "TEAMS", teams.0, teams.1),
            ]
        };
        for (counts, unread) in [(true, true), (true, false), (false, false)] {
            let labels = build(counts, unread);
            let widths: Vec<usize> = labels.iter().map(TabText::width).collect();
            let used = view::tab_cells(&widths).last().map_or(0, |c| c.end);
            if used <= width {
                return labels;
            }
        }
        build(false, false)
    }

    /// Show `tab` in the left pane and put the keyboard there. The filter belongs to the list it was typed in.
    pub fn show_tab(&mut self, tab: Tab) {
        if tab == Tab::Teams && !self.teams_available() {
            self.set_status("this service has no teams", true);
            return;
        }
        if self.tab != tab {
            self.filter.clear();
            self.filtering = false;
            self.tab = tab;
        }
        // The left pane is where the change shows; leaving the message box drops vim to normal mode as `pane_left` does.
        self.pane_left();
        if tab == Tab::Teams {
            self.teams.requested = true;
            if !self.teams.loaded {
                self.refresh_teams(true);
            } else if self.teams.cached {
                // What the last session left is on screen; the first request of this one still happens now, quietly.
                self.refresh_teams(false);
            }
            if self.team_cursor.is_none() {
                self.team_cursor = self
                    .current_channel()
                    .map(|o| RowKey::Channel(o.id.clone()));
            }
        }
    }

    /// The tab an Alt-1 or Alt-2 asks for. Only when there are tabs (otherwise the key is nothing to us), and not with
    /// Control, which together with Alt is how Windows reports AltGr, a way of typing characters.
    pub(super) fn tab_shortcut(&self, key: &ratatui::crossterm::event::KeyEvent) -> Option<Tab> {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        if !self.teams_available()
            || !key.modifiers.contains(KeyModifiers::ALT)
            || key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return None;
        }
        match key.code {
            KeyCode::Char('1') => Some(Tab::Chats),
            KeyCode::Char('2') => Some(Tab::Teams),
            _ => None,
        }
    }

    /// The other tab (`gt`, `gT`): there are two, so either direction goes to the one not showing.
    pub(super) fn switch_tab(&mut self) {
        if !self.teams_available() {
            return;
        }
        let next = match self.tab {
            Tab::Chats => Tab::Teams,
            Tab::Teams => Tab::Chats,
        };
        self.show_tab(next);
    }

    /// Ask for the teams list. `asked` is for what a person asked for (the tab shown the first time, `:refresh`): it
    /// shows the spinner and says when the service has asked us to wait. False when nothing was started.
    pub(super) fn refresh_teams(&mut self, asked: bool) -> bool {
        if !self.teams_available() || !self.sign_in.is_online() || self.teams.is_fetching() {
            return false;
        }
        if let Some(left) = self.teams.sched.held_for() {
            if asked {
                self.set_status(
                    format!(
                        "the service asked us to wait; try again in {} s",
                        left.as_secs() + 1
                    ),
                    true,
                );
            }
            return false;
        }
        self.teams.requested = true;
        self.teams.load = if asked || !self.teams.loaded {
            Load::Visible
        } else {
            Load::Quiet
        };
        self.teams.sched.started();
        log::debug!(
            "teams: refresh started ({} failure(s) in a row)",
            self.teams.sched.fails()
        );
        self.spawn_call(|b| b.teams(), Ev::Teams);
        true
    }

    /// The scheduled refresh (called from the tick): nothing until the tab has been shown, then every ten minutes while
    /// it is in front and every half hour while it is not; a first load that failed is tried again sooner.
    pub(super) fn refresh_teams_if_due(&mut self) {
        if !self.teams.requested {
            return;
        }
        let interval = if !self.teams.loaded {
            RETRY_FIRST_LOAD
        } else if self.on_teams_tab() {
            REFRESH_VISIBLE
        } else {
            REFRESH_HIDDEN
        };
        if self.teams.sched.due(interval) {
            self.refresh_teams(false);
        }
    }

    /// Put what the conversation list says about channels over the teams list (called from the tick).
    pub(super) fn merge_channel_activity(&mut self) {
        if self.teams.loaded && self.backend.can_list_teams() {
            let activity = self.backend.channel_activity();
            self.teams.merge_activity(activity);
        }
    }

    pub(super) fn on_teams(&mut self, result: Result<Vec<Team>, Error>) {
        match result {
            Ok(teams) => {
                log::debug!(
                    "teams: {} team(s) in {} ms",
                    teams.len(),
                    self.teams.sched.since_last().as_millis()
                );
                // How the person has folded the teams stands: a team opens when a channel in it is opened (`open_channel`),
                // and a new list does not open or close any.
                self.teams.receive(teams);
                self.mark_cache_dirty();
            }
            Err(e) => {
                let hold = throttle_hold(&e);
                let first_failure = self.teams.sched.fails() == 0;
                log::warn!(
                    "teams: refresh failed ({} in a row){}: {}",
                    self.teams.sched.fails() + 1,
                    hold.map_or_else(String::new, |h| format!(
                        ", throttled for {} s",
                        h.as_secs()
                    )),
                    e.for_log()
                );
                self.teams.load = Load::Idle;
                if matches!(e, Error::Unsupported { .. }) {
                    // The service cannot do this for this account: the tab goes and nothing asks again.
                    self.teams.unsupported = true;
                    if self.tab == Tab::Teams {
                        self.tab = Tab::Chats;
                        self.filter.clear();
                        self.filtering = false;
                    }
                    self.set_status(format!("no teams: {}", describe_side(&e)), true);
                    return;
                }
                self.teams.sched.failed(hold);
                // A side call: the text is the library's own, never "signed out".
                self.teams.error = Some(describe_side(&e));
                if self.teams.loaded && first_failure {
                    self.set_status(format!("teams not refreshed: {}", describe_side(&e)), true);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Rows and the cursor
    // -----------------------------------------------------------------------

    /// Open every team (and show the channels the person has not chosen to see), as a measurement of the biggest list
    /// needs.
    #[cfg(test)]
    pub(crate) fn unfold_every_team(&mut self) {
        let ids: Vec<String> = self.teams.items.iter().map(|t| t.id.clone()).collect();
        self.teams.open.extend(ids.iter().cloned());
        self.teams.more.extend(ids);
    }

    /// The rows the teams list shows now (see [`Teams::rows`]).
    pub fn team_rows(&self) -> Vec<TeamRow> {
        self.teams.rows(&self.filter, self.pin_favorites)
    }

    /// What the cursor would stand on for `row`.
    pub fn row_key(&self, row: &TeamRow) -> RowKey {
        let items = &self.teams.items;
        match *row {
            TeamRow::Favorite { team, channel } => {
                RowKey::Favorite(items[team].channels[channel].id.clone())
            }
            TeamRow::Team { team, .. } => RowKey::Team(items[team].id.clone()),
            TeamRow::Channel { team, channel } => {
                RowKey::Channel(items[team].channels[channel].id.clone())
            }
            TeamRow::More { team, .. } => RowKey::More(items[team].id.clone()),
        }
    }

    /// Where the cursor is among `rows` (the first row when it is on nothing the list shows now). A channel's row under
    /// its team and its row among the favourites are two rows of one channel: when the one the cursor was on has gone
    /// (the favourites were turned off, the team was closed or the filter changed), the other takes it.
    pub fn team_cursor_index(&self, rows: &[TeamRow]) -> usize {
        let find = |key: &RowKey| rows.iter().position(|r| &self.row_key(r) == key);
        self.team_cursor
            .as_ref()
            .and_then(|key| {
                find(key).or_else(|| match key {
                    RowKey::Favorite(id) => find(&RowKey::Channel(id.clone())),
                    RowKey::Channel(id) => find(&RowKey::Favorite(id.clone())),
                    _ => None,
                })
            })
            .unwrap_or(0)
    }

    /// Move the cursor `delta` rows, stopping at the ends.
    pub(super) fn teams_move(&mut self, delta: isize) {
        let rows = self.team_rows();
        if rows.is_empty() {
            return;
        }
        let at = self.team_cursor_index(&rows) as isize;
        let next = (at + delta).clamp(0, rows.len() as isize - 1) as usize;
        self.team_cursor = Some(self.row_key(&rows[next]));
    }

    pub(super) fn teams_goto(&mut self, last: bool) {
        let rows = self.team_rows();
        let pick = if last { rows.last() } else { rows.first() };
        if let Some(row) = pick {
            self.team_cursor = Some(self.row_key(row));
        }
    }

    /// After the filter changes: keep the cursor on a row that is still listed.
    pub(super) fn teams_keep_cursor(&mut self) {
        let rows = self.team_rows();
        let at = self.team_cursor_index(&rows);
        self.team_cursor = rows.get(at).map(|r| self.row_key(r));
    }

    /// Enter, Right or a click on the row under the cursor: open or close a team, open a channel, show or hide the
    /// channels the person has not chosen to see. `enter` is Enter or a click, which closes an open team; Right on an
    /// open team steps into its first channel.
    pub(super) fn teams_activate(&mut self, enter: bool) {
        let rows = self.team_rows();
        let at = self.team_cursor_index(&rows);
        let Some(&row) = rows.get(at) else { return };
        match row {
            TeamRow::Favorite { team, channel } => {
                let id = self.teams.items[team].channels[channel].id.clone();
                self.open_channel(&id, true);
            }
            TeamRow::Team { team, open } => {
                let id = self.teams.items[team].id.clone();
                if !open {
                    self.teams.open.insert(id);
                } else if enter {
                    self.teams.open.remove(&id);
                } else if let Some(next @ TeamRow::Channel { team: t, .. }) = rows.get(at + 1)
                    && *t == team
                {
                    self.team_cursor = Some(self.row_key(next));
                }
            }
            TeamRow::Channel { team, channel } => {
                let id = self.teams.items[team].channels[channel].id.clone();
                self.open_channel(&id, false);
            }
            TeamRow::More { team, .. } => {
                let id = self.teams.items[team].id.clone();
                if !self.teams.more.remove(&id) {
                    self.teams.more.insert(id);
                }
            }
        }
        // The cursor stays on its row when the list folds around it, or goes to the row it was moved to.
    }

    /// Left: close the team under the cursor, or go from one of its rows to the team.
    pub(super) fn teams_back(&mut self) {
        let rows = self.team_rows();
        let at = self.team_cursor_index(&rows);
        let Some(&row) = rows.get(at) else { return };
        match row {
            // A favourite is not part of the tree: there is nothing to close and no team above it.
            TeamRow::Favorite { .. } => {}
            TeamRow::Team { team, open } => {
                if open {
                    let id = self.teams.items[team].id.clone();
                    self.teams.open.remove(&id);
                }
            }
            TeamRow::Channel { team, .. } | TeamRow::More { team, .. } => {
                let id = self.teams.items[team].id.clone();
                self.team_cursor = Some(RowKey::Team(id));
            }
        }
    }

    /// Open the channel `id` like a chat: its messages are read and polled by its id, and the keyboard goes to them.
    /// From the tree (`favorite` false) its team opens to show it; from the favourites the tree is left as it was and
    /// the cursor stays on the favourites row.
    pub(super) fn open_channel(&mut self, id: &str, favorite: bool) {
        let Some((team, ch)) = self.teams.channel(id) else {
            return;
        };
        let (team_id, snap) = (team.id.clone(), snapshot(team, ch));
        if !favorite {
            self.teams.open.insert(team_id);
        }
        self.teams.opened = Some(snap);
        self.filtering = false;
        self.select_chat(id);
        self.team_cursor = Some(if favorite {
            RowKey::Favorite(id.to_string())
        } else {
            RowKey::Channel(id.to_string())
        });
        self.focus_pane(Focus::Messages);
    }

    /// Mark the open channel read here: past its newest message, whatever the service has said so far. The same
    /// local override a chat gets, kept by id.
    pub(super) fn mark_current_channel_read(&mut self) {
        let Some(id) = self.current_channel().map(|o| o.id.clone()) else {
            return;
        };
        let newest_loaded = self.states.get(&id).and_then(|st| {
            st.messages()
                .iter()
                .filter(|m| m.delivery == teams_client::Delivery::Sent)
                .map(|m| m.created)
                .max()
        });
        let newest_listed = self
            .teams
            .channel(&id)
            .and_then(|(_, c)| c.last.as_ref().map(|p| p.created));
        if let Some(at) = newest_loaded.max(newest_listed) {
            self.read.mark_local(id, at);
        } else if self.teams.channel(&id).is_some_and(|(_, c)| c.unread) {
            // Flagged unread with nothing to date the flag by: opening it is reading it.
            self.read.mark_local(id, chrono::Utc::now());
        }
    }

    // -----------------------------------------------------------------------
    // Keys
    // -----------------------------------------------------------------------

    /// The teams list in the vim keymap (`was_g`: the key before was `g`).
    pub(super) fn on_teams_key(&mut self, key: ratatui::crossterm::event::KeyEvent, was_g: bool) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let half = (self.view.list_height() / 2).max(1) as isize;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down if !ctrl => self.teams_move(1),
            KeyCode::Char('k') | KeyCode::Up if !ctrl => self.teams_move(-1),
            KeyCode::Char('n') if ctrl => self.teams_move(1),
            KeyCode::Char('p') if ctrl => self.teams_move(-1),
            KeyCode::Char('d') if ctrl => self.teams_move(half),
            KeyCode::Char('u') if ctrl => self.teams_move(-half),
            KeyCode::Char('g') if !ctrl => {
                if was_g {
                    self.teams_goto(false);
                } else {
                    self.pending_g = true;
                }
            }
            KeyCode::Char('G') => self.teams_goto(true),
            KeyCode::Enter => self.teams_activate(true),
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => self.teams_activate(false),
            KeyCode::Char('h') | KeyCode::Left => self.teams_back(),
            KeyCode::Char('i') | KeyCode::Char('a') => {
                if self.current.is_some() {
                    self.focus = Focus::Editor;
                    self.editor.set_mode(Mode::Insert);
                }
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.filter.clear();
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.teams_keep_cursor();
            }
            KeyCode::Char('f') => self.say_channels_are_not_favourites(),
            KeyCode::Char('F') => self.toggle_pinned(),
            KeyCode::Char('n') if !ctrl => self.cmdline = Some("new ".into()),
            KeyCode::Char('r') => self.run_command("refresh"),
            KeyCode::Char('I') => self.cycle_images(),
            KeyCode::Char(':') => self.cmdline = Some(String::new()),
            KeyCode::Char('?') => self.overlay = super::Overlay::Help(0),
            KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    /// The teams list in the standard keymap: arrows and paging move, Enter and Right open, Left closes, and typing starts
    /// a filter, so no letter does anything surprising.
    pub(super) fn on_teams_key_standard(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::KeyCode;
        let page = self.view.list_height().max(1) as isize;
        match key.code {
            KeyCode::Down => self.teams_move(1),
            KeyCode::Up => self.teams_move(-1),
            KeyCode::PageDown => self.teams_move(page),
            KeyCode::PageUp => self.teams_move(-page),
            KeyCode::Home => self.teams_goto(false),
            KeyCode::End => self.teams_goto(true),
            KeyCode::Enter => self.teams_activate(true),
            KeyCode::Right | KeyCode::Tab => self.teams_activate(false),
            KeyCode::Left => self.teams_back(),
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.teams_keep_cursor();
            }
            KeyCode::Char(c) if super::input::types_text(key.modifiers) => {
                self.filtering = true;
                self.filter.clear();
                self.filter.push(c);
                self.teams_keep_cursor();
            }
            _ => {}
        }
    }

    pub(super) fn say_channels_are_not_favourites(&mut self) {
        self.set_status("channels are not favourites here yet", true);
    }

    /// A click on row `row` of the list body (counted from the screen's top) while the teams list is showing: the cursor
    /// goes to the row and it is opened as Enter would.
    pub(super) fn click_team_row(&mut self, row: u16) {
        let body = self.view.geometry.list_body;
        let Some(rel) = row.checked_sub(body.y) else {
            return;
        };
        let rows = self.team_rows();
        let pinned = favorite_count(&rows);
        // The divider between the favourites and the teams is a screen row and not a list row: a click on it is nothing.
        let view::TeamsLine::Row(at) =
            view::teams_row(self.view.team_offset, pinned, usize::from(rel))
        else {
            return;
        };
        let Some(picked) = rows.get(at) else { return };
        self.team_cursor = Some(self.row_key(picked));
        self.filtering = false;
        self.teams_activate(true);
    }

    /// A click on the tab labels of the list's heading.
    pub(super) fn click_tab(&mut self, column: u16) {
        // While a filter is shown the heading holds it, not the tabs.
        if self.filtering || !self.filter.is_empty() {
            return;
        }
        let head = self.view.geometry.list_head;
        let Some(rel) = column.checked_sub(head.x) else {
            return;
        };
        let labels = self.tab_labels(usize::from(head.width));
        let widths: Vec<usize> = labels.iter().map(TabText::width).collect();
        if let Some(i) = view::tab_at(&widths, usize::from(rel)) {
            self.show_tab(labels[i].tab);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use teams_client::Preview;

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
            followed: false,
            favorite: None,
            unread: false,
            last: None,
        }
    }

    fn team(id: &str, name: &str, channels: Vec<Channel>) -> Team {
        Team {
            id: id.into(),
            name: name.into(),
            archived: false,
            muted: false,
            members: None,
            channels,
        }
    }

    fn names(t: &Teams, rows: &[TeamRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match *r {
                TeamRow::Favorite { team, channel } => {
                    format!(
                        "* {}",
                        favorite_label(&t.items[team], &t.items[team].channels[channel])
                    )
                }
                TeamRow::Team { team, open } => {
                    format!("{} {}", if open { "v" } else { ">" }, t.items[team].name)
                }
                TeamRow::Channel { team, channel } => {
                    format!("  {}", t.items[team].channels[channel].name)
                }
                TeamRow::More { hidden, open, .. } => {
                    format!("  {}{hidden} more", if open { "-" } else { "+" })
                }
            })
            .collect()
    }

    fn sample() -> Teams {
        let mut general = channel("t1", "General");
        general.general = true;
        general.shown = true;
        let mut pinned = channel("c-pin", "Releases");
        pinned.pinned = true;
        let mut shown = channel("c-shown", "Design");
        shown.shown = true;
        let hidden = channel("c-hid", "Random");
        let mut old = channel("c-old", "Old");
        old.archived = true;
        let mut only_general = channel("t2", "General");
        only_general.general = true;
        only_general.shown = true;
        let mut t = Teams::new();
        // The service's order is not the order shown: shown, hidden, pinned, General last.
        t.receive(vec![
            team("t1", "Eng", vec![shown, hidden, old, pinned, general]),
            team("t2", "Mkt", vec![only_general]),
        ]);
        t
    }

    #[test]
    fn a_team_lists_general_then_pinned_then_shown_then_a_row_for_the_rest() {
        let mut t = sample();
        assert_eq!(
            names(&t, &t.rows("", true)),
            ["> Eng", "> Mkt"],
            "all start closed"
        );
        t.open.insert("t1".into());
        assert_eq!(
            names(&t, &t.rows("", true)),
            [
                "v Eng",
                "  General",
                "  Releases",
                "  Design",
                "  +2 more",
                "> Mkt"
            ]
        );
        t.more.insert("t1".into());
        assert_eq!(
            names(&t, &t.rows("", true)),
            [
                "v Eng",
                "  General",
                "  Releases",
                "  Design",
                "  -2 more",
                "  Random",
                "  Old",
                "> Mkt"
            ],
            "the rest follow the row, in the service's order"
        );
        // A team with nothing hidden has no such row.
        t.open.insert("t2".into());
        assert!(
            !names(&t, &t.rows("", true))
                .last()
                .is_some_and(|l| l.contains("more"))
        );
    }

    #[test]
    fn a_filter_keeps_the_teams_with_a_match_and_opens_them() {
        let mut t = sample();
        assert_eq!(
            names(&t, &t.rows("RAND", true)),
            ["v Eng", "  Random"],
            "any case, and a hidden channel is found and shown"
        );
        assert_eq!(
            names(&t, &t.rows("mkt", true)),
            ["> Mkt"],
            "a team matched by its name keeps its fold state"
        );
        t.open.insert("t2".into());
        assert_eq!(names(&t, &t.rows("mkt", true)), ["v Mkt", "  General"]);
        assert!(t.rows("zzz", true).is_empty());
    }

    /// `sample()` with places in the Favorites folder: Mkt's General first, Eng's Design second, Eng's pinned Releases
    /// third, an archived team's General (not shown) and a channel of the archived team.
    fn with_favourites() -> Teams {
        let mut t = sample();
        let mut items = std::mem::take(&mut t.items);
        let mut place = |team: usize, id: &str, n: u32| {
            let c = items[team]
                .channels
                .iter_mut()
                .find(|c| c.id == id)
                .unwrap();
            c.favorite = Some(n);
        };
        place(1, "t2", 1);
        place(0, "c-shown", 2);
        place(0, "c-pin", 3);
        let mut gone = channel("t3", "General");
        gone.general = true;
        gone.favorite = Some(0);
        let mut archived = team("t3", "Gone", vec![gone]);
        archived.archived = true;
        items.push(archived);
        t.receive(items);
        t
    }

    #[test]
    fn favourites_head_the_list_as_team_arrow_channel_in_the_folders_order() {
        let mut t = with_favourites();
        let rows = t.rows("", true);
        assert_eq!(
            names(&t, &rows),
            [
                "* Mkt › General",
                "* Eng › Design",
                "* Eng › Releases",
                "> Eng",
                "> Mkt"
            ],
            "the folder's order, then the teams as before; the archived team is not shown at all"
        );
        assert_eq!(favorite_count(&rows), 3);
        // They never fold, and the team tree below them is the same one.
        t.open.insert("t1".into());
        let rows = t.rows("", true);
        assert_eq!(favorite_count(&rows), 3);
        assert_eq!(
            names(&t, &rows)[3..],
            [
                "v Eng",
                "  General",
                "  Releases",
                "  Design",
                "  +2 more",
                "> Mkt"
            ]
        );
        // The favourites turned off: no section at all, and so no divider.
        let rows = t.rows("", false);
        assert_eq!(favorite_count(&rows), 0);
        assert_eq!(names(&t, &rows)[0], "v Eng");
    }

    #[test]
    fn no_favourite_channel_means_no_section() {
        let t = sample();
        let rows = t.rows("", true);
        assert_eq!(favorite_count(&rows), 0);
        assert_eq!(names(&t, &rows), ["> Eng", "> Mkt"]);
    }

    #[test]
    fn a_filter_matches_favourites_by_their_team_and_channel_text_and_opens_no_team() {
        let t = with_favourites();
        assert_eq!(
            names(&t, &t.rows("eng › des", true)),
            ["* Eng › Design"],
            "the team's name, the arrow and the channel's, as shown"
        );
        assert_eq!(
            names(&t, &t.rows("mkt", true)),
            ["* Mkt › General", "> Mkt"],
            "a match in the favourites opens nothing"
        );
        assert_eq!(
            names(&t, &t.rows("releases", true)),
            ["* Eng › Releases", "v Eng", "  Releases"],
            "and the tree below is filtered as it always was"
        );
        assert!(
            t.rows("gone", true).is_empty(),
            "an archived team matches nothing"
        );
        assert_eq!(
            names(&t, &t.rows("releases", false)),
            ["v Eng", "  Releases"]
        );
    }

    #[test]
    fn an_archived_team_is_not_listed_but_its_channels_are_still_found_by_id() {
        let t = with_favourites();
        assert_eq!(t.visible().count(), 2);
        assert_eq!(t.items().len(), 3);
        assert!(t.channel("t3").is_some(), "the data stays loaded");
    }

    #[test]
    fn activity_replaces_unread_and_the_newest_message_but_a_missing_one_wipes_nothing() {
        let mut t = sample();
        let at = Utc::now();
        let preview = Preview {
            created: at,
            from_id: None,
            from_name: None,
            text: "hello".into(),
            automated: false,
        };
        t.merge_activity(vec![
            ChannelActivity {
                id: "c-pin".into(),
                unread: true,
                last: Some(preview),
            },
            ChannelActivity {
                id: "not-a-channel".into(),
                unread: true,
                last: None,
            },
        ]);
        let (_, c) = t.channel("c-pin").unwrap();
        assert!(c.unread && c.last.as_ref().unwrap().text == "hello");
        t.merge_activity(vec![ChannelActivity {
            id: "c-pin".into(),
            unread: false,
            last: None,
        }]);
        let (_, c) = t.channel("c-pin").unwrap();
        assert!(!c.unread && c.last.is_some());
    }
}
