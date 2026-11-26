//! The start-up cache: what the left pane needs to draw before the network has answered.
//!
//! A snapshot of the chat list and the teams tab from the last session, so the lists are on screen at once and the real
//! refresh replaces them a moment later. It is **minimal and sealed**: who is in which chat and what the chats and
//! teams are called is sensitive, so the file is encrypted (see `vault`) and holds only what the lists draw. No message
//! text, no preview text, no subject, no attachment name, no link: the newest message of a chat or channel keeps its
//! time, who sent it (an id) and whether a service posted it, which is what "unread" is worked out from, and nothing it
//! said.
//!
//! The structs here are the file's own (the library's model types carry no serde derives); they are mapped to and from
//! `teams_client`'s types. Nothing here sends anything, and a snapshot that cannot be used is never an error for the
//! person: it is a log line and the normal first load.

use crate::vault::{Aad, KeyError, KeyProvider, OpenError, open, seal};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use teams_client::{
    Channel, ChannelKind, Chat, ChatKind, CredentialStore, FileStore, Member, Preview, Team, clean,
};

/// The version of the file's contents. A file of another version is ignored.
pub const VERSION: u8 = 1;
/// A snapshot older than this is not shown: what it says about who is online or unread is too far gone to be a help.
pub fn max_age() -> Duration {
    Duration::days(7)
}
/// What the file may hold. The readers of the lists cap what they accept (a Teams list is at most 500 chats, with
/// favourites on top); these are the same bounds with room, and a file beyond them is refused before it is parsed.
pub const MAX_CHATS: usize = 600;
pub const MAX_MEMBERS: usize = 500;
pub const MAX_TEAMS: usize = 600;
pub const MAX_CHANNELS_PER_TEAM: usize = 1_000;
/// All channels of all teams together.
pub const MAX_CHANNELS: usize = 20_000;
/// The largest file written or read, sealed or not.
pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CachedChatKind {
    OneOnOne,
    Group,
    Meeting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CachedChannelKind {
    Standard,
    Private,
    Shared,
}

/// The newest message of a chat or channel as far as the lists need it: when, from whom (an id), and whether a service
/// posted it. Never what it said, nor the sender's name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedLast {
    /// Unix milliseconds.
    at: i64,
    from: Option<String>,
    automated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedMember {
    id: String,
    name: String,
    /// `Member::not_in_directory`; absent in a cache written before it was kept.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    not_in_directory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedChat {
    id: String,
    kind: CachedChatKind,
    topic: Option<String>,
    members: Vec<CachedMember>,
    /// Unix milliseconds.
    updated: i64,
    last_read: Option<i64>,
    favorite: Option<u32>,
    last: Option<CachedLast>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedChannel {
    id: String,
    name: String,
    general: bool,
    kind: CachedChannelKind,
    archived: bool,
    shown: bool,
    pinned: bool,
    muted: bool,
    followed: bool,
    favorite: Option<u32>,
    unread: bool,
    last: Option<CachedLast>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedTeam {
    id: String,
    name: String,
    archived: bool,
    muted: bool,
    members: Option<u32>,
    channels: Vec<CachedChannel>,
}

/// What is written. `chats_at` and `teams_at` say when each part was last read from the service (not when the file was
/// written), so a part nobody refreshes, such as a teams tab never opened, does not stay young by being copied forward.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    version: u8,
    /// The signed-in user's object id, which the file is also sealed to.
    account: String,
    /// When the file was written, unix milliseconds.
    saved_at: i64,
    chats: Vec<CachedChat>,
    chats_at: Option<i64>,
    teams: Vec<CachedTeam>,
    teams_at: Option<i64>,
}

fn ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

fn last_of(p: &Preview) -> CachedLast {
    CachedLast {
        at: ms(p.created),
        from: p.from_id.clone(),
        automated: p.automated,
    }
}

impl Snapshot {
    /// What the app holds now. A part that has not been read yet (`None`) is left out. Anything beyond the bounds is
    /// cut off, newest chats first.
    pub fn capture(
        account: &str,
        now: DateTime<Utc>,
        chats: Option<(&[Chat], DateTime<Utc>)>,
        teams: Option<(&[Team], DateTime<Utc>)>,
    ) -> Self {
        let mut channels_left = MAX_CHANNELS;
        Self {
            version: VERSION,
            account: account.to_string(),
            saved_at: ms(now),
            chats: chats.map_or_else(Vec::new, |(chats, _)| {
                chats.iter().take(MAX_CHATS).map(cached_chat).collect()
            }),
            chats_at: chats.map(|(_, at)| ms(at)),
            teams: teams.map_or_else(Vec::new, |(teams, _)| {
                teams
                    .iter()
                    .take(MAX_TEAMS)
                    .map(|t| {
                        let take = t
                            .channels
                            .len()
                            .min(MAX_CHANNELS_PER_TEAM)
                            .min(channels_left);
                        channels_left -= take;
                        cached_team(t, take)
                    })
                    .collect()
            }),
            teams_at: teams.map(|(_, at)| ms(at)),
        }
    }

    /// Whether there is anything in it worth writing.
    pub fn is_empty(&self) -> bool {
        self.chats_at.is_none() && self.teams_at.is_none()
    }
}

fn cached_chat(c: &Chat) -> CachedChat {
    CachedChat {
        id: c.id.clone(),
        kind: match c.kind {
            ChatKind::OneOnOne => CachedChatKind::OneOnOne,
            ChatKind::Meeting => CachedChatKind::Meeting,
            // A group, and any kind a later library starts reporting (drawn as a group, as the list does).
            _ => CachedChatKind::Group,
        },
        topic: c.topic.clone(),
        members: c
            .members
            .iter()
            .take(MAX_MEMBERS)
            .map(|m| CachedMember {
                id: m.id.clone(),
                name: m.name.clone(),
                not_in_directory: m.not_in_directory,
            })
            .collect(),
        updated: ms(c.updated),
        last_read: c.last_read.map(ms),
        favorite: c.favorite,
        last: c.preview.as_ref().map(last_of),
    }
}

fn cached_team(t: &Team, channels: usize) -> CachedTeam {
    CachedTeam {
        id: t.id.clone(),
        name: t.name.clone(),
        archived: t.archived,
        muted: t.muted,
        members: t.members,
        channels: t
            .channels
            .iter()
            .take(channels)
            .map(|c| CachedChannel {
                id: c.id.clone(),
                name: c.name.clone(),
                general: c.general,
                kind: match c.kind {
                    ChannelKind::Private => CachedChannelKind::Private,
                    ChannelKind::Shared => CachedChannelKind::Shared,
                    _ => CachedChannelKind::Standard,
                },
                archived: c.archived,
                shown: c.shown,
                pinned: c.pinned,
                muted: c.muted,
                followed: c.followed,
                favorite: c.favorite,
                unread: c.unread,
                last: c.last.as_ref().map(last_of),
            })
            .collect(),
    }
}

/// What a snapshot gives back: the lists as the app holds them, each with when it was read from the service. A part that
/// was too old, or never written, is empty with no time.
#[derive(Debug)]
pub struct Restored {
    pub account: String,
    pub chats: Vec<Chat>,
    pub chats_at: Option<DateTime<Utc>>,
    pub teams: Vec<Team>,
    pub teams_at: Option<DateTime<Utc>>,
}

fn time(ms: i64) -> Result<DateTime<Utc>, Ignored> {
    DateTime::from_timestamp_millis(ms).ok_or(Ignored::Unparsable)
}

fn restored_last(l: &CachedLast) -> Result<Preview, Ignored> {
    Ok(Preview {
        created: time(l.at)?,
        from_id: l.from.clone(),
        from_name: None,
        // Not kept: the list draws no text until the real one arrives.
        text: String::new(),
        automated: l.automated,
    })
}

impl Snapshot {
    /// Check a decrypted snapshot and turn it into the app's types. Text that comes back is cleaned again, as it is
    /// whenever it enters from outside the program.
    fn restore(self, account: &str, now: DateTime<Utc>) -> Result<Restored, Ignored> {
        if self.version != VERSION {
            return Err(Ignored::Version(self.version));
        }
        if self.account != account {
            return Err(Ignored::OtherAccount);
        }
        let saved = time(self.saved_at)?;
        if saved > now + Duration::days(1) {
            // Written by a clock that was far ahead: not trusted to say how old it is.
            return Err(Ignored::Unparsable);
        }
        if self.chats.len() > MAX_CHATS
            || self.teams.len() > MAX_TEAMS
            || self.chats.iter().any(|c| c.members.len() > MAX_MEMBERS)
            || self
                .teams
                .iter()
                .any(|t| t.channels.len() > MAX_CHANNELS_PER_TEAM)
            || self.teams.iter().map(|t| t.channels.len()).sum::<usize>() > MAX_CHANNELS
        {
            return Err(Ignored::TooMany);
        }
        let fresh = |at: Option<i64>| -> Result<Option<DateTime<Utc>>, Ignored> {
            match at {
                Some(at) => {
                    let at = time(at)?;
                    Ok((now - at <= max_age()).then_some(at))
                }
                None => Ok(None),
            }
        };
        let chats_at = fresh(self.chats_at)?;
        let teams_at = fresh(self.teams_at)?;
        if chats_at.is_none() && teams_at.is_none() {
            return Err(Ignored::Stale);
        }
        let mut chats = Vec::new();
        if chats_at.is_some() {
            for c in &self.chats {
                chats.push(Chat {
                    id: c.id.clone(),
                    kind: match c.kind {
                        CachedChatKind::OneOnOne => ChatKind::OneOnOne,
                        CachedChatKind::Group => ChatKind::Group,
                        CachedChatKind::Meeting => ChatKind::Meeting,
                    },
                    topic: c.topic.as_deref().map(clean),
                    members: c
                        .members
                        .iter()
                        .map(|m| Member {
                            not_in_directory: m.not_in_directory,
                            id: m.id.clone(),
                            name: clean(&m.name),
                        })
                        .collect(),
                    updated: time(c.updated)?,
                    preview: c.last.as_ref().map(restored_last).transpose()?,
                    last_read: c.last_read.map(time).transpose()?,
                    web_url: None,
                    favorite: c.favorite,
                });
            }
        }
        let mut teams = Vec::new();
        if teams_at.is_some() {
            for t in &self.teams {
                let mut channels = Vec::new();
                for c in &t.channels {
                    channels.push(Channel {
                        id: c.id.clone(),
                        name: clean(&c.name),
                        general: c.general,
                        kind: match c.kind {
                            CachedChannelKind::Standard => ChannelKind::Standard,
                            CachedChannelKind::Private => ChannelKind::Private,
                            CachedChannelKind::Shared => ChannelKind::Shared,
                        },
                        archived: c.archived,
                        shown: c.shown,
                        pinned: c.pinned,
                        muted: c.muted,
                        followed: c.followed,
                        favorite: c.favorite,
                        unread: c.unread,
                        last: c.last.as_ref().map(restored_last).transpose()?,
                    });
                }
                teams.push(Team {
                    id: t.id.clone(),
                    name: clean(&t.name),
                    archived: t.archived,
                    muted: t.muted,
                    members: t.members,
                    channels,
                });
            }
        }
        Ok(Restored {
            account: self.account,
            chats,
            chats_at,
            teams,
            teams_at,
        })
    }
}

/// Why a snapshot was not used. None of these is an error for the person; each is a line in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ignored {
    /// There is no file.
    Missing,
    /// The file is larger than any this program writes.
    Oversize,
    /// There is a file but no key for it in the credential store.
    NoKey,
    /// The credential store could not be asked (it stays as it is: the key may be there when it is unlocked).
    KeyStore(KeyError),
    /// The file is unreadable, of another version, for another account, or was changed.
    Unopenable(OpenError),
    /// What was inside is not a snapshot.
    Unparsable,
    /// It is a snapshot of another version.
    Version(u8),
    /// It is for another account.
    OtherAccount,
    /// It holds more than a snapshot may.
    TooMany,
    /// Everything in it is older than a week.
    Stale,
    /// A transient I/O error (permission denied, file locked, etc).
    Transient(String),
}

impl std::fmt::Display for Ignored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("no file"),
            Self::Oversize => f.write_str("the file is larger than any this program writes"),
            Self::NoKey => f.write_str("the key for it is not in the credential store"),
            Self::KeyStore(e) => write!(f, "{e}"),
            Self::Unopenable(e) => write!(f, "{e}"),
            Self::Unparsable => f.write_str("what is in it is not a snapshot"),
            Self::Version(v) => write!(f, "it is version {v}"),
            Self::OtherAccount => f.write_str("it is for another account"),
            Self::TooMany => f.write_str("it holds more than a snapshot may"),
            Self::Stale => f.write_str("it is older than a week"),
            Self::Transient(e) => write!(f, "read failed for the moment: {e}"),
        }
    }
}

