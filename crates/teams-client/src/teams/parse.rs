//! Pure functions that turn Teams web API JSON into the crate's model, and build the JSON it expects back.
//!
//! No I/O happens here, so everything is covered by plain unit tests. The shapes come from the msteams-mcp project's
//! notes and have not been checked against every tenant.

use crate::emoji::reaction_emoji;
use crate::http::urlencode;
use crate::model::{
    Attachment, Availability, Channel, ChannelActivity, ChannelKind, Chat, ChatKind, Contact,
    Delivery, Msg, OutMention, Presence, Preview, Reaction, ReplyTarget, StatusNote, Team,
    unread_after,
};
use crate::text::{clean, html_to_plain};
use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};
use std::collections::HashMap;

/// What parsing one element of a list found.
#[derive(Debug)]
pub(super) enum Parsed<T> {
    /// A usable item.
    Item(T),
    /// Deliberately not shown (a channel, a typing indicator, a hidden or empty conversation).
    Skipped,
    /// Should have been usable but lacks something required: the service format may have changed.
    Malformed,
}

impl<T> Parsed<T> {
    /// The item, if there was one.
    #[cfg(test)]
    pub(super) fn item(self) -> Option<T> {
        match self {
            Self::Item(t) => Some(t),
            Self::Skipped | Self::Malformed => None,
        }
    }
}

/// `8:orgid:<guid>`, or a contact URL ending in one, -> the bare id. Other kinds of sender (bots) keep their MRI.
pub(super) fn mri(s: &str) -> Option<String> {
    let tail = s.rsplit('/').next()?;
    let id = tail.strip_prefix("8:orgid:").unwrap_or(tail);
    (!id.is_empty()).then(|| id.to_string())
}

fn time(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc))
            .or_else(|| millis(v)),
        _ => millis(v),
    }
}

/// Unix milliseconds as a number or numeric string.
fn millis(v: &Value) -> Option<DateTime<Utc>> {
    let ms = v
        .as_i64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|ms| *ms > 0)?;
    Utc.timestamp_millis_opt(ms).single()
}

/// Properties are sometimes JSON documents stored as strings.
fn embedded(v: &Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        other => other.clone(),
    }
}

/// The conversation everyone has with themselves ("Chat with yourself" in Teams; it can be a favourite). Its id is not
/// like the others and it has no members in the list.
pub(super) const NOTES_ID: &str = "48:notes";
/// What it is called here: the Teams client shows the person's own name, which a list entry does not carry.
pub(super) const NOTES_TITLE: &str = "Notes to self";

fn kind_of(id: &str, v: &Value) -> ChatKind {
    if id == NOTES_ID {
        ChatKind::OneOnOne
    } else if id.contains("meeting_")
        || v["threadProperties"]["threadType"].as_str() == Some("meeting")
    {
        ChatKind::Meeting
    } else if id.ends_with("@unq.gbl.spaces") {
        ChatKind::OneOnOne
    } else {
        ChatKind::Group
    }
}

/// A 1:1 chat id is `19:<oid>_<oid>@unq.gbl.spaces`, so its members need no lookup.
pub(super) fn pair_members(id: &str) -> Vec<String> {
    let inner = id
        .trim_start_matches("19:")
        .trim_end_matches("@unq.gbl.spaces");
    let mut ids: Vec<String> = inner
        .split('_')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    ids.dedup();
    ids
}

/// The emoji for a Teams reaction key. The classic six are words (`like`, `heart`, ...); everything else is the code
/// point(s) in hex, then a name: `1f37b_clinkingbeermugs`, or `1f468-200d-1f4bb_manTechnologist` for a sequence.
fn reaction_glyph(key: &str) -> String {
    let Some((codes, _name)) = key.split_once('_') else {
        return word_or_glyph(key);
    };
    let chars: Option<Vec<char>> = codes
        .split('-')
        .map(|h| u32::from_str_radix(h, 16).ok().and_then(char::from_u32))
        .collect();
    match chars {
        Some(mut chars) if !codes.is_empty() => {
            // Symbols in the Basic Multilingual Plane (like U+2764) are drawn as text unless asked for emoji presentation.
            if let [c] = chars[..]
                && (c as u32) <= 0xFFFF
            {
                chars.push('\u{FE0F}');
            }
            crate::emoji::shown(&chars.into_iter().collect::<String>())
        }
        _ => word_or_glyph(key),
    }
}

/// The emoji of a classic word, or else the key itself if it is an id from Teams' emoji catalogue (letters, digits,
/// `_` and `-`, at most 80), kept whole: many popular reactions are filed under a plain word (`salute`, `fire`) that
/// only the catalogue turns into an emoji, and the backend looks it up there (`TeamsBackend::resolve_reaction_ids`)
/// before the message is shown. Anything else from the network is made safe to draw.
fn word_or_glyph(key: &str) -> String {
    let is_id = !key.is_empty()
        && key.chars().count() <= 80
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if is_id && !crate::emoji::CLASSIC.iter().any(|(word, _)| *word == key) {
        key.to_string()
    } else {
        reaction_emoji(key)
    }
}

/// GUID text (`8-4-4-4-12` hex digits, either case).
pub(super) fn is_guid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// The Entra object id of a directory suggestion: from its MRI, else from `Id` (a GUID, `GUID@tenant`, or the base64 of
/// a .NET GUID byte array).
fn object_id(id: &str, mri: &str) -> Option<String> {
    let from_mri = mri.rsplit(':').next().unwrap_or("");
    if is_guid(from_mri) {
        return Some(from_mri.to_ascii_lowercase());
    }
    let id = id.split('@').next().unwrap_or("");
    if is_guid(id) {
        return Some(id.to_ascii_lowercase());
    }
    let b = base64::engine::general_purpose::STANDARD
        .decode(id)
        .ok()
        .filter(|b| b.len() == 16)?;
    let tail: String = b[10..].iter().map(|x| format!("{x:02x}")).collect();
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{tail}",
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9]
    ))
}

