//! Backend-independent data model and the [`Backend`] trait that Microsoft Graph, the Teams web API and any other
//! source of chats implement.
//!
//! The record types are plain data with public fields so applications (and test doubles) can construct them freely.
//! Every id is a bare Entra object id, whatever the service itself calls it, so ids compare equal across backends.

use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use image::RgbImage;

/// What kind of conversation a chat is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChatKind {
    /// A conversation between two people.
    OneOnOne,
    /// A named or unnamed group chat.
    Group,
    /// The chat attached to a meeting.
    Meeting,
}

/// A participant in a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Entra object id.
    pub id: String,
    /// Display name; empty when not (yet) known.
    pub name: String,
    /// The organisation's directory was asked about this person and did not list them: the account has been deleted,
    /// as it is some time after someone leaves, or is not one the directory can return. The name, if any, is from what
    /// they left behind (messages, an earlier answer). False when the directory was never asked or listed them.
    pub not_in_directory: bool,
}

/// The signed-in user.
#[derive(Debug, Clone)]
pub struct Me {
    /// Entra object id.
    pub id: String,
    /// Display name; may be empty if the service did not say.
    pub name: String,
}

/// The newest message of a chat, as shown in a list.
#[derive(Debug, Clone)]
pub struct Preview {
    /// When it was sent.
    pub created: DateTime<Utc>,
    /// Sender's object id, if it is a person.
    pub from_id: Option<String>,
    /// Sender's display name.
    pub from_name: Option<String>,
    /// Plain one-line text (already stripped of HTML).
    pub text: String,
    /// Whether a service posted it on someone's behalf rather than a person writing it, such as a meeting recording
    /// notice. Such a message does not make a chat unread.
    pub automated: bool,
}

/// A conversation.
#[derive(Debug, Clone)]
pub struct Chat {
    /// Service-assigned chat id (opaque; pass it back to the backend unchanged).
    pub id: String,
    /// One-on-one, group or meeting.
    pub kind: ChatKind,
    /// The chat's name, when it has one.
    pub topic: Option<String>,
    /// Participants (names may be empty if lookup failed).
    pub members: Vec<Member>,
    /// When the chat last changed; the list is ordered by this.
    pub updated: DateTime<Utc>,
    /// The newest message, if any.
    pub preview: Option<Preview>,
    /// Server-side "read up to" marker for the signed-in user.
    pub last_read: Option<DateTime<Utc>>,
    /// A link that opens the chat in the Teams web app.
    pub web_url: Option<String>,
    /// Where the chat is among the signed-in user's favourites (1 is first), if it is one.
    pub favorite: Option<u32>,
}

impl Chat {
    /// Title shown in a list: the topic, or the other participants' names.
    #[must_use]
    pub fn title(&self, me_id: &str) -> String {
        self.title_parts(me_id)
            .into_iter()
            .map(|(text, _)| text)
            .collect()
    }

    /// [`Chat::title`] in pieces, each with whether it is the name of someone the directory no longer lists
    /// ([`Member::not_in_directory`]), so a client can draw those names differently. Joined, the pieces are the title.
    #[must_use]
    pub fn title_parts(&self, me_id: &str) -> Vec<(String, bool)> {
        if let Some(t) = self
            .topic
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            return vec![(t.to_string(), false)];
        }
        let others = self.members.iter().filter(|m| m.id != me_id);
        let total = others.clone().count();
        let named: Vec<&Member> = others.filter(|m| !m.name.is_empty()).collect();
        if named.is_empty() {
            // Nobody else is a member only when the roster says so; an empty roster, or people whose names could not
            // be looked up, is not knowing who is there, and "(just you)" would be a false statement.
            let text = if self.members.is_empty() || total > 0 {
                "(untitled chat)"
            } else {
                "(just you)"
            };
            return vec![(text.to_string(), false)];
        }
        // Everyone who is there counts, named or not.
        let shown = &named[..named.len().min(if total <= 3 { 3 } else { 2 })];
        let rest = total - shown.len();
        let mut parts = Vec::with_capacity(shown.len() * 2);
        for (i, m) in shown.iter().enumerate() {
            if i > 0 {
                parts.push((", ".to_string(), false));
            }
            parts.push((m.name.clone(), m.not_in_directory));
        }
        if rest > 0 {
            parts.push((format!(" +{rest}"), false));
        }
        parts
    }

    /// Whether there is something to read: the newest message is newer than the read marker, was written by someone
    /// other than `me_id`, and is not an automated notice. A chat with no read marker has never been opened, which
    /// makes it unread unless it is a meeting chat, which a person is added to without asking.
    #[must_use]
    pub fn is_unread(&self, me_id: &str) -> bool {
        unread_after(
            self.preview.as_ref(),
            self.last_read,
            me_id,
            self.kind != ChatKind::Meeting,
        )
    }
}

/// The one rule for "there is something to read", for a chat ([`Chat::is_unread`]) and for a channel the
/// conversation list mentions ([`ChannelActivity`]): the newest message (`preview`) is after the read marker, was
/// not written by `me_id` and is not automated. With no marker the conversation has never been opened, which is
/// unread when `unmarked_is_unread`; no message at all is never unread.
pub(crate) fn unread_after(
    preview: Option<&Preview>,
    last_read: Option<DateTime<Utc>>,
    me_id: &str,
    unmarked_is_unread: bool,
) -> bool {
    let Some(p) = preview else { return false };
    if p.automated || p.from_id.as_deref() == Some(me_id) {
        return false;
    }
    match last_read {
        Some(read) => p.created > read,
        None => unmarked_is_unread,
    }
}

/// Whether a message has reached the service. Backends only ever return [`Delivery::Sent`] messages; the other two
/// states belong to a client that shows a message before the service has confirmed it (an optimistic send), so any
/// chat UI built on this crate can use the same type for its pending and failed messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// It is on the server.
    Sent,
    /// It is being sent.
    Sending,
    /// Sending failed; the text says why.
    Failed(String),
}

