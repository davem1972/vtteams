//! The teams the person is in and their channels (read only), and what the conversation list says about the channels
//! it happens to mention. See `docs/teams-web-protocol.md` ("Teams and channels") for how each shape was found.
//!
//! The list of teams is one large answer (about 2.8 MB and a couple of seconds for 32 teams and 242 channels) from the
//! chat service aggregator, so it is asked for rarely. The order the teams are shown in is the person's own, read from
//! a small user property with each read of the list ([`TeamsBackend::order_teams`]). How recently a channel was written in comes from the
//! conversation list the application already polls instead ([`TeamsBackend::note_channel_activity`], read back by
//! [`Backend::channel_activity`](crate::Backend::channel_activity)), which is cheap and fresh but holds only the
//! channels with recent activity. Nothing about a team or channel (names, ids, message text) reaches the log: only
//! counts and timings.

use super::parse::{
    MAX_TEAMS, Parsed, apply_team_order, favourite_channels, mark_favourite_channels,
    parse_channel_activity, parse_team, team_order,
};
use super::{Bearer, Live, TeamsBackend};
use crate::error::{Error, Result};
use crate::lock;
use crate::model::{ChannelActivity, Team};
use crate::watch;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

/// Most channels whose activity is remembered from the conversation list.
pub(super) const MAX_ACTIVITY: usize = 2_000;

/// The map of channel activity the backend keeps between reads of the conversation list.
pub(super) type ActivityMap = HashMap<String, ChannelActivity>;

impl TeamsBackend {
    /// The teams the person is in, read from the chat service aggregator (its `v3` list, which is a fifth the size of
    /// the `v1` one for the same teams). A failure is the same typed error any other aggregator read gets: the caller
    /// treats this as a side call, so a tenant that withholds the aggregator's scope only loses the teams.
    pub(super) fn read_teams(&self) -> Result<Vec<Team>> {
        const OPERATION: &str = "list teams";
        let live = self.live(None)?;
        if live.csa_base.is_empty() {
            return Err(Error::Unsupported {
                operation: OPERATION,
                detail: "the service named no chat service aggregator for this account",
            });
        }
        let url = format!(
            "{}/api/v3/teams/users/me?isPrefetch=false&enableMembershipSummary=true",
            live.csa_base
        );
        let started = Instant::now();
        let answer = self.request_with(Bearer::Csa, OPERATION, "GET", &url, None)?;
        let Some(raw) = answer["teams"].as_array() else {
            return Err(Error::Unexpected {
                operation: OPERATION,
                detail: "no list of teams in the answer",
            });
        };
        self.watch_teams(&answer, raw);

        let mut teams = Vec::new();
        let (mut deleted, mut malformed, mut bad_channels) = (0usize, 0usize, 0usize);
        for item in raw.iter().take(MAX_TEAMS) {
            match parse_team(item) {
                Parsed::Item((team, unreadable)) => {
                    bad_channels += unreadable;
                    teams.push(team);
                }
                Parsed::Skipped => deleted += 1,
                Parsed::Malformed => {
                    malformed += 1;
                    if malformed <= 3 {
                        log::warn!(
                            "{OPERATION}: unreadable team, shape {}",
                            watch::masked(item)
                        );
                    }
                }
            }
        }
        let over_cap = raw.len().saturating_sub(MAX_TEAMS);
        // The order of the teams is the person's own (a user property), not the list's: see `order_teams`.
        let mut teams = self.order_teams(&live, teams);
        // The favourites are the "Favorites" conversation folder the same answer carries (a team's or channel's own
        // `isFavorite` flag is something else); without the folder there are none, and nothing more is asked for.
        let favourites = favourite_channels(&answer["conversationFolders"]["conversationFolders"]);
        let marked = mark_favourite_channels(&mut teams, &favourites);
        log::debug!(
            "{OPERATION}: {} team(s) in the answer, {} read with {} channel(s), {deleted} deleted, {malformed} unreadable, \
             {bad_channels} unreadable channel(s), {over_cap} team(s) over the cap, {} ms",
            raw.len(),
            teams.len(),
            teams.iter().map(|t| t.channels.len()).sum::<usize>(),
            started.elapsed().as_millis()
        );
        if favourites.found {
            log::debug!(
                "{OPERATION}: {} team or channel favourite(s) in the folder, {marked} matched a channel, {} matched none, \
                 {} chat(s) left to the chat list",
                favourites.places.len(),
                favourites.places.len().saturating_sub(marked),
                favourites.others
            );
        } else {
            log::debug!("{OPERATION}: no Favorites folder in the answer");
        }
        if malformed > 0 || bad_channels > 0 {
            self.auth.warn(Error::Unexpected {
                operation: OPERATION,
                detail: "some teams or channels in the answer could not be read; the service format may have changed",
            });
        }
        Ok(teams)
    }

    /// The address of the user property that holds the person's own order of their teams.
    pub(super) fn teams_order_url(live: &Live) -> String {
        format!("{}/v1/users/ME/properties?name=teamsOrder", live.chat_base)
    }

    /// Put `teams` in the person's own order, which is the user property `teamsOrder` (the list of teams is not in the
    /// order the client shows; see `docs/teams-web-protocol.md`): the teams the order names first, in its order, then
    /// every other team alphabetically by name (a guess: how the real client orders those is not known).
    ///
    /// This is a side read of a side call and never fails the list. It is one request, made once with each read of
    /// the list and not repeated. An account with no such property (a 404, or a service that says it is unsupported)
    /// keeps the service's order and says nothing; any other failure, or an answer that is not an order, keeps the
    /// service's order and is reported once through the warnings, with the cause in the log.
    fn order_teams(&self, live: &Live, teams: Vec<Team>) -> Vec<Team> {
        const OPERATION: &str = "read the order of the teams";
        let started = Instant::now();
        let answer = match self.request(OPERATION, "GET", &Self::teams_order_url(live), None) {
            Ok(answer) => answer,
            Err(Error::Status { status: 404, .. } | Error::Unsupported { .. }) => {
                log::debug!("{OPERATION}: the account has no order; the service's order is kept");
                return teams;
            }
            Err(e) => {
                log::debug!("{OPERATION}: {}", e.for_log());
                self.warn_teams_order_unread();
                return teams;
            }
        };
        self.watch.fields("teams order answer", [&answer]);
        let Some(order) = team_order(&answer) else {
            log::debug!("{OPERATION}: the answer is not an order");
            self.warn_teams_order_unread();
            return teams;
        };
        self.watch.fields("teams order entry", order.entries.iter());
        if order.ids.is_empty() {
            log::debug!("{OPERATION}: the account has no order; the service's order is kept");
            return teams;
        }
        let total = teams.len();
        let (teams, matched) = apply_team_order(teams, &order.ids);
        log::debug!(
            "{OPERATION}: {} team id(s) in the order, {matched} matched a team, {} matched none, {} team(s) not in it, {} ms",
            order.ids.len(),
            order.ids.len().saturating_sub(matched),
            total.saturating_sub(matched),
            started.elapsed().as_millis()
        );
        teams
    }