/// People-search answer -> contacts (those with a usable id, without duplicates).
pub(super) fn parse_people(v: &Value) -> Vec<Contact> {
    let mut out: Vec<Contact> = Vec::new();
    for s in v["Groups"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|g| g["Suggestions"].as_array().into_iter().flatten())
    {
        let Some(id) = object_id(
            s["Id"].as_str().unwrap_or(""),
            s["MRI"].as_str().unwrap_or(""),
        ) else {
            continue;
        };
        if out.iter().any(|c| c.id == id) {
            continue;
        }
        let detail = [s["JobTitle"].as_str(), s["Department"].as_str()]
            .into_iter()
            .flatten()
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        out.push(Contact {
            id,
            name: clean(s["DisplayName"].as_str().unwrap_or("")),
            email: clean(
                s["EmailAddresses"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            ),
            detail: clean(&detail),
            phone: clean(&phone_of(&s["Phones"])),
            office: clean(s["OfficeLocation"].as_str().unwrap_or("")),
        });
    }
    out
}

/// A work number from a people search's `Phones`: a list of numbers, or of `{Type, Number}` (a business number
/// first). Neither form has been seen from the service yet; anything else is no number.
fn phone_of(phones: &Value) -> String {
    let entries = phones.as_array().map(Vec::as_slice).unwrap_or_default();
    let number = |p: &Value| -> Option<String> {
        p.as_str()
            .or_else(|| p["Number"].as_str())
            .or_else(|| p["number"].as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
    };
    let business = entries.iter().find(|p| {
        p["Type"]
            .as_str()
            .or_else(|| p["type"].as_str())
            .is_some_and(|t| t.eq_ignore_ascii_case("business"))
    });
    business
        .and_then(number)
        .or_else(|| entries.iter().find_map(number))
        .unwrap_or_default()
}

/// Id of the 1:1 chat between two people: both object ids, sorted.
pub(super) fn one_on_one_id(a: &str, b: &str) -> String {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    format!("19:{x}_{y}@unq.gbl.spaces")
}

/// A random version-4 GUID (request correlation ids).
pub(super) fn guid() -> String {
    use std::hash::{BuildHasher, Hasher};
    let keys = std::collections::hash_map::RandomState::new();
    let half = |n: u8| {
        let mut h = keys.build_hasher();
        h.write_u8(n);
        h.finish().to_be_bytes()
    };
    let mut b = [half(1), half(2)].concat();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn is_chat_type(mt: &str) -> bool {
    matches!(mt, "RichText/Html" | "Text" | "RichText")
}

fn system_text(mt: &str) -> &'static str {
    match mt {
        "ThreadActivity/AddMember" => "someone was added to the chat",
        "ThreadActivity/DeleteMember" => "someone left or was removed",
        "ThreadActivity/MemberJoined" => "someone joined",
        "ThreadActivity/MemberLeft" => "someone left",
        "ThreadActivity/TopicUpdate" => "the chat was renamed",
        "ThreadActivity/MeetingPolicyUpdated" => "meeting options changed",
        "RichText/Media_CallRecording" => "call recording",
        "RichText/Media_CallTranscript" => "call transcript",
        t if t.starts_with("Event/Call") => "call",
        _ => "chat updated",
    }
}

/// Teams fills in the topic of some unnamed group chats with the members' ids (`8:orgid:<guid>, 8:orgid:<guid>`).
/// That is not a name anyone chose, so the chat is titled from its members instead.
fn is_member_list(topic: &str) -> bool {
    topic.split(',').all(|part| {
        let part = part.trim();
        part.starts_with("8:") && part.len() > 2 && !part.contains(char::is_whitespace)
    })
}

/// The text of a message as one plain line, for a list: a chat message's words with markup, line breaks and runs of
/// spaces gone; nothing for a system message (a join, a call), which has no words of its own.
fn one_line_text(message_type: &str, raw: &str) -> String {
    if !is_chat_type(message_type) {
        return String::new();
    }
    let plain = if message_type == "Text" {
        clean(raw)
    } else {
        html_to_plain(raw)
    };
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A conversation-list entry's `lastMessage` as a preview; `None` when it has no usable time.
fn list_preview(last: &Value) -> Option<Preview> {
    let created = time(&last["originalarrivaltime"]).or_else(|| time(&last["composetime"]))?;
    let text = one_line_text(
        last["messagetype"].as_str().unwrap_or(""),
        last["content"].as_str().unwrap_or(""),
    );
    let from_name = last["imdisplayname"]
        .as_str()
        .map(clean)
        .filter(|n| !n.is_empty());
    Some(Preview {
        created,
        from_id: last["from"].as_str().and_then(mri),
        // Meeting recordings and the like are posted by a service (`s2spartnername`) under a person's id, with no
        // display name: nobody wrote them.
        automated: last["s2spartnername"].is_string() && from_name.is_none(),
        from_name,
        text,
    })
}

/// The time up to which the signed-in user has read a conversation: the first field of
/// `properties.consumptionhorizon` (`<ms>;<ms>;<message id>`).
fn read_marker(v: &Value) -> Option<DateTime<Utc>> {
    v["properties"]["consumptionhorizon"]
        .as_str()
        .and_then(|h| millis(&json!(h.split(';').next()?)))
}

/// What a channel in the conversation list is allowed to say about itself: a team's root conversation or a channel,
/// which [`parse_conversation`] leaves out of the chats.
const CHANNEL_PRODUCT_TYPES: [&str; 3] =
    ["TeamsTeam", "TeamsStandardChannel", "TeamsPrivateChannel"];

/// Whether a conversation-list entry is a team's root (its id is that of the General channel) or a channel.
fn is_team_or_channel(v: &Value) -> bool {
    let tp = &v["threadProperties"];
    (matches!(tp["threadType"].as_str(), Some("space" | "topic")) || !tp["spaceType"].is_null())
        && tp["productThreadType"]
            .as_str()
            .is_some_and(|p| CHANNEL_PRODUCT_TYPES.contains(&p))
}

/// A conversation-list entry that is a team's root or a channel -> what it says about activity. `None` for every other
/// entry (a chat, a feed, an entry without an id). It follows the chat rule for what is unread (see
/// [`crate::model::unread_after`]; a channel counts as one that has been opened, so with no marker a message is
/// unread), and a channel nobody has written in (no `lastMessage`, or an empty one, as a change feed has) has no
/// preview and is not unread.
pub(super) fn parse_channel_activity(v: &Value, me_id: &str) -> Option<ChannelActivity> {
    if !is_team_or_channel(v) {
        return None;
    }
    let id = v["id"].as_str().filter(|id| !id.is_empty())?.to_string();
    let last = &v["lastMessage"];
    let preview = if last.is_null() || last.as_object().is_some_and(|o| o.is_empty()) {
        None
    } else {
        list_preview(last)
    };
    let unread = unread_after(preview.as_ref(), read_marker(v), me_id, true);
    Some(ChannelActivity {
        id,
        unread,
        last: preview,
    })
}

/// Most teams and, in one team, most channels taken from the list of teams: the answer is somebody else's data and
/// nothing here is worth an unbounded list. (The account surveyed had 32 teams and at most 36 channels in one.)
pub(super) const MAX_TEAMS: usize = 500;
pub(super) const MAX_CHANNELS: usize = 1_000;
/// The longest preview text of a channel's newest message, in characters.
const MAX_CHANNEL_PREVIEW: usize = 120;

/// What a channel's `channelType` number means. **A guess from other clients, not verified**: 0 standard, 1 private,
/// 2 shared. (`isShared` is the more direct word for shared and wins; see [`parse_channel`].)
fn channel_kind(v: &Value) -> ChannelKind {
    if v["isShared"].as_bool() == Some(true) {
        return ChannelKind::Shared;
    }
    match v["channelType"].as_i64() {
        Some(1) => ChannelKind::Private,
        Some(2) => ChannelKind::Shared,
        _ => ChannelKind::Standard,
    }
}

/// A channel's `lastMessage` in the list of teams (camel-case names, unlike the conversation list's): a preview when it
/// has an id and a time and is a message a person wrote; `None` for a system message (someone was added) or one the
/// service did not describe.
fn channel_preview(last: &Value) -> Option<Preview> {
    last["id"].as_str().filter(|id| !id.is_empty())?;
    let mt = last["messageType"].as_str().unwrap_or("");
    if !is_chat_type(mt) {
        return None;
    }
    let created = time(&last["originalArrivalTime"]).or_else(|| time(&last["composeTime"]))?;
    Some(Preview {
        created,
        from_id: last["from"].as_str().and_then(mri),
        from_name: last["imDisplayName"]
            .as_str()
            .map(clean)
            .filter(|n| !n.is_empty()),
        text: one_line_text(mt, last["content"].as_str().unwrap_or(""))
            .chars()
            .take(MAX_CHANNEL_PREVIEW)
            .collect(),
        automated: false,
    })
}

/// A team's or channel's name from the list of teams: cleaned and on one line.
fn display_name(v: &Value) -> String {
    clean(v["displayName"].as_str().unwrap_or(""))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// One channel of the list of teams. Without an id or a name it is unreadable.
fn parse_channel(v: &Value) -> Parsed<Channel> {
    let Some(id) = v["id"].as_str().filter(|id| !id.is_empty()) else {
        return Parsed::Malformed;
    };
    let name = display_name(v);
    if name.is_empty() {
        return Parsed::Malformed;
    }
    let flag = |key: &str| v[key].as_bool() == Some(true);
    let general = flag("isGeneral");
    let archived = flag("isArchived");
    Parsed::Item(Channel {
        id: id.to_string(),
        name,
        general,
        kind: channel_kind(v),
        archived,
        shown: flag("isFavorite") || general,
        pinned: flag("isPinned"),
        muted: flag("isMuted"),
        followed: flag("isFollowed"),
        // The place comes from the "Favorites" folder (`mark_favourite_channels`), not from the channel's own flag.
        favorite: None,
        // An archived channel cannot be written in, so there is nothing to catch up on.
        unread: !archived && v["isMessageRead"].as_bool() == Some(false),
        last: channel_preview(&v["lastMessage"]),
    })
}

/// One team of the list of teams, and how many of its channels could not be read. A team without an id or a name is
/// unreadable; one the service marks deleted is left out; one with no `channels` has none; only the first
/// [`MAX_CHANNELS`] channels are taken. Channels: General first, then pinned ones, then the rest as the service
/// ordered them.
pub(super) fn parse_team(v: &Value) -> Parsed<(Team, usize)> {
    if v["isDeleted"].as_bool() == Some(true) {
        return Parsed::Skipped;
    }
    let Some(id) = v["id"].as_str().filter(|id| !id.is_empty()) else {
        return Parsed::Malformed;
    };
    let name = display_name(v);
    if name.is_empty() {
        return Parsed::Malformed;
    }
    let raw = v["channels"].as_array().map_or(&[][..], Vec::as_slice);
    let mut unreadable = 0;
    let mut channels = Vec::new();
    for c in raw.iter().take(MAX_CHANNELS) {
        match parse_channel(c) {
            Parsed::Item(c) => channels.push(c),
            Parsed::Skipped | Parsed::Malformed => unreadable += 1,
        }
    }
    channels.sort_by_key(|c| (!c.general, !c.pinned));
    Parsed::Item((
        Team {
            id: id.to_string(),
            name,
            archived: v["isArchived"].as_bool() == Some(true),
            muted: v["isUserMuted"].as_bool() == Some(true),
            members: v["membershipSummary"]["totalMemberCount"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok()),
            channels,
        },
        unreadable,
    ))
}

/// Most items of the "Favorites" folder read from the list of teams: the folder is the person's own short list, and the
/// answer is somebody else's data.
pub(super) const MAX_FOLDER_ITEMS: usize = 500;

/// The teams and channels among the person's favourites, as the list of teams' own copy of the conversation folders
/// gives them.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct FolderFavourites {
    /// Whether the answer had a "Favorites" folder at all (without one there are no favourites to apply, and none is
    /// asked for elsewhere).
    pub(super) found: bool,
    /// Conversation id -> place among the favourite teams and channels (1 is first, in the folder's own order). A
    /// team's root conversation is its General channel's, so a favourite team's id is its General channel's id.
    pub(super) places: HashMap<String, u32>,
    /// Items of the folder that are not teams or channels (chats, which the chat list marks itself).
    pub(super) others: usize,
}

/// Read the "Favorites" folder from `folders`, the `conversationFolders` array inside the list of teams' own
/// `conversationFolders` object (the same shape the favourite chats' read parses: `folderType`, `isDeleted`,
/// `conversationFolderItems` of `conversationId` and `threadType`).
///
/// The first folder that is a "Favorites" one and not deleted is taken, and at most [`MAX_FOLDER_ITEMS`] of its items.
/// An item that is not an object, or has no conversation id, is left out; so is a repeat of an id (the first place is
/// kept and the repeat takes no place). Teams and channels (thread types `space`, a team's root, and `topic`) are
/// numbered from 1 in order; any other item is a chat and only counted.
pub(super) fn favourite_channels(folders: &Value) -> FolderFavourites {
    let mut found = FolderFavourites::default();
    let Some(folder) = folders.as_array().and_then(|all| {
        all.iter()
            .find(|f| f["folderType"] == "Favorites" && f["isDeleted"] != true)
    }) else {
        return found;
    };
    found.found = true;
    let items = folder["conversationFolderItems"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    for item in items.iter().take(MAX_FOLDER_ITEMS) {
        let Some(id) = item["conversationId"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        if !matches!(item["threadType"].as_str(), Some("space" | "topic")) {
            found.others += 1;
        } else if !found.places.contains_key(id) {
            let place = u32::try_from(found.places.len() + 1).unwrap_or(u32::MAX);
            found.places.insert(id.to_string(), place);
        }
    }
    found
}

/// Put each favourite's place on its channel, and say how many of them were found among `teams`. A favourite whose id is
/// a team's own but that no channel carries (the General channel is expected to) goes to the team's General channel.
pub(super) fn mark_favourite_channels(teams: &mut [Team], favourites: &FolderFavourites) -> usize {
    let mut matched = 0;
    for team in teams {
        for c in &mut team.channels {
            c.favorite = favourites.places.get(&c.id).copied();
            matched += usize::from(c.favorite.is_some());
        }
        if let Some(&place) = favourites.places.get(&team.id)
            && !team.channels.iter().any(|c| c.id == team.id)
            && let Some(general) = team
                .channels
                .iter_mut()
                .find(|c| c.general && c.favorite.is_none())
        {
            general.favorite = Some(place);
            matched += 1;
        }
    }
    matched
}

/// Most entries read from the person's order of their teams (the account surveyed had 22).
pub(super) const MAX_ORDER_ENTRIES: usize = 1_000;

/// The person's own order of their teams, as the `teamsOrder` user property holds it.
#[derive(Debug, Default, PartialEq)]
pub(super) struct TeamOrder {
    /// The entries that were read (objects of the array), for noting their fields.
    pub(super) entries: Vec<Value>,
    /// The team ids in the person's order, each once.
    pub(super) ids: Vec<String>,
}

/// A number from a JSON number or a string holding one (`order` has been seen as a number; a string is tolerated).
fn order_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|n| n.is_finite())
}

/// Read the order of the teams from the answer to `GET .../v1/users/ME/properties?name=teamsOrder`.
///
/// `teamsOrder` is a string holding JSON, an array of `{"order": <number>, "teamId": "<id>"}` (a plain array in
/// place of the string is tolerated). Entries are sorted by `order`; one whose `order` is not a number comes after
/// every numbered one, and ties keep the array's order (the sort is stable). An entry that is not an object, or has no
/// team id, is left out; so is a repeat of an id (the first place is kept). At most [`MAX_ORDER_ENTRIES`] entries are
/// read.
///
/// `Some` with no ids is an account with no order (the property absent, null or empty, or an empty array); `None` is an
/// answer that is there but cannot be read as an order (not JSON, not an array, or entries none of which names a team),
/// which the caller reports.
pub(super) fn team_order(answer: &Value) -> Option<TeamOrder> {
    let parsed: Value;
    let list = match &answer["teamsOrder"] {
        Value::Null => return Some(TeamOrder::default()),
        Value::String(s) if s.trim().is_empty() => return Some(TeamOrder::default()),
        Value::String(s) => {
            parsed = serde_json::from_str(s).ok()?;
            &parsed
        }
        other => other,
    };
    let items = match list {
        Value::Null => return Some(TeamOrder::default()),
        Value::Array(items) => items,
        _ => return None,
    };
    let mut keyed: Vec<(Option<f64>, usize, &Value, &str)> = items
        .iter()
        .take(MAX_ORDER_ENTRIES)
        .enumerate()
        .filter_map(|(position, item)| {
            let id = item["teamId"].as_str().filter(|id| !id.is_empty())?;
            Some((order_number(&item["order"]), position, item, id))
        })
        .collect();
    if keyed.is_empty() && !items.is_empty() {
        return None;
    }
    keyed.sort_by(|a, b| match (a.0, b.0) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    let mut seen = std::collections::HashSet::new();
    let mut ids = Vec::new();
    for (_, _, _, id) in &keyed {
        if seen.insert(*id) {
            ids.push((*id).to_string());
        }
    }
    Some(TeamOrder {
        entries: keyed.iter().map(|k| k.2.clone()).collect(),
        ids,
    })
}

/// Put `teams` in the person's `order`: the teams the order names first, in its order; then every other team,
/// alphabetically by name without regard to case (both stable). An id in the order that matches no team is ignored.
/// Returns the teams and how many of them the order named.
///
/// **A guess:** how the real client orders teams that are not in the order is not known. Nothing is removed (an archived
/// team stays; the application decides what to show).
pub(super) fn apply_team_order(mut teams: Vec<Team>, order: &[String]) -> (Vec<Team>, usize) {
    let mut rank: HashMap<&str, usize> = HashMap::new();
    for (place, id) in order.iter().enumerate() {
        rank.entry(id.as_str()).or_insert(place);
    }
    let matched = teams
        .iter()
        .filter(|t| rank.contains_key(t.id.as_str()))
        .count();
    teams.sort_by_cached_key(|t| match rank.get(t.id.as_str()) {
        Some(&place) => (false, place, String::new()),
        None => (true, 0, t.name.to_lowercase()),
    });
    (teams, matched)
}

/// A conversation-list entry -> chat (without member names) and the member ids it names. Channels, hidden and empty
/// conversations are skipped.
pub(super) fn parse_conversation(v: &Value) -> Parsed<(Chat, Vec<String>)> {
    let Some(id) = v["id"].as_str().map(str::to_string) else {
        return Parsed::Malformed;
    };
    let tp = &v["threadProperties"];
    let notes = id == NOTES_ID && tp["threadType"].as_str() == Some("streamofnotes");
    // The other `48:` streams (notifications, mentions, call logs) are feeds, not conversations.
    if !notes && (!id.starts_with("19:") || id.ends_with("@thread.tacv2")) {
        return Parsed::Skipped;
    }
    // A team (its root conversation lists channels, not messages) is not a chat.
    if matches!(tp["threadType"].as_str(), Some("space" | "topic")) || !tp["spaceType"].is_null() {
        return Parsed::Skipped;
    }
    if tp["hidden"].as_str() == Some("true")
        || v["properties"]["isemptyconversation"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("true"))
    {
        return Parsed::Skipped;
    }
    let last = &v["lastMessage"];
    // A conversation nobody has written in has no last message: absent or null in the list, `{}` in a change feed.
    if last.is_null() || last.as_object().is_some_and(|o| o.is_empty()) {
        return Parsed::Skipped;
    }
    let Some(preview) = list_preview(last) else {
        return Parsed::Malformed;
    };
    let created = preview.created;
    let kind = kind_of(&id, v);
    let member_ids = if notes {
        // Nobody else is in it, and its id holds no member ids.
        Vec::new()
    } else if kind == ChatKind::OneOnOne {
        pair_members(&id)
    } else {
        v["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                m["id"]
                    .as_str()
                    .or(m["mri"].as_str())
                    .or(m.as_str())
                    .and_then(mri)
            })
            .collect()
    };
    let last_read = read_marker(v);
    let chat = Chat {
        web_url: Some(format!(
            "https://teams.microsoft.com/l/chat/{}/0",
            urlencode(&id)
        )),
        id,
        kind,
        topic: tp["topic"]
            .as_str()
            .map(clean)
            .filter(|t| !t.trim().is_empty() && !is_member_list(t))
            .or_else(|| notes.then(|| NOTES_TITLE.to_string())),
        members: Vec::new(),
        updated: created,
        preview: Some(preview),
        last_read,
        favorite: None,
    };
    Parsed::Item((chat, member_ids))
}

/// One message from a conversation. Typing indicators and other control messages are skipped.
pub(super) fn parse_msg(v: &Value) -> Parsed<Msg> {
    let mt = v["messagetype"].as_str().unwrap_or("");
    if mt.starts_with("Control/") {
        return Parsed::Skipped;
    }
    let id = match &v["id"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Parsed::Malformed,
    };
    let Some(created) = time(&v["originalarrivaltime"]).or_else(|| time(&v["composetime"])) else {
        return Parsed::Malformed;
    };
    let props = &v["properties"];
    let system = !is_chat_type(mt);
    let html = !system && mt != "Text";
    let mentions_raw = embedded(&props["mentions"]);
    let body = if system {
        system_text(mt).to_string()
    } else {
        normalize_mentions(
            &clean(v["content"].as_str().unwrap_or("")),
            &mention_ids(&mentions_raw),
        )
    };

    let mut reactions: Vec<Reaction> = Vec::new();
    for e in embedded(&props["emotions"])
        .as_array()
        .into_iter()
        .flatten()
    {
        let count = e["users"].as_array().map_or(0, Vec::len);
        if let (Some(key), true) = (e["key"].as_str(), count > 0) {
            let users: Vec<String> = e["users"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|u| u["mri"].as_str().and_then(mri))
                .collect();
            reactions.push(Reaction {
                emoji: reaction_glyph(&clean(key)),
                count,
                names: vec![String::new(); users.len()],
                users,
            });
        }
    }
    let attachments = embedded(&props["files"])
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| Attachment {
            name: clean(f["fileName"].as_str().or(f["title"].as_str()).unwrap_or("")),
            content_type: clean(f["fileType"].as_str().unwrap_or("")),
            url: f["objectUrl"].as_str().map(clean).filter(|u| !u.is_empty()),
        })
        .collect();
    let mentions: Vec<String> = mentions_raw
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["mri"].as_str().and_then(mri))
        .collect();
    // `properties.edittime` is not always a real, later edit: a message can carry one that predates its own
    // `created` by a few minutes (confirmed live, 2026-11-09, with the message's `version` still equal to its id,
    // which a genuinely edited message's is not — see the Edits convention) — most likely the time its draft was
    // last saved before actually being sent, not a later revision. An edit cannot happen before the message
    // existed, so only a later `edittime` counts as one.
    let edittime = millis(&props["edittime"]).filter(|t| *t > created);
    // In a channel every message carries `rootMessageId`: its own id for a post, the post's for a reply (verified
    // live on one account: 50 of 50 messages of a channel page had it). Only a reply keeps it here.
    let root_id = v["rootMessageId"]
        .as_str()
        .filter(|r| !r.is_empty() && *r != id && r.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_string);
    Parsed::Item(Msg {
        id,
        created,
        modified: edittime.unwrap_or(created),
        edited: edittime,
        from_id: v["from"].as_str().and_then(mri),
        from_name: clean(v["imdisplayname"].as_str().unwrap_or("")),
        system,
        deleted: millis(&props["deletetime"]).is_some(),
        html,
        body,
        subject: props["subject"]
            .as_str()
            .map(clean)
            .filter(|s| !s.is_empty()),
        attachments,
        reactions,
        mentions,
        delivery: Delivery::Sent,
        root_id,
    })
}

/// `itemid -> bare user id` from `properties.mentions` (already parsed by [`embedded`]), the same per-mention
/// map `parse_msg` reads to build `Msg.mentions`, so [`normalize_mentions`] can rewrite the small index in the
/// body's own markup into a real id — the bare form (`mri`, `8:orgid:` stripped), matching `Msg.mentions`/`me_id`,
/// since neither would ever match the raw `mri` string.
fn mention_ids(mentions: &Value) -> HashMap<String, String> {
    mentions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let itemid = match &m["itemid"] {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => return None,
            };
            let id = m["mri"].as_str().and_then(mri)?;
            Some((itemid, id))
        })
        .collect()
}