/// A file or card attached to a message.
#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    /// File name or title.
    pub name: String,
    /// Content type or file extension, as the service reports it.
    pub content_type: String,
    /// Where it can be opened, if the service gave a link.
    pub url: Option<String>,
}

/// An emoji a service can react with, for offering a choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiInfo {
    /// The emoji itself.
    pub emoji: String,
    /// What the service calls it, such as `Party popper`.
    pub name: String,
    /// Other words it can be found by (`celebrate`).
    pub aliases: Vec<String>,
}

/// A reaction on a message, grouped by emoji.
#[derive(Debug, Clone, PartialEq)]
pub struct Reaction {
    /// The emoji to show.
    pub emoji: String,
    /// How many people reacted with it.
    pub count: usize,
    /// Object ids of the people who did, as far as the service says (it may name fewer than `count`).
    pub users: Vec<String>,
    /// Display names of those people, in the same order as `users`; each is the person's display name if the backend
    /// knows it, else empty.
    pub names: Vec<String>,
}

/// A message in a chat.
#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    /// Service-assigned id, unique within the chat.
    pub id: String,
    /// When it was sent.
    pub created: DateTime<Utc>,
    /// When the service last changed it, for any reason (a reaction counts on some services); equal to `created` if
    /// never. Use [`Msg::edited`] to tell whether the text was changed.
    pub modified: DateTime<Utc>,
    /// When its sender last edited the text, if they did. Not set by a reaction.
    pub edited: Option<DateTime<Utc>>,
    /// Sender's object id, if it is a person.
    pub from_id: Option<String>,
    /// Sender's display name.
    pub from_name: String,
    /// Messages that are not human chat (member added, call ended, ...).
    pub system: bool,
    /// Deleted by its sender.
    pub deleted: bool,
    /// Whether `body` is HTML (otherwise plain text).
    pub html: bool,
    /// The message text, sanitised of terminal control characters.
    pub body: String,
    /// The message's subject line, if it has one.
    pub subject: Option<String>,
    /// Attached files.
    pub attachments: Vec<Attachment>,
    /// Reactions, grouped by emoji.
    pub reactions: Vec<Reaction>,
    /// Object ids of everyone @mentioned in `body`, from Teams' or Graph's own mention markup (not matched from
    /// plain text), in writing order; not necessarily deduplicated if the sender mentioned someone twice.
    pub mentions: Vec<String>,
    /// Whether it has reached the server (always [`Delivery::Sent`] for messages read from a backend).
    pub delivery: Delivery,
    /// In a channel, the id of the post this message is a reply to: `Some` only for a reply, `None` for a post, for
    /// every message of a chat, and on a service with no threads. A channel's messages come as one feed in which
    /// posts and replies are mixed; only the post carries a [`Msg::subject`].
    pub root_id: Option<String>,
}

impl Msg {
    /// Whether `me_id` is among the people @mentioned in this message.
    #[must_use]
    pub fn mentions_me(&self, me_id: &str) -> bool {
        self.mentions.iter().any(|id| id == me_id)
    }
}

/// One page of messages.
#[derive(Debug, Clone, Default)]
pub struct Page {
    /// Oldest first.
    pub messages: Vec<Msg>,
    /// Opaque cursor for the next (older) page, to pass back to [`Backend::messages`].
    pub older: Option<String>,
}

/// A team: a group of people with channels (Teams web only).
#[derive(Debug, Clone)]
pub struct Team {
    /// The team's id, which is also the id of its General channel.
    pub id: String,
    /// Its name.
    pub name: String,
    /// The team has been archived (read-only).
    pub archived: bool,
    /// The person has muted the team's notifications.
    pub muted: bool,
    /// How many members the service says it has, when it says.
    pub members: Option<u32>,
    /// Its channels, General first, in the service's order.
    pub channels: Vec<Channel>,
}

/// The kind of a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChannelKind {
    /// Open to everyone in the team.
    Standard,
    /// Only for chosen members of the team.
    Private,
    /// Shared with people outside the team.
    Shared,
}

/// A channel of a team (Teams web only). Its id is a conversation id like a chat's: [`Backend::messages`] reads it.
#[derive(Debug, Clone)]
pub struct Channel {
    /// The conversation id.
    pub id: String,
    /// The channel's name.
    pub name: String,
    /// The team's General channel (its id is the team's id).
    pub general: bool,
    /// Standard, private or shared.
    pub kind: ChannelKind,
    /// The channel has been archived (read-only).
    pub archived: bool,
    /// Believed, not verified, to mean a channel the person has chosen to see under its team (the channel's own
    /// `isFavorite` flag, which is **not** the person's favourites: see [`Channel::favorite`]); the General channel is
    /// always shown.
    pub shown: bool,
    /// The person has pinned it.
    pub pinned: bool,
    /// The person has muted it.
    pub muted: bool,
    /// The person follows it (is notified of new posts).
    pub followed: bool,
    /// Its place among the person's favourites, from the "Favorites" conversation folder (1 is first, like
    /// [`Chat::favorite`]); `None` when it is not one. A favourite team's root conversation is the team's General
    /// channel's.
    pub favorite: Option<u32>,
    /// There are messages the person has not read.
    pub unread: bool,
    /// The newest message, if the service gives it.
    pub last: Option<Preview>,
}

/// What is currently known of one channel's activity, from the conversation list the application already polls, which
/// is much fresher and cheaper than the teams list itself (see [`Backend::channel_activity`]).
#[derive(Debug, Clone)]
pub struct ChannelActivity {
    /// The channel's conversation id (a team's General channel has the team's id).
    pub id: String,
    /// There are messages the person has not read, by the same rule as [`Chat::is_unread`].
    pub unread: bool,
    /// The newest message.
    pub last: Option<Preview>,
}