/// The encrypted file for one service and account, in the cache directory.
pub struct Cache {
    keys: Arc<dyn KeyProvider>,
    file: FileStore,
    /// `teams` or `graph`, with the account: what the file is sealed to.
    identity: String,
    account: String,
    written: AtomicUsize,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("path", &self.file.path())
            .finish_non_exhaustive()
    }
}

impl Cache {
    /// The cache of `account` on `service` (`teams` or `graph`), in `dir`.
    pub fn new(
        keys: Arc<dyn KeyProvider>,
        dir: &Path,
        service: &str,
        account: impl Into<String>,
    ) -> Self {
        let account = account.into();
        Self {
            keys,
            file: FileStore::new(dir.join(format!("snapshot-{service}.cache"))),
            identity: format!("{service}:{account}"),
            account,
            written: AtomicUsize::new(0),
        }
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    #[cfg(test)]
    pub fn path(&self) -> PathBuf {
        self.file.path().to_path_buf()
    }

    /// How many times the file has been written by this handle.
    #[cfg(test)]
    pub fn writes(&self) -> usize {
        self.written.load(Ordering::SeqCst)
    }

    fn aad(&self) -> Aad<'_> {
        Aad {
            purpose: "cache",
            version: VERSION,
            identity: &self.identity,
        }
    }