    fn warn_teams_order_unread(&self) {
        self.auth.warn(Error::Unexpected {
            operation: "read the order of the teams",
            detail: "the order of your teams could not be read, so they are shown in the service's order",
        });
    }

    /// Note the structure of the list of teams (names, types and the small closed set of channel types only).
    fn watch_teams(&self, answer: &Value, teams: &[Value]) {
        self.watch.fields("teams answer", [answer]);
        self.watch.fields(
            "teams conversation folders",
            [&answer["conversationFolders"]],
        );
        let folders: Vec<&Value> = answer["conversationFolders"]["conversationFolders"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        self.watch
            .fields("teams conversation folder", folders.iter().copied());
        self.watch.tokens(
            "teams conversation folder type",
            folders.iter().filter_map(|f| f["folderType"].as_str()),
        );
        let items: Vec<&Value> = folders
            .iter()
            .flat_map(|f| {
                f["conversationFolderItems"]
                    .as_array()
                    .into_iter()
                    .flatten()
            })
            .collect();
        self.watch
            .fields("teams conversation folder item", items.iter().copied());
        self.watch.tokens(
            "teams conversation folder item thread type",
            items.iter().filter_map(|i| i["threadType"].as_str()),
        );
        self.watch.fields("team", teams);
        self.watch.fields(
            "team membership summary",
            teams.iter().map(|t| &t["membershipSummary"]),
        );
        self.watch
            .fields("team settings", teams.iter().map(|t| &t["teamSettings"]));
        let channels: Vec<&Value> = teams
            .iter()
            .flat_map(|t| t["channels"].as_array().into_iter().flatten())
            .collect();
        self.watch.fields("channel", channels.iter().copied());
        self.watch.fields(
            "channel last message",
            channels.iter().map(|c| &c["lastMessage"]),
        );
        self.watch.fields(
            "channel read horizon",
            channels.iter().map(|c| &c["consumptionHorizon"]),
        );
        self.watch.tokens(
            "channel last message type",
            channels
                .iter()
                .filter_map(|c| c["lastMessage"]["messageType"].as_str()),
        );
        // The numbers `channelType` takes are not known (docs/teams-web-protocol.md): the first sighting of each
        // lands in the log, beside what `isShared` says, to settle which is which.
        let types: Vec<String> = channels
            .iter()
            .filter_map(|c| {
                Some(format!(
                    "{} shared={}",
                    c["channelType"].as_i64()?,
                    c["isShared"].as_bool().unwrap_or(false)
                ))
            })
            .collect();
        self.watch
            .tokens("channel type", types.iter().map(String::as_str));
    }

    /// Remember what the conversation list says about the teams and channels in it (the entries `chats` leaves out), so
    /// that [`Backend::channel_activity`](crate::Backend::channel_activity) can answer without a request. `convs` is
    /// the list as `chats` sees it, the whole of it on an incremental read too, so each call replaces what was
    /// remembered: a channel that has left the list is forgotten. At most [`MAX_ACTIVITY`] channels are kept.
    pub(super) fn note_channel_activity(&self, convs: &[Value]) {
        let me = self.auth.me().0;
        let mut map = ActivityMap::new();
        let mut over_cap = 0usize;
        for activity in convs.iter().filter_map(|c| parse_channel_activity(c, &me)) {
            if map.len() < MAX_ACTIVITY || map.contains_key(&activity.id) {
                map.insert(activity.id.clone(), activity);
            } else {
                over_cap += 1;
            }
        }
        log::debug!(
            "channel activity: {} channel(s) in the chat list, {} unread, {over_cap} over the cap",
            map.len(),
            map.values().filter(|a| a.unread).count()
        );
        *lock(&self.channel_activity) = map;
    }

    /// The activity [`note_channel_activity`](Self::note_channel_activity) last found, by id so the order is the same
    /// every time.
    pub(super) fn channel_activity_now(&self) -> Vec<ChannelActivity> {
        let mut all: Vec<ChannelActivity> =
            lock(&self.channel_activity).values().cloned().collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        all
    }
}

#[cfg(test)]
pub(in crate::teams) mod tests {
    use super::super::auth::TeamsAuth;
    use super::super::tests::{ADA, ME, backend};
    use super::super::{BrowserOptions, TeamsSession, now};
    use super::*;
    use crate::error::AuthError;
    use crate::model::{Backend, Channel, ChannelKind, Page};
    use crate::store::MemoryStore;
    use crate::testserver::{Reply, Server, serve};
    use chrono::DateTime;
    use serde_json::json;
    use std::sync::Arc;

    /// `base` with the keys of `extra` added or replaced.
    pub(in crate::teams) fn with(mut base: Value, extra: Value) -> Value {
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    }

    fn contact_url(id: &str) -> String {
        format!("https://teams.microsoft.com/api/chatsvc/amer/v1/users/ME/contacts/8:orgid:{id}")
    }

    /// A channel's `lastMessage` in the list of teams, with the keys the service sends there.
    pub(in crate::teams) fn last_message(extra: Value) -> Value {
        with(
            json!({"id": "1763460000123", "composeTime": "2026-11-18T10:00:00.000Z",
                   "originalArrivalTime": "2026-11-18T10:00:01.123Z",
                   "content": "<p>Hello   <b>team</b></p>", "from": contact_url(ADA), "imDisplayName": "Ada",
                   "messageType": "RichText/Html", "parentMessageId": "1763460000123", "clientMessageId": "42",
                   "sequenceId": 7, "version": "1763460000123", "fromDisplayNameInToken": null}),
            extra,
        )
    }

    /// A channel of the list of teams with every key the service sends (and no `lastMessage`: `extra` may add one).
    pub(in crate::teams) fn channel(id: &str, name: &str, extra: Value) -> Value {
        with(
            json!({"id": id, "displayName": name, "description": "", "isGeneral": false, "isArchived": false,
                   "isFavorite": false, "isPinned": false, "isMuted": false, "isFollowed": false, "isShared": false,
                   "isMember": true, "isModerator": false, "channelType": 0, "threadType": "topic",
                   "threadSubType": "", "parentTeamId": "19:aaaa@thread.tacv2", "isMessageRead": true,
                   "isImportantMessageRead": true,
                   "consumptionHorizon": {"clientMessageId": "1", "originalArrivalTime": 1_763_460_000_000_u64,
                                          "timeStamp": 1_763_460_000_000_u64},
                   "isEmptyConversation": false, "lastL2MessageIdNFS": 5, "version": 1, "membershipVersion": 1}),
            extra,
        )
    }

    pub(in crate::teams) fn team(
        id: &str,
        name: &str,
        channels: Vec<Value>,
        extra: Value,
    ) -> Value {
        with(
            json!({"id": id, "displayName": name, "description": "", "isArchived": false, "isCollapsed": false,
                   "isFavorite": false, "isFollowed": true, "isGeneralChannelFavorite": false, "isUserMuted": false,
                   "isDeleted": false, "lastTimeFavorited": "2026-11-01T00:00:00Z",
                   "membershipSummary": {"totalMemberCount": 3, "adminRoleCount": 1},
                   "teamSiteInformation": {"groupId": "g"},
                   "teamSettings": {"generalChannelPosting": 1, "messageThreadingEnabled": true,
                                    "channelMention": true, "teamMention": true},
                   "memberRole": 1, "memberRoleString": "owner", "channels": channels}),
            extra,
        )
    }