/// A message that matched a search, as the service reports it: enough to list it and to find it in its chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// The message's id in its chat, the same as [`Msg::id`].
    pub message_id: String,
    /// When it was sent.
    pub sent: DateTime<Utc>,
    /// Who sent it, as the service names them; empty when it does not say.
    pub sender: String,
    /// A short plain-text excerpt, as the service gives it (it may be the start of the message rather than the part
    /// that matched). Already cleaned: no markup and no control characters.
    pub preview: String,
    /// The id of the conversation the message belongs to, one of the ids that were searched (in the spelling the caller
    /// gave): the message id above is an id in that conversation. A reply in a channel is in the channel's conversation.
    /// When a search covers one conversation and the service names none for a hit, it is that one.
    pub conversation: String,
    /// The name of the channel the message is in, as the service gives it; empty for a chat, and when the service does
    /// not say. Already cleaned and cut to one short line.
    pub channel: String,
}

/// Most conversations one [`Backend::search_conversations`] call covers; the ones after these are left out.
pub const MAX_SEARCH_CONVERSATIONS: usize = 50;

/// One page of search results.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchPage {
    /// In the order the service gave them: its own ranking, which was observed to be newest first on the answers seen
    /// (but is not promised to be by date).
    pub hits: Vec<SearchHit>,
    /// Where the next page starts, to pass back as `from` to the call that gave this page; `None` when the service
    /// has no more.
    pub next: Option<usize>,
    /// How many matches the service says there are altogether. Approximate.
    pub total: Option<u64>,
}

/// A mention the author put in an outgoing message: the HTML contains `<at id="{id}">{name}</at>`.
#[derive(Debug, Clone)]
pub struct OutMention {
    /// The `id` used in the HTML's `<at>` tag.
    pub id: u32,
    /// Object id of the mentioned user.
    pub user_id: String,
    /// Display name shown in the message.
    pub name: String,
}

/// Enough about a message being replied to for a backend to build its own reply-quote markup, if it has one.
/// Kept generic the same way [`OutMention`] is for mentions: the application builds this from the message being
/// replied to, without knowing or caring what, if anything, a particular service does with it.
#[derive(Debug, Clone)]
pub struct ReplyTarget {
    /// The quoted message's own id ([`Msg::id`]).
    pub id: String,
    /// The quoted message's sender, if it was a person.
    pub from_id: Option<String>,
    /// The quoted message's sender's display name.
    pub from_name: String,
    /// A short, plain-text summary of the quoted message (already stripped of HTML and length-capped by the
    /// caller) — one short line, not the whole message.
    pub preview: String,
}

/// What [`Backend::post_to_channel`] sends: the message, and where in the channel it goes. A name typed in `html` is
/// plain text to the service unless it is one of `mentions`.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ChannelPost<'a> {
    /// The message as HTML, as [`Backend::send`] takes it, with an `<at id="{id}">{name}</at>` for each of `mentions`.
    /// Must not be empty.
    pub html: &'a str,
    /// A title for a new post, shown above it. `None` for no title. Refused together with `thread`: a reply has none.
    pub subject: Option<&'a str>,
    /// The id of the post being replied to (its [`Msg::id`], as a reply's [`Msg::root_id`] names it): `Some` makes
    /// this a reply in that post's thread, `None` a new post in the channel.
    pub thread: Option<&'a str>,
    /// The people mentioned, described as for [`Backend::send`]; a post and a reply carry them the same way. Empty for
    /// none.
    pub mentions: &'a [OutMention],
}

impl<'a> ChannelPost<'a> {
    /// A new post of `html` with no title and no mentions.
    #[must_use]
    pub fn new(html: &'a str) -> Self {
        Self {
            html,
            subject: None,
            thread: None,
            mentions: &[],
        }
    }

    /// The same post mentioning `mentions`.
    #[must_use]
    pub fn with_mentions(mut self, mentions: &'a [OutMention]) -> Self {
        self.mentions = mentions;
        self
    }

    /// The same post with the title `subject`.
    #[must_use]
    pub fn with_subject(mut self, subject: &'a str) -> Self {
        self.subject = Some(subject);
        self
    }

    /// The same message as a reply in the thread of the post `root_id`.
    #[must_use]
    pub fn in_thread(mut self, root_id: &'a str) -> Self {
        self.thread = Some(root_id);
        self
    }
}

/// Someone found in the directory (to start a chat with).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Contact {
    /// Entra object id, the same id the backend uses for chat members and senders.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Primary email address; empty if unknown.
    pub email: String,
    /// Job title / department, when known.
    pub detail: String,
    /// A work phone number, as the directory gives it; empty if unknown.
    pub phone: String,
    /// Where they work (the directory's office location); empty if unknown.
    pub office: String,
}

/// How available a person is, as the service reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Availability {
    /// Online and free.
    Available,
    /// Online, but has not touched the computer for a while (Teams shows this as away).
    AvailableIdle,
    /// Away from the computer.
    Away,
    /// Back soon.
    BeRightBack,
    /// In a meeting or a call, or set to busy.
    Busy,
    /// Busy, and idle.
    BusyIdle,
    /// Does not want to be disturbed.
    DoNotDisturb,
    /// Not signed in anywhere.
    Offline,
    /// The service does not say (or said something this version does not know).
    Unknown,
}

impl Availability {
    /// The availability a service's own word stands for (`"Available"`, `"BeRightBack"`, ...); anything else is
    /// [`Unknown`](Self::Unknown).
    #[must_use]
    pub fn from_label(label: &str) -> Self {
        match label {
            "Available" => Self::Available,
            "AvailableIdle" => Self::AvailableIdle,
            "Away" => Self::Away,
            "BeRightBack" => Self::BeRightBack,
            "Busy" => Self::Busy,
            "BusyIdle" => Self::BusyIdle,
            "DoNotDisturb" => Self::DoNotDisturb,
            "Offline" => Self::Offline,
            _ => Self::Unknown,
        }
    }