/// The value of attribute `name` in `attrs` (the text between a tag's name and its closing `>`), or `None` if it
/// is not there. Does not handle a `>` or `"` inside the value itself, which a mention span's own attributes
/// never need to (an id and a fixed URL).
fn attr_value<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let start = attrs.find(&needle)? + needle.len();
    let end = attrs[start..].find('"')?;
    Some(&attrs[start..start + end])
}

/// Teams' own mention markup — `<span itemscope itemtype="http://schema.skype.com/Mention" itemid="n">Name</span>`
/// (`docs/teams-web-protocol.md`) — normalized to the `<at id="n">Name</at>` form Graph documents, so `Msg.body`
/// carries one mention tag regardless of which backend sent it, and the app's HTML->Markdown conversion only has
/// to know that one. A literal scan rather than a real HTML parse, mirroring [`teams_mentions`]'s own outgoing
/// replace just below: both directions of this wire format are ours to control, down to the exact shape the
/// service echoes back, and a mention's content is always plain text, never a nested tag. **The three attributes
/// are not always in the same order** — confirmed live, 2026-11-06: one message had `itemtype` before
/// `itemscope`, not after, as every earlier capture and the one fixture here had shown — so each opening `<span`
/// tag's attributes are read as a small, order-independent set via [`attr_value`] rather than matched as one
/// fixed literal string; only `itemtype` is required to recognise a mention, and a `<span>` that is not one of
/// Teams' own mention spans is left completely untouched. `id` is rewritten from the small `itemid` index to the
/// real mentioned user's bare id via `by_itemid` (see [`mention_ids`]), so the app can tell which mention is of
/// the signed-in user without needing `properties.mentions` separately; an `itemid` with no entry (unexpected,
/// or absent) keeps its original numeric form (or is dropped, if there was no `itemid` at all) rather than losing
/// the mention outright.
fn normalize_mentions(html: &str, by_itemid: &HashMap<String, String>) -> String {
    const MENTION_TYPE: &str = "http://schema.skype.com/Mention";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let Some(start) = rest.find("<span ") else {
            out.push_str(rest);
            break;
        };
        let Some(tag_end) = rest[start..].find('>').map(|i| start + i) else {
            out.push_str(rest);
            break;
        };
        let attrs = &rest[start + "<span".len()..tag_end];
        if attr_value(attrs, "itemtype") != Some(MENTION_TYPE) {
            // Not one of Teams' own mention spans: copy the tag as-is and keep scanning after it.
            out.push_str(&rest[..=tag_end]);
            rest = &rest[tag_end + 1..];
            continue;
        }
        let id = attr_value(attrs, "itemid").unwrap_or("");
        let Some((inner, tail)) = rest[tag_end + 1..].split_once("</span>") else {
            // A span that never closes is left as it is, with everything before it.
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        // The id goes into an attribute of the tag written here: nothing in it may end the attribute or start a tag.
        let resolved = by_itemid
            .get(id)
            .map(String::as_str)
            .unwrap_or(id)
            .replace('"', "&quot;")
            .replace('<', "&lt;");
        out.push_str(&format!("<at id=\"{resolved}\">{inner}</at>"));
        rest = tail;
    }
    out
}

/// The people a presence answer says something about, by object id. The answer is an array of `{mri, etag,
/// presence: {availability, activity, calendarData: {isOutOfOffice, ..}, ..}}` (`docs/teams-web-protocol.md`); an
/// entry with no `presence` (the service answering "unchanged"), one that is not a person's object id and one without
/// an availability are left out, and an availability this version does not know is `Availability::Unknown`.
///
/// The notes are `presence.note` (a status message) and `presence.calendarData.outOfOfficeNote`, each
/// `{message, expiry, publishTime}` with `message` holding HTML. A note that is not an object, has no text once made
/// plain, or has expired as of `now` is no note; a time that does not parse, or an expiry more than about five years
/// away (the service's "never"), is no time.
pub(super) fn presence(answer: &Value, now: DateTime<Utc>) -> HashMap<String, Presence> {
    answer
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item["mri"].as_str()?.strip_prefix("8:orgid:")?;
            if !is_guid(id) {
                return None;
            }
            let p = item["presence"].as_object()?;
            let availability = Availability::from_label(p.get("availability")?.as_str()?);
            let calendar = p.get("calendarData");
            let out_of_office = calendar
                .and_then(|c| c["isOutOfOffice"].as_bool())
                .unwrap_or(false);
            let mut presence = Presence::new(availability, out_of_office);
            presence.note = p.get("note").and_then(|n| status_note(n, now));
            // A reply written for a calendar entry that is no longer on is not shown as if it were.
            if out_of_office {
                presence.out_of_office_note = calendar
                    .and_then(|c| c.get("outOfOfficeNote"))
                    .and_then(|n| status_note(n, now));
            }
            Some((id.to_ascii_lowercase(), presence))
        })
        .collect()
}

/// A time of the service's, in RFC 3339 with or without fractional seconds.
fn note_time(value: &Value) -> Option<DateTime<Utc>> {
    let t = DateTime::parse_from_rfc3339(value.as_str()?).ok()?;
    Some(t.with_timezone(&Utc))
}