    pub(in crate::teams) fn teams_answer(teams: &[Value]) -> String {
        teams_answer_with_folders(teams, json!([]))
    }

    /// The whole answer, its `conversationFolders` object holding `folders` as the service shapes it (an object with
    /// the folders and their order).
    pub(in crate::teams) fn teams_answer_with_folders(teams: &[Value], folders: Value) -> String {
        json!({"chats": [], "conversationFolders": {"conversationFolderOrder": [], "conversationFolders": folders},
               "metadata": {}, "privateFeeds": [], "teams": teams, "users": []})
        .to_string()
    }

    /// A conversation folder as the service sends it.
    pub(in crate::teams) fn folder(kind: &str, deleted: bool, items: Vec<Value>) -> Value {
        json!({"id": format!("folder-{kind}"), "name": kind, "folderType": kind, "sortType": "UserDefinedCustomOrder",
               "isDeleted": deleted, "isExpanded": true, "version": 1, "createdTime": "2026-01-01T00:00:00Z",
               "conversationFolderItems": items})
    }

    pub(in crate::teams) fn folder_item(id: &str, thread_type: &str) -> Value {
        json!({"conversationId": id, "threadType": thread_type, "createdTime": "2026-01-01T00:00:00Z",
               "lastUpdatedTime": "2026-01-01T00:00:00Z"})
    }

    /// A backend that can reach the chat service aggregator at `server`, with a token for it.
    pub(in crate::teams) fn csa_backend(server: &Server) -> TeamsBackend {
        let b = backend(server);
        if let Some(live) = lock(&b.live).as_mut() {
            live.csa_base = format!("{}/api/csa/amer", server.origin());
        }
        *lock(&b.csa) = Some(("csa-token".into(), now() + 3600));
        b
    }

    const TEAMS_PATH: &str =
        "/api/csa/amer/api/v3/teams/users/me?isPrefetch=false&enableMembershipSummary=true";

    const ORDER_PATH: &str = "/api/chatsvc/amer/v1/users/ME/properties?name=teamsOrder";

    /// The answer to the read of the `teamsOrder` property, shaped as the service sends it: `teamsOrder` is a string
    /// holding the JSON array.
    fn order_reply(entries: &Value) -> Reply {
        Reply::status(200).json(
            &json!({"cid": "c", "cidHex": "h", "dogfoodUser": false, "primaryMemberName": "n",
                    "skypeName": "s", "teamsOrder": entries.to_string()})
            .to_string(),
        )
    }