    /// The service's own word for it (`"Available"`, `"BeRightBack"`, ...), as [`from_label`](Self::from_label) reads it
    /// and a request that sets it writes it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Available => "Available",
            Self::AvailableIdle => "AvailableIdle",
            Self::Away => "Away",
            Self::BeRightBack => "BeRightBack",
            Self::Busy => "Busy",
            Self::BusyIdle => "BusyIdle",
            Self::DoNotDisturb => "DoNotDisturb",
            Self::Offline => "Offline",
            Self::Unknown => "PresenceUnknown",
        }
    }

    /// Whether a person can choose this for themselves (as opposed to the service working it out, as for an idle
    /// computer): the six of the status menu in the Teams client.
    #[must_use]
    pub fn is_choosable(self) -> bool {
        matches!(
            self,
            Self::Available
                | Self::Busy
                | Self::DoNotDisturb
                | Self::BeRightBack
                | Self::Away
                | Self::Offline
        )
    }

    /// What to call it in a sentence: "Available", "Be right back", "Do not disturb".
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Available => "Available",
            // Signed in but idle: what Teams calls away, the same as one who set it.
            Self::Away | Self::AvailableIdle => "Away",
            Self::BeRightBack => "Be right back",
            Self::Busy | Self::BusyIdle => "Busy",
            Self::DoNotDisturb => "Do not disturb",
            Self::Offline => "Offline",
            Self::Unknown => "Status unknown",
        }
    }
}

/// A short message a person has put up about themselves: a status message, or the note of an out-of-office reply.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StatusNote {
    /// The words, as plain text on one line: tags and entities taken out, controls and private-use characters
    /// removed, whitespace collapsed, at most [`StatusNote::MAX_CHARS`] characters (a cut one ends in `…`). Never empty:
    /// an empty message is no note.
    pub text: String,
    /// When it was put up, if the service said.
    pub published: Option<DateTime<Utc>>,
    /// When it stops being shown, if the service said (an expiry years away is "never" and is `None`).
    pub expires: Option<DateTime<Utc>>,
}

impl StatusNote {
    /// The most characters of a note kept.
    pub const MAX_CHARS: usize = 300;

    /// A note with these words and no times. The text is made plain and one line, as every note's is; `None` if nothing
    /// is left of it.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        let plain = crate::text::html_to_plain(text);
        let mut kept: String = plain
            .chars()
            .filter(|c| !matches!(c, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{10FFFF}'))
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if kept.is_empty() {
            return None;
        }
        if kept.chars().count() > Self::MAX_CHARS {
            kept = kept.chars().take(Self::MAX_CHARS - 1).collect();
            kept = kept.trim_end().to_string();
            kept.push('…');
        }
        Some(Self {
            text: kept,
            published: None,
            expires: None,
        })
    }

    /// The same note, shown until `expires`.
    #[must_use]
    pub fn expiring(mut self, expires: Option<DateTime<Utc>>) -> Self {
        self.expires = expires;
        self
    }

    /// The same note, put up at `published`.
    #[must_use]
    pub fn published(mut self, published: Option<DateTime<Utc>>) -> Self {
        self.published = published;
        self
    }

    /// Whether the note is still to be shown at `now`.
    #[must_use]
    pub fn is_current(&self, now: DateTime<Utc>) -> bool {
        self.expires.is_none_or(|e| e > now)
    }
}

/// Whether someone is around: their availability, whether their calendar says they are out of office, and the notes
/// they have put up.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Presence {
    /// How available they are.
    pub availability: Availability,
    /// Their calendar says they are out of office.
    pub out_of_office: bool,
    /// Their status message, if they have one.
    pub note: Option<StatusNote>,
    /// The message of their out-of-office reply, if they are out of office and wrote one.
    pub out_of_office_note: Option<StatusNote>,
}

impl Presence {
    /// A presence with these parts and no notes.
    #[must_use]
    pub fn new(availability: Availability, out_of_office: bool) -> Self {
        Self {
            availability,
            out_of_office,
            note: None,
            out_of_office_note: None,
        }
    }

    /// The same presence with a status message.
    #[must_use]
    pub fn with_note(mut self, note: StatusNote) -> Self {
        self.note = Some(note);
        self
    }

    /// The same presence with the message of an out-of-office reply.
    #[must_use]
    pub fn with_out_of_office_note(mut self, note: StatusNote) -> Self {
        self.out_of_office_note = Some(note);
        self
    }
}

/// The HTML a service wants for a code block in a message it is sent (see [`Backend::code_block_style`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodeBlockStyle {
    /// `<codeblock class="rust"><code>..</code></codeblock>`, the form Microsoft Graph documents.
    #[default]
    Codeblock,
    /// The structure the Teams web client writes for the card with a language header, line numbers and a copy button:
    /// a marker paragraph and a `<pre class="language-rust">`, tied together by a GUID. The Teams web service stores
    /// that as it is sent but rewrites `<codeblock>` to a bare `<code>` and drops its class, which loses the language.
    EditorCard,
}

/// What a push hint from [`Backend::poll_pushes`] is about. Generic enough for any backend with its own push
/// channel (Teams' Trouter today; nothing here is Teams-specific): it is a hint, not the change itself, so the
/// receiver is expected to poll as usual to actually get it, the same already-tested path used for every update.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushHint {
    /// This chat may have changed; refresh it (or, if it is not open this session, the chat list).
    Chat(String),
    /// Something changed, but which chat is not known from the push alone; refresh broadly. A backend that does its
    /// own slow, best-effort work after a list was returned (reading who is in a group chat so it can be titled) says
    /// so this way when that work has made a new read of the list worth having.
    Unknown,
}

/// A source of chats. Implementations block the calling thread, so call them from a worker thread.
///
/// Free text from the network (names, message bodies, subjects, topics) has been sanitised with [`crate::clean`], so
/// it is safe to print to a terminal. Identifiers, URLs and paging cursors are opaque and returned exactly as the
/// service sent them: pass them back unchanged rather than displaying them.
pub trait Backend: Send + Sync {
    /// A short name for the service, for display.
    fn label(&self) -> String;

