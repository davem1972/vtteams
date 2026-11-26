//! Reading and writing the signed-in user's favourite chats, and marking the backend's own chat list with them. See
//! `docs/teams-web-protocol.md` for how each shape was found.

use super::{Bearer, Live, Parsed, TeamsBackend, parse_conversation};
use crate::error::{AuthError, Error, Result};
use crate::http::urlencode;
use crate::lock;
use crate::model::Chat;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Instant;

/// Favourites older than the newest chats are fetched one by one so that they are never missing from the list; no
/// more than this many.
const MAX_FAVOURITE_FETCHES: usize = 30;

/// Where the favourites were read from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FavouriteSource {
    /// The "Favorites" conversation folder, which the current Teams client keeps.
    Folder,
    /// The `favorites` user property, which an older client kept and which a current one no longer updates.
    Property,
}

/// A favourite older than the page, fetched by id: when it was fetched, the chat (without members resolved) and the
/// member object ids still to resolve.
pub(super) type FavouriteCache = HashMap<String, (Instant, Chat, Vec<String>)>;

/// Whether a failed read says only that it could not be done just now, as opposed to the account having no favourites
/// folder or not being allowed to read it: a dropped connection, a server having a bad moment, the service asking us
/// to wait, a sign-in being renewed.
fn read_failed_for_now(e: &Error) -> bool {
    match e {
        Error::Transport { .. }
        | Error::Decode { .. }
        | Error::Throttled { .. }
        | Error::TooLarge { .. }
        | Error::TooManyRedirects { .. } => true,
        Error::Status { status, .. } => *status >= 500 || *status == 408,
        Error::Auth(
            AuthError::SignInInProgress | AuthError::RenewalPaused | AuthError::TimedOut,
        ) => true,
        Error::NotSent(inner) => read_failed_for_now(inner),
        _ => false,
    }
}

/// The error [`TeamsBackend::set_favorite`] gives on an account whose favourites live in the conversation folder:
/// writing the old property would not show in Teams, and writing the folder itself has not been implemented yet.
/// Worded without naming a particular application, since any caller of this library can see it.
pub(super) fn folder_write_unsupported(operation: &'static str) -> Error {
    Error::Unsupported {
        operation,
        detail: "Teams keeps your favourites in a folder this library can read but not change yet",
    }
}

impl TeamsBackend {
    pub(super) fn favourites_url(live: &Live) -> String {
        format!("{}/v1/users/ME/properties?name=favorites", live.chat_base)
    }

    /// The user's favourites from the "Favorites" conversation folder: conversation id to its place (1 is first).
    /// The folder lists teams and channels too (thread types `space` and `topic`), which this backend does not show;
    /// they are left out. Folders live in the chat service aggregator, which wants its own token.
    fn favourites_in_folder(&self, live: &Live) -> Result<serde_json::Map<String, Value>> {
        const OPERATION: &str = "read favourites";
        let url = format!(
            "{}/api/v1/teams/users/me/conversationFolders?supportsAdditionalSystemGeneratedFolders=true&supportsSliceItems=true",
            live.csa_base
        );
        let v = self.request_with(Bearer::Csa, OPERATION, "GET", &url, None)?;
        let folder = v["conversationFolders"]
            .as_array()
            .and_then(|folders| {
                folders
                    .iter()
                    .find(|f| f["folderType"] == "Favorites" && f["isDeleted"] != true)
            })
            .ok_or(Error::Unexpected {
                operation: OPERATION,
                detail: "no favourites folder in the answer",
            })?;
        Ok(folder["conversationFolderItems"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| !matches!(i["threadType"].as_str(), Some("space" | "topic")))
            .filter_map(|i| i["conversationId"].as_str())
            .enumerate()
            .map(|(n, id)| (id.to_string(), json!(n + 1)))
            .collect())
    }