/// One `{message, expiry, publishTime}` note, or `None` when it is not one worth showing at `now`.
fn status_note(value: &Value, now: DateTime<Utc>) -> Option<StatusNote> {
    /// An expiry further away than this means "never".
    const NEVER_AFTER_DAYS: i64 = 5 * 366;
    let object = value.as_object()?;
    let note = StatusNote::new(object.get("message")?.as_str()?)?;
    let expires = object
        .get("expiry")
        .and_then(note_time)
        .filter(|e| *e <= now + chrono::Duration::days(NEVER_AFTER_DAYS));
    if expires.is_some_and(|e| e <= now) {
        return None;
    }
    Some(
        note.expiring(expires)
            .published(object.get("publishTime").and_then(note_time)),
    )
}

/// `<at id="N">Name</at>` (what the editor produces) -> Teams' mention markup, plus the matching `properties.mentions`.
pub(super) fn teams_mentions(html: &str, mentions: &[OutMention]) -> (String, Option<String>) {
    if mentions.is_empty() {
        return (html.to_string(), None);
    }
    let mut out = html.to_string();
    for m in mentions {
        out = out.replace(
            &format!("<at id=\"{}\">", m.id),
            &format!(
                "<span itemscope=\"\" itemtype=\"http://schema.skype.com/Mention\" itemid=\"{}\">",
                m.id
            ),
        );
    }
    let out = out.replace("</at>", "</span>");
    let list: Vec<Value> = mentions
        .iter()
        .map(|m| json!({ "@type": "http://schema.skype.com/Mention", "itemid": m.id, "mri": format!("8:orgid:{}", m.user_id), "mentionType": "person", "displayName": m.name }))
        .collect();
    (out, Some(Value::Array(list).to_string()))
}