    /// An account that has no order: the answer holds no `teamsOrder` at all.
    fn no_order() -> Reply {
        Reply::status(200).json(r#"{"cid": "c", "cidHex": "h"}"#)
    }

    fn plain_team(id: &str, name: &str, extra: Value) -> Value {
        team(id, name, vec![], extra)
    }

    fn team_names_of(teams: &[Team]) -> Vec<&str> {
        teams.iter().map(|t| t.name.as_str()).collect()
    }

    /// Teams as the service lists them: Zed, Alpha, Mid, Bravo (archived), apple.
    fn listed_teams() -> String {
        teams_answer(&[
            plain_team("t-zed", "Zed", json!({})),
            plain_team("t-a", "Alpha", json!({})),
            plain_team("t-mid", "Mid", json!({})),
            plain_team("t-b", "Bravo", json!({"isArchived": true})),
            plain_team("t-apple", "apple", json!({})),
        ])
    }

    #[test]
    fn the_teams_come_in_the_persons_own_order_and_the_others_follow_alphabetically() {
        // The array's own order disagrees with the order numbers, one id names no team, and the order does not
        // name Bravo (archived) or apple.
        let order = json!([
            {"order": 2, "teamId": "t-zed"},
            {"order": 0, "teamId": "t-mid"},
            {"order": 1, "teamId": "t-stale"},
            {"order": 3, "teamId": "t-a"},
        ]);
        let server = serve(vec![
            Reply::status(200).json(&listed_teams()),
            order_reply(&order),
        ]);
        let b = csa_backend(&server);
        let teams = b.teams().unwrap();
        assert_eq!(
            team_names_of(&teams),
            ["Mid", "Zed", "Alpha", "apple", "Bravo"],
            "ordered by the numbers, then the rest by name without regard to case; the archived team is kept"
        );
        assert!(teams[4].archived);

        let seen = server.seen();
        assert_eq!(seen.len(), 2, "the list and one request for the order");
        assert_eq!(seen[0].path, TEAMS_PATH);
        assert_eq!(seen[1].method, "GET");
        assert_eq!(seen[1].path, ORDER_PATH);
        assert!(seen[1].body.is_empty());
        assert_eq!(
            seen[1].header("authorization"),
            Some("Bearer spaces-token"),
            "the ordinary chat token, not the aggregator's"
        );
        assert_eq!(
            seen[1].header("authentication"),
            Some("skypetoken=skype-token")
        );
        assert!(
            seen[1]
                .headers
                .iter()
                .all(|(_, v)| !v.contains("csa-token")),
            "{:?}",
            seen[1].headers
        );
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn each_read_of_the_list_reads_the_order_again_and_follows_a_change() {
        let server = serve(vec![
            Reply::status(200).json(&listed_teams()),
            order_reply(&json!([{"order": 0, "teamId": "t-a"}])),
            Reply::status(200).json(&listed_teams()),
            order_reply(&json!([{"order": 0, "teamId": "t-apple"}])),
        ]);
        let b = csa_backend(&server);
        assert_eq!(b.teams().unwrap()[0].name, "Alpha");
        assert_eq!(b.teams().unwrap()[0].name, "apple");
        assert_eq!(server.seen().len(), 4);
    }

    #[test]
    fn an_account_with_no_order_keeps_the_services_order_without_a_word() {
        for reply in [
            no_order(),
            order_reply(&json!([])),
            Reply::status(200).json(r#"{"teamsOrder": null}"#),
            Reply::status(200).json(r#"{"teamsOrder": ""}"#),
            Reply::status(404).json(r#"{"message": "no such property"}"#),
        ] {
            let server = serve(vec![Reply::status(200).json(&listed_teams()), reply]);
            let b = csa_backend(&server);
            let teams = b.teams().unwrap();
            assert_eq!(
                team_names_of(&teams),
                ["Zed", "Alpha", "Mid", "Bravo", "apple"]
            );
            assert_eq!(server.seen().len(), 2);
            assert!(b.take_warnings().is_empty());
        }
    }

    #[test]
    fn an_order_that_cannot_be_had_never_fails_the_list_and_is_reported_once() {
        let failures = [
            Reply::status(500).json(r#"{"message": "boom"}"#),
            Reply::status(403).json(r#"{"message": "forbidden"}"#),
            Reply::status(429).header("Retry-After", "7").body(""),
            Reply::status(200).body("this is not json"),
            Reply::status(200).json(r#"{"teamsOrder": "this is not json"}"#),
            Reply::status(200).json(r#"{"teamsOrder": "{\"order\": 1}"}"#),
            Reply::status(200).json(r#"{"teamsOrder": 7}"#),
            Reply::status(200).json(r#"{"teamsOrder": "[1, 2, 3]"}"#),
        ];
        for (n, reply) in failures.into_iter().enumerate() {
            let server = serve(vec![Reply::status(200).json(&listed_teams()), reply]);
            let b = csa_backend(&server);
            let teams = b.teams().unwrap_or_else(|e| panic!("failure {n}: {e:?}"));
            assert_eq!(
                team_names_of(&teams),
                ["Zed", "Alpha", "Mid", "Bravo", "apple"],
                "failure {n}: the service's order"
            );
            assert_eq!(server.seen().len(), 2, "failure {n}: not asked again");
            let warnings = b.take_warnings();
            assert_eq!(warnings.len(), 1, "failure {n}: {warnings:?}");
            assert!(matches!(
                warnings[0],
                Error::Unexpected {
                    operation: "read the order of the teams",
                    ..
                }
            ));
            assert!(
                warnings[0]
                    .to_string()
                    .contains("shown in the service's order"),
                "{}",
                warnings[0]
            );
        }
    }

    fn channel_named<'a>(team: &'a Team, name: &str) -> &'a Channel {
        team.channels
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no channel {name}"))
    }

    fn channel_names(team: &Team) -> Vec<&str> {
        team.channels.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn teams_are_read_with_the_aggregators_own_token_and_parsed_as_the_service_describes_them() {
        let alpha = team(
            "19:aaaa@thread.tacv2",
            "Alpha",
            vec![
                channel(
                    "19:plans@thread.tacv2",
                    "Plans",
                    json!({"isFavorite": true, "isMessageRead": false, "lastMessage": last_message(json!({}))}),
                ),
                channel(
                    "19:aaaa@thread.tacv2",
                    "General",
                    json!({"isGeneral": true, "lastMessage": last_message(json!({}))}),
                ),
                channel(
                    "19:pin@thread.skype",
                    "Pinned",
                    json!({"isPinned": true, "isFollowed": true, "isMuted": true}),
                ),
                channel(
                    "19:old@thread.tacv2",
                    "Old",
                    json!({"isArchived": true, "isMessageRead": false}),
                ),
                channel(
                    "19:secret@thread.skype",
                    "Secret",
                    json!({"channelType": 1}),
                ),
                channel(
                    "19:guests@thread.tacv2",
                    "Guests",
                    json!({"isShared": true, "channelType": 0}),
                ),
                channel(
                    "19:quiet@thread.tacv2",
                    "Quiet",
                    json!({"isMessageRead": false, "lastMessage": {}}),
                ),
                channel("19:two@thread.tacv2", "Two", json!({"channelType": 2})),
                channel("19:seven@thread.tacv2", "Seven", json!({"channelType": 7})),
            ],
            json!({"membershipSummary": {"totalMemberCount": 12}}),
        );
        let bravo = team(
            "19:bbbb@thread.skype",
            "Bravo",
            vec![channel(
                "19:bbbb@thread.skype",
                "General",
                json!({"isGeneral": true}),
            )],
            json!({"isFavorite": true, "isUserMuted": true, "membershipSummary": {"totalMemberCount": "many"}}),
        );
        let gone = team(
            "19:cccc@thread.tacv2",
            "Gone",
            vec![],
            json!({"isDeleted": true}),
        );
        // Nothing but what a bare team has: no channels, no summary, a null where a time was.
        let delta = json!({"id": "19:dddd@thread.tacv2", "displayName": "Delta", "isArchived": true,
                           "lastTimeFavorited": null});
        let server = serve(vec![
            Reply::status(200).json(&teams_answer(&[alpha, gone, delta, bravo])),
            no_order(),
        ]);
        let b = csa_backend(&server);
        assert!(b.can_list_teams());
        let teams = b.teams().unwrap();

        let names: Vec<&str> = teams.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            ["Alpha", "Delta", "Bravo"],
            "the deleted team gone, the rest in the service's order (a team's own `isFavorite` is not a favourite)"
        );
        let (alpha, delta, bravo) = (&teams[0], &teams[1], &teams[2]);
        assert!(bravo.muted && !bravo.archived);
        assert_eq!(
            bravo.members, None,
            "a count that is not a number is not a count"
        );
        assert_eq!(alpha.members, Some(12));
        assert!(!alpha.muted && !alpha.archived);
        assert!(
            teams
                .iter()
                .flat_map(|t| &t.channels)
                .all(|c| c.favorite.is_none()),
            "no folder in the answer, so no favourites, whatever the channels' own flags say"
        );
        assert!(delta.archived && delta.channels.is_empty() && delta.members.is_none());

        assert_eq!(
            channel_names(alpha),
            [
                "General", "Pinned", "Plans", "Old", "Secret", "Guests", "Quiet", "Two", "Seven"
            ],
            "General first, then pinned, then the service's order"
        );
        let general = channel_named(alpha, "General");
        assert!(general.general && general.shown && !general.unread);
        assert_eq!(general.kind, ChannelKind::Standard);
        let last = general.last.as_ref().expect("a last message");
        assert_eq!(last.text, "Hello team");
        assert_eq!(last.from_name.as_deref(), Some("Ada"));
        assert_eq!(last.from_id.as_deref(), Some(ADA));
        assert!(!last.automated);
        assert_eq!(
            last.created,
            DateTime::parse_from_rfc3339("2026-11-18T10:00:01.123Z").unwrap(),
            "the arrival time, not the compose time"
        );
        let plans = channel_named(alpha, "Plans");
        assert!(plans.shown && plans.unread && plans.last.is_some() && !plans.general);
        let pinned = channel_named(alpha, "Pinned");
        assert!(pinned.pinned && pinned.followed && pinned.muted && !pinned.shown);
        let old = channel_named(alpha, "Old");
        assert!(old.archived, "archived");
        assert!(!old.unread, "nothing to catch up on in an archived channel");
        assert_eq!(channel_named(alpha, "Secret").kind, ChannelKind::Private);
        assert_eq!(
            channel_named(alpha, "Guests").kind,
            ChannelKind::Shared,
            "isShared says shared whatever the type number"
        );
        assert_eq!(channel_named(alpha, "Two").kind, ChannelKind::Shared);
        assert_eq!(
            channel_named(alpha, "Seven").kind,
            ChannelKind::Standard,
            "a type number nobody knows is standard"
        );
        let quiet = channel_named(alpha, "Quiet");
        assert!(quiet.unread && quiet.last.is_none());
        assert!(channel_named(bravo, "General").last.is_none());

        let seen = server.seen();
        assert_eq!(
            seen.len(),
            2,
            "two requests: the whole list and the order of the teams"
        );
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].path, TEAMS_PATH);
        assert_eq!(seen[0].header("authorization"), Some("Bearer csa-token"));
        assert_eq!(
            seen[0].header("authentication"),
            Some("skypetoken=skype-token")
        );
        assert!(
            seen[0]
                .headers
                .iter()
                .all(|(_, v)| !v.contains("spaces-token")),
            "the chat service's own token stays at the chat service: {:?}",
            seen[0].headers
        );
        assert!(b.take_warnings().is_empty());
    }

    /// Teams the way the answer for the surveyed account was shaped: many carry `isFavorite` (a team's or channel's own
    /// flag, which is not the person's favourites), and the favourites are the folder.
    fn favourites_fixture() -> Vec<Value> {
        let general = |id: &str| {
            channel(
                id,
                "General",
                json!({"isGeneral": true, "isFavorite": true}),
            )
        };
        vec![
            team(
                "19:kernel@thread.tacv2",
                "Platform Group",
                vec![
                    channel(
                        "19:ksched@thread.tacv2",
                        "Scheduler",
                        json!({"isFavorite": true}),
                    ),
                    general("19:kernel@thread.tacv2"),
                ],
                json!({"isFavorite": true}),
            ),
            team(
                "19:tools@thread.skype",
                "Tooling",
                vec![
                    general("19:tools@thread.skype"),
                    channel("19:preview@thread.skype", "Preview Programme", json!({})),
                    channel(
                        "19:other@thread.skype",
                        "Other",
                        json!({"isFavorite": true}),
                    ),
                ],
                json!({"isFavorite": true}),
            ),
            team(
                "19:old@thread.tacv2",
                "Old team",
                vec![general("19:old@thread.tacv2")],
                json!({"isFavorite": true, "isArchived": true}),
            ),
            team(
                "19:plain@thread.tacv2",
                "Plain",
                vec![general("19:plain@thread.tacv2")],
                json!({}),
            ),
        ]
    }

    #[test]
    fn the_favorites_folder_in_the_answer_marks_the_favourite_channels_in_its_own_order() {
        let folders = json!([
            // A folder the service marks deleted is not the favourites, whatever it holds.
            folder(
                "Favorites",
                true,
                vec![folder_item("19:plain@thread.tacv2", "space")]
            ),
            folder(
                "Favorites",
                false,
                vec![
                    folder_item("19:preview@thread.skype", "topic"),
                    folder_item("19:a_chat@thread.v2", "chat"),
                    folder_item("19:kernel@thread.tacv2", "space"),
                    folder_item("19:gone@thread.tacv2", "topic"),
                    folder_item("19:preview@thread.skype", "topic"),
                    json!({"threadType": "topic"}),
                    json!("not an object"),
                ],
            ),
            folder(
                "Custom",
                false,
                vec![folder_item("19:other@thread.skype", "topic")]
            ),
        ]);
        let server = serve(vec![
            Reply::status(200).json(&teams_answer_with_folders(&favourites_fixture(), folders)),
            no_order(),
        ]);
        let b = csa_backend(&server);
        let teams = b.teams().unwrap();

        let places: Vec<(&str, &str, Option<u32>)> = teams
            .iter()
            .flat_map(|t| {
                t.channels
                    .iter()
                    .filter(|c| c.favorite.is_some())
                    .map(|c| (t.name.as_str(), c.name.as_str(), c.favorite))
            })
            .collect();
        assert_eq!(
            places,
            [
                ("Platform Group", "General", Some(2)),
                ("Tooling", "Preview Programme", Some(1)),
            ],
            "by the folder's order, the team's root item on its General channel (a chat takes no place of its own; \
             nor does a repeat)"
        );
        // `isFavorite` anywhere else marks nothing.
        assert_eq!(
            channel_named(&teams[0], "Scheduler").favorite,
            None,
            "a channel's own flag"
        );
        assert_eq!(channel_named(&teams[1], "General").favorite, None);
        assert_eq!(
            channel_named(&teams[3], "General").favorite,
            None,
            "the deleted folder"
        );
        assert_eq!(
            server.seen().len(),
            2,
            "no request beyond the list and the order of the teams"
        );
        assert!(b.take_warnings().is_empty());
        // The archived team is read like any other (the application decides to hide it).
        assert!(teams[2].archived);
    }

    #[test]
    fn an_answer_without_the_favorites_folder_has_no_favourites_and_asks_for_nothing_more() {
        for answer in [
            teams_answer(&favourites_fixture()),
            teams_answer_with_folders(
                &favourites_fixture(),
                json!([folder("Custom", false, vec![folder_item("19:preview@thread.skype", "topic")])]),
            ),
            // The folders in the shape the older path has them (no wrapping object) are not this answer's.
            json!({"conversationFolders": [folder("Favorites", false, vec![folder_item("19:preview@thread.skype", "topic")])],
                   "teams": favourites_fixture()})
            .to_string(),
        ] {
            let server = serve(vec![Reply::status(200).json(&answer), no_order()]);
            let b = csa_backend(&server);
            let teams = b.teams().unwrap();
            assert_eq!(teams.len(), 4);
            assert!(
                teams
                    .iter()
                    .flat_map(|t| &t.channels)
                    .all(|c| c.favorite.is_none())
            );
            assert_eq!(server.seen().len(), 2);
            assert!(b.take_warnings().is_empty());
        }
    }

    #[test]
    fn text_from_the_service_is_made_safe_and_short_before_it_is_shown() {
        let hostile = "Al\u{1b}[31mpha\u{202e}\u{200f}\nSecond\tline\u{7}";
        let long = "a".repeat(300);
        let server = serve(vec![Reply::status(200).json(&teams_answer(&[team(
            "19:aaaa@thread.tacv2",
            hostile,
            vec![
                channel(
                    "19:aaaa@thread.tacv2",
                    "Gen\u{0}eral\r\u{85}",
                    json!({"isGeneral": true, "lastMessage": last_message(json!({
                        "content": format!("<p>Hi\u{1b}[2J there \u{202e}evil</p><p>{long}</p>"),
                        "imDisplayName": "A\u{1b}da\u{202e}",
                    }))}),
                ),
                channel(
                    "19:text@thread.tacv2",
                    "Text",
                    json!({"lastMessage": last_message(json!({
                        "messageType": "Text", "content": "line one\r\nline\u{1b} two\n\n  three",
                    }))}),
                ),
                channel(
                    "19:system@thread.tacv2",
                    "System",
                    json!({"lastMessage": last_message(json!({"messageType": "ThreadActivity/AddMember"}))}),
                ),
                channel(
                    "19:nokind@thread.tacv2",
                    "NoKind",
                    json!({"lastMessage": last_message(json!({"messageType": null}))}),
                ),
                channel(
                    "19:notime@thread.tacv2",
                    "NoTime",
                    json!({"lastMessage": {"id": "5", "messageType": "Text", "content": "x"}}),
                ),
                channel(
                    "19:noid@thread.tacv2",
                    "NoId",
                    json!({"lastMessage": {"messageType": "Text", "content": "x",
                                           "originalArrivalTime": "2026-11-18T10:00:01Z"}}),
                ),
            ],
            json!({}),
        )])), no_order()]);
        let teams = csa_backend(&server).teams().unwrap();
        let alpha = &teams[0];
        assert_eq!(
            alpha.name, "Al[31mpha Second line",
            "no controls or direction marks, one line"
        );
        let general = channel_named(alpha, "General");
        assert_eq!(general.name, "General");
        let last = general.last.as_ref().unwrap();
        assert_eq!(last.from_name.as_deref(), Some("Ada"));
        assert_eq!(last.text.chars().count(), 120, "cut at 120 characters");
        assert!(
            last.text.starts_with("Hi[2J there evil aaaa"),
            "{}",
            last.text
        );
        assert!(
            last.text
                .chars()
                .all(|c| !c.is_control() && !matches!(c, '\u{202e}' | '\u{200f}')),
            "{:?}",
            last.text
        );
        assert_eq!(
            channel_named(alpha, "Text").last.as_ref().unwrap().text,
            "line one line two three"
        );
        assert!(
            channel_named(alpha, "System").last.is_none(),
            "a system message is not a preview"
        );
        assert!(channel_named(alpha, "NoKind").last.is_none());
        assert!(channel_named(alpha, "NoTime").last.is_none());
        assert!(channel_named(alpha, "NoId").last.is_none());
    }

    #[test]
    fn at_most_five_hundred_teams_and_a_thousand_channels_to_a_team_are_taken() {
        let channels: Vec<Value> = (0..=1_000)
            .map(|i| {
                channel(
                    &format!("19:c{i}@thread.tacv2"),
                    &format!("C{i}"),
                    json!({}),
                )
            })
            .collect();
        let teams: Vec<Value> = (0..=500)
            .map(|i| {
                team(
                    &format!("19:t{i}@thread.tacv2"),
                    &format!("T{i}"),
                    if i == 0 { channels.clone() } else { vec![] },
                    json!({}),
                )
            })
            .collect();
        let server = serve(vec![
            Reply::status(200).json(&teams_answer(&teams)),
            no_order(),
        ]);
        let b = csa_backend(&server);
        let read = b.teams().unwrap();
        assert_eq!(read.len(), 500);
        assert_eq!(read[499].name, "T499");
        assert_eq!(read[0].channels.len(), 1_000);
        assert_eq!(read[0].channels[999].name, "C999");
        assert!(
            b.take_warnings().is_empty(),
            "a cap is a limit of ours, not a sign the format changed"
        );
    }

    #[test]
    fn what_cannot_be_read_is_left_out_and_reported_in_one_warning_for_the_read() {
        let good = team(
            "19:good@thread.tacv2",
            "Good",
            vec![
                channel("19:ok@thread.tacv2", "Fine", json!({})),
                channel("19:x@thread.tacv2", "No id", json!({"id": null})),
                channel("19:blank@thread.tacv2", "\u{1b}\u{202e}  ", json!({})),
                json!("not even an object"),
                channel("19:ok2@thread.tacv2", "Also fine", json!({})),
            ],
            json!({}),
        );
        let answer = teams_answer(&[
            json!({"displayName": "No id"}),
            team("19:noname@thread.tacv2", "\u{1b}", vec![], json!({})),
            json!({"id": "19:null@thread.tacv2", "displayName": null}),
            json!(42),
            good,
        ]);
        let server = serve(vec![
            Reply::status(200).json(&answer),
            no_order(),
            Reply::status(200).json(&teams_answer(&[])),
            no_order(),
        ]);
        let b = csa_backend(&server);
        let teams = b.teams().unwrap();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].name, "Good");
        assert_eq!(channel_names(&teams[0]), ["Fine", "Also fine"]);
        let warnings = b.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(matches!(
            warnings[0],
            Error::Unexpected {
                operation: "list teams",
                ..
            }
        ));
        // A clean read afterwards says nothing more.
        assert!(b.teams().unwrap().is_empty());
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn an_answer_that_is_not_a_list_of_teams_is_an_error_and_no_aggregator_is_no_request() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"chats": [], "teams": {"not": "a list"}}"#),
            Reply::status(200).json(r#"{"chats": []}"#),
        ]);
        let b = csa_backend(&server);
        assert!(matches!(
            b.teams(),
            Err(Error::Unexpected {
                operation: "list teams",
                ..
            })
        ));
        assert!(matches!(b.teams(), Err(Error::Unexpected { .. })));

        let server = serve(vec![]);
        let b = backend(&server);
        assert!(matches!(
            b.teams(),
            Err(Error::Unsupported {
                operation: "list teams",
                ..
            })
        ));
        assert!(server.seen().is_empty());
    }

    #[test]
    fn a_refusal_is_a_typed_error_and_never_a_lost_sign_in_by_itself() {
        let server = serve(vec![
            Reply::status(403).json(r#"{"message":"forbidden"}"#),
            Reply::status(429).header("Retry-After", "7").body(""),
            Reply::status(500).json(r#"{"message":"boom"}"#),
            Reply::status(404).json(r#"{"message":"no such service"}"#),
        ]);
        let b = csa_backend(&server);
        let e = b.teams().unwrap_err();
        assert!(matches!(e, Error::Status { status: 403, .. }), "{e:?}");
        assert!(!e.needs_sign_in());
        match b.teams() {
            Err(Error::Throttled {
                operation: "list teams",
                retry_after: Some(d),
            }) => assert_eq!(d.as_secs(), 7),
            other => panic!("{other:?}"),
        }
        let e = b.teams().unwrap_err();
        assert!(matches!(e, Error::Status { status: 500, .. }), "{e:?}");
        assert!(e.is_transient() && !e.needs_sign_in());
        assert!(matches!(
            b.teams().unwrap_err(),
            Error::Status { status: 404, .. }
        ));
        assert!(b.take_warnings().is_empty(), "a failed read is the error");
    }

    /// A backend whose tokens are renewed at `server`, so the test can count what it asks for.
    fn csa_backend_renewing_at(server: &Server) -> TeamsBackend {
        let mut b = csa_backend(server);
        b.auth = TeamsAuth::new(
            TeamsSession::for_test(ME, "Me"),
            Arc::new(MemoryStore::new()),
            BrowserOptions::new("unused-profile"),
        )
        .with_endpoints(server.origin(), &server.url("/v1.0/authz"));
        b
    }

    #[test]
    fn a_refused_side_token_is_renewed_alone_without_renewing_the_whole_sign_in() {
        let server = serve(vec![
            Reply::status(401).json(r#"{"message":"token expired"}"#),
            Reply::status(200).json(r#"{"access_token":"csa-renewed","expires_in":3600}"#),
            Reply::status(200).json(&teams_answer(&[team(
                "19:t@thread.tacv2",
                "Platform Group",
                vec![],
                json!({}),
            )])),
            Reply::status(404),
        ]);
        let b = csa_backend_renewing_at(&server);
        let teams = b.teams().unwrap();
        assert_eq!(teams.len(), 1);
        let paths: Vec<String> = server.seen().iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths
                .iter()
                .filter(|p| p.ends_with("/oauth2/v2.0/token"))
                .count(),
            1,
            "one token request, for the aggregator's scope: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.contains("/authz")),
            "the main sign-in was not renewed: {paths:?}"
        );
        assert_eq!(paths.len(), 4, "{paths:?}");
        assert_eq!(
            lock(&b.csa).as_ref().map(|(t, _)| t.as_str()),
            Some("csa-renewed")
        );
    }
    #[test]
    fn a_tenant_that_withholds_the_aggregators_scope_fails_the_teams_alone() {
        let server = serve(vec![Reply::status(400).json(
            r#"{"error":"invalid_scope","error_description":"AADSTS70011: the scope is not valid"}"#,
        )]);
        let mut b = csa_backend(&server);
        b.auth = TeamsAuth::new(
            TeamsSession::for_test(ME, "Me"),
            Arc::new(MemoryStore::new()),
            BrowserOptions::new("unused-profile"),
        )
        .with_endpoints(server.origin(), &server.url("/v1.0/authz"));
        *lock(&b.csa) = None;
        match b.teams() {
            Err(Error::Auth(AuthError::Rejected { code, .. })) => assert_eq!(code, "invalid_scope"),
            other => panic!("{other:?}"),
        }
        let seen = server.seen();
        assert_eq!(seen.len(), 1, "only the renewal was asked, no list");
        assert!(
            seen[0].path.ends_with("/oauth2/v2.0/token"),
            "{}",
            seen[0].path
        );
    }

    fn ms(iso: &str) -> i64 {
        DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .timestamp_millis()
    }

    /// A `lastMessage` of a conversation-list entry (lower-case names, unlike the list of teams'), by `who`.
    fn entry_last(who: &str, at: &str, extra: Value) -> Value {
        with(
            json!({"id": "1763460005000", "composetime": at, "originalarrivaltime": at,
                   "content": "<p>Something   new</p>", "from": contact_url(who), "imdisplayname": "Ada",
                   "messagetype": "RichText/Html"}),
            extra,
        )
    }

    /// A team root or channel as the conversation list has it: `marker` is when the person last read it.
    fn channel_entry(
        id: &str,
        thread_type: &str,
        product: &str,
        marker: Option<&str>,
        last: Option<Value>,
    ) -> Value {
        let mut v = json!({"id": id, "threadProperties": {"threadType": thread_type, "productThreadType": product,
                           "groupId": "g", "spaceThreadTopic": "TOPSECRET-SPACE"}, "properties": {}});
        if let Some(m) = marker {
            v["properties"]["consumptionhorizon"] = json!(format!("{};{};1", ms(m), ms(m)));
        }
        if let Some(l) = last {
            v["lastMessage"] = l;
        }
        v
    }

    fn chat_entry(name: &str) -> Value {
        json!({"id": format!("19:{name}@thread.v2"), "threadProperties": {"topic": name},
               "lastMessage": {"messagetype": "Text", "content": "x", "originalarrivaltime": "2026-11-18T09:00:00Z"}})
    }

    fn conversations_answer(entries: &[Value], sync: Option<&str>) -> String {
        let mut v = json!({ "conversations": entries });
        if let Some(link) = sync {
            v["_metadata"] = json!({ "syncState": link });
        }
        v.to_string()
    }

    fn activity_of(b: &TeamsBackend, id: &str) -> ChannelActivity {
        b.channel_activity()
            .into_iter()
            .find(|a| a.id == id)
            .unwrap_or_else(|| panic!("no activity for {id}"))
    }

    #[test]
    fn the_conversation_list_hands_over_channel_activity_and_the_chats_are_as_they_were() {
        let t = "2026-11-18T10:00:00Z";
        let before = "2026-11-18T09:00:00Z";
        let after = "2026-11-18T11:00:00Z";
        let entries = vec![
            chat_entry("a"),
            // A team's root conversation is its General channel's.
            channel_entry(
                "19:t@thread.tacv2",
                "space",
                "TeamsTeam",
                Some(before),
                Some(entry_last(ADA, t, json!({}))),
            ),
            channel_entry(
                "19:std@thread.tacv2",
                "topic",
                "TeamsStandardChannel",
                Some(after),
                Some(entry_last(ADA, t, json!({}))),
            ),
            channel_entry(
                "19:priv@thread.skype",
                "space",
                "TeamsPrivateChannel",
                Some(before),
                Some(entry_last(ME, t, json!({}))),
            ),
            channel_entry(
                "19:auto@thread.tacv2",
                "topic",
                "TeamsStandardChannel",
                Some(before),
                Some(entry_last(
                    ADA,
                    t,
                    json!({"s2spartnername": "recordings", "imdisplayname": null}),
                )),
            ),
            channel_entry(
                "19:nomarker@thread.tacv2",
                "topic",
                "TeamsStandardChannel",
                None,
                Some(entry_last(ADA, t, json!({}))),
            ),
            channel_entry(
                "19:empty@thread.tacv2",
                "topic",
                "TeamsStandardChannel",
                None,
                None,
            ),
            // Told apart by `spaceType` alone, with no thread type.
            with(
                channel_entry(
                    "19:space@thread.tacv2",
                    "",
                    "TeamsStandardChannel",
                    Some(before),
                    Some(entry_last(ADA, t, json!({}))),
                ),
                json!({"threadProperties": {"spaceType": "standard", "productThreadType": "TeamsStandardChannel"}}),
            ),
            // Read up to the very moment of its newest message: nothing after the marker.
            channel_entry(
                "19:edge@thread.tacv2",
                "topic",
                "TeamsStandardChannel",
                Some(t),
                Some(entry_last(ADA, t, json!({}))),
            ),
            // A kind of conversation nobody has described to us.
            channel_entry(
                "19:other@thread.tacv2",
                "topic",
                "TeamsSomethingNew",
                Some(before),
                Some(entry_last(ADA, t, json!({}))),
            ),
        ];
        let server = serve(vec![
            Reply::status(200).json(&conversations_answer(&entries, None)),
        ]);
        let b = backend(&server);
        assert!(
            b.channel_activity().is_empty(),
            "nothing is known before a list has been read"
        );

        let chats = b.chats().unwrap();
        assert_eq!(
            chats.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["19:a@thread.v2"],
            "channels and team roots are not chats"
        );
        assert_eq!(server.seen().len(), 1, "the activity costs no request");

        let activity = b.channel_activity();
        let ids: Vec<&str> = activity.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "19:auto@thread.tacv2",
                "19:edge@thread.tacv2",
                "19:empty@thread.tacv2",
                "19:nomarker@thread.tacv2",
                "19:priv@thread.skype",
                "19:space@thread.tacv2",
                "19:std@thread.tacv2",
                "19:t@thread.tacv2",
            ],
            "every team and channel, the same order every time, and not the chat or the unknown kind"
        );
        let unread = |id: &str| activity_of(&b, id).unread;
        assert!(unread("19:t@thread.tacv2"), "a message after the marker");
        assert!(!unread("19:std@thread.tacv2"), "read after it arrived");
        assert!(!unread("19:priv@thread.skype"), "my own message");
        assert!(!unread("19:auto@thread.tacv2"), "a service's notice");
        assert!(
            unread("19:nomarker@thread.tacv2"),
            "never opened, with a message"
        );
        assert!(
            !unread("19:empty@thread.tacv2"),
            "never opened, nothing in it"
        );
        assert!(unread("19:space@thread.tacv2"));
        assert!(
            !unread("19:edge@thread.tacv2"),
            "the marker is the message's own time"
        );

        let std = activity_of(&b, "19:std@thread.tacv2");
        let last = std.last.expect("its newest message");
        assert_eq!(last.text, "Something new");
        assert_eq!(last.from_id.as_deref(), Some(ADA));
        assert_eq!(last.created.timestamp_millis(), ms(t));
        assert!(activity_of(&b, "19:empty@thread.tacv2").last.is_none());
        assert!(
            activity_of(&b, "19:auto@thread.tacv2")
                .last
                .unwrap()
                .automated
        );
    }

    #[test]
    fn later_reads_of_the_list_bring_the_activity_up_to_date_and_a_failed_read_keeps_it() {
        let before = "2026-11-18T09:00:00Z";
        let t = "2026-11-18T10:00:00Z";
        let x = |marker: &str, last: Option<Value>| {
            channel_entry(
                "19:x@thread.tacv2",
                "topic",
                "TeamsStandardChannel",
                Some(marker),
                last,
            )
        };
        let server = serve(vec![
            Reply::status(200).json(&conversations_answer(
                &[
                    chat_entry("a"),
                    x(before, Some(entry_last(ADA, t, json!({})))),
                    channel_entry(
                        "19:y@thread.tacv2",
                        "topic",
                        "TeamsStandardChannel",
                        Some(t),
                        Some(entry_last(ADA, t, json!({}))),
                    ),
                ],
                Some("{origin}/api/chatsvc/amer/sync/1"),
            )),
            // The change feed: X was read since (and comes without its last message, as a feed sends it), Z is new.
            Reply::status(200).json(&conversations_answer(
                &[
                    x("2026-11-18T10:30:00Z", Some(json!({}))),
                    channel_entry(
                        "19:z@thread.tacv2",
                        "topic",
                        "TeamsStandardChannel",
                        Some(before),
                        Some(entry_last(ADA, t, json!({}))),
                    ),
                ],
                Some("{origin}/api/chatsvc/amer/sync/2"),
            )),
            Reply::status(500).json(r#"{"message":"boom"}"#),
        ]);
        let b = backend(&server);
        b.chats().unwrap();
        assert!(activity_of(&b, "19:x@thread.tacv2").unread);
        assert!(!activity_of(&b, "19:y@thread.tacv2").unread);

        let chats = b.chats().unwrap();
        assert_eq!(chats.len(), 1, "still just the one chat");
        assert!(
            server.seen()[1].path.ends_with("/sync/1"),
            "{}",
            server.seen()[1].path
        );
        let x = activity_of(&b, "19:x@thread.tacv2");
        assert!(!x.unread, "read since");
        assert_eq!(
            x.last.expect("the last message it had stays").text,
            "Something new"
        );
        let y = activity_of(&b, "19:y@thread.tacv2");
        assert!(
            !y.unread && y.last.is_some(),
            "a channel the change feed did not mention is as it was"
        );
        assert!(activity_of(&b, "19:z@thread.tacv2").unread);
        assert_eq!(b.channel_activity().len(), 3);

        assert!(b.chats().is_err());
        assert_eq!(
            b.channel_activity().len(),
            3,
            "a read that failed leaves what was known"
        );
        assert!(!activity_of(&b, "19:x@thread.tacv2").unread);
    }

    #[test]
    fn no_more_than_two_thousand_channels_are_remembered() {
        let entries: Vec<Value> = (0..=2_000)
            .map(|i| {
                channel_entry(
                    &format!("19:c{i}@thread.tacv2"),
                    "topic",
                    "TeamsStandardChannel",
                    None,
                    None,
                )
            })
            .collect();
        let server = serve(vec![
            Reply::status(200).json(&conversations_answer(&entries, None)),
        ]);
        let b = backend(&server);
        b.chats().unwrap();
        let known = b.channel_activity();
        assert_eq!(known.len(), 2_000);
        assert!(known.iter().any(|a| a.id == "19:c0@thread.tacv2"));
        assert!(known.iter().all(|a| a.id != "19:c2000@thread.tacv2"));
    }

    /// A message of a channel's feed: a post (`root` is its own id, it may have a subject) or a reply to `root`.
    fn channel_message(
        id: u64,
        root: u64,
        version: u64,
        subject: Option<&str>,
        body: &str,
    ) -> Value {
        let mut m = json!({"id": id.to_string(), "rootMessageId": root.to_string(),
                           "version": version.to_string(), "messagetype": "RichText/Html",
                           "content": format!("<p>{body}</p>"), "from": contact_url(ADA), "imdisplayname": "Ada",
                           "originalarrivaltime": format!("2026-11-18T10:{:02}:00Z", id % 100),
                           "properties": {}});
        if let Some(s) = subject {
            m["properties"]["subject"] = json!(s);
        }
        m
    }

    #[test]
    fn a_channels_posts_and_replies_are_read_like_a_chats_and_an_incremental_read_keeps_the_threads()
     {
        const CHANNEL: &str = "19:chan@thread.tacv2";
        let post = channel_message(1_700, 1_700, 1_700_000, Some("Plan"), "the post");
        let reply = channel_message(1_710, 1_700, 1_710_000, None, "a reply");
        let server = serve(vec![
            Reply::status(200).json(
                &json!({"messages": [reply, post],
                    "_metadata": {"backwardLink": "{origin}/api/chatsvc/amer/older"}})
                .to_string(),
            ),
            // What changed: the reply was edited, and a second reply (to the same post) arrived.
            Reply::status(200).json(
                &json!({"messages": [
                    channel_message(1_720, 1_700, 1_720_000, None, "another reply"),
                    channel_message(1_710, 1_700, 1_730_000, None, "a reply, edited"),
                ]})
                .to_string(),
            ),
        ]);
        let b = backend(&server);
        let first = b.messages(CHANNEL, None).unwrap();
        let summary = |p: &Page| -> Vec<(String, Option<String>, Option<String>)> {
            p.messages
                .iter()
                .map(|m| (m.id.clone(), m.root_id.clone(), m.subject.clone()))
                .collect()
        };
        assert_eq!(
            summary(&first),
            [
                ("1700".to_string(), None, Some("Plan".to_string())),
                ("1710".to_string(), Some("1700".to_string()), None),
            ],
            "a post names no root and has its subject; a reply names its post and has no subject"
        );
        let second = b.messages(CHANNEL, None).unwrap();
        assert_eq!(
            summary(&second),
            [
                ("1700".to_string(), None, Some("Plan".to_string())),
                ("1710".to_string(), Some("1700".to_string()), None),
                ("1720".to_string(), Some("1700".to_string()), None),
            ]
        );
        assert!(second.messages[1].body.contains("edited"));
        assert!(
            server.seen()[1].path.contains("&startTime=1705000"),
            "the second read asked only for what changed: {}",
            server.seen()[1].path
        );
    }
}