    /// The user's favourites and where they came from: the folder when the service has one, else the older property.
    /// Remembers the source in [`Self::favourite_source`] on success, for [`can_favorite`](crate::Backend::can_favorite)
    /// and [`set_favorite`](crate::Backend::set_favorite) to consult without a request of their own.
    ///
    /// The property is only a fallback for an account that has no folder, or is not allowed to read it. A folder
    /// that could not be read *this time* (the network, a server error, a sign-in being renewed) is an error here, so
    /// that a hiccup never makes an account whose favourites live in the folder look like one whose live in the
    /// property, with a stale list and a write that Teams would never show.
    pub(super) fn favourites(
        &self,
        live: &Live,
    ) -> Result<(serde_json::Map<String, Value>, FavouriteSource)> {
        let found = if !live.csa_base.is_empty() {
            match self.favourites_in_folder(live) {
                Ok(f) => Some((f, FavouriteSource::Folder)),
                Err(e) if read_failed_for_now(&e) => return Err(e),
                Err(e) => {
                    log::debug!(
                        "favourites folder unavailable, using the property: {}",
                        e.for_log()
                    );
                    None
                }
            }
        } else {
            None
        };
        let (favourites, source) = match found {
            Some(found) => found,
            None => self
                .favourites_in_property(live)
                .map(|f| (f, FavouriteSource::Property))?,
        };
        *lock(&self.favourite_source) = Some(source);
        *lock(&self.last_favourites) = Some((favourites.clone(), source));
        Ok((favourites, source))
    }

    /// The favourites an older client kept: one user property holding a JSON map as a string
    /// (`{"48:notes":1,"19:...":2}`); a property that is absent or empty means none.
    fn favourites_in_property(&self, live: &Live) -> Result<serde_json::Map<String, Value>> {
        const OPERATION: &str = "read favourites";
        let v = self.request(OPERATION, "GET", &Self::favourites_url(live), None)?;
        match v["favorites"].as_str().map(str::trim) {
            None | Some("") => Ok(serde_json::Map::new()),
            Some(text) => serde_json::from_str::<Value>(text)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .ok_or(Error::Unexpected {
                    operation: OPERATION,
                    detail: "the favourites are not a list of chats",
                }),
        }
    }

    /// Mark the favourites among `chats`, and bring in the ones that are not in them (older than the page) by reading
    /// each. Best effort: favourites that cannot be read leave the list as it was, and say so in the log only, because
    /// this runs with every refresh.
    pub(super) fn add_favourites(&self, live: &Live, chats: &mut Vec<Chat>) {
        let favourites = match self.favourites(live) {
            Ok((f, _)) => f,
            // A read that failed for the moment keeps what the last one found, so the list does not lose its
            // favourites (and their order) for a refresh. Only the list uses it: a change is never worked out from it.
            Err(e) if read_failed_for_now(&e) => match lock(&self.last_favourites).clone() {
                Some((known, _)) => {
                    log::debug!(
                        "favourites could not be read just now, keeping the last ones: {}",
                        e.for_log()
                    );
                    known
                }
                None => {
                    log::debug!("favourites unavailable: {}", e.for_log());
                    return;
                }
            },
            Err(e) => {
                log::debug!("favourites unavailable: {}", e.for_log());
                return;
            }
        };
        let missing: Vec<String> = favourites
            .keys()
            .filter(|id| !chats.iter().any(|c| &c.id == *id))
            .take(MAX_FAVOURITE_FETCHES)
            .cloned()
            .collect();
        let mut older = Vec::new();
        for id in missing {
            let cached = {
                let cache = lock(&self.favourite_cache);
                cache.get(&id).and_then(|(fetched_at, c, ids)| {
                    (fetched_at.elapsed() < self.full_refresh).then(|| (c.clone(), ids.clone()))
                })
            };
            if let Some(item) = cached {
                older.push(item);
                continue;
            }
            let url = format!(
                "{}/v1/users/ME/conversations/{}?view=msnp24Equivalent",
                live.chat_base,
                urlencode(&id)
            );
            match self.request("read a favourite", "GET", &url, None) {
                Ok(doc) => {
                    if let Parsed::Item((c, ids)) = parse_conversation(&doc) {
                        lock(&self.favourite_cache)
                            .insert(id, (Instant::now(), c.clone(), ids.clone()));
                        older.push((c, ids));
                    }
                }
                Err(e) => log::debug!("a favourite could not be read: {}", e.for_log()),
            }
        }
        let ids: Vec<String> = older.iter().flat_map(|(_, ids)| ids.clone()).collect();
        if !ids.is_empty()
            && let Err(e) = self.resolve_names(live, &ids, true)
        {
            log::debug!("names of favourites unavailable: {}", e.for_log());
        }
        chats.extend(older.into_iter().map(|(mut c, ids)| {
            c.members = ids.into_iter().map(|id| self.member(id)).collect();
            c
        }));
        for c in chats.iter_mut() {
            c.favorite = favourites.get(&c.id).map(|order| {
                order
                    .as_u64()
                    .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
            });
        }
    }
}