    /// The signed-in user.
    ///
    /// # Errors
    /// [`Error`] when the credentials cannot be used or the service cannot be reached.
    fn me(&self) -> Result<Me>;

    /// The user's recent chats, newest first.
    ///
    /// # Errors
    /// [`Error`] when the credentials cannot be used or the service cannot be reached.
    fn chats(&self) -> Result<Vec<Chat>>;

    /// Directory search by name or email address.
    ///
    /// # Errors
    /// [`Error`] when the credentials cannot be used or the service cannot be reached.
    fn find_people(&self, query: &str) -> Result<Vec<Contact>>;

    /// The 1:1 chat with `who`: the existing one, or one that exists once its first message is sent.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `who` has no usable id, otherwise as for the other calls.
    fn open_chat(&self, who: &Contact) -> Result<Chat>;

    /// The newest page of messages when `older` is `None`, otherwise the page the cursor points at.
    ///
    /// # Errors
    /// [`Error::UntrustedUrl`] if `older` was not produced by this backend, otherwise as for the other calls.
    fn messages(&self, chat_id: &str, older: Option<&str>) -> Result<Page>;

    /// Send `html` (with `<at id>` mentions described by `mentions`) to a chat and return the sent message.
    /// `reply_to`, when given, is the message this one replies to; a backend with no reply-quote markup of its own
    /// is free to ignore it and send the same message as if it were not a reply.
    ///
    /// # Errors
    /// [`Error`] when the message was not accepted.
    fn send(
        &self,
        chat_id: &str,
        html: &str,
        mentions: &[OutMention],
        reply_to: Option<&ReplyTarget>,
    ) -> Result<Msg>;

    /// Whether [`Backend::set_favorite`] works on this service.
    fn can_favorite(&self) -> bool {
        false
    }

    /// Add `chat_id` to the signed-in user's favourites (at the end of them) or take it out. Doing either to a chat
    /// that already is, or is not, a favourite changes nothing. The new state shows in [`Chat::favorite`] on the next
    /// [`Backend::chats`].
    ///
    /// The default says favourites are not supported.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `chat_id` is not usable, otherwise [`Error`] for a refused or failed request.
    fn set_favorite(&self, chat_id: &str, on: bool) -> Result<()> {
        let _ = (chat_id, on);
        Err(Error::Unsupported {
            operation: "change favourites",
            detail: "this backend has no favourites",
        })
    }

    /// Who is in `chat_id`, for a chat whose list entry did not say (a meeting or group chat with a name of its own
    /// is listed without its roster). Names may be empty for people not looked up yet; asking again a little later
    /// finds them.
    ///
    /// The default says the roster cannot be read; a service that lists chats with their members never needs it.
    ///
    /// # Errors
    /// [`Error`] for a refused or failed request.
    fn members(&self, chat_id: &str) -> Result<Vec<Member>> {
        let _ = chat_id;
        Err(Error::Unsupported {
            operation: "read chat members",
            detail: "this backend lists chats with their members",
        })
    }

    /// Whether [`Backend::edit`] works on this service.
    fn can_edit(&self) -> bool {
        false
    }

    /// Replace the text of one of your messages with `html` (with `<at id>` mentions described by `mentions`, as for
    /// [`Backend::send`]). Only the sender can edit a message. The message keeps its id; its `edited` time is set.
    ///
    /// The default says editing is not supported.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `message_id` is not usable, otherwise [`Error`] for a refused or failed request.
    fn edit(
        &self,
        chat_id: &str,
        message_id: &str,
        html: &str,
        mentions: &[OutMention],
    ) -> Result<()> {
        let _ = (chat_id, message_id, html, mentions);
        Err(Error::Unsupported {
            operation: "edit message",
            detail: "this backend cannot edit messages",
        })
    }

    /// Whether [`Backend::post_to_channel`] works on this service.
    fn can_post_to_channels(&self) -> bool {
        false
    }

    /// Post to a channel (see [`Backend::teams`]) or reply in the thread of one of its posts, as you, visible to
    /// everyone in the channel. Returns the message as sent: a post has [`Msg::subject`] when one was given and no
    /// [`Msg::root_id`], a reply has [`Msg::root_id`] set to the post it answers. The next read of the channel brings
    /// the service's own copy, which replaces this one by id.
    ///
    /// The same failure rules as [`Backend::send`] hold: [`Error::delivery_unknown`] says the post may have been
    /// delivered (do not send it again without looking), and a refusal means it was not.
    ///
    /// The default says posting is not supported.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `channel`, the thread, the subject, the message or a mention is not usable (nothing is
    /// sent then), [`Error::Unsupported`] when this service cannot post, otherwise [`Error`] for a refused or failed
    /// request.
    fn post_to_channel(&self, channel: &str, post: &ChannelPost<'_>) -> Result<Msg> {
        let _ = (channel, post);
        Err(Error::Unsupported {
            operation: "post to channel",
            detail: "this backend cannot post to channels",
        })
    }

    /// Whether [`Backend::edit_post`] works on this service.
    fn can_edit_posts(&self) -> bool {
        false
    }

    /// Replace the text of one of your own posts in a channel with `html`, keeping its title: `subject` is the title
    /// the post has now, restated because the service drops a title an edit does not repeat (`None` for a post that
    /// has none). `mentions` describe the `<at id>` tags in `html`, as for [`Backend::edit`]. Editing a reply in a
    /// thread is not covered.
    ///
    /// The default says editing posts is not supported.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `channel`, `message_id`, `subject`, `html` or a mention is not usable (nothing is sent
    /// then), otherwise [`Error`] for a refused or failed request.
    fn edit_post(
        &self,
        channel: &str,
        message_id: &str,
        subject: Option<&str>,
        html: &str,
        mentions: &[OutMention],
    ) -> Result<()> {
        let _ = (channel, message_id, subject, html, mentions);
        Err(Error::Unsupported {
            operation: "edit post",
            detail: "this backend cannot edit posts in channels",
        })
    }