    /// Read the snapshot, if there is a usable one. A file that can never be used (changed, of another version or
    /// account, too old, too big, without a key) is removed so it is not looked at again; one that merely cannot be read
    /// now (the credential store did not answer) is left. Logs why, in words only: no id or name.
    pub fn read(&self, now: DateTime<Utc>) -> Result<Restored, Ignored> {
        let result = self.try_read(now);
        match &result {
            Ok(r) => log::info!(
                "start-up cache: {} chat(s) and {} team(s) from the last session",
                r.chats.len(),
                r.teams.len()
            ),
            Err(Ignored::Missing) => log::debug!("start-up cache: none yet"),
            Err(why) => log::info!("start-up cache ignored: {why}; the normal first load runs"),
        }
        if let Err(
            Ignored::Oversize
            | Ignored::NoKey
            | Ignored::Unopenable(_)
            | Ignored::Unparsable
            | Ignored::Version(_)
            | Ignored::OtherAccount
            | Ignored::TooMany
            | Ignored::Stale,
        ) = &result
        {
            let _ = self.file.clear();
        }
        result
    }

    fn try_read(&self, now: DateTime<Utc>) -> Result<Restored, Ignored> {
        let path = self.file.path();
        // The size is looked at before the file is read, so a huge file is never pulled into memory.
        match std::fs::metadata(path) {
            Ok(m) if m.len() > MAX_FILE_BYTES as u64 + 64 => return Err(Ignored::Oversize),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Ignored::Missing),
            Err(e) => return Err(Ignored::Transient(e.to_string())),
        }
        let sealed = match self.file.load() {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Err(Ignored::Missing),
            Err(e) => return Err(Ignored::Transient(e.to_string())),
        };
        let key = match self.keys.get() {
            Ok(Some(key)) => key,
            Ok(None) => return Err(Ignored::NoKey),
            Err(e) => return Err(Ignored::KeyStore(e)),
        };
        let plain =
            open(&key, &self.aad(), &sealed, MAX_FILE_BYTES).map_err(Ignored::Unopenable)?;
        let snapshot: Snapshot = serde_json::from_slice(&plain).map_err(|_| Ignored::Unparsable)?;
        snapshot.restore(&self.account, now)
    }

    /// Write the snapshot: serialised, sealed with a fresh nonce, and put in place by writing a temporary file and
    /// renaming it, so a crash leaves the old file or the new one. Returns the file's size.
    pub fn write(&self, snapshot: &Snapshot) -> Result<usize, String> {
        if snapshot.is_empty() {
            return Err("nothing to write".into());
        }
        let plain = zeroize::Zeroizing::new(
            serde_json::to_vec(snapshot).map_err(|e| format!("cannot encode it: {e}"))?,
        );
        if plain.len() > MAX_FILE_BYTES {
            return Err(format!(
                "it would be {} bytes, more than the {MAX_FILE_BYTES} allowed",
                plain.len()
            ));
        }
        let key = self.keys.get_or_create().map_err(|e| e.to_string())?;
        let sealed = seal(&key, &self.aad(), &plain).map_err(|e| e.to_string())?;
        self.file.save(&sealed).map_err(|e| e.to_string())?;
        self.written.fetch_add(1, Ordering::SeqCst);
        Ok(sealed.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir;
    use crate::vault::MemoryKeys;
    use std::collections::HashSet;

    const PLANTED_PREVIEW: &str = "Quarterly layoffs are on Friday";
    const PLANTED_NAME: &str = "Zaphod Beeblebrox";
    const PLANTED_TOPIC: &str = "Project Heartbreak";
    const PLANTED_SENDER_NAME: &str = "Trillian Astra";
    const PLANTED_URL: &str = "https://teams.example/l/chat/secret-link";

    fn at(days_ago: i64) -> DateTime<Utc> {
        Utc::now() - Duration::days(days_ago)
    }

    fn chat(id: &str, topic: Option<&str>, unread: bool) -> Chat {
        let sent = at(0) - Duration::minutes(5);
        Chat {
            id: id.into(),
            kind: ChatKind::Group,
            topic: topic.map(Into::into),
            members: vec![
                Member {
                    not_in_directory: false,
                    id: "me".into(),
                    name: "Me Myself".into(),
                },
                Member {
                    not_in_directory: true,
                    id: "zaphod-id".into(),
                    name: PLANTED_NAME.into(),
                },
            ],
            updated: sent,
            preview: Some(Preview {
                created: sent,
                from_id: Some("zaphod-id".into()),
                from_name: Some(PLANTED_SENDER_NAME.into()),
                text: PLANTED_PREVIEW.into(),
                automated: false,
            }),
            last_read: Some(if unread {
                sent - Duration::hours(1)
            } else {
                sent
            }),
            web_url: Some(PLANTED_URL.into()),
            favorite: Some(2),
        }
    }

    fn team() -> Team {
        let last = Preview {
            created: at(0),
            from_id: Some("zaphod-id".into()),
            from_name: Some(PLANTED_SENDER_NAME.into()),
            text: PLANTED_PREVIEW.into(),
            automated: false,
        };
        Team {
            id: "team-1".into(),
            name: "Hitchhikers".into(),
            archived: false,
            muted: true,
            members: Some(42),
            channels: vec![Channel {
                id: "chan-1".into(),
                name: "Towels".into(),
                general: true,
                kind: ChannelKind::Private,
                archived: false,
                shown: true,
                pinned: true,
                muted: false,
                followed: true,
                favorite: Some(1),
                unread: true,
                last: Some(last),
            }],
        }
    }

    fn temp(name: &str) -> testdir::TestDir {
        testdir::TestDir::new(&format!("cache-{name}"))
    }

    fn cache(dir: &Path, keys: &Arc<MemoryKeys>, account: &str) -> Cache {
        Cache::new(keys.clone(), dir, "teams", account)
    }

    fn snapshot(account: &str, chats_ago: i64, teams_ago: i64) -> Snapshot {
        Snapshot::capture(
            account,
            at(0),
            Some((
                &[chat("chat-planted-id", Some(PLANTED_TOPIC), true)],
                at(chats_ago),
            )),
            Some((&[team()], at(teams_ago))),
        )
    }

    /// A cache file another program holds open with no sharing (Windows) cannot be read for the moment; that is not a
    /// reason to throw the cache away.
    #[cfg(windows)]
    #[test]
    fn a_cache_that_cannot_be_read_for_the_moment_is_left_in_place() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp("held");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        cache.write(&snapshot("me", 0, 0)).unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(cache.path())
            .unwrap();
        assert!(
            matches!(cache.read(at(0)), Err(Ignored::Transient(_))),
            "a read that failed for the moment is said as such"
        );
        drop(held);
        assert!(cache.path().exists(), "the file is still there");
        assert!(
            cache.read(at(0)).is_ok(),
            "and it is read once nothing holds it"
        );
    }
    #[test]
    fn a_snapshot_written_is_read_back_as_the_lists_without_what_the_messages_said() {
        let dir = temp("round-trip");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        cache.write(&snapshot("me", 0, 0)).unwrap();
        let back = cache.read(Utc::now()).unwrap();
        assert_eq!(back.account, "me");
        let c = &back.chats[0];
        assert_eq!(c.id, "chat-planted-id");
        assert_eq!(c.topic.as_deref(), Some(PLANTED_TOPIC));
        assert_eq!(c.kind, ChatKind::Group);
        assert_eq!(c.favorite, Some(2));
        assert_eq!(
            c.members
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["Me Myself", PLANTED_NAME]
        );
        assert_eq!(
            c.members
                .iter()
                .map(|m| m.not_in_directory)
                .collect::<Vec<_>>(),
            [false, true],
            "who the directory no longer lists is kept, so a cached title is drawn the same way"
        );
        let p = c.preview.as_ref().unwrap();
        assert_eq!(
            p.from_id.as_deref(),
            Some("zaphod-id"),
            "who sent it, for unread"
        );
        assert_eq!(p.text, "", "what it said is gone");
        assert_eq!(p.from_name, None, "and so is the sender's name");
        assert_eq!(c.web_url, None);
        assert!(c.is_unread("me"), "unread is still worked out");
        let t = &back.teams[0];
        assert_eq!(
            (t.name.as_str(), t.muted, t.members),
            ("Hitchhikers", true, Some(42))
        );
        let ch = &t.channels[0];
        assert_eq!(
            (ch.name.as_str(), ch.general, ch.unread, ch.favorite),
            ("Towels", true, true, Some(1))
        );
        assert_eq!(ch.kind, ChannelKind::Private);
        assert_eq!(ch.last.as_ref().unwrap().text, "");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_member_written_before_the_directory_mark_was_kept_reads_as_listed() {
        let old: CachedMember = serde_json::from_str(r#"{"id": "a", "name": "Ada"}"#).unwrap();
        assert!(!old.not_in_directory);
        let listed = CachedMember {
            id: "a".into(),
            name: "Ada".into(),
            not_in_directory: false,
        };
        assert_eq!(
            serde_json::to_string(&listed).unwrap(),
            r#"{"id":"a","name":"Ada"}"#,
            "and nothing is added for someone listed"
        );
    }

    #[test]
    fn neither_the_file_nor_what_is_inside_it_holds_any_message_text_or_link() {
        let dir = temp("minimal");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        let snap = snapshot("me", 0, 0);
        // What is serialised, before it is sealed.
        let json = serde_json::to_string(&snap).unwrap();
        for never in [
            PLANTED_PREVIEW,
            PLANTED_SENDER_NAME,
            PLANTED_URL,
            "layoffs",
            "secret-link",
        ] {
            assert!(!json.contains(never), "{never} is in the snapshot");
        }
        for needed in [PLANTED_TOPIC, PLANTED_NAME, "Towels", "Hitchhikers"] {
            assert!(json.contains(needed), "{needed} should be in the snapshot");
        }
        // What is on disk: nothing readable at all.
        cache.write(&snap).unwrap();
        let raw = std::fs::read(cache.path()).unwrap();
        for planted in [
            PLANTED_PREVIEW,
            PLANTED_NAME,
            PLANTED_TOPIC,
            PLANTED_SENDER_NAME,
            PLANTED_URL,
            "Towels",
            "Hitchhikers",
            "zaphod-id",
            "chat-planted-id",
        ] {
            assert!(
                !raw.windows(planted.len()).any(|w| w == planted.as_bytes()),
                "{planted} is readable in the file"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn every_write_has_a_new_nonce() {
        let dir = temp("nonce");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        let snap = snapshot("me", 0, 0);
        let mut nonces = HashSet::new();
        let mut files = HashSet::new();
        for _ in 0..20 {
            cache.write(&snap).unwrap();
            let raw = std::fs::read(cache.path()).unwrap();
            nonces.insert(raw[5..29].to_vec());
            files.insert(raw);
        }
        assert_eq!((nonces.len(), files.len()), (20, 20));
        assert_eq!(cache.writes(), 20);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_changed_byte_a_cut_file_or_another_account_is_ignored_and_the_file_removed() {
        let dir = temp("tamper");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        let original = {
            cache.write(&snapshot("me", 0, 0)).unwrap();
            std::fs::read(cache.path()).unwrap()
        };
        let mut changed = original.clone();
        let mid = changed.len() / 2;
        changed[mid] ^= 0x40;
        std::fs::write(cache.path(), &changed).unwrap();
        assert!(matches!(
            cache.read(Utc::now()),
            Err(Ignored::Unopenable(OpenError::Rejected))
        ));
        assert!(
            !cache.path().exists(),
            "a file that can never be used is removed"
        );

        std::fs::write(cache.path(), &original[..original.len() - 1]).unwrap();
        assert!(matches!(
            cache.read(Utc::now()),
            Err(Ignored::Unopenable(_))
        ));
        assert!(!cache.path().exists());

        std::fs::write(cache.path(), &original).unwrap();
        let other = Cache::new(keys.clone(), &dir, "teams", "someone-else");
        assert!(matches!(
            other.read(Utc::now()),
            Err(Ignored::Unopenable(OpenError::Rejected))
        ));
        assert!(!cache.path().exists());

        // Another service's file cannot be passed off as this one's either.
        std::fs::write(cache.path(), &original).unwrap();
        std::fs::copy(cache.path(), dir.join("snapshot-graph.cache")).unwrap();
        let graph = Cache::new(keys.clone(), &dir, "graph", "me");
        assert!(matches!(
            graph.read(Utc::now()),
            Err(Ignored::Unopenable(_))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_wrong_version_inside_or_outside_is_ignored() {
        let dir = temp("version");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        cache.write(&snapshot("me", 0, 0)).unwrap();
        // The file's own version byte.
        let mut raw = std::fs::read(cache.path()).unwrap();
        raw[4] = 9;
        std::fs::write(cache.path(), raw).unwrap();
        assert_eq!(
            cache.read(Utc::now()).unwrap_err(),
            Ignored::Unopenable(OpenError::Version {
                found: 9,
                expected: VERSION
            })
        );
        // A snapshot that says another version, sealed correctly.
        let mut snap = snapshot("me", 0, 0);
        snap.version = 2;
        cache.write(&snap).unwrap();
        assert_eq!(cache.read(Utc::now()).unwrap_err(), Ignored::Version(2));
        assert!(!cache.path().exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_snapshot_for_another_account_inside_the_seal_is_ignored_too() {
        let dir = temp("account-inside");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        cache.write(&snapshot("not-me", 0, 0)).unwrap();
        assert_eq!(cache.read(Utc::now()).unwrap_err(), Ignored::OtherAccount);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_snapshot_older_than_a_week_is_ignored_and_removed_and_a_young_part_is_kept() {
        let dir = temp("stale");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        cache.write(&snapshot("me", 8, 8)).unwrap();
        assert_eq!(cache.read(Utc::now()).unwrap_err(), Ignored::Stale);
        assert!(!cache.path().exists());

        cache.write(&snapshot("me", 6, 6)).unwrap();
        assert!(cache.read(Utc::now()).is_ok(), "six days is young enough");

        // The chat list is old and the teams are young: only the teams are used.
        cache.write(&snapshot("me", 8, 1)).unwrap();
        let r = cache.read(Utc::now()).unwrap();
        assert!(r.chats.is_empty() && r.chats_at.is_none());
        assert_eq!(r.teams.len(), 1);
        // The same file judged eight days on is too old altogether.
        cache.write(&snapshot("me", 8, 1)).unwrap();
        assert_eq!(
            cache.read(Utc::now() + Duration::days(8)).unwrap_err(),
            Ignored::Stale
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_larger_than_allowed_is_refused_before_it_is_read_and_a_big_list_is_cut_to_the_bounds()
    {
        let dir = temp("size");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        std::fs::write(cache.path(), vec![0u8; MAX_FILE_BYTES + 1000]).unwrap();
        assert_eq!(cache.read(Utc::now()).unwrap_err(), Ignored::Oversize);
        assert!(!cache.path().exists());

        // More chats than a snapshot holds: only the first MAX_CHATS are kept, so what is written reads back.
        let many: Vec<Chat> = (0..MAX_CHATS + 50)
            .map(|i| chat(&format!("c{i}"), None, false))
            .collect();
        let snap = Snapshot::capture("me", at(0), Some((&many, at(0))), None);
        assert_eq!(snap.chats.len(), MAX_CHATS);
        cache.write(&snap).unwrap();
        assert_eq!(cache.read(Utc::now()).unwrap().chats.len(), MAX_CHATS);

        // A file written by something else, with too many in it, is refused when read.
        let mut big = snapshot("me", 0, 0);
        big.chats = (0..MAX_CHATS + 1)
            .map(|i| cached_chat(&chat(&format!("c{i}"), None, false)))
            .collect();
        cache.write(&big).unwrap();
        assert_eq!(cache.read(Utc::now()).unwrap_err(), Ignored::TooMany);

        let mut roomy = team();
        roomy.channels = (0..MAX_CHANNELS_PER_TEAM + 5)
            .map(|i| Channel {
                id: format!("ch{i}"),
                ..team().channels.remove(0)
            })
            .collect();
        let snap = Snapshot::capture("me", at(0), None, Some((&[roomy], at(0))));
        assert_eq!(snap.teams[0].channels.len(), MAX_CHANNELS_PER_TEAM);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn something_that_is_not_a_snapshot_or_has_a_bad_time_is_ignored() {
        let dir = temp("garbage");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        // Sealed correctly, but not what a snapshot is.
        let key = keys.get_or_create().unwrap();
        let sealed = seal(&key, &cache.aad(), b"{\"hello\":1}").unwrap();
        std::fs::write(cache.path(), sealed).unwrap();
        assert_eq!(cache.read(Utc::now()).unwrap_err(), Ignored::Unparsable);
        let mut snap = snapshot("me", 0, 0);
        snap.chats[0].updated = i64::MAX;
        cache.write(&snap).unwrap();
        assert_eq!(cache.read(Utc::now()).unwrap_err(), Ignored::Unparsable);
        // Not a file at all.
        std::fs::write(cache.path(), "just some text").unwrap();
        assert!(matches!(
            cache.read(Utc::now()),
            Err(Ignored::Unopenable(OpenError::NotSealed))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn without_the_key_nothing_is_read_and_a_credential_store_that_does_not_answer_leaves_the_file()
    {
        let dir = temp("nokey");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        cache.write(&snapshot("me", 0, 0)).unwrap();
        keys.delete().unwrap();
        assert_eq!(cache.read(Utc::now()).unwrap_err(), Ignored::NoKey);
        assert!(
            !cache.path().exists(),
            "without its key the file is of no use"
        );

        let cache2 = cache_with_broken_store(&dir);
        assert!(matches!(cache2.read(Utc::now()), Err(Ignored::KeyStore(_))));
        assert!(
            cache2.path().exists(),
            "it may open when the store is unlocked"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn cache_with_broken_store(dir: &Path) -> Cache {
        // A file is there, sealed under some other key; the store cannot be asked.
        let cache = Cache::new(
            MemoryKeys::broken(KeyError::Failed("locked".into())),
            dir,
            "teams",
            "me",
        );
        std::fs::write(
            cache.path(),
            b"VTSE-pretend-this-is-sealed-data-of-some-length-xxxxxxxx",
        )
        .unwrap();
        cache
    }

    #[test]
    fn nothing_is_written_when_the_key_store_fails_and_an_empty_snapshot_is_not_written() {
        let dir = temp("nowrite");
        let broken = Cache::new(
            MemoryKeys::broken(KeyError::Unavailable("none".into())),
            &dir,
            "teams",
            "me",
        );
        assert!(broken.write(&snapshot("me", 0, 0)).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        let empty = Snapshot::capture("me", at(0), None, None);
        assert!(cache.write(&empty).is_err());
        assert!(!cache.path().exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn text_from_a_file_is_cleaned_like_text_from_the_network() {
        let dir = temp("clean");
        let keys = MemoryKeys::new();
        let cache = cache(&dir, &keys, "me");
        let mut snap = snapshot("me", 0, 0);
        snap.chats[0].topic = Some("Plan\u{1b}[31m red".into());
        snap.teams[0].name = "Team\u{7}".into();
        cache.write(&snap).unwrap();
        let back = cache.read(Utc::now()).unwrap();
        assert!(!back.chats[0].topic.as_deref().unwrap().contains('\u{1b}'));
        assert!(!back.teams[0].name.contains('\u{7}'));
        let _ = std::fs::remove_dir_all(dir);
    }
}