/// Teams' own reply-quote markup (see `AGENTS.md`, "Messages, mentions and replies") to prepend to a sent message's
/// content: a `<blockquote>` naming the quoted sender and the quoted time, in the exact shape `convert::quote_time`
/// (the reading side) expects back. `target`'s `from_name` and `preview` are free text, not markup, so they are
/// escaped here rather than trusted as HTML.
pub(super) fn reply_quote_html(target: &ReplyTarget) -> String {
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

/// `properties.qtdMsgs`: the structured reference to the quoted message (confirmed live, 2026-11-06, a genuine
/// reply's own request body, `docs/teams-web-protocol.md`'s "Reply-quote verification") that the real client sends
/// alongside the blockquote and that the client checks to show a reply's quote as verified rather than flagging it.
pub(super) fn qtd_msgs_json(target: &ReplyTarget) -> String {
    let sender = target
        .from_id
        .as_deref()
        .map(|id| format!("8:orgid:{id}"))
        .unwrap_or_default();
    let time: i64 = target.id.parse().unwrap_or_default();
    json!([{ "messageId": target.id, "sender": sender, "time": time }]).to_string()
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const ADA: &str = "22222222-2222-2222-2222-222222222222";

    #[test]
    fn mri_strips_prefix_and_url() {
        assert_eq!(mri("8:orgid:abc").as_deref(), Some("abc"));
        assert_eq!(
            mri("https://teams.microsoft.com/api/chatsvc/amer/v1/users/ME/contacts/8:orgid:abc")
                .as_deref(),
            Some("abc")
        );
        assert_eq!(mri("28:bot").as_deref(), Some("28:bot"));
        assert_eq!(mri(""), None);
    }

    /// The property's answer as the service shapes it: `teamsOrder` is a string holding JSON.
    fn order_answer(entries: &Value) -> Value {
        json!({"cid": "c", "cidHex": "h", "dogfoodUser": false, "primaryMemberName": "n", "skypeName": "s",
               "teamsOrder": entries.to_string()})
    }

    fn order_ids(answer: &Value) -> Vec<String> {
        team_order(answer).expect("an order").ids
    }

    #[test]
    fn the_order_is_read_from_a_string_holding_json_by_its_order_numbers() {
        // The array's own order and the numbers disagree: the numbers win.
        let answer = order_answer(&json!([
            {"order": 2, "teamId": "c"}, {"order": 0, "teamId": "a"}, {"order": 1, "teamId": "b"},
        ]));
        assert_eq!(order_ids(&answer), ["a", "b", "c"]);
        let read = team_order(&answer).unwrap();
        assert_eq!(read.entries.len(), 3);
        assert_eq!(read.entries[0]["teamId"], "a");
    }

    #[test]
    fn a_plain_array_in_place_of_the_string_is_tolerated() {
        let answer =
            json!({"teamsOrder": [{"order": 1, "teamId": "b"}, {"order": 0, "teamId": "a"}]});
        assert_eq!(order_ids(&answer), ["a", "b"]);
    }

    #[test]
    fn no_order_is_not_an_unreadable_one() {
        for answer in [
            json!({}),
            json!({"teamsOrder": null}),
            json!({"teamsOrder": ""}),
            json!({"teamsOrder": "  \n"}),
            json!({"teamsOrder": "[]"}),
            json!({"teamsOrder": []}),
            json!({"teamsOrder": "null"}),
            json!(null),
        ] {
            assert_eq!(team_order(&answer), Some(TeamOrder::default()), "{answer}");
        }
    }

    #[test]
    fn an_order_that_cannot_be_read_says_so() {
        for answer in [
            json!({"teamsOrder": "not json"}),
            json!({"teamsOrder": "{\"order\": 1}"}),
            json!({"teamsOrder": "42"}),
            json!({"teamsOrder": 42}),
            json!({"teamsOrder": true}),
            json!({"teamsOrder": {"a": 1}}),
            json!({"teamsOrder": "[1, 2, \"x\", null]"}),
            json!({"teamsOrder": [{"order": 1}, {"teamId": ""}, {"teamId": 7}]}),
        ] {
            assert_eq!(team_order(&answer), None, "{answer}");
        }
    }

    #[test]
    fn entries_that_are_not_teams_are_left_out_of_an_order_that_has_some() {
        let answer = order_answer(&json!([
            1, "x", null, {"order": 0}, {"order": 1, "teamId": 5}, {"order": 2, "teamId": ""},
            {"order": 3, "teamId": "kept"}, [], {"teamId": "also"},
        ]));
        assert_eq!(order_ids(&answer), ["kept", "also"]);
    }

    #[test]
    fn the_order_number_may_be_a_number_a_string_or_missing_and_ties_keep_the_arrays_order() {
        let answer = order_answer(&json!([
            {"teamId": "none-1"},
            {"order": "2", "teamId": "string-2"},
            {"order": 1.5, "teamId": "float"},
            {"order": "soon", "teamId": "none-2"},
            {"order": 2, "teamId": "tie-after-string"},
            {"order": null, "teamId": "none-3"},
            {"order": -1, "teamId": "negative"},
            {"order": 1.5, "teamId": "float-tie"},
        ]));
        assert_eq!(
            order_ids(&answer),
            [
                "negative",
                "float",
                "float-tie",
                "string-2",
                "tie-after-string",
                "none-1",
                "none-2",
                "none-3"
            ],
            "numbered first by number, ties by position, then the unnumbered by position"
        );
    }

    #[test]
    fn a_repeated_team_keeps_its_first_place() {
        let answer = order_answer(&json!([
            {"order": 3, "teamId": "a"}, {"order": 1, "teamId": "b"}, {"order": 0, "teamId": "a"},
        ]));
        assert_eq!(order_ids(&answer), ["a", "b"]);
    }

    #[test]
    fn hostile_text_in_the_order_is_only_ever_compared() {
        let hostile = "19:\u{1b}[31m\u{202e}x@thread.tacv2";
        let answer = order_answer(&json!([{"order": 0, "teamId": hostile}]));
        assert_eq!(order_ids(&answer), [hostile]);
        // It matches only a team whose id is exactly that.
        let (teams, matched) = apply_team_order(
            vec![named_team("19:x@thread.tacv2", "X")],
            &[hostile.to_string()],
        );
        assert_eq!((teams.len(), matched), (1, 0));
    }

    #[test]
    fn at_most_a_thousand_entries_of_the_order_are_read() {
        let entries: Vec<Value> = (0..1_500)
            .map(|i| json!({"order": i, "teamId": format!("t{i}")}))
            .collect();
        let ids = order_ids(&order_answer(&Value::Array(entries)));
        assert_eq!(ids.len(), 1_000);
        assert_eq!(ids[999], "t999");
    }

    fn named_team(id: &str, name: &str) -> Team {
        Team {
            id: id.into(),
            name: name.into(),
            archived: false,
            muted: false,
            members: None,
            channels: vec![],
        }
    }

    fn team_names(teams: &[Team]) -> Vec<&str> {
        teams.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn teams_follow_the_order_then_the_rest_alphabetically_without_regard_to_case() {
        let teams = vec![
            named_team("t-zeta", "zeta"),
            named_team("t-b", "Bravo"),
            named_team("t-apple", "apple"),
            named_team("t-a", "Alpha"),
            named_team("t-c", "Charlie"),
            named_team("t-banana", "Banana"),
        ];
        let order = ["t-c".to_string(), "gone".to_string(), "t-a".to_string()];
        let (sorted, matched) = apply_team_order(teams, &order);
        assert_eq!(matched, 2, "the stale id is not counted");
        assert_eq!(
            team_names(&sorted),
            ["Charlie", "Alpha", "apple", "Banana", "Bravo", "zeta"]
        );
    }

    #[test]
    fn with_no_order_the_teams_are_alphabetical_and_equal_names_keep_their_places() {
        let teams = vec![
            named_team("1", "same"),
            named_team("2", "Same"),
            named_team("3", "SAME"),
            named_team("0", "Abc"),
        ];
        let (sorted, matched) = apply_team_order(teams, &[]);
        assert_eq!(matched, 0);
        let ids: Vec<&str> = sorted.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["0", "1", "2", "3"]);
    }

    #[test]
    fn an_order_never_adds_or_removes_a_team() {
        let mut archived = named_team("t-old", "Old");
        archived.archived = true;
        let teams = vec![named_team("t-a", "A"), archived];
        let (sorted, _) = apply_team_order(teams, &["t-old".to_string(), "t-old".to_string()]);
        assert_eq!(team_names(&sorted), ["Old", "A"]);
    }

    fn folder(kind: &str, deleted: bool, items: Value) -> Value {
        json!({"id": "f", "name": kind, "folderType": kind, "isDeleted": deleted, "conversationFolderItems": items})
    }

    fn item(id: &str, thread_type: &str) -> Value {
        json!({"conversationId": id, "threadType": thread_type})
    }

    fn places(found: &FolderFavourites) -> Vec<(&str, u32)> {
        let mut all: Vec<(&str, u32)> =
            found.places.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        all.sort_by_key(|&(_, place)| place);
        all
    }

    #[test]
    fn the_favorites_folder_gives_teams_and_channels_their_places_in_the_folders_order() {
        let found = favourite_channels(&json!([
            folder("Favorites", true, json!([item("deleted", "topic")])),
            folder("Other", false, json!([item("custom", "topic")])),
            folder(
                "Favorites",
                false,
                json!([
                    item("b", "topic"),
                    item("chat1", "chat"),
                    item("a", "space"),
                    item("meeting", "meeting"),
                    item("b", "topic"),
                    item("c", "topic"),
                ])
            ),
            folder("Favorites", false, json!([item("second-folder", "topic")])),
        ]));
        assert!(found.found);
        assert_eq!(
            places(&found),
            [("b", 1), ("a", 2), ("c", 3)],
            "the folder's order, a repeat keeps the first place and takes none, a chat takes none"
        );
        assert_eq!(
            found.others, 2,
            "chats and meetings are counted, not placed"
        );
    }

    #[test]
    fn no_favorites_folder_is_not_found_and_hostile_shapes_are_left_out() {
        for folders in [
            json!(null),
            json!([]),
            json!({}),
            json!("Favorites"),
            json!([folder("Other", false, json!([item("a", "topic")]))]),
            json!([folder("Favorites", true, json!([item("a", "topic")]))]),
            json!([42, null, "x"]),
        ] {
            let found = favourite_channels(&folders);
            assert_eq!(found, FolderFavourites::default(), "{folders}");
        }
        let found = favourite_channels(&json!([folder(
            "Favorites",
            false,
            json!([
                null,
                7,
                "x",
                [],
                {"conversationId": null, "threadType": "topic"},
                {"conversationId": 5, "threadType": "topic"},
                {"conversationId": "", "threadType": "topic"},
                {"threadType": "topic"},
                {"conversationId": "a"},
                {"conversationId": "b", "threadType": 3},
                {"conversationId": "c", "threadType": "topic", "extra": {"deep": [[[[]]]]}},
            ])
        )]));
        assert!(found.found);
        assert_eq!(places(&found), [("c", 1)]);
        assert_eq!(
            found.others, 2,
            "an item whose type is missing or odd is not a channel"
        );
        // Items that are not a list at all.
        for items in [json!(null), json!({"a": 1}), json!("x")] {
            let found = favourite_channels(&json!([folder("Favorites", false, items)]));
            assert!(found.found && found.places.is_empty() && found.others == 0);
        }
    }

    #[test]
    fn no_more_than_five_hundred_items_of_the_folder_are_read() {
        let items: Vec<Value> = (0..600)
            .map(|i| item(&format!("19:c{i}@thread.tacv2"), "topic"))
            .collect();
        let found = favourite_channels(&json!([folder("Favorites", false, json!(items))]));
        assert_eq!(found.places.len(), MAX_FOLDER_ITEMS);
        assert_eq!(found.places.get("19:c499@thread.tacv2"), Some(&500));
        assert!(!found.places.contains_key("19:c500@thread.tacv2"));
        // Chats count toward the cap too: it bounds the items looked at, not the ones kept.
        let mut mixed: Vec<Value> = (0..MAX_FOLDER_ITEMS)
            .map(|i| item(&format!("chat{i}"), "chat"))
            .collect();
        mixed.push(item("late", "topic"));
        let found = favourite_channels(&json!([folder("Favorites", false, json!(mixed))]));
        assert!(found.places.is_empty());
        assert_eq!(found.others, MAX_FOLDER_ITEMS);
    }

    fn plain_channel(id: &str, name: &str, general: bool) -> Channel {
        parse_channel(&json!({"id": id, "displayName": name, "isGeneral": general}))
            .item()
            .unwrap()
    }

    fn plain_team(id: &str, channels: Vec<Channel>) -> Team {
        parse_team(&json!({"id": id, "displayName": id}))
            .item()
            .map(|(mut t, _)| {
                t.channels = channels;
                t
            })
            .unwrap()
    }

    #[test]
    fn places_land_on_the_channels_with_that_id_and_a_team_root_on_its_general_channel() {
        let found = favourite_channels(&json!([folder(
            "Favorites",
            false,
            json!([
                item("t1", "space"),
                item("t2-chan", "topic"),
                item("t3", "space"),
                item("t4", "space"),
                item("nowhere", "topic"),
            ])
        )]));
        let mut teams = vec![
            // The root's id is the General channel's: a plain match.
            plain_team(
                "t1",
                vec![
                    plain_channel("t1", "General", true),
                    plain_channel("t2-chan", "Other", false),
                ],
            ),
            // The root names the team but no channel carries that id: the General channel takes it.
            plain_team("t3", vec![plain_channel("t3-general", "General", true)]),
            // The root names a team that has no General channel to put it on.
            plain_team("t4", vec![plain_channel("t4-x", "Other", false)]),
        ];
        // A stale value is replaced, not kept.
        teams[2].channels[0].favorite = Some(9);
        let matched = mark_favourite_channels(&mut teams, &found);
        let got: Vec<(&str, Option<u32>)> = teams
            .iter()
            .flat_map(|t| t.channels.iter().map(|c| (c.id.as_str(), c.favorite)))
            .collect();
        assert_eq!(
            got,
            [
                ("t1", Some(1)),
                ("t2-chan", Some(2)),
                ("t3-general", Some(3)),
                ("t4-x", None),
            ]
        );
        assert_eq!(
            matched, 3,
            "the unmatched favourites are the difference to the folder's count"
        );
    }

    #[test]
    fn a_message_is_edited_only_when_the_service_says_so() {
        let msg = |props: Value| {
            parse_msg(&json!({"id": "1700000000000", "messagetype": "RichText/Html", "content": "<p>x</p>",
                "originalarrivaltime": "2026-10-27T10:00:00.000Z", "properties": props}))
                .item()
                .unwrap()
        };
        // A reaction changes the message but is not an edit.
        let reacted =
            msg(json!({"emotions": "[{\"key\":\"like\",\"users\":[{\"mri\":\"8:orgid:a\"}]}]"}));
        assert_eq!(reacted.edited, None);
        let edited = msg(json!({"edittime": 1_793_790_855_517_i64}));
        assert_eq!(edited.edited.unwrap().timestamp_millis(), 1_793_790_855_517);
        assert_eq!(edited.modified, edited.edited.unwrap());
        let as_text = msg(json!({"edittime": "1793790855517"}));
        assert_eq!(as_text.edited, edited.edited);
        assert_eq!(msg(json!({})).edited, None);
        // An `edittime` before the message's own `created` is not a real, later edit — confirmed live, 2026-11-09,
        // on a message whose `version` was still its own id, not the draft-save-looking `edittime` a genuine edit
        // would have bumped it to. `created` here is "2026-10-27T10:00:00.000Z" = epoch ms 1_793_095_200_000; this
        // `edittime` is a minute earlier.
        let before_created = msg(json!({"edittime": 1_793_095_140_000_i64}));
        assert_eq!(before_created.edited, None);
        assert_eq!(
            before_created.modified, before_created.created,
            "modified falls back to created, not the pre-dated edittime"
        );
    }

    #[test]
    fn parses_a_one_on_one_conversation() {
        let id = format!("19:{ME}_{ADA}@unq.gbl.spaces");
        let v = json!({
            "id": id, "threadProperties": {"threadType": "chat"},
            "properties": {"consumptionhorizon": "1700000000000;1700000000000;99"},
            "lastMessage": {"messagetype": "RichText/Html", "content": "<p>Hello <b>there</b></p>", "imdisplayname": "Ada",
                "from": format!("https://teams.microsoft.com/api/chatsvc/amer/v1/users/ME/contacts/8:orgid:{ADA}"),
                "originalarrivaltime": "2026-10-27T10:00:00.000Z"}
        });
        let (chat, members) = parse_conversation(&v).item().unwrap();
        assert_eq!(chat.kind, ChatKind::OneOnOne);
        assert_eq!(members, vec![ME.to_string(), ADA.to_string()]);
        let p = chat.preview.as_ref().unwrap();
        assert_eq!(
            (
                p.text.as_str(),
                p.from_id.as_deref(),
                p.from_name.as_deref()
            ),
            ("Hello there", Some(ADA), Some("Ada"))
        );
        assert_eq!(
            chat.last_read.unwrap().timestamp_millis(),
            1_700_000_000_000
        );
        assert!(chat.is_unread(ME));
    }

    #[test]
    fn the_conversation_with_yourself_is_a_chat_and_the_other_streams_are_not() {
        let last = json!({"messagetype": "RichText/Html", "content": "<p>note</p>", "imdisplayname": "Me",
            "from": format!("https://teams.microsoft.com/api/chatsvc/amer/v1/users/ME/contacts/8:orgid:{ME}"),
            "originalarrivaltime": "2026-10-27T10:00:00.000Z"});
        let notes = json!({"id": "48:notes", "threadProperties": {"threadType": "streamofnotes"}, "lastMessage": last});
        let (chat, members) = parse_conversation(&notes).item().unwrap();
        assert_eq!(
            (chat.kind, chat.id.as_str()),
            (ChatKind::OneOnOne, "48:notes")
        );
        assert_eq!(chat.title(ME), "Notes to self");
        assert!(members.is_empty());
        assert!(!chat.is_unread(ME), "your own note is never unread");
        // Another thread type under that id, or the other feeds, are not conversations.
        for (id, kind) in [
            ("48:notes", "chat"),
            ("48:notifications", "streamofnotifications"),
            ("48:mentions", "streamofmentions"),
            ("48:calllogs", "streamofcalllogs"),
        ] {
            let v =
                json!({"id": id, "threadProperties": {"threadType": kind}, "lastMessage": last});
            assert!(
                matches!(parse_conversation(&v), Parsed::Skipped),
                "{id} {kind}"
            );
        }
    }

    #[test]
    fn skips_channels_virtual_hidden_and_empty_conversations() {
        let last = json!({"messagetype": "Text", "content": "x", "originalarrivaltime": "2026-10-27T10:00:00.000Z"});
        for id in ["19:abc@thread.tacv2", "48:notifications"] {
            assert!(
                matches!(
                    parse_conversation(&json!({"id": id, "lastMessage": last})),
                    Parsed::Skipped
                ),
                "{id}"
            );
        }
        for tp in [
            json!({"threadType": "space", "spaceType": "standard", "groupId": "g"}),
            json!({"threadType": "topic"}),
        ] {
            assert!(
                matches!(
                    parse_conversation(
                        &json!({"id": "19:team@thread.skype", "threadProperties": tp, "lastMessage": last})
                    ),
                    Parsed::Skipped
                ),
                "a team is not a chat: {tp}"
            );
        }
        let hidden = json!({"id": "19:x@thread.v2", "threadProperties": {"hidden": "true"}, "lastMessage": last});
        assert!(matches!(parse_conversation(&hidden), Parsed::Skipped));
        assert!(
            matches!(
                parse_conversation(&json!({"id": "19:x@thread.v2"})),
                Parsed::Skipped
            ),
            "no lastMessage is quietly skipped"
        );
    }

    #[test]
    fn a_topic_that_is_only_a_list_of_member_ids_is_not_a_topic() {
        let last = json!({"messagetype": "Text", "content": "x", "originalarrivaltime": "2026-10-27T10:00:00.000Z"});
        let chat = |topic: &str| {
            parse_conversation(&json!({"id": "19:x@thread.v2", "threadProperties": {"topic": topic}, "lastMessage": last}))
                .item()
                .unwrap()
                .0
        };
        let ids = "8:orgid:11111111-2222-4333-8444-555555555555, 8:orgid:66666666-7777-4888-9999-aaaaaaaaaaaa";
        assert_eq!(chat(ids).topic, None);
        assert_eq!(
            chat("8:orgid:11111111-2222-4333-8444-555555555555").topic,
            None
        );
        for real in [
            "Search team",
            "Lunch, 8:30 sharp",
            "8:30 standup",
            "Ada, Linus",
        ] {
            assert_eq!(chat(real).topic.as_deref(), Some(real));
        }
    }

    #[test]
    fn group_and_meeting_kinds() {
        let last = json!({"messagetype": "Text", "content": "x", "originalarrivaltime": "2026-10-27T10:00:00.000Z"});
        let g = parse_conversation(&json!({"id": "19:x@thread.v2", "threadProperties": {"topic": "Search team"}, "lastMessage": last})).item().unwrap().0;
        assert_eq!(
            (g.kind, g.topic.as_deref()),
            (ChatKind::Group, Some("Search team"))
        );
        let m = parse_conversation(&json!({"id": "19:meeting_abc@thread.v2", "lastMessage": last}))
            .item()
            .unwrap()
            .0;
        assert_eq!(m.kind, ChatKind::Meeting);
    }

    #[test]
    fn only_a_reply_names_the_post_it_belongs_to() {
        let msg = |id: &str, root: Value| {
            parse_msg(
                &json!({"id": id, "rootMessageId": root, "messagetype": "Text", "content": "x",
                "originalarrivaltime": "2026-10-27T10:00:00Z"}),
            )
            .item()
            .unwrap()
        };
        assert_eq!(
            msg("1700", json!("1700")).root_id,
            None,
            "a post is its own root"
        );
        assert_eq!(
            msg("1710", json!("1700")).root_id.as_deref(),
            Some("1700"),
            "a reply names its post"
        );
        assert_eq!(msg("1710", json!("")).root_id, None);
        assert_eq!(msg("1710", json!(1700)).root_id, None, "not a string");
        assert_eq!(
            msg("1710", json!("1700;messageid=1")).root_id,
            None,
            "an id that is not digits is not used"
        );
        assert_eq!(
            parse_msg(&json!({"id": "1", "messagetype": "Text", "content": "x",
                "originalarrivaltime": "2026-10-27T10:00:00Z"}))
            .item()
            .unwrap()
            .root_id,
            None,
            "a chat's messages have none"
        );
    }

    #[test]
    fn an_entry_is_a_team_or_channel_only_by_its_kind_of_thread_and_its_product() {
        let entry = |tp: Value| json!({"id": "19:x@thread.tacv2", "threadProperties": tp});
        for product in CHANNEL_PRODUCT_TYPES {
            assert!(is_team_or_channel(&entry(
                json!({"threadType": "space", "productThreadType": product})
            )));
            assert!(is_team_or_channel(&entry(
                json!({"threadType": "topic", "productThreadType": product})
            )));
            assert!(is_team_or_channel(&entry(
                json!({"spaceType": "standard", "productThreadType": product})
            )));
        }
        assert!(!is_team_or_channel(&entry(
            json!({"threadType": "chat", "productThreadType": "TeamsStandardChannel"})
        )));
        assert!(!is_team_or_channel(&entry(
            json!({"threadType": "topic", "productThreadType": "TeamsMeeting"})
        )));
        assert!(!is_team_or_channel(&entry(json!({"threadType": "topic"}))));
        assert!(!is_team_or_channel(&json!({"id": "19:x@thread.v2"})));
        // What it says about activity needs an id, whatever else it has.
        assert!(
            parse_channel_activity(
                &json!({"threadProperties": {"threadType": "topic", "productThreadType": "TeamsStandardChannel"}}),
                ME
            )
            .is_none()
        );
    }

    #[test]
    fn parses_a_message_with_reactions_files_and_edit() {
        let v = json!({
            "id": "1700000000123", "messagetype": "RichText/Html", "contenttype": "text",
            "content": "<p>see <b>this</b></p>", "imdisplayname": "Ada",
            "from": format!("8:orgid:{ADA}"), "originalarrivaltime": "2026-10-27T10:00:00.000Z",
            "properties": {
                "edittime": "1793095260000",
                "emotions": "[{\"key\":\"like\",\"users\":[{\"mri\":\"a\"},{\"mri\":\"b\"}]},{\"key\":\"heart\",\"users\":[]}]",
                "files": "[{\"fileName\":\"spec.docx\",\"fileType\":\"docx\",\"objectUrl\":\"https://x/spec.docx\"}]",
                "subject": "Design",
                "mentions": format!("[{{\"@type\":\"http://schema.skype.com/Mention\",\"itemid\":0,\"mri\":\"8:orgid:{ME}\",\"mentionType\":\"person\",\"displayName\":\"Me\"}}]")
            }
        });
        let m = parse_msg(&v).item().unwrap();
        assert!(m.html && !m.system && !m.deleted);
        assert_eq!(
            (m.from_id.as_deref(), m.from_name.as_str()),
            (Some(ADA), "Ada")
        );
        assert_eq!(m.reactions.len(), 1);
        assert_eq!(m.reactions[0].count, 2);
        assert_eq!(m.attachments[0].name, "spec.docx");
        assert_eq!(m.subject.as_deref(), Some("Design"));
        assert!(m.modified > m.created);
        assert_eq!(m.mentions, vec![ME.to_string()]);
        assert!(m.mentions_me(ME) && !m.mentions_me(ADA));
    }

    #[test]
    fn plain_text_system_and_control_messages() {
        let text = parse_msg(&json!({"id": "1", "messagetype": "Text", "content": "a < b", "originalarrivaltime": "2026-10-27T10:00:00Z"})).item().unwrap();
        assert!(!text.html && !text.system);
        let sys = parse_msg(&json!({"id": "2", "messagetype": "ThreadActivity/AddMember", "content": "<addmember>..</addmember>", "originalarrivaltime": "2026-10-27T10:00:00Z"})).item().unwrap();
        assert!(sys.system && sys.body == "someone was added to the chat");
        assert!(matches!(
            parse_msg(
                &json!({"id": "3", "messagetype": "Control/Typing", "originalarrivaltime": "2026-10-27T10:00:00Z"})
            ),
            Parsed::Skipped
        ));
    }

    /// A meeting-chat conversation whose newest message is `last`.
    fn meeting_with(last: Value, horizon: Option<&str>) -> Value {
        let mut v = json!({"id": "19:meeting_abc@thread.v2", "threadProperties": {"threadType": "meeting", "topic": "Weekly"},
                           "lastMessage": last});
        if let Some(h) = horizon {
            v["properties"] = json!({ "consumptionhorizon": h });
        }
        v
    }

    fn parsed_chat(v: &Value) -> Chat {
        match parse_conversation(v) {
            Parsed::Item((chat, _)) => chat,
            other => panic!("not a chat: {:?}", matches!(other, Parsed::Skipped)),
        }
    }

    #[test]
    fn a_recording_notice_posted_by_a_service_is_automated_and_not_unread() {
        let recording = json!({"messagetype": "RichText/Media_CallRecording", "content": "<x/>", "s2spartnername": "teams",
            "from": format!("8:orgid:{ADA}"), "originalarrivaltime": "2026-10-27T10:00:00Z"});
        let chat = parsed_chat(&meeting_with(
            recording,
            Some("1700000000000;1700000000000;1"),
        ));
        let p = chat.preview.as_ref().unwrap();
        assert!(p.automated && p.from_name.is_none());
        assert!(
            !chat.is_unread(ME),
            "posted long after the marker, but nobody wrote it"
        );
    }

    #[test]
    fn a_message_a_person_wrote_is_not_automated_even_in_a_meeting_chat() {
        let said = json!({"messagetype": "RichText/Html", "content": "<p>see you</p>", "imdisplayname": "Ada",
            "from": format!("8:orgid:{ADA}"), "originalarrivaltime": "2026-10-27T10:00:00Z"});
        let chat = parsed_chat(&meeting_with(
            said.clone(),
            Some("1700000000000;1700000000000;1"),
        ));
        assert!(!chat.preview.as_ref().unwrap().automated);
        assert!(
            chat.is_unread(ME),
            "a meeting chat you have opened, with a newer message from a person"
        );
        let unopened = parsed_chat(&meeting_with(said, None));
        assert!(
            !unopened.is_unread(ME),
            "a meeting chat that was never opened"
        );
    }
    #[test]
    fn a_conversation_without_a_last_message_is_skipped_however_the_service_says_so() {
        for last in [json!(null), json!({})] {
            assert!(
                matches!(
                    parse_conversation(&json!({"id": "19:x@thread.v2", "lastMessage": last})),
                    Parsed::Skipped
                ),
                "{last}"
            );
        }
        assert!(
            matches!(
                parse_conversation(&json!({"id": "19:x@thread.v2"})),
                Parsed::Skipped
            ),
            "absent"
        );
    }

    #[test]
    fn unreadable_elements_are_malformed_rather_than_quietly_skipped() {
        assert!(
            matches!(
                parse_conversation(&json!({"lastMessage": {}})),
                Parsed::Malformed
            ),
            "no id"
        );
        let no_time = json!({"id": "19:x@thread.v2", "lastMessage": {"content": "x"}});
        assert!(
            matches!(parse_conversation(&no_time), Parsed::Malformed),
            "last message without a time"
        );
        assert!(
            matches!(
                parse_msg(&json!({"messagetype": "Text"})),
                Parsed::Malformed
            ),
            "no id"
        );
        assert!(
            matches!(
                parse_msg(&json!({"id": "1", "messagetype": "Text"})),
                Parsed::Malformed
            ),
            "no time"
        );
    }

    #[test]
    fn reaction_keys_become_emoji() {
        assert_eq!(reaction_glyph("1f37b_clinkingbeermugs"), "🍻");
        assert_eq!(reaction_glyph("1f44d_thumbsup"), "👍");
        assert_eq!(
            reaction_glyph("1f468-200d-1f4bb_manTechnologist"),
            "👨\u{200d}💻"
        );
        assert_eq!(reaction_glyph("2764_heart"), "❤\u{FE0F}");
        // The classic reactions are words; anything unrecognised is shown as its key.
        assert_eq!(reaction_glyph("like"), "👍");
        assert_eq!(reaction_glyph("heart"), "❤️");
        assert_eq!(reaction_glyph("customthing"), "customthing");
        // A word that happens to be all hex digits is not a code point without the `_name` part.
        assert_eq!(reaction_glyph("face"), "face");
        assert_eq!(reaction_glyph("zzzz_notHex"), "zzzz_notHex");
        assert_eq!(reaction_glyph("_x"), "_x");
        // Not a code point at all (too big): keep the text rather than invent something.
        assert_eq!(reaction_glyph("ffffffff_nope"), "ffffffff_nope");
    }

    #[test]
    fn a_reaction_key_cannot_decode_to_controls_bidi_marks_or_a_huge_string() {
        for key in ["1b_x", "202e_x", "1b-5b-32-4a_x", "2028_x"] {
            let glyph = reaction_glyph(key);
            assert!(
                glyph
                    .chars()
                    .all(|c| !c.is_control() && !c.is_whitespace() && c != '\u{202E}'),
                "{key:?} -> {glyph:?}"
            );
        }
        let long = format!("{}_x", vec!["1f600"; 5_000].join("-"));
        assert_eq!(reaction_glyph(&long).chars().count(), 16);
    }

    #[test]
    fn people_who_reacted_are_listed_by_object_id() {
        let m = parse_msg(&json!({
            "id": "5", "messagetype": "Text", "content": "x", "originalarrivaltime": "2026-10-27T10:00:00Z",
            "properties": {"emotions": [{"key": "like", "users": [{"mri": format!("8:orgid:{ADA}")}, {"mri": format!("8:orgid:{ME}")}]}]}
        }));
        let Parsed::Item(m) = m else {
            panic!("not a message")
        };
        assert_eq!(m.reactions[0].users, [ADA.to_string(), ME.to_string()]);
    }

    #[test]
    fn message_reactions_use_the_decoded_emoji() {
        let v = json!({
            "id": "1", "messagetype": "Text", "content": "Can you read this?", "originalarrivaltime": "2026-10-27T10:00:00Z",
            "properties": {"emotions": [{"key": "1f37b_clinkingbeermugs", "users": [{"mri": "a"}]}]}
        });
        let m = parse_msg(&v).item().unwrap();
        assert_eq!(
            (m.reactions[0].emoji.as_str(), m.reactions[0].count),
            ("🍻", 1)
        );
    }

    #[test]
    fn events_in_a_meeting_chat_are_named_for_what_they_are() {
        for (message_type, text) in [
            ("ThreadActivity/MemberJoined", "someone joined"),
            ("ThreadActivity/MemberLeft", "someone left"),
            (
                "ThreadActivity/MeetingPolicyUpdated",
                "meeting options changed",
            ),
            ("RichText/Media_CallRecording", "call recording"),
            ("RichText/Media_CallTranscript", "call transcript"),
            ("Event/Call", "call"),
            ("SomethingNew/Else", "chat updated"),
        ] {
            let m = parse_msg(&json!({"id": "2", "messagetype": message_type, "content": "<x/>", "originalarrivaltime": "2026-10-27T10:00:00Z"}))
                .item()
                .unwrap();
            assert!(m.system, "{message_type}");
            assert_eq!(m.body, text, "{message_type}");
        }
    }
    #[test]
    fn deleted_messages_are_flagged() {
        let v = json!({"id": "1", "messagetype": "RichText/Html", "content": "", "originalarrivaltime": "2026-10-27T10:00:00Z", "properties": {"deletetime": 1793095260000i64}});
        assert!(parse_msg(&v).item().unwrap().deleted);
    }

    #[test]
    fn mentions_become_teams_spans() {
        let ms = [OutMention {
            id: 0,
            user_id: ADA.into(),
            name: "Ada".into(),
        }];
        let (html, props) = teams_mentions("<p><at id=\"0\">Ada</at> hi</p>", &ms);
        assert_eq!(
            html,
            "<p><span itemscope=\"\" itemtype=\"http://schema.skype.com/Mention\" itemid=\"0\">Ada</span> hi</p>"
        );
        let list: Value = serde_json::from_str(&props.unwrap()).unwrap();
        assert_eq!(
            (list[0]["mri"].as_str(), list[0]["itemid"].as_i64()),
            (Some(format!("8:orgid:{ADA}").as_str()), Some(0))
        );
        assert_eq!(
            teams_mentions("<p>x</p>", &[]),
            ("<p>x</p>".to_string(), None)
        );
    }

    #[test]
    fn reply_quote_html_builds_teams_blockquote_markup() {
        let target = ReplyTarget {
            id: "1793985834981".into(),
            from_id: Some(ADA.into()),
            from_name: "Ada".into(),
            preview: "see you then".into(),
        };
        assert_eq!(
            reply_quote_html(&target),
            "<blockquote itemscope=\"\" itemtype=\"http://schema.skype.com/Reply\" itemid=\"1793985834981\">\
             <strong itemprop=\"mri\" itemid=\"8:orgid:22222222-2222-2222-2222-222222222222\">Ada</strong>\
             <span itemprop=\"time\" itemid=\"1793985834981\"></span>\
             <p itemprop=\"preview\">see you then</p></blockquote>"
        );
    }

    #[test]
    fn reply_quote_html_without_a_sender_id_drops_the_mri_attribute() {
        let target = ReplyTarget {
            id: "1".into(),
            from_id: None,
            from_name: "A bot".into(),
            preview: "hi".into(),
        };
        let html = reply_quote_html(&target);
        assert!(html.contains("<strong>A bot</strong>"), "{html}");
        assert!(!html.contains("itemprop=\"mri\""), "{html}");
    }

    #[test]
    fn reply_quote_html_escapes_free_text() {
        let target = ReplyTarget {
            id: "1".into(),
            from_id: Some(ADA.into()),
            from_name: "<script>".into(),
            preview: "a & b \"quoted\"".into(),
        };
        let html = reply_quote_html(&target);
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("a &amp; b &quot;quoted&quot;"), "{html}");
    }

    #[test]
    fn qtd_msgs_json_matches_a_genuine_replys_own_shape() {
        let target = ReplyTarget {
            id: "1793790852152".into(),
            from_id: Some(ADA.into()),
            from_name: "Ada".into(),
            preview: "see you then".into(),
        };
        assert_eq!(
            qtd_msgs_json(&target),
            format!(
                r#"[{{"messageId":"1793790852152","sender":"8:orgid:{ADA}","time":1793790852152}}]"#
            )
        );
    }

    #[test]
    fn reading_undoes_teams_mentions_round_trip() {
        let ms = [OutMention {
            id: 0,
            user_id: ADA.into(),
            name: "Ada".into(),
        }];
        let (sent, _) = teams_mentions("<p><at id=\"0\">Ada</at> hi</p>", &ms);
        assert_eq!(
            normalize_mentions(&sent, &HashMap::new()),
            "<p><at id=\"0\">Ada</at> hi</p>",
            "no entry for itemid 0: it keeps its original numeric form"
        );
        // Two mentions, and text with no mention at all, both round-trip.
        let ms2 = [
            OutMention {
                id: 0,
                user_id: ADA.into(),
                name: "Ada".into(),
            },
            OutMention {
                id: 1,
                user_id: "grace".into(),
                name: "Grace".into(),
            },
        ];
        let (sent2, _) = teams_mentions("<at id=\"0\">Ada</at> and <at id=\"1\">Grace</at>", &ms2);
        assert_eq!(
            normalize_mentions(&sent2, &HashMap::new()),
            "<at id=\"0\">Ada</at> and <at id=\"1\">Grace</at>"
        );
        assert_eq!(
            normalize_mentions("<p>no mention here</p>", &HashMap::new()),
            "<p>no mention here</p>"
        );
        // When properties.mentions does have an entry, the small index is rewritten to the real (bare) id.
        let by_itemid = mention_ids(&json!([{"itemid": 0, "mri": format!("8:orgid:{ADA}")}]));
        assert_eq!(
            normalize_mentions(&sent, &by_itemid),
            format!("<p><at id=\"{ADA}\">Ada</at> hi</p>")
        );
    }

    #[test]
    fn a_mention_span_that_never_closes_loses_nothing_before_it() {
        let open =
            "<span itemscope=\"\" itemtype=\"http://schema.skype.com/Mention\" itemid=\"0\">";
        let cut = format!("<p>hello there {open}Ada and more");
        assert_eq!(normalize_mentions(&cut, &HashMap::new()), cut);
        // Text and an ordinary mention before the broken one are kept too.
        let two = format!("<p>one {open}Ada</span> two {open}Grace");
        assert_eq!(
            normalize_mentions(&two, &HashMap::new()),
            format!("<p>one <at id=\"0\">Ada</at> two {open}Grace")
        );
    }

    #[test]
    fn an_id_is_written_into_its_attribute_without_a_raw_angle_bracket() {
        // `attr_value` only reads double-quoted values, so a quote cannot be in one; a `<` can.
        let hostile =
            "<span itemtype=\"http://schema.skype.com/Mention\" itemid=\"a<b\">Ada</span>";
        assert_eq!(
            normalize_mentions(hostile, &HashMap::new()),
            "<at id=\"a&lt;b\">Ada</at>"
        );
    }
    #[test]
    fn a_message_with_teams_own_mention_markup_is_normalized_before_the_app_ever_sees_it() {
        let v = json!({
            "id": "1", "messagetype": "RichText/Html", "contenttype": "text",
            "content": "<p><span itemscope=\"\" itemtype=\"http://schema.skype.com/Mention\" itemid=\"0\">John</span> hi</p>",
            "originalarrivaltime": "2026-10-27T10:00:00Z",
            "properties": { "mentions": format!("[{{\"itemid\":0,\"mri\":\"8:orgid:{ME}\"}}]") }
        });
        let m = parse_msg(&v).item().unwrap();
        assert_eq!(
            m.body,
            format!("<p><at id=\"{ME}\">John</at> hi</p>"),
            "the small itemid is rewritten to the real (bare) id, so the app can tell this mention is of ME \
             without a separate lookup"
        );
        assert!(!m.body.contains("skype.com"), "{}", m.body);
        assert!(m.mentions_me(ME));
    }

    #[test]
    fn an_itemid_with_no_matching_mentions_entry_keeps_its_numeric_form() {
        let v = json!({
            "id": "1", "messagetype": "RichText/Html", "contenttype": "text",
            "content": "<p><span itemscope=\"\" itemtype=\"http://schema.skype.com/Mention\" itemid=\"0\">John</span> hi</p>",
            "originalarrivaltime": "2026-10-27T10:00:00Z",
        });
        let m = parse_msg(&v).item().unwrap();
        assert_eq!(m.body, "<p><at id=\"0\">John</at> hi</p>");
    }

    #[test]
    fn the_mention_spans_attributes_are_not_always_in_the_same_order() {
        // Exactly as seen live, 2026-11-06: `itemtype` before `itemscope`, not after, unlike every earlier
        // capture (including the fixture above) and the one documented example.
        let v = json!({
            "id": "1", "messagetype": "RichText/Html", "contenttype": "text",
            "content": "<p>is it complete now&nbsp;<span itemtype=\"http://schema.skype.com/Mention\" itemscope=\"\" itemid=\"0\">Ada Lovelace</span>?</p>",
            "originalarrivaltime": "2026-10-27T10:00:00Z",
            "properties": { "mentions": format!("[{{\"itemid\":0,\"mri\":\"8:orgid:{ME}\"}}]") }
        });
        let m = parse_msg(&v).item().unwrap();
        assert_eq!(
            m.body,
            format!("<p>is it complete now&nbsp;<at id=\"{ME}\">Ada Lovelace</at>?</p>")
        );
        assert!(m.mentions_me(ME));
    }

    #[test]
    fn a_span_that_is_not_a_mention_is_left_completely_untouched() {
        let v = json!({
            "id": "1", "messagetype": "RichText/Html", "contenttype": "text",
            "content": "<p>look <span style=\"color:#B6424C\">here</span></p>",
            "originalarrivaltime": "2026-10-27T10:00:00Z",
        });
        let m = parse_msg(&v).item().unwrap();
        assert_eq!(
            m.body,
            "<p>look <span style=\"color:#B6424C\">here</span></p>"
        );
    }

    #[test]
    fn people_suggestions_become_contacts() {
        let v = json!({"Groups": [{"Suggestions": [
            {"Id": format!("{ADA}@contoso.onmicrosoft.com"), "MRI": format!("8:orgid:{ADA}"), "DisplayName": "Ada Lovelace",
             "EmailAddresses": ["ada@contoso.com", "other@contoso.com"], "JobTitle": "Analyst", "Department": "Engines"},
            {"Id": ADA, "DisplayName": "Duplicate"},
            {"Id": "not-an-id", "DisplayName": "Nobody"}
        ]}]});
        let people = parse_people(&v);
        assert_eq!(people.len(), 1);
        assert_eq!(
            (
                people[0].id.as_str(),
                people[0].name.as_str(),
                people[0].email.as_str()
            ),
            (ADA, "Ada Lovelace", "ada@contoso.com")
        );
        assert_eq!(people[0].detail, "Analyst, Engines");
    }

    #[test]
    fn a_contact_has_a_work_number_and_an_office_in_either_form_the_service_may_send() {
        let person = |phones: Value| {
            let v = json!({"Groups": [{"Suggestions": [{"Id": ADA, "DisplayName": "Ada",
                "Phones": phones, "OfficeLocation": "Building 4"}]}]});
            parse_people(&v).remove(0)
        };
        let ada = person(json!([
            {"Type": "Mobile", "Number": "+1 555 0101"},
            {"Type": "Business", "Number": "+1 555 0100"}
        ]));
        assert_eq!(ada.phone, "+1 555 0100", "a business number first");
        assert_eq!(ada.office, "Building 4");
        assert_eq!(person(json!(["+1 555 0102"])).phone, "+1 555 0102");
        assert_eq!(
            person(json!([{"type": "mobile", "number": " +1 555 0103 "}])).phone,
            "+1 555 0103",
            "any other number when there is no business one"
        );
        for nothing in [
            json!(null),
            json!("+1 555"),
            json!([{"Type": "Business"}]),
            json!([]),
        ] {
            assert_eq!(person(nothing.clone()).phone, "", "{nothing}");
        }
        assert_eq!(
            person(json!(["+1 555\u{1b}[2J"])).phone,
            "+1 555[2J",
            "cleaned like every other remote text"
        );
    }

    #[test]
    fn object_ids_come_from_mri_guid_or_base64() {
        assert_eq!(
            object_id("", &format!("8:orgid:{}", ADA.to_uppercase())).as_deref(),
            Some(ADA)
        );
        assert_eq!(object_id(&format!("{ADA}@t"), "").as_deref(), Some(ADA));
        // base64 of the byte-swapped GUID 00112233-4455-6677-8899-aabbccddeeff
        let bytes = [
            0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert_eq!(
            object_id(&b64, "").as_deref(),
            Some("00112233-4455-6677-8899-aabbccddeeff")
        );
        assert_eq!(object_id("short", ""), None);
    }

    #[test]
    fn one_on_one_ids_are_the_sorted_pair_and_match_regardless_of_order() {
        assert_eq!(
            one_on_one_id(ME, ADA),
            format!("19:{ME}_{ADA}@unq.gbl.spaces")
        );
        assert_eq!(one_on_one_id(ADA, ME), one_on_one_id(ME, ADA));
        assert_eq!(
            pair_members(&one_on_one_id(ADA, ME)),
            vec![ME.to_string(), ADA.to_string()]
        );
    }

    #[test]
    fn generated_guids_look_like_v4_guids_and_differ() {
        let (a, b) = (guid(), guid());
        assert!(is_guid(&a) && a.as_bytes()[14] == b'4', "{a}");
        assert_ne!(a, b);
    }

    use proptest::prelude::*;

    /// Arbitrary JSON, nested a few levels deep.
    fn any_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            any::<String>().prop_map(Value::from),
        ];
        leaf.prop_recursive(4, 48, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
                prop::collection::hash_map("[a-zA-Z]{1,10}", inner, 0..5)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    /// Objects that use the field names the parsers read, with arbitrary values, so the interesting branches are reached.
    fn plausible_item() -> impl Strategy<Value = Value> {
        const KEYS: [&str; 18] = [
            "id",
            "messagetype",
            "originalarrivaltime",
            "composetime",
            "content",
            "from",
            "imdisplayname",
            "properties",
            "lastMessage",
            "threadProperties",
            "members",
            "topic",
            "emotions",
            "files",
            "edittime",
            "deletetime",
            "hidden",
            "consumptionhorizon",
        ];
        prop::collection::hash_map(prop::sample::select(KEYS.to_vec()), any_json(), 0..10)
            .prop_map(|m| Value::Object(m.into_iter().map(|(k, v)| (k.to_string(), v)).collect()))
    }

    proptest! {
        #[test]
        fn parsers_accept_any_json_without_panicking(v in any_json()) {
            let _ = parse_conversation(&v);
            let _ = parse_msg(&v);
            let _ = parse_people(&v);
        }

        #[test]
        fn parsers_accept_plausible_objects_and_only_return_sanitised_text(v in plausible_item()) {
            if let Parsed::Item(m) = parse_msg(&v) {
                let clean_text = |s: &str| !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t');
                prop_assert!(clean_text(&m.body) && clean_text(&m.from_name));
                prop_assert!(m.modified >= m.created || m.modified.timestamp_millis() > 0);
            }
            if let Parsed::Item((chat, ids)) = parse_conversation(&json!({"id": "19:x@thread.v2", "lastMessage": v, "threadProperties": {"topic": "t"}})) {
                prop_assert!(chat.id.starts_with("19:") && ids.iter().all(|i| !i.is_empty()));
            }
        }

        #[test]
        fn reaction_keys_of_any_shape_give_text(key in any::<String>()) {
            let _ = reaction_glyph(&key);
        }

        #[test]
        fn object_ids_are_always_lower_case_guids(id in any::<String>(), mri in any::<String>()) {
            if let Some(found) = object_id(&id, &mri) {
                prop_assert!(is_guid(&found) && found == found.to_ascii_lowercase());
            }
        }

        #[test]
        fn a_one_on_one_chat_has_one_id_whichever_side_asks(a in "[0-9a-f-]{1,36}", b in "[0-9a-f-]{1,36}") {
            prop_assert_eq!(one_on_one_id(&a, &b), one_on_one_id(&b, &a));
        }

        #[test]
        fn sender_ids_are_never_empty(s in any::<String>()) {
            if let Some(id) = mri(&s) {
                prop_assert!(!id.is_empty());
            }
        }

        #[test]
        fn mentions_keep_every_id_in_step_with_the_markup(n in 1usize..5) {
            let ms: Vec<OutMention> = (0..n).map(|i| OutMention { id: i as u32, user_id: format!("u{i}"), name: format!("N{i}") }).collect();
            let html: String = (0..n).map(|i| format!("<at id=\"{i}\">N{i}</at> ")).collect();
            let (out, props) = teams_mentions(&html, &ms);
            prop_assert_eq!(out.matches("schema.skype.com/Mention").count(), n);
            prop_assert!(!out.contains("<at") && !out.contains("</at>"));
            let list: Value = serde_json::from_str(&props.unwrap()).unwrap();
            prop_assert_eq!(list.as_array().unwrap().len(), n);
        }
    }
}

#[cfg(test)]
mod presence_note_tests {
    use super::*;

    const ADA: &str = "22222222-2222-2222-2222-222222222222";

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-21T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn one(presence: Value) -> Presence {
        let answer = json!([{"mri": format!("8:orgid:{ADA}"), "etag": "E1", "etagMatch": false,
                             "status": 20000, "presence": presence}]);
        presence_of(&answer)
    }

    fn presence_of(answer: &Value) -> Presence {
        super::presence(answer, now())
            .remove(ADA)
            .expect("an entry")
    }

    fn note_only(note: Value) -> Option<StatusNote> {
        one(json!({"availability": "Available", "note": note})).note
    }

    #[test]
    fn a_status_message_with_html_is_plain_text_with_its_times() {
        let note = note_only(json!({
            "message": "<a href=\"https://example.test/x\">Back soon</a>&nbsp;&amp; <b>reachable</b> on 555-0100",
            "expiry": "2026-11-22T09:30:00Z",
            "publishTime": "2026-11-21T10:11:12.1234567Z",
        }))
        .unwrap();
        assert_eq!(note.text, "Back soon & reachable on 555-0100");
        assert_eq!(note.expires, Some(at("2026-11-22T09:30:00Z")));
        assert_eq!(note.published, Some(at("2026-11-21T10:11:12.1234567Z")));
    }

    #[test]
    fn an_out_of_office_reply_is_read_with_its_message_and_seven_digit_times() {
        let p = one(json!({
            "availability": "Away",
            "calendarData": {"isOutOfOffice": true, "outOfOfficeNote": {
                "message": "Out of office until 3:00 PM, 12/05.",
                "expiry": "2026-12-05T15:00:00.0000000Z",
                "publishTime": "2026-11-21T08:00:00.123456Z"}},
        }));
        assert!(p.out_of_office);
        let note = p.out_of_office_note.unwrap();
        assert_eq!(note.text, "Out of office until 3:00 PM, 12/05.");
        assert_eq!(note.expires, Some(at("2026-12-05T15:00:00Z")));
        assert!(note.published.is_some());
        assert_eq!(
            p.note, None,
            "an out-of-office reply is not a status message"
        );
    }

    #[test]
    fn out_of_office_without_a_message_has_no_note_and_no_calendar_means_not_out() {
        let p = one(json!({"availability": "Offline", "calendarData": {"isOutOfOffice": true}}));
        assert!(p.out_of_office);
        assert_eq!(p.out_of_office_note, None);
        let p = one(json!({"availability": "Offline"}));
        assert!(!p.out_of_office && p.note.is_none() && p.out_of_office_note.is_none());
        // A reply left over from a calendar entry that is not on now is not shown as the person's.
        let p = one(
            json!({"availability": "Available", "calendarData": {"isOutOfOffice": false,
            "outOfOfficeNote": {"message": "old reply"}}}),
        );
        assert_eq!(p.out_of_office_note, None);
    }

    #[test]
    fn a_note_that_has_expired_is_dropped_and_one_about_to_expire_is_kept() {
        assert_eq!(
            note_only(json!({"message": "gone", "expiry": "2026-11-21T11:59:59Z"})),
            None
        );
        assert_eq!(
            note_only(json!({"message": "gone", "expiry": "2026-11-21T12:00:00Z"})),
            None,
            "an expiry that is now has passed"
        );
        assert!(note_only(json!({"message": "here", "expiry": "2026-11-21T12:00:01Z"})).is_some());
        let p = one(
            json!({"availability": "Away", "calendarData": {"isOutOfOffice": true,
            "outOfOfficeNote": {"message": "back", "expiry": "2026-11-20T12:00:00.5000000Z"}}}),
        );
        assert!(p.out_of_office, "still out of office, as the calendar says");
        assert_eq!(p.out_of_office_note, None, "but the reply has lapsed");
    }

    #[test]
    fn an_expiry_years_away_means_never_and_a_bad_time_is_no_time() {
        for far in ["9999-12-31T23:59:59Z", "2032-01-01T00:00:00Z"] {
            let note = note_only(json!({"message": "always", "expiry": far})).unwrap();
            assert_eq!(note.expires, None, "{far}");
        }
        let near = note_only(json!({"message": "x", "expiry": "2030-01-01T00:00:00Z"})).unwrap();
        assert_eq!(
            near.expires,
            Some(at("2030-01-01T00:00:00Z")),
            "four years is a date"
        );
        for bad in [
            json!("yesterday"),
            json!(""),
            json!(12345),
            json!(null),
            json!({"a": 1}),
            json!("2026-13-45T99:00:00Z"),
        ] {
            let note = note_only(
                json!({"message": "x", "expiry": bad.clone(), "publishTime": bad.clone()}),
            )
            .unwrap_or_else(|| panic!("{bad}"));
            assert_eq!((note.expires, note.published), (None, None), "{bad}");
        }
    }

    #[test]
    fn a_note_that_is_not_a_usable_object_is_no_note() {
        for bad in [
            json!("a string"),
            json!(5),
            json!(null),
            json!([{"message": "in an array"}]),
            json!({}),
            json!({"message": 7}),
            json!({"message": null}),
            json!({"message": ""}),
            json!({"message": "   \n\t "}),
            json!({"message": "<p></p><br>&nbsp;"}),
            json!({"text": "wrong key"}),
        ] {
            assert_eq!(note_only(bad.clone()), None, "{bad}");
        }
        // And a bad note does not stop the rest of the answer from being read.
        let p = one(json!({"availability": "Busy", "note": "oops"}));
        assert_eq!(p.availability, Availability::Busy);
    }

    #[test]
    fn hostile_text_is_made_plain_one_line_and_short() {
        let marker = '\u{E001}';
        let text = format!(
            "<script>alert(1)</script>Hi\u{1b}]52;c;AAAA\u{7}\u{202E}there{marker}\u{F0001}\u{200F}\r\n\n\tdone &#27;[31m"
        );
        let note = note_only(json!({"message": text})).unwrap();
        for c in note.text.chars() {
            assert!(
                !c.is_control()
                    && !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
                    && !matches!(c, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{10FFFF}'),
                "{c:?} in {:?}",
                note.text
            );
        }
        // The escape and bell are gone, so what was a sequence is only letters.
        assert!(
            note.text.contains("Hi]52;c;AAAAthere done"),
            "{:?}",
            note.text
        );
        assert!(!note.text.contains(['\n', '\t', '\r']));

        let long = "word ".repeat(2_500);
        let note = note_only(json!({"message": long})).unwrap();
        assert_eq!(note.text.chars().count(), StatusNote::MAX_CHARS);
        assert!(note.text.ends_with('…'));

        let nested = format!("{}deep{}", "<b>".repeat(5_000), "</b>".repeat(5_000));
        assert_eq!(note_only(json!({"message": nested})).unwrap().text, "deep");
    }

    #[test]
    fn what_was_there_before_is_still_read_with_and_without_notes() {
        let answer = json!([
            {"mri": format!("8:orgid:{ADA}"), "presence": {"availability": "Busy",
                "calendarData": {"isOutOfOffice": true}}},
            {"mri": "8:orgid:33333333-3333-3333-3333-333333333333", "etagMatch": true},
            {"mri": "4:+15555550100", "presence": {"availability": "Available", "note": {"message": "hi"}}},
            {"mri": "8:orgid:44444444-4444-4444-4444-444444444444", "presence": {"note": {"message": "no availability"}}},
        ]);
        let got = super::presence(&answer, now());
        assert_eq!(got.len(), 1);
        assert_eq!(got[ADA], Presence::new(Availability::Busy, true));
    }
}