    /// Add (`on`) or remove your reaction `emoji` on a message. The six classic reactions (👍 ❤️ 😆 😮 🙁 😠) work on
    /// every service; which others do depends on it ([`Backend::reaction_emoji`] lists them). An emoji the service has
    /// no reaction for is refused rather than sent, because a reaction the service does not know shows as a broken
    /// image to everyone in the chat.
    ///
    /// The default says reactions are not supported.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `emoji` or `message_id` is not usable or the service has no such reaction, otherwise
    /// [`Error`] for a refused or failed request.
    fn react(&self, chat_id: &str, message_id: &str, emoji: &str, on: bool) -> Result<()> {
        let _ = (chat_id, message_id, emoji, on);
        Err(Error::Unsupported {
            operation: "react to message",
            detail: "this backend cannot react to messages",
        })
    }

    /// Whether [`Backend::teams`] works on this service.
    fn can_list_teams(&self) -> bool {
        false
    }

    /// The teams the person is in, with their channels. The answer is large (megabytes for a person in many teams) and
    /// slow, so ask rarely; use [`Backend::channel_activity`] for what changed since. A channel is opened with
    /// [`Backend::messages`] and its id, like a chat.
    ///
    /// The teams come in the person's own order where the service keeps one (the ones it names first, in its order),
    /// then the others by name; where it keeps none, or it could not be read, in the service's order.
    ///
    /// The default says teams are not supported.
    ///
    /// # Errors
    /// [`Error::Unsupported`] when this service or account cannot list teams, otherwise [`Error`] for a refused or
    /// failed request.
    fn teams(&self) -> Result<Vec<Team>> {
        Err(Error::Unsupported {
            operation: "list teams",
            detail: "this backend has no teams",
        })
    }

    /// The newest known state of every channel the conversation list has mentioned so far, each time it was read by
    /// [`Backend::chats`]: this costs no request. Empty by default, and until a list has been read. The application
    /// merges it over what [`Backend::teams`] said.
    fn channel_activity(&self) -> Vec<ChannelActivity> {
        Vec::new()
    }

    /// Whether [`Backend::search_messages`] works on this service.
    fn can_search_messages(&self) -> bool {
        false
    }

    /// Search the messages of one chat on the service, so that the caller does not have to fetch and scan them all.
    /// `query` is what the person typed, as plain words: the implementation turns it into a phrase search of that one
    /// chat and nothing else, whatever characters it holds (it cannot widen the search to other chats or add
    /// operators). `from` is where in the service's results to start: 0 for the first page, then
    /// [`SearchPage::next`]. Results are in the service's order, not by date, and a message sent a moment ago may not
    /// be found yet.
    ///
    /// The default says message search is not supported.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `chat_id` or `query` is not usable, [`Error::Unsupported`] when this service or
    /// account cannot search, otherwise [`Error`] for a refused or failed request.
    fn search_messages(&self, chat_id: &str, query: &str, from: usize) -> Result<SearchPage> {
        let _ = (chat_id, query, from);
        Err(Error::Unsupported {
            operation: "search messages",
            detail: "this backend has no message search",
        })
    }

    /// Whether [`Backend::search_conversations`] works on this service.
    fn can_search_conversations(&self) -> bool {
        false
    }

    /// Search the messages of several conversations at once (the channels of one team, say) for `query`, a page of
    /// results starting `from` results in. The same rules as [`Backend::search_messages`] hold for `query`, and for each
    /// id: the search covers exactly the given conversations and nothing else, whatever characters any of them holds.
    /// Duplicate ids (ASCII case aside) are searched once, and only the first [`MAX_SEARCH_CONVERSATIONS`] of what is
    /// left are covered, so put the ones that matter first. Each hit says which conversation it is in
    /// ([`SearchHit::conversation`]); one the service places elsewhere is left out.
    ///
    /// The default says message search is not supported.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if no id is given, or any id or `query` is not usable (nothing is sent then),
    /// [`Error::Unsupported`] when this service or account cannot search, otherwise [`Error`] for a refused or failed
    /// request.
    fn search_conversations(
        &self,
        conversation_ids: &[String],
        query: &str,
        from: usize,
    ) -> Result<SearchPage> {
        let _ = (conversation_ids, query, from);
        Err(Error::Unsupported {
            operation: "search messages",
            detail: "this backend has no message search",
        })
    }

    /// Every emoji [`Backend::react`] accepts, most common first. The first call may download the service's emoji
    /// list; the result is kept. The default says there is no such list (only the six classic reactions are known).
    ///
    /// # Errors
    /// [`Error`] if the list cannot be read.
    fn reaction_emoji(&self) -> Result<Vec<EmojiInfo>> {
        Err(Error::Unsupported {
            operation: "list reaction emoji",
            detail: "this backend has no list of emoji",
        })
    }

    /// Whether [`Backend::sign_in_again`] can restore a sign-in that has expired.
    fn can_sign_in_again(&self) -> bool {
        false
    }

    /// Sign in again, in a browser window, and wait for it to finish (up to thirty minutes), for a sign-in that has
    /// expired and could not be renewed silently. Calls made meanwhile fail at once, and work normally afterwards.
    /// The same person must sign in.
    ///
    /// The default says this is not supported; [`Backend::can_sign_in_again`] says whether it is.
    ///
    /// # Errors
    /// [`Error`] if no browser can be used, the wrong account signs in, or the sign-in is not finished in time.
    fn sign_in_again(&self) -> Result<()> {
        Err(Error::Unsupported {
            operation: "sign in again",
            detail: "this backend cannot sign in again while running",
        })
    }

    /// Whether [`Backend::presence`] does anything on this service.
    fn can_presence(&self) -> bool {
        false
    }

    /// Whether the people with these object ids are around, keyed by id. A person the service says nothing about is
    /// left out, and so is anyone whose id is not an object id (it is never put into a request).
    ///
    /// The default says presence cannot be read; [`Backend::can_presence`] says whether it can.
    ///
    /// # Errors
    /// [`Error`] for a refused or failed request, including a tenant that does not grant the presence service.
    fn presence(&self, ids: &[String]) -> Result<std::collections::HashMap<String, Presence>> {
        let _ = ids;
        Err(Error::Unsupported {
            operation: "read presence",
            detail: "this backend cannot read who is available",
        })
    }

    /// Whether [`Backend::set_status`] and [`Backend::report_activity`] do anything on this service.
    fn can_set_status(&self) -> bool {
        false
    }

    /// Choose your own availability, which the service shows everyone and keeps on all your devices (`Some`: busy, do
    /// not disturb, ...), or give the choice up so that it is worked out again from whether you are around (`None`).
    ///
    /// The default says this cannot be done; [`Backend::can_set_status`] says whether it can.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] for an availability a person cannot choose (see [`Availability::is_choosable`]),
    /// otherwise [`Error`] for a refused or failed request.
    fn set_status(&self, status: Option<Availability>) -> Result<()> {
        let _ = status;
        Err(Error::Unsupported {
            operation: "set status",
            detail: "this backend cannot set your availability",
        })
    }

    /// Tell the service whether you are at the keyboard (`active`), so that it shows you as available and, left alone,
    /// as away: what a Teams client does, and without it a program is not one of the places you are online. The first
    /// call registers this program with the service. Call it when the state changes and now and then while active.
    ///
    /// The default says this cannot be done; [`Backend::can_set_status`] says whether it can.
    ///
    /// # Errors
    /// [`Error`] for a refused or failed request.
    fn report_activity(&self, active: bool) -> Result<()> {
        let _ = active;
        Err(Error::Unsupported {
            operation: "report activity",
            detail: "this backend cannot report whether you are around",
        })
    }

    /// Whether [`Backend::mark_read`] does anything on this service.
    fn marks_read(&self) -> bool {
        false
    }

    /// How a code block should be written in the HTML given to [`Backend::send`].
    fn code_block_style(&self) -> CodeBlockStyle {
        CodeBlockStyle::default()
    }

    /// Tell the service that you have read `chat_id` up to the message `message_id`, which arrived at `arrived`, so
    /// that the chat is not unread in your other apps either. Call it once for the newest message a person has seen.
    ///
    /// The default says reading cannot be recorded; [`Backend::marks_read`] says whether it can.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `message_id` is not usable, otherwise [`Error`] for a refused or failed request.
    fn mark_read(&self, chat_id: &str, message_id: &str, arrived: DateTime<Utc>) -> Result<()> {
        let _ = (chat_id, message_id, arrived);
        Err(Error::Unsupported {
            operation: "mark chat read",
            detail: "this backend cannot mark chats read",
        })
    }

    /// Whether [`Backend::image`] can fetch `url`: it is content this service hosts. Credentials are sent only to such
    /// URLs, so a caller deciding what is worth downloading asks here instead of trying and failing.
    fn serves_image(&self, url: &str) -> bool;

    /// Download an image that appears in a message, kept whole under 1.7 million pixels and 2400 px on a side, else
    /// scaled down to fit both by area.
    ///
    /// # Errors
    /// [`Error::UntrustedUrl`] if [`Backend::serves_image`] is false for `url` (credentials are never sent
    /// elsewhere), [`Error::UnverifiedContent`] if the image is from outside the signed-in user's organization
    /// (a SharePoint/OneDrive attachment from another tenant, or whose tenant cannot be verified), otherwise
    /// [`Error`] for a failed download or an undecodable image.
    fn image(&self, url: &str) -> Result<RgbImage>;

    /// Download an image from outside the signed-in user's organization, after the user explicitly chose to load it.
    /// Bypasses the tenant check that [`Backend::image`] applies to SharePoint/OneDrive attachments, but still
    /// applies host checks ([`Backend::serves_image`]) and credential rules for non-SharePoint URLs. Only call this
    /// for URLs the user explicitly requested; scrolling and viewport observation must never call it.
    ///
    /// The default says explicit loading is not supported.
    ///
    /// # Errors
    /// [`Error::Unsupported`] by default, otherwise as for [`Backend::image`] except that
    /// [`Error::UnverifiedContent`] is not returned (the whole point is to load content that would otherwise be
    /// refused).
    fn image_from_outside(&self, url: &str) -> Result<RgbImage> {
        let _ = url;
        Err(Error::Unsupported {
            operation: "load unverified image",
            detail: "this backend does not support explicit loading of outside content",
        })
    }

    /// Problems that did not fail an operation but that the user should hear about, such as new credentials that
    /// could not be saved. Returns and clears what has accumulated since the last call.
    fn take_warnings(&self) -> Vec<Error> {
        Vec::new()
    }

    /// Hints that something may have changed, from this backend's own push channel, if it has one. Returns and
    /// clears what has accumulated since the last call. The default says there is no push channel: the poll-based
    /// cadence alone is authoritative, and callers should treat an empty result the same way either way.
    fn poll_pushes(&self) -> Vec<PushHint> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    const ME: &str = "me";

    #[test]
    fn a_note_is_plain_one_line_short_and_never_empty() {
        let note =
            StatusNote::new("  <p>Back <b>soon</b></p>\n\t&amp; away\u{1b}[0m \u{E001}").unwrap();
        assert_eq!(note.text, "Back soon & away[0m");
        assert_eq!((note.published, note.expires), (None, None));
        for nothing in ["", "   ", "<br>", "<p></p>&nbsp;", "\u{1b}\u{E001}\u{202E}"] {
            assert_eq!(StatusNote::new(nothing), None, "{nothing:?}");
        }
        let long = StatusNote::new(&"abc ".repeat(1_000)).unwrap();
        assert_eq!(long.text.chars().count(), StatusNote::MAX_CHARS);
        assert!(long.text.ends_with('…'));
        let exact = "x".repeat(StatusNote::MAX_CHARS);
        assert_eq!(
            StatusNote::new(&exact).unwrap().text,
            exact,
            "at the limit is not cut"
        );
    }

    #[test]
    fn a_note_with_no_end_is_always_current_and_one_with_an_end_stops_at_it() {
        let end = Utc.with_ymd_and_hms(2026, 11, 21, 12, 0, 0).unwrap();
        let open = StatusNote::new("x").unwrap();
        assert!(open.is_current(end + Duration::days(9_000)));
        let timed = open.expiring(Some(end));
        assert!(timed.is_current(end - Duration::seconds(1)));
        assert!(!timed.is_current(end), "the moment it ends it is over");
        assert!(!timed.is_current(end + Duration::seconds(1)));
    }

    fn chat(
        kind: ChatKind,
        from: &str,
        automated: bool,
        read_before_message_by: Option<Duration>,
    ) -> Chat {
        let sent = Utc.with_ymd_and_hms(2026, 10, 27, 12, 0, 0).unwrap();
        Chat {
            id: "19:x@thread.v2".into(),
            kind,
            topic: None,
            members: vec![],
            updated: sent,
            preview: Some(Preview {
                created: sent,
                from_id: Some(from.into()),
                from_name: Some("Someone".into()),
                text: "hi".into(),
                automated,
            }),
            last_read: read_before_message_by.map(|d| sent - d),
            web_url: None,
            favorite: None,
        }
    }

    #[test]
    fn a_title_says_in_its_parts_which_names_are_of_people_no_longer_in_the_directory() {
        let mut c = chat(ChatKind::Group, "ada", false, None);
        c.members = vec![member(ME, "Me"), member("a", "Ada"), member("g", "Grace")];
        c.members[2].not_in_directory = true;
        assert_eq!(
            c.title_parts(ME),
            [
                ("Ada".to_string(), false),
                (", ".to_string(), false),
                ("Grace".to_string(), true)
            ]
        );
        assert_eq!(c.title(ME), "Ada, Grace", "joined, the parts are the title");
        c.topic = Some("Planning".into());
        assert_eq!(c.title_parts(ME), [("Planning".to_string(), false)]);
    }

    fn member(id: &str, name: &str) -> Member {
        Member {
            not_in_directory: false,
            id: id.into(),
            name: name.into(),
        }
    }

    fn titled(members: Vec<Member>) -> String {
        let mut c = chat(ChatKind::Group, "ada", false, None);
        c.members = members;
        c.title(ME)
    }

    #[test]
    fn a_title_never_claims_you_are_alone_when_the_people_are_merely_unknown() {
        // Only you: that is what "(just you)" means.
        assert_eq!(titled(vec![member(ME, "Me")]), "(just you)");
        // No roster loaded, or a roster whose names could not be looked up: unknown, not empty.
        assert_eq!(titled(vec![]), "(untitled chat)");
        assert_eq!(
            titled(vec![member(ME, "Me"), member("ada", "")]),
            "(untitled chat)"
        );
    }

    #[test]
    fn a_title_counts_the_people_whose_names_are_not_known() {
        let named = |n: u32| member(&format!("p{n}"), &format!("P{n}"));
        let nameless = |n: u32| member(&format!("q{n}"), "");
        assert_eq!(titled(vec![named(1), named(2), named(3)]), "P1, P2, P3");
        assert_eq!(
            titled(vec![named(1), named(2), named(3), named(4)]),
            "P1, P2 +2"
        );
        assert_eq!(
            titled(vec![named(1), nameless(1), nameless(2)]),
            "P1 +2",
            "the two without names still count"
        );
        assert_eq!(
            titled(vec![named(1), named(2), named(3), nameless(1)]),
            "P1, P2 +2"
        );
    }

    #[test]
    fn a_message_from_someone_else_after_the_read_marker_is_unread() {
        let behind = Some(Duration::minutes(5));
        assert!(chat(ChatKind::OneOnOne, "ada", false, behind).is_unread(ME));
        assert!(!chat(ChatKind::OneOnOne, "ada", false, Some(Duration::zero())).is_unread(ME));
        assert!(
            !chat(
                ChatKind::OneOnOne,
                "ada",
                false,
                Some(-Duration::minutes(5))
            )
            .is_unread(ME)
        );
        assert!(
            !chat(ChatKind::OneOnOne, ME, false, behind).is_unread(ME),
            "your own message"
        );
    }

    #[test]
    fn a_notice_posted_by_a_service_never_makes_a_chat_unread() {
        let behind = Some(Duration::days(90));
        assert!(!chat(ChatKind::Meeting, "organizer", true, behind).is_unread(ME));
        assert!(!chat(ChatKind::Group, "organizer", true, None).is_unread(ME));
    }

    #[test]
    fn a_chat_never_opened_is_unread_unless_it_is_a_meeting_chat() {
        assert!(
            chat(ChatKind::OneOnOne, "ada", false, None).is_unread(ME),
            "a new conversation"
        );
        assert!(chat(ChatKind::Group, "ada", false, None).is_unread(ME));
        assert!(
            !chat(ChatKind::Meeting, "ada", false, None).is_unread(ME),
            "being added to a meeting chat is not news"
        );
        assert!(
            chat(ChatKind::Meeting, "ada", false, Some(Duration::minutes(5))).is_unread(ME),
            "but a meeting chat you have opened is judged by its marker like any other"
        );
    }

    #[test]
    fn a_chat_without_a_newest_message_is_not_unread() {
        let mut c = chat(ChatKind::OneOnOne, "ada", false, None);
        c.preview = None;
        assert!(!c.is_unread(ME));
    }
}
