# The Teams web API, as vtteams uses it

The unofficial API behind the Teams web client, as far as vtteams relies on it. It is **not** documented or supported by
Microsoft and can change without notice; vtteams's `--teams` mode says so (see the README). The official route is Microsoft
Graph, covered at the end.

**Reading this file**

- Anything marked **(unverified)** has not been observed against the real service: it comes from the behaviour of other
  open-source clients (read for behaviour only; no code is copied, see the README's Acknowledgements), from Microsoft's
  documentation, or is an assumption. Treat it as a hypothesis.
- Everything else has been observed against a real tenant.
- Where a fact is also a test, the test name is given.

Rules for this file: state what a request looks like and what comes back, as facts, in the present tense, with no dates
and no account of how a fact was found. Never record a token, a person's name, message text or an id of a real chat. Use
field names, shapes and sizes. Mark what is unverified.

---

## 1. Sign-in and credentials

| Step | Detail |
|---|---|
| Browser sign-in | The user's own Edge or Chrome is driven over DevTools (loopback `--remote-debugging-port=0`, only while capturing). The Teams page's `localStorage` holds an MSAL cache: JSON documents with a `credentialType` field, which some setups encrypt at rest (see "Some setups encrypt the MSAL cache"). vtteams keeps only the refresh token and identity. |
| Client | The first-party Teams web app, a public SPA client. vtteams presents it as such. |
| Access token | Refresh-token grant against `login.microsoftonline.com/{tenant}/oauth2/v2.0/token`, scope `https://api.spaces.skype.com/.default offline_access`, with the SPA `Origin` header. A second scope, `https://substrate.office.com/.default`, is for people search. |
| Skype token | `POST https://authsvc.teams.microsoft.com/v1.0/authz` with the access token as bearer returns `tokens.skypeToken` (+ `expiresIn`) and `regionGtms` (`chatServiceAfd`, `middleTier`, ...): the regional chat service base. Every chat request carries both the bearer and `Authentication: skypetoken=...`. |
| Refresh-token life | SPA refresh tokens have a **fixed 24 hour life** that rotation does not extend (`AADSTS700084`). |
| Renewal | The browser profile keeps Teams' SSO session. Reopening it headless lets Teams' own sign-in issue a new refresh token. **The profile's cached token is older than vtteams's rotated copy and just as dead until SSO has run**, so a token found there is *redeemed* before it is believed (`working_session`; test `a_refresh_token_in_the_browser_is_accepted_only_once_entra_honours_it`): the profile's stored token differs from vtteams's and is rejected by Entra until SSO runs. A full renewal cycle starting from a genuine 24-hour expiry is unverified. |
| Profile in use | A browser left running on the profile makes the next one exit at once (Chromium exit code 21). Its debugging port is in `DevToolsActivePort`; `Browser.close` over that port ends it. |
| Debug port | Unauthenticated on loopback while the browser is open (documented in SECURITY.md). |

Credentials go only to their own service. Hosts are checked (`trusted_url`); redirects are not followed.

**What a refused grant looks like, and what it does not mean.** When the grant dies, about 24 hours after the first
sign-in, the cached access tokens keep chat reads working. The first sign of it is not in reading: it is presence,
activity reports and sends being refused, repeatedly, and nothing in the chat itself says so. A tenant that withholds only
one scope (chat service aggregator, Substrate, presence, Graph) rejects that scope's token request **in the same way** an
expired grant is rejected. So a refusal for a scope other than the main (spaces) one cannot be read as "the sign-in is
dead" on its own: asking the main scope tells the two apart (fine: only that scope is withheld; refused too: the grant is
dead). Recovering through the browser cannot fix a scope that was never granted. What the application does about it is in
`AGENTS.md`, "Sign-in and renewal".

### Device-code sign-in works only where the tenant allows it

Browser-driven sign-in needs a Chromium-family browser (Edge, Chrome or Chromium) installed. The OAuth 2.0 device-code
flow needs no browser at all, and the Teams client id supports it:

- **The device-code request succeeds**: `POST {login_base}/{tenant}/oauth2/v2.0/devicecode` with
  `client_id=5e3ce6c0-2b1f-4285-8d4b-75ee78787346` (the Teams web client) and
  `scope=https://api.spaces.skype.com/.default offline_access` returns `200 OK` with a normal
  `user_code`/`device_code`/`verification_uri`/`expires_in`/`interval` body.
- **Completing it can be refused by the tenant's policy**: polling `{login_base}/{tenant}/oauth2/v2.0/token` after signing
  in at the verification URL can return `AADSTS53003` ("Conditional Access"), even when the error page's own `Device state`
  is `Compliant`. `AADSTS53003` is Entra's generic Conditional Access refusal. A tenant can configure a Conditional Access
  policy that blocks the device-code flow itself (a published Microsoft hardening recommendation, because device-code
  sign-in is a known phishing vector), independently of whether the app or the device is trusted. This is tenant-specific:
  some tenants allow the flow, others disallow it.
- **Consequence**: device-code sign-in cannot be the only path, since whether it works depends on a policy each tenant
  chooses. It could be tried first, falling back to a driven browser only when Conditional Access refuses it. That is not
  built: sign-in drives a browser.

### Safari (and Firefox) cannot be driven

The sign-in is driven over the Chrome DevTools protocol (`teams/browser.rs`: a loopback debugging port, a page's
`localStorage` and cookies read through it, the MSAL decryption script run in the page). Safari does not speak that
protocol, nor does Firefox in any form vtteams uses. The one way to drive Safari is its WebDriver, `safaridriver`, which
ships with macOS. That would be a second sign-in path beside the DevTools one (a W3C WebDriver client over HTTP, which the
library's HTTP client could do, running the page-storage reads and the decryption script through "execute script"). It has
not been tried; its costs, as far as they are known, are (unverified):

- **Setup**: `safaridriver --enable` needs an administrator password once, and Safari's "Allow Remote Automation" setting
  has to be on.
- **No saved session**: a WebDriver session starts Safari from a clean profile (no saved cookies or Microsoft session,
  nothing kept afterwards, one automation session at a time, no headless mode). That would end the silent renewal every
  24 hours that the Chromium profile gives (`TeamsAuth::recover` reopens the private profile headless and lets Teams' SSO
  store a new refresh token): each 24 hours would be a full interactive sign-in, MFA included. Whether a clean profile
  would keep the page storage long enough to read is not known.
- **Needs a Mac to verify**.

The alternatives are Edge or Chrome (installing it is enough: `--browser <path>` or `browser =` picks one, and it does not
have to be the default browser), or the device-code flow above, the only route that needs no browser and so would also
serve a Mac with only Safari, but only where Conditional Access allows it. Copying the refresh token out of Safari by hand
every 24 hours is possible and not worth building. The sign-in is Chromium-only; a Safari path is worth building only for
someone with a real reason not to have a Chromium browser.

### Chrome/Chromium needs `--disable-features=ThirdPartyStoragePartitioning` to populate `localStorage`

In Chromium (not Edge), a real interactive sign-in can finish completely, with the page fully loaded and showing a signed-in
Teams UI, while `window.localStorage` of that top-level page stays at zero keys. `Browser::teams_storage` reads only the
top-level page's own `localStorage`, so it finds nothing and the capture loop runs until it times out. Edge populates
`localStorage` normally with the same web app and account. The difference points at Chrome's third-party storage
partitioning (part of its privacy-sandbox rollout, which Edge has not adopted on the same timeline) isolating the
cross-origin step that commits the MSAL cache from the top-level page (the cause is unverified). Launching Chromium with
`--disable-features=ThirdPartyStoragePartitioning` leaves over a hundred keys in `localStorage`, MSAL's among them, but they
are encrypted (next sections), so the flag alone is not enough. `Browser::launch_once` passes it unconditionally, Edge
included (a Chromium-family browser silently ignores a `--disable-features` name it does not recognise or has already
turned off, so it is harmless where it is not needed). Whether it is still needed now that the encrypted entries are read is
unverified.

### Presence (availability)

How the Teams web client learns whether someone is around is **unverified** against a live tenant, except for the status
messages and out-of-office notes below. What other open-source clients do:

- **Request** (unverified): `POST https://presence.teams.microsoft.com/v1/presence/getpresence/` with a JSON array of
  `{"mri": "8:orgid:<object id>"}`, up to 650 people in one request. It is polled every 60 s for contacts that do not get
  presence by push.
- **Token** (unverified): its own audience. A separate access token with the scope
  `https://presence.teams.microsoft.com/.default openid profile offline_access` is obtained by redeeming the refresh token,
  the way the Substrate and chat-service-aggregator tokens are. (In `regionGtms` the service lists
  `presenceUPS`/`unifiedPresence`; there is also a `/ups/<region>/v1/...` path on `teams.microsoft.com`, used for writing a
  work location and a status note.)
- **Answer** (unverified): an array of `{mri, etag, presence: {availability, activity, deviceType, lastActiveTime,
  calendarData: {isOutOfOffice, outOfOfficeNote: {message}}, workLocation: {location, approximateDetails: {name}},
  note: {message, ...}}, etagMatch?, status: 20000}`. `availability` is one of Available, AvailableIdle, Away, BeRightBack,
  Busy, BusyIdle, DoNotDisturb, Offline, PresenceUnknown. An entry with `etagMatch` and no `presence` is "unchanged since
  the etag you sent".
- **Push** (unverified): a client subscribes with
  `POST https://teams.microsoft.com/ups/<region>/v1/pubsub/subscriptions/<endpoint id>` (body: `trouterUri` = the Trouter
  `surl` + `/TeamsUnifiedPresenceService`, `subscriptionsToAdd`: `[{mri}]`, `subscriptionsToRemove`: `[]`,
  `shouldPurgePreviousSubscriptions`: false, up to 100 people) and then receives `{presence: [...], isSnapshot}` bodies over
  the Trouter connection. vtteams does not subscribe: it polls.
- **Setting your own** (unverified; these change what other people see):
  - `PUT /v1/me/forceavailability/` with `{"availability": "<word>"}`: a status you chose (Available, Busy, DoNotDisturb,
    BeRightBack, Away, Offline), which applies on all your devices; no expiry is sent. Giving the choice up is believed to
    be `DELETE` on the same path.
  - `PUT /v1/me/endpoints/` with `{id: <this program's own id>, availability: "Available", activity: "Available",
    activityReporting: "Transport", deviceType: "Desktop"}`: registers the program as one of the places you are online.
  - `POST /v1/me/reportmyactivity/` with `{"endpointId": <that id>, "isActive": true|false}`: whether you are at the
    keyboard. Left to the service, active shows as available and idle as away (AvailableIdle), as in the Teams app. It is
    sent every two minutes from an idle timer, and a 404 means the endpoint must be registered again.
  - A status message (`PUT /v1/me/publishnote`, body `{message, expiry}`) and a work location
    (`PUT /ups/<region>/v1/me/workLocation/`) exist and are not used.
- **`AvailableIdle`** (signed in, idle) reads as available to other clients, but the **Teams client itself shows it as away**
  (yellow), and vtteams follows the Teams client (`BusyIdle` as busy). Out of office shows purple in the Teams client unless
  the person is available, busy or do-not-disturb.
- **Status messages and out-of-office notes.** Inside `presence` of an entry:
  - `note: {expiry, message, publishTime}`: a status message. `message` is a string that **can hold HTML** (an `<a>` anchor
    around a phone number or link, 144 characters), so it is read as HTML and made plain. `publishTime` looks like
    `YYYY-MM-DDThh:mm:ss.fffffffZ` (seven fractional digits, sometimes six) and `expiry` like `YYYY-MM-DDThh:mm:ssZ` (none).
  - `calendarData: {isOutOfOffice: bool, outOfOfficeNote: {expiry, message, publishTime}}`: `outOfOfficeNote` is present when
    `isOutOfOffice` is true and the person has written a reply (one line of plain text, `expiry` with seven fractional
    digits); an out-of-office person with no reply has `isOutOfOffice: true` and no note.
  - Your own entry also carries `forcedAvailability: {activity, availability, expiry, publishTime}` (the status you chose);
    it is not read for notes.
  - An expiry very far ahead (year 9999 and the like) is believed to mean "no end", so vtteams treats an expiry more than
    about five years away as none (that this is what such a value means is unverified).
  - The answer has **no field for "show when people message me"**: whether a person chose that is not in it, so vtteams
    cannot honour it; it shows a note whenever the answer has one.
  - **Unverified:** whether the Teams client shows a note to people who are not in the chat with that person (a colleague in
    a meeting only, someone outside the organisation), what it does for a note whose "show when people message me" choice
    is off, and the exact conditions under which it is shown. vtteams asks for the people of the open 1:1, group or meeting
    chat (at most 40) and shows what the answer holds for them. Nothing in the answer says the service withheld a note.
- What vtteams does with it: `TeamsBackend::presence`, `set_status`, `report_activity`, `app/presence.rs`,
  `app/status.rs`; see `AGENTS.md`, "Presence and your own status".

### Some setups encrypt the MSAL cache in `localStorage`

On `teams.cloud.microsoft` the MSAL entries are under MSAL.js v4's key schema
(`msal.2|<homeAccountId>|<environment>|refreshtoken|<clientId>|||`, the client id being Teams' own), and in some setups each
value is `{"id", "nonce", "data", "lastUpdatedAt"}` with `data` a base64url string that is not JSON: no `credentialType` is
visible. The key is in a session cookie, `msal.cache.encryption`, readable from the page; its `id` matches the entries'
`id`. The cache is unreadable at rest without that cookie, which dies with the browser session (the profile's SSO cookies,
not this cache, are what silent renewal depends on).

The cookie is URL-encoded JSON `{"id", "key"}` (`key` base64). Each entry's key is HKDF-SHA256 over that key with the
entry's `nonce` as salt and the client id as `info` (or empty; which of the two applies is not known, so both are tried),
and `data` is AES-256-GCM under a zero IV (safe only because the derived key is per nonce), as in MSAL.js v4. The browser
poll (`browser::look`) decrypts in the page with WebCrypto (`READ_STORAGE`, `Runtime.evaluate` with `awaitPromise`), trying
both `info` values, and hands back plaintext in place of each wrapper (a test runs that script under Node against known
answers made with Node's own `crypto` module, so the parameters are pinned independently of the page's API), so
`session_from_storage` reads the same documents either way; an entry that is not encrypted passes through, so a plain cache
still works. It logs, at debug and only on change, how many entries it saw, how many were encrypted and how many it
decrypted (counts only).

Whether the cache is encrypted depends on the setup, not on the client's age: a Chrome profile on Linux encrypts, while an
Edge profile on Windows on the same host (`teams.cloud.microsoft`, the same `msal.2|` keys) holds the refresh-token entry as
plain JSON with no `msal.cache.encryption` cookie anywhere. Why is unknown (a per-platform or per-browser choice, or a
different rollout ring, are guesses; the encrypting page also sets a `ringFinder` cookie). Either form can appear on any
browser, which is why both are read.

### `regionGtms` names a push channel (Trouter)

The `authz` answer's `regionGtms` has well over a hundred service URLs; vtteams reads three
(`chatServiceAfd`/`middleTier`/`chatSvcAggAfd`, used for `chat_base`/`mt_base`/`csa_base`) and ignores the rest as opaque
JSON. A handful of the ignored ones name a real-time channel: `calling_trouterUrl`, `teamsPushServiceAfd`,
`notificationFilteringService`, plus the separate `presenceUPS`/`unifiedPresence` (presence, not chat) and
`calling_registrarUrl` (call signalling, not chat). Teams' push-notification service is called "Trouter".

**The whole lifecycle works headless.** None of it needs a new credential: registration, the socket.io handshake and the
WebSocket dial use only `Live.skype_token`, as a raw `X-Skypetoken` header (not `Authentication: skypetoken=...`, the form
every other chat request uses; this is a different, older API on a different host), and only the registrar step also wants
`Live.spaces_token`, as `Authorization: Bearer`.

1. `POST https://go.trouter.teams.microsoft.com/v4/a?epid=<client-generated uuid>`, header `X-Skypetoken` only, body empty.
   Returns 200 with a JSON object: `ccid`, `connectparams` (an object of short-lived, pre-signed query fields,
   `sig`/`se`/`st`/`sp`/`sr`/`scae`/`issuer`, to be copied verbatim into the next two requests' query strings), `curlb`,
   `healthUrl`, `id`, `registrarUrl` (`https://communications.svc.cloud.microsoft/registrar/{region}/v3/registrations`;
   older references name `https://teams.microsoft.com/registrar/prod/V2/registrations`, which may be stale), `socketio`
   (a bare host, used for the handshake and the WebSocket dial below), `surl`/`url` (this registration's own `v4/f/<sr>/`
   path on two ports, 3443 and 8443, used only to build this client's own reachable address when registering with
   `registrarUrl`, never for the client's own outbound connection).
2. `GET {socketio}/socket.io/1/?v=v4&tc=<json>&con_num=<ms>_<n>&epid=<uuid>&auth=true&timeout=40&<connectparams
   fields>&ccid=<ccid>`, same `X-Skypetoken` header. `tc` is a fixed tracing blob,
   `{"cv":"2024.23.01.2","ua":"TeamsCDL","hr":"","v":"27/24070119616"}`; **an empty `cv` is refused with a bare 400** (no
   body) rather than with a reason, so it is the one field to get exactly right. A working request returns 200,
   `text/plain`, body `<32-char sid>:<heartbeat>:<close>:<transports>` (socket.io v0.9's handshake shape).
3. `wss://{socketio}/socket.io/1/websocket/{sid}?<same query, con_num incremented>`, same header, upgraded with a plain
   TLS-wrapped WebSocket (no pipe transport, no special subprotocol). On the socket.io `1` (connect) frame, send
   `5:::{"name":"user.authenticate","args":[{"headers":{"Authorization":"Bearer <spaces token>"},
   "connectparams":<the same object>}]}` and `5:::{"name":"user.activity","args":[{"state":"active"}]}`, then
   `POST {registrarUrl}` once per app (`NextGenCalling`, `SkypeSpacesWeb`, `SkypeSpacesCallAgent`, `TeamsCDLWebWorker`; the
   one path-less, nameless registration being `TeamsCDLWebWorker`), each with both `X-Skypetoken` and
   `Authorization: Bearer <spaces token>`. All four answer **202 Accepted**.
4. After that: `trouter.connected` (a `5` event frame), several `trouter.message_loss` events (expected: a new endpoint has
   no history to resume from, so this is not a real gap), periodic `2` heartbeats (answered with a bare `2::`), and `3`
   (request) frames whose `url` is `/v4/f/<sr>/messaging`. These arrive as the event happens (a reaction or an edit made
   from another Teams client), each acked with `3:::{"id":<n>,"status":200,"body":""}`. The push carries the event as it
   happens; no polling is involved.

This is implemented in `teams/trouter.rs` (`AGENTS.md`, "Push (Trouter)"). The `messaging` request's own JSON is a small
REST-request envelope, not the message shape directly:
```
{"id": <n>, "method": "...", "url": "/v4/f/<sr>/messaging", "headers": {...}, "body": "<json string>"}
```
(field types: `body: string, headers: object, id: number, method: string, url: string`). **`body` is itself a string**, not an
object: the actual event is JSON encoded a second time inside it, and has to be decoded again before anything of
`teams::parse::parse_msg`'s own message shape is visible there.

The decoded string is a Microsoft-style change-notification envelope:
```
{"type": "...", "resourceType": "...", "resourceLink": "...", "time": "...", "isactive": true,
 "mentiontypes": "...", "resource": {...}}
```
(field types: `isactive: bool, mentiontypes: string, resource: object, resourceLink: string, resourceType: string, time:
string, type: string`). **`resource` is a nested object**, one level deeper than the decoded body itself: the
conversation-identifying fields are not at the top level, so `extract_chat_id` looks inside `resource`.

For a reaction, `resource` is an ordinary message-shaped object (the same family `teams::parse::parse_msg` reads), with
these fields and types:
```
annotationsSummary: object, clientmessageid: string, composetime: string, content: string, contenttype: string,
conversationLink: string, from: string, fromDisplayNameInToken: string, fromFamilyNameInToken: string,
fromGivenNameInToken: string, id: string, imdisplayname: string, inQuarantine: bool, messagetype: string,
originalarrivaltime: string, prioritizeimdisplayname: null, properties: object, sequenceId: number,
threadtype: string, to: string, type: string, version: string
```
**`conversationLink` is there; `conversationid` is not** (a reaction's `resource`, at least, carries the link form only),
so `extract_chat_id` tries both and takes the trailing path segment of `conversationLink`, giving `PushHint::Chat(id)`;
an unrecognised shape falls back to `Unknown`.

More of what the pushes carry: the decoded body's `type` and `resource`'s `resourceType` are a small closed set of protocol
labels (`EventMessage` / `NewMessage` occur). One push's `resource` was **a whole conversation document** (`threadtopic`,
`lastMessage`, `memberProperties`, `threadProperties`, `members`, `rosterVersion`), not a message, so pushes cover more than
new messages. A `3` frame whose `url` does not end `/messaging` (a typing indicator is believed to be one; unverified) and a
`5` event's `name` (`trouter.connected`, `trouter.message_loss` occur) are logged by kind only; the url's last segment is
logged, never the path, which carries the connection's own session id.

## 2. Reading

### Conversation list

`GET {chat}/v1/users/ME/conversations?view=msnp24Equivalent&pageSize=N`. `conversations[]`, each with: `id`, `lastMessage`
(a message), `threadProperties` (`threadType`, `topic`, `hidden`, `spaceType`, ...), `properties` (`consumptionhorizon`,
`isemptyconversation`, ...), `version`, and a `_metadata.syncState` link.

- Ids: `19:<oid>_<oid>@unq.gbl.spaces` one-to-one (sorted ids); `19:...@thread.v2` group; `19:meeting_...` meeting
  (`threadType: meeting`); `...@thread.tacv2` channel; team roots have `threadType: space`/`topic` or a `spaceType`.
  Teams, channels, hidden and empty conversations are skipped (not chats; what a team's or channel's entry says about its
  newest message and read marker is kept apart, see "Teams and channels").
- `properties.consumptionhorizon` = `<read ms>;<ms>;<message id>`; the first field is the read marker.
- A conversation with no message has `lastMessage` absent, null or `{}` (the change feed uses `{}`).
- **Streams, not conversations.** Four ids start `48:`: `48:notes` (`threadType: streamofnotes`, your chat with yourself,
  no `members`), `48:notifications`, `48:mentions`, `48:calllogs` (feeds). vtteams lists only `48:notes`, as "Notes to self";
  `:new` on your own name opens it instead of computing a `19:<oid>_<oid>` chat with two copies of you. Test
  `the_conversation_with_yourself_is_a_chat_and_the_other_streams_are_not`.
- A conversation document carries no `members`. Group rosters come from `GET {chat}/v1/threads/{id}?view=msnp24Equivalent`
  (`members[].id` = `8:orgid:<guid>`); names from `fetchShortProfile`.
- `fetchShortProfile` (asked with `includeDisabledAccounts=true&enableGuest=true`) answers 200 but **leaves some ids out of
  `value` altogether**: about one id in ten is left out of an answer, and some batches lose more than half of their ids. A
  profile that is returned has `displayName`, `mri`, `objectId`, `userType`, `isShortProfile` and the like; nothing marks a
  disabled account. A favourite 1:1 chat with someone who left the organisation long ago is titled "Just me" by the Teams
  client, which fits the account no longer being in the directory. vtteams therefore reads "left out of an answer that
  listed others" as `Member::not_in_directory` and draws the name dim and in italics. **Unverified:** that every id left out
  is a deleted account; a person from another organisation (federation) may be left out too, which would draw their name
  the same way.
- Topic: unnamed group chats get a topic made of member ids (`8:orgid:..., 8:orgid:...`); vtteams ignores those.

### What the list holds (rounded counts from one account)

`pageSize=500` returns **about 480 conversations in one answer**. How the page size behaves:

| `pageSize` | conversations returned | bytes | time |
|---|---|---|---|
| 50 | just under 50 | 150 kB | 0.3 s |
| 200 | just under 200 | 620 kB | 0.4 s |
| 300 | just under 300 | 920 kB | 0.5 s |
| 500 | about 480 | 1.5 MB | 0.6 s |
| 1000, 5000 | HTTP 400 | | |

A few fewer than asked come back (the service drops some entries), every answer has a `backwardLink`, and 500 is enough to
get the whole list of this account. vtteams asks for 300 by default and allows up to 500 (`max_chats`). A single
conversation can be read by id (`GET {chat}/v1/users/ME/conversations/{id}?view=msnp24Equivalent`): the document has the
same shape as a list entry and parses the same way, `lastMessage` included; vtteams uses it to bring in a favourite that is
older than the page. By id shape:

| Id shape | `threadType` | Count | vtteams |
|---|---|---|---|
| `19:..@thread.v2` | `chat` (group) | about 60 | listed (a couple had no message yet) |
| `19:..@unq.gbl.spaces` | `chat` (one to one) | about 80 | listed (one had no message yet) |
| `19:..@thread.v2` with `meeting_` | `meeting` | about 290 | listed; about a sixth skipped, nothing said in them |
| `19:..@thread.skype` | `space`, `topic`: a team and its topics | about 15 | skipped (not a chat) |
| `19:..@thread.tacv2` | `space`, `topic`: channels | about 30 | skipped (channels are not supported) |
| `48:notes` | `streamofnotes` | 1 | listed as "Notes to self" |
| `48:notifications`, `48:mentions`, `48:calllogs`, `48:threads` | `streamofnotifications`, `streamofmentions`, `streamofcalllogs`, `streamofthreads` | 4 | skipped (feeds; `threads` is the threads you follow) |

`threadProperties.productThreadType` says the same in words: `Chat`, `OneToOneChat`, `Meeting`, `TeamsTeam`,
`TeamsStandardChannel`, `TeamsPrivateChannel`, `StreamOfNotes`, `StreamOfThreads`, `StreamOfCallLogs`, `StreamOfMentions`,
`StreamOfNotifications`. Every shape parses (`Malformed` is 0 for each). The newest message of the conversations is
`RichText/Html` (about half), `RichText/Media_CallRecording` (about a quarter), `Text` (about a tenth),
`ThreadActivity/AddMember` (a few), `RichText` (one), and none (about a tenth). More message types exist inside
conversations: `ThreadActivity/*`, `RichText/Media_Card`, `Event/Call`. Conversation fields: `id`, `lastMessage`,
`lastUpdatedMessageId`, `lastUpdatedMessageVersion`, `memberProperties`, `messages`, `properties`, `targetLink`,
`threadProperties`, `type`, `version`.

### Favourites

- **The current Teams client keeps favourites in a conversation folder, not in the property below.**
  `GET {csa}/api/v1/teams/users/me/conversationFolders?supportsAdditionalSystemGeneratedFolders=true&supportsSliceItems=true`,
  where `{csa}` is `regionGtms.chatSvcAggAfd` from `authz` (`https://teams.microsoft.com/api/csa/<region>`). It needs a
  bearer token for the scope `https://chatsvcagg.teams.microsoft.com/.default offline_access` (a refresh-token exchange like
  the others); the spaces token gets 401. The skypetoken header goes along. The answer (200, about 8 kB) has
  `conversationFolders`, `conversationFolderOrder`, `folderHierarchyVersion`, `isPartialResponse`,
  `migratedPinnedToFavorites` and others. Each folder has `id`, `folderType`, `name`, `sortType`, `isDeleted`,
  `isExpanded`, `version`, `createdTime` and `conversationFolderItems`. Folder types: `Favorites`
  (`UserDefinedCustomOrder`), `RecentChats`, `TeamsAndChannels`, `QuickViews`, `MutedChats`, `MeetingChats`,
  `EngageCommunities`. A `Favorites` item has `conversationId`, `threadType` (`chat`, `streamofnotes`, `space`, `topic`),
  `createdTime`, `lastUpdatedTime`; the array order is the order. The folder holds teams and channels too (`space`/`topic`,
  which vtteams does not list as chats) and can hold more than the legacy property, which the current client no longer
  updates: a chat favourited recently can be in the folder only. Reading uses the folder and falls back to the property when
  the folder cannot be read. **Writing to the folder is unexplored (unverified)**, so vtteams does not change favourites on
  an account where it reads them from the folder.
- **Legacy: the user-level property `favorites`**, not anything on the conversation.
  `GET {chat}/v1/users/ME/properties?name=favorites` answers `{"favorites": "<JSON string>", "cid", "skypeName", ...}`;
  the string is a map from conversation id to its order, numbered from 1: `{"48:notes":1,"19:..@unq.gbl.spaces":2,...}`.
  (`GET {chat}/v1/users/ME/properties` without a name returns every user property, about 23 kB; others there include
  `teamsOrder`, `autoUnfavoriteTeams`, `selfChatSettings`, `recentReactions`.)
- **Setting it**: `PUT {chat}/v1/users/ME/properties?name=favorites` with `{"favorites": "<the whole map as a JSON string>"}`.
  The answer is 200 with a `null` body; the next read returns it. Adding a chat as `max + 1` works, and putting the saved
  original string back restores it byte for byte.
- On the conversation itself, `properties.favorite` (`"true"`/`"false"`/`"True"`/`"False"`, any case), `lastTimeFavorited`
  (ms) and `ispinned` exist **only for teams and channels** (`@thread.skype`, `@thread.tacv2`), and
  `isGeneralChannelFavorite` for channels. No one-to-one, group or meeting chat carries any of them.

### Incremental reads

- Messages: `startTime=<newest version seen - 5 s>` returns the changes (sends, edits, reactions, deletions) since; merge
  into the remembered page. A message's `version` changes on edit, reaction and delete.
- Chat list: follow the `_metadata.syncState` link one step per call; after a full read it repeats most of the list for
  about fourteen calls, then answers become empty (0.5 kB against 611 kB). The messages' `syncState` link is useless
  (`startTime` 0, same page every time).
- Full read every five minutes, when an answer has 50 or more items, and after a refused incremental request.

### Messages

`GET {chat}/v1/users/ME/conversations/{id}/messages?pageSize=N&view=msnp24Equivalent`. Fields: `id`, `clientmessageid`,
`composetime`, `originalarrivaltime`, `from` (a contacts URL ending `8:orgid:<guid>`), `imdisplayname`, `messagetype`,
`contenttype`, `content`, `properties`, `version`, `sequenceId`, `conversationLink`, `type`.

- `messagetype`: `Text`, `RichText/Html`, `RichText` (mentions stream), `Event/Call`, `ThreadActivity/*` (joins, settings,
  favourites), `RichText/Media_CallRecording`/`_CallTranscript`/`_Card`, `Control/*` (typing: skipped). A service posting
  under a person's id has `s2spartnername` and no display name.
- `properties` keys: `files`, `cards`, `links`, `mentions`, `emotions`, `formatVariant`, `languageStamp`, `subject`,
  `qtdMsgs`, `atp`, `edittime`, `deletetime`. Several hold **JSON inside a string**.
- A deleted message stays in the list with empty content; `deletetime` is set on it (unverified).
- Unread rule (matches the Teams client's count on a real tenant with about 150 chats): newest message after the read
  marker, not yours, not automated; no marker = never opened = unread unless a meeting chat.

### Teams and channels

Structure only; nothing here records an id or text.

**Writing to a channel** (`teams/channels.rs`, `Backend::post_to_channel` / `edit_post`):

- **A root post with a subject** is the request a chat message is (the IC3 bearer and the genuine client's other headers
  `send_message` sends, `POST {chat}/v1/users/ME/conversations/{channel}/messages`, the same body and `properties`) with
  `properties.subject` set. HTTP 201, and the answer holds `{"OriginalArrivalTime": <ms>}` only. Read back: stored once,
  `rootMessageId` equal to its own `id`, `properties.subject` equal to what was sent, the content as sent. A post with no
  subject sends `"subject": ""`, as a chat message does.
- **A reply in a post's thread** goes to the **thread's** address, `.../conversations/{channel};messageid={root id}/messages`,
  with the `;` itself (not `%3B`): HTTP 201; read back with `rootMessageId` equal to the root's id, no subject, and in the
  thread (not a new root). The body is the same as a post's without a subject, with **no** reply-quote blockquote and no
  `properties.qtdMsgs`: a channel thread needs none. The body's own `conversationid` names the channel; the thread is only
  in the address. (The `%3B` form is untried: the literal one works.)
- **An edit that does not repeat the subject loses it.** `PUT .../conversations/{channel}/messages/{root id}` with
  `skypeeditedid` (what `Backend::edit` sends for a chat) replaces the content and **removes** `properties.subject`; the
  same edit with `properties.subject` restated keeps it. So `Backend::edit_post` takes the subject the post has now and
  puts it back (`None` for a post that has none sends the edit exactly as a chat edit goes).
- **A delete** (not used by vtteams, which has no delete): `DELETE .../messages/{id}?behavior=softDelete` leaves the message
  listed with empty content and `deletetime` set.

What vtteams checks before any request (nothing is sent when one fails): the channel id must be `19:<letters, digits,
-_.>@thread.tacv2` or `@thread.skype` (a chat, a meeting or a stream is refused, and an id with a `;` or `/` could name
another address), the thread id digits only, a subject trimmed, stripped of control characters and 1 to 200 characters, none
together with a thread, and the message not empty. Failures are classified as for a chat send: a refusal (4xx, throttling)
means nothing was delivered, a 5xx, a timeout or a dropped connection means it may have been (`Error::delivery_unknown`), and
a write is never sent again by the library. Tests: `teams::channels::tests`.

**Mentions of people in a channel.** A person mention built by the chat code (`teams::parse::teams_mentions`:
`<span itemscope="" itemtype="http://schema.skype.com/Mention" itemid="0">Name</span>` in the content plus
`properties.mentions`, a JSON string of `[{"@type":"http://schema.skype.com/Mention","itemid":0,"mri":
"8:orgid:<guid>","mentionType":"person","displayName":"Name"}]`) is **accepted and stored exactly as sent**, in a root post
(with a subject) and in a thread reply (the `;messageid=` address): the content equal to what was sent, `properties.mentions`
present with the same element and the `itemid`s agreeing, the span's attributes in the order itemscope, itemtype, itemid. So
`ChannelPost::mentions` and `Backend::edit_post`'s `mentions` go through the very function the chat send uses; each mention
is checked before any request (an object id of the GUID shape, a name of 1 to 200 printable characters, distinct numbers, a
matching `<at id>` tag in the message, at most 50). **A channel has no roster the application can read:**
`GET {chat}/v1/threads/{channel}?view=msnp24Equivalent` answers `members` with **one** entry (the signed-in user), both for a
channel of a 25-member team and for the General channel of a 4-member team, so `Backend::members` cannot name a channel's
people and is not asked about channels. The application offers the people who have **posted in the channel** (from its loaded
messages, `Msg::from_id` and `from_name`). `@channel` and `@team` mentions (`mentionType` "channel"/"team" with the channel's
or team's id as `mri`) are stored as sent, but what the client makes of them is unknown, so vtteams builds none.

**Unverified:** how the real client draws such a mention; a mention in an **edit** (the edit sends `properties.mentions` as a
chat edit does, and its read-back is untried); a mention of a person who is not in the channel; editing a **reply** (which
address, and what it needs repeated; vtteams refuses it); formatting variants; posting with attachments or a code card; what
the service does with a subject sent on a reply by some other client; and whether the id in the answer (`OriginalArrivalTime`)
equals the stored message's id for a reply as it does for a chat message (a post's read-back finds it by its client message
id; the application takes the answer's number as the id until the next read brings the stored copy).

**The list of teams.** `GET {csa}/api/v3/teams/users/me?isPrefetch=false&enableMembershipSummary=true`, where `{csa}` is
`regionGtms.chatSvcAggAfd` and the bearer is the chat service aggregator's own token (the one favourites use; the
skypetoken header goes along). It answers **200, about 3 MB, about 2 s** for about 30 teams with about 240 channels in all
(channels per team: from 1 to a few dozen, typically a handful). The `v1` path answers the same question with about 15 MB in
about 8 s, so **vtteams uses `v3` and the caller should ask rarely** (`Backend::teams`). The answer is an object with
`chats`, `conversationFolders`, `metadata`, `privateFeeds`, `teams` and `users`; only `teams` and the favourites in
`conversationFolders` are read.

- Team keys: `id`, `displayName`, `description`, `isArchived`, `isCollapsed`, `isFavorite`, `isFollowed`,
  `isGeneralChannelFavorite`, `isUserMuted`, `isDeleted`, `lastTimeFavorited` (a string, sometimes absent),
  `membershipSummary` (`totalMemberCount`, a number, and other counts), `teamSiteInformation` (`groupId`, ...),
  `teamSettings` (`generalChannelPosting` a number, `messageThreadingEnabled`, `channelMention`, `teamMention`, ...),
  `memberRole` (a number), `memberRoleString` and `channels`. The team's **`id` equals the id of its General channel** in
  every team; the ids end `@thread.tacv2` (about two thirds) or `@thread.skype` (the rest).
- Channel keys: `id` (roughly half end `@thread.skype`, the rest `@thread.tacv2`), `displayName`, `description`, `isGeneral`
  (true for one channel in each team), `isArchived` (a few), `isFavorite` (about a fifth), `isPinned` (a few), `isMuted`
  (none), `isFollowed` (a couple), `isShared`, `isMember`, `isModerator`, `channelType` (a number), `threadType` and
  `threadSubType` (strings), `parentTeamId`, `isMessageRead` and `isImportantMessageRead` (booleans; **there is no
  `isRead`**), `consumptionHorizon` (`clientMessageId` a string, `originalArrivalTime` and `timeStamp` numbers),
  `lastMessage`, `isEmptyConversation`, `lastL2MessageIdNFS` (a number), `version` and `membershipVersion`.
- A channel's `lastMessage` is an object on about half of the channels and absent or empty on the rest. Its names are
  **camel case**, unlike the conversation list's lower case: `id`, `composeTime` and `originalArrivalTime` (ISO strings),
  `content`, `from` (a contacts URL ending `8:orgid:<guid>`), `imDisplayName`, `messageType`, `parentMessageId`,
  `clientMessageId`, `sequenceId`, `version` and `fromDisplayNameInToken` (sometimes null).
- Fields can be null or absent; vtteams reads each as it finds it and counts a team or channel without an id or a name as
  unreadable (one warning for the read, never a failed list). It takes at most 500 teams and 1,000 channels to a team.

What vtteams makes of them (`teams/teams.rs`, `teams/parse.rs`):

- A team with `isDeleted` is left out; the rest are put in the person's own order (see the next item; nothing is sorted
  by `isFavorite`, which is not the favourite flag: see below). A team has no `channels` array when it has none. The
  application does not list teams with `isArchived` (see below) but keeps their data.
- **The order of the teams is a user property, not the list's order.** The real client orders the teams by the property
  `teamsOrder`: `GET {chat}/v1/users/ME/properties?name=teamsOrder`, the same request as the `favorites` property read (the
  ordinary spaces bearer and the skypetoken; **not** the aggregator's token). It answers an object with the keys `cid`,
  `cidHex`, `dogfoodUser`, `primaryMemberName`, `skypeName` and `teamsOrder`. `teamsOrder` is a **string holding JSON**: an
  array of `{"order": <number>, "teamId": "<team id>"}` (about as many entries as there are teams; element 0 has `order` 0).
  Most of those ids are teams in the v3 list and a few are stale (not in the list); about two in five of the teams are not
  in the order at all (a couple of those are the archived ones). A sibling property, `autoUnfavoriteTeams` (a JSON array of
  a few ids), exists and is not read. vtteams (`TeamsBackend::order_teams`, `parse::team_order` / `apply_team_order`) reads
  it with every read of the list (one more request, made once, never repeated), sorts the entries by `order` (a string
  holding a number is read as that number; an entry with no usable number comes after the numbered ones; ties keep the
  array's position), puts the teams it names first in that order, ignores ids that match no team, then lists every team not
  in the order alphabetically by name without regard to case. A team is never removed (the application hides the archived
  ones). It reads at most 1,000 entries. **If the property cannot be read** (any failure but a 404 or an `Unsupported`) or
  does not parse, the list is returned in the service's order with one warning ("the order of your teams could not be read,
  so they are shown in the service's order"); a 404, or an empty or missing `teamsOrder`, is an account with no order and
  keeps the service's order without a word. The log line has counts only. **Unverified:** the rule for teams that are not in
  the order (alphabetical is a **guess**; the real client's rule for them is not known), that `order` and the array position
  always agree (they appear to), and whether the real client shows the teams in this order but hides the stale ids, or shows
  something else for them.
- Inside a team: General first, then pinned channels, then the rest in the service's order.
- `unread` is `isMessageRead == false`, and never for an archived channel. **(unverified)** that this is what the official
  client counts.
- `last` is only taken when it has an id, a time and a chat message type (`Text`, `RichText/Html`, `RichText`); text is
  made one line and cut at 120 characters.
- **Unverified: the `channelType` mapping.** The mapping to standard, private and shared (0, 1 and 2) is a **guess from other
  clients**. `isShared == true` is taken as shared whatever the number, and an unknown number is standard. The first sighting
  of each `channelType` (with `isShared`) is written to the log at debug, as the drift log's `channel type` values, so the
  mapping can be settled by comparing with what Teams shows.
- **`isFavorite` is not the favourite flag, on a team or a channel.** About two thirds of the teams and about a fifth of the
  channels have `isFavorite: true`, while the official client shows only a handful of favourites (a few teams' General
  channels and a couple of other channels). The favourites are the **"Favorites" conversation folder**, which holds channels
  as well as chats: an item of thread type `space` is a team's root conversation (its id equals the team's General channel's
  id) and `topic` is another channel.
- **The list of teams already carries the folders.** The answer's `conversationFolders` is an object whose own
  `conversationFolders` is an array (a few folders) of the same shape the folders path (`GET .../conversationFolders`, see
  Favourites) gives: `conversationFolderItems`, `createdTime`, `folderType`, `id`, `isDeleted`, `isExpanded`, `name`,
  `sortType`, `version`; the object also has `conversationFolderOrder`. The folder with `folderType == "Favorites"` that is
  not deleted holds the items in the person's order, each with `conversationId` and `threadType`. So reading the teams needs
  no second request for favourites; without that folder there are none. vtteams reads it with `parse::favourite_channels`
  (at most 500 items, a repeated id keeps its first place, chats are counted and left to the chat list) and sets
  `Channel::favorite` (a favourite team root goes to its General channel).
- **Archived teams are not shown by the official client.** A couple of the teams have `isArchived: true` and the person does
  not see them in the client. vtteams lists no archived team.
- **Unverified: whether `isFavorite` on a team means "not hidden".** About a third of the teams (including the archived ones)
  have it false; it is not known whether the client hides those, so vtteams does not filter on it.
- **Unverified: what `isFavorite` on a channel means.** About a fifth are true. It is believed to be "shown under its team"
  in the official client (the others being hidden until asked for), which is why `Channel::shown` is `isFavorite` or
  `isGeneral`.

**The conversation list holds only some of them.** The plain list (`conversations()`, what the chat list reads) holds
**about a sixth of the teams and channels** (the recently active ones). Entries: `threadType` `space` with
`productThreadType` `TeamsPrivateChannel` (a few) or `TeamsTeam` (about twenty: a team root, whose id is the General
channel's, so it is that channel's conversation), and `topic` with `TeamsStandardChannel` (about twenty). `threadProperties`
carries `productThreadType` and `groupId`, and only **sometimes** `spaceId`, `spaceThreadTopic` and `topicThreadTopic`, so
the hierarchy and the names cannot be derived reliably from it. `properties.consumptionhorizon` is on most; `lastMessage` has
`id`, `composetime`, `originalarrivaltime`, `content`, `from`, `imdisplayname`, `messagetype`, `conversationLink` and
`conversationid`, and `rootMessageId` on only some.

**Freshness comes from the conversation list.** The list of teams costs about 3 MB and seconds; the conversation list is
polled anyway and is incremental, so `chats()` records what it says about the entries it does not show as chats
(`threadType` `space`/`topic` or a `spaceType`, with `productThreadType` `TeamsTeam`, `TeamsStandardChannel` or
`TeamsPrivateChannel`) as `ChannelActivity` (`Backend::channel_activity`: id, unread, newest message; no request, at most
2,000, replaced whole by each read). `unread` is the chat rule (newest message after the read marker, not yours, not
automated), so one rule serves chats and channels. The application merges that over the list of teams. The chats the list
shows are unchanged: no team root and no channel is a chat.

**Reading a channel** is the same call as a chat: `GET {chat}/v1/users/ME/conversations/{channelId}/messages?pageSize=50&view=msnp24Equivalent`.
50 a page, newest first, **posts and replies mixed**. Every message has `rootMessageId`: its own id for a post, the post's id
for a reply (`Msg::root_id` is set only for a reply). Only a post has `properties.subject` (about 60% of posts have one;
replies never); some messages have `postType`. `_metadata` has `backwardLink` (it works for older pages: 50 older messages,
no overlap, identical to `startTime=1&endTime=<oldest composetime ms - 1>`), `syncState` (not used) and
`lastCompleteSegmentStartTime`/`EndTime`. The incremental read (`startTime=<newest version - 5000>`) returns just what
changed (the newest message comes back). `properties.mentions` has the chat shape. `ThreadActivity/AddMember` and
`DeleteMember` system messages appear in the feed. A thread read, `GET .../conversations/{channelId};messageid={rootId}/messages`
(with a literal `;` or with `%3B`), returns the post and its replies; vtteams does not use it. Tests:
`a_channels_posts_and_replies_are_read_like_a_chats_and_an_incremental_read_keeps_the_threads`,
`only_a_reply_names_the_post_it_belongs_to`.

**Push.** A push frame's `conversationLink` or `conversationid` for a reply in a channel is **believed** to be
`<channel id>;messageid=<root id>` **(unverified)**. The push hint cuts everything from the `;` (or `%3B`), so it names the
channel, which is what is polled (`a_reply_in_a_channel_names_the_channel_not_the_thread`). Whether a channel's ordinary new
post arrives as a push, and with which id, is unverified.

**Not done:** editing a reply, `@channel`/`@team` mentions or attachments in a channel, deleting, reading one thread by itself,
channel membership and roles, the other kinds of channel (shared channels from another tenant may come with another
`productThreadType`), and reading the teams of an account whose aggregator scope is withheld (that read fails alone, with the
same typed errors the favourites get).

### Message search (one chat, or the channels of a team)

Searching the messages of ONE chat, or of several conversations at once (the channels of a team), on the server, so a client
need not fetch and scan every message. Code: `crates/teams-client/src/teams/search.rs`, `TeamsBackend::search_messages`.
Structure only is recorded here, never text. What is not marked **(unverified)** has been observed on a real tenant.

- **Request**: `POST https://substrate.office.com/searchservice/api/v2/query`, `Authorization: Bearer <Substrate token>`
  **(unverified)** for this exact URL: another URL, `https://substrate.office.com/search/api/v2/query`, is also in use by
  other clients, and which one a tenant answers on is open (the URL is one constant, `SEARCH_URL`). The token is the one
  people search already uses (scope `https://substrate.office.com/.default`); it goes to that host only. No skypetoken and no
  chat-service headers are sent; `Origin`/`Referer` are those of `teams.microsoft.com`, as for people search (unverified).
- **Body** (field names exactly; the service is reported to accept both `EntityRequests` and `entityRequests`, unverified):

  ```json
  {"cvid": "<fresh random uuid>", "scenario": {"Name": "powerbar"},
   "EntityRequests": [{
     "entityType": "Message", "contentSources": ["Teams"],
     "fields": ["Extension_SkypeSpaces_ConversationPost_Extension_FromSkypeInternalId_String",
                "Extension_SkypeSpaces_ConversationPost_Extension_ThreadType_String"],
     "propertySet": "Optimized",
     "query": {"queryString": "ClientThreadId:\"<chat id>\" AND \"<phrase>\" AND NOT (isClientSoftDeleted:TRUE)",
               "displayQueryString": "\"<phrase>\""},
     "from": 0, "size": 25}]}
  ```

  - `ClientThreadId:"<id>"` limits a search to one conversation. Another field, `clientconversationid:"<id>"`, is reported to
    work too (unverified). The field name is one constant (`THREAD_FIELD`).
  - `from` is an offset into the results and `size` the page length. 25 is used; other clients use 25 to 50 and the service's
    limit is not known, so nothing larger is sent.
  - `NOT (isClientSoftDeleted:TRUE)` leaves out deleted messages (unverified).
  - **The phrase is untrusted, and no backslash escape is relied on.** What a person types becomes the text of one quoted
    string in a query whose scope clause is fixed text, so it cannot add an operator, name another chat or end its quotes
    (an unparenthesised `OR` can escape the scope). Whether the service reads `\"` and `\\` inside a quoted string as escapes
    is **(unverified)**, so every double quote and every backslash typed is treated as a separator between words, like
    whitespace (tabs and line breaks too), and runs of them shrink to one space: nothing the person typed can then end the
    quotes whatever the service does. **What is lost: an exact search for punctuation that is a quote or a backslash**
    (`say "hi"` is searched as the phrase `say hi`; a path `C:\dir` as `C: dir`). The helper that quotes the phrase still
    escapes `\` and `"`, but has nothing to escape in practice. `displayQueryString` is described as display-only; it is sent
    as the same quoted phrase in case the service reads it (unverified). A phrase of nothing but quotes, backslashes and
    whitespace is refused ("empty") before any request, as is one over 200 characters (counted after this change), or with
    another control character, and a chat id over 200 characters or holding a quote, a backslash or a control character (real
    ids: `19:...@thread.v2`, `19:<guid>_<guid>@unq.gbl.spaces`, `48:notes`). Relax the rule only on a clear "escapes
    HONOURED" from a read-only experiment: a phrase followed by an escaped quote and a junk word inside one quoted string,
    against its control with an unescaped quote.
- **Answer**: `EntitySets[]` -> `ResultSets[]` with `Total` (approximate), `MoreResultsAvailable` and `Results[]`. A result:
  `Id` (the search hit's own id, **not** the message id), `Source` with `InternetMessageId` (the chat service's message id:
  other clients' fixtures show both a number and a string, so both are read and the digits-only form is required),
  `DateTimeSent` (ISO 8601), `Preview` (plain-text excerpt) and the sender in `From` or `Sender` (`EmailAddress.Name` or
  similar); a `HitHighlightedSummary` with `<c0>..</c0>` tags may stand in for the preview. U+E000 and U+E001 bracket the
  matched words in `Preview` (unverified); both kinds of marker are removed, and so is the rest of the private-use range,
  because the application uses private-use characters for its own markers.
  - `Properties.HasParseException == true` is treated as an error, not as "no hits" (where it sits, on the answer, the entity
    set or the result set, is not known, so all three are checked). **`EntitySets[].IsPartial` is `true` on ordinary
    answers**, one that found its message and one that found nothing, both with no parse exception, so it is ignored.
  - **Channels**: `ClientThreadId:"<channel id>"` finds a message in a channel, including a thread reply (the hit's
    `Source.ClientThreadId`/`ClientConversationId` carries `...;messageid=<root>`, which the reader cuts off): about twenty
    hits in one channel.
  - **An OR of a team's channels**: `(ClientThreadId:"a" OR ClientThreadId:"b" ...) AND "phrase" AND NOT
    (isClientSoftDeleted:TRUE)` with all seven channels of one team answers status 200, no parse exception, about twenty hits
    from 2 distinct channels (nearly all in one), every label inside the team, newest first (descending `DateTimeSent`), one
    page. That is the form `Backend::search_conversations` sends (one id is the plain form above, without parentheses). The
    order is newest first on these answers; the documentation does not promise it, so nothing sorts or relies on it. A
    channel hit's `Source` also has `ChannelName`, `TeamName`, `ConversationTopic`, `Subject` and `AnnouncementTitle`
    (strings, present on the hits seen); `ChannelName` is what vtteams labels a team-search row with (cleaned, one line, 80
    characters).
  - **Unverified for a team-wide search**: an OR of more than seven ids (a team can have a few dozen channels; vtteams covers
    at most 50 ids and leaves the rest out, the open channel first), paging past the first page of a team-wide search
    (`from` is passed as for one chat), and how a large team behaves.
  - **The ids of a scope are untrusted like the phrase.** Each is checked before it goes between quotes (not empty, at most
    200 characters, no quote, backslash or control character) and one bad id refuses the whole call with nothing sent; ids
    repeated (ASCII case aside) are searched once. The query has fixed text outside its quoted strings, so neither a phrase
    nor an id can add an operator or name a conversation that was not asked for.
  - An answer with no `EntitySets`, or a result set with neither `Results` nor a `Total` of 0, is an error, not zero hits, so
    a changed format cannot read as an empty search.
  - A hit is checked against the conversations asked about: `Source.ClientThreadId`, else `Source.ClientConversationId` with
    any `;messageid=...` suffix cut off, compared ASCII case-insensitively with the requested ids. A hit naming another
    conversation is left out and counted, and a warning says so once per search ("some search results belonged to another
    chat and were left out"; the number is in the debug log line); a hit naming none is kept when one conversation was asked
    about (it is that one), and left out as unreadable when several were (which of them it is cannot be known; it is counted
    with the other unreadable results). A kept hit says which conversation it is in, in the caller's spelling of the id. The
    labels are on the answers seen; that every hit has one, in which spelling, and whether the service ever returns another
    chat's hit for a scoped query are unverified.
  - A result without a usable message id or time is left out and counted (a warning says so once); the page is still
    returned. The next page starts at `from` plus the number of results the service sent, whether or not all could be read or
    belonged to this chat, and only when `MoreResultsAvailable` is true and the page was not empty.
- **Order** is the service's; it is not re-sorted. The answers seen (a channel, and a team of seven channels) are newest
  first; whether that holds for ranking by relevance elsewhere is unverified.
- **Errors**: 401/403/429/5xx go through the usual status mapping (429 is `Throttled`); a 401 renews the Substrate token
  once. A tenant that withholds the Substrate scope fails the token request, and only this feature (and people search) is
  affected: the browser recovery is for the spaces scope alone.
- **Logging**: only counts and timings (results, how many were read, the total, milliseconds) and the names and JSON types of
  new fields (`Watch`: "search answer", "search entity set", "search result set", "search result", "search result source").
  The phrase, a hit's text or sender and the chat id (except as `id_tag`) are never logged; the exchange is also handed to
  `--record-shapes`, masked like every other call.
- **Unverified**:
  - Which URL (`searchservice/api/v2/query` or `search/api/v2/query`) and which scope spelling (`ClientThreadId` or
    `clientconversationid`) a tenant needs, and whether the request key's case matters.
  - Whether a backslash escape of a quote or backslash is honoured inside a quoted phrase; until it is, quotes and
    backslashes in what is typed are turned into spaces.
  - Whether hits always carry `ClientThreadId` or `ClientConversationId`, in what form (a `;messageid=` suffix, another case
    or an encoded id), and whether any hit from another chat ever appears.
  - Whether one-to-one chats are searchable in the same way as group and meeting chats.
  - How long a new message takes to be indexed (a message sent a moment ago may not be found yet; nothing here tries to make
    up for that).
  - Rate limits: whether the service throttles a person who types a query and pages through the results.
  - Whether the Substrate token vtteams already holds carries the search scope, or the service wants a token of its own; a
    refusal (403, or `invalid_scope` on the token request) would say so.
  - Whether `InternetMessageId` is always a number or always a string, whether the ids are the digits-only form the chat
    service uses (`Source.InternetMessageId` against `Msg::id`), whether a hit's message can always be found in the chat (the
    application scrolls to it by id), and which of `From`/`Sender` carries the sender.
  - Whether the page limit is 25, 50 or something else, and whether `Total` is exact.

### Message HTML as stored

The service stores the HTML it is given, plus a line break (`\r\n`) after block elements. Details that matter:

- Formatting: `<i>`, `<strong>`, `<s>` (strikethrough), `<u>` (underline), `<span style="color:#B6424C">` (text colour),
  `<span style="background-color:#9DD9DB">` (highlight), `<ul>/<ol>/<li>`, `<blockquote><p>`.
- Mentions: `<span itemscope itemtype="http://schema.skype.com/Mention" itemid="n">Name</span>` plus `properties.mentions`
  (JSON string). Reading is observed; sending is **(unverified)**. **The attributes come in any order**: a message can have
  `itemtype` before `itemscope` (`<span itemtype="..." itemscope="" itemid="0">Name</span>`), the opposite of the example
  above. `teams::parse::normalize_mentions` therefore reads `itemtype`/`itemid` as an order-independent pair of attributes
  (`attr_value`), as `convert.rs`'s own handlers read `style`/`color` off arbitrary elements; matching the whole opening tag
  as one fixed string would let such a mention fall through as a plain, uncoloured `<span>`. (The outgoing replace in
  `teams_mentions` writes the tag in one fixed order and is unaffected.)
- **How the Teams client draws a mention**: a mention of the signed-in user is **bold and red**; a mention of anyone else is
  a plain blue, link-styled name (with a hover profile card, which vtteams has no equivalent of). vtteams tells them apart
  per mention, not per message.
- **A reply's quote**: `<blockquote itemscope itemtype="http://schema.skype.com/Reply" itemid="<ms>">
  <strong itemprop="mri" itemid="<sender id>">Name</strong><span itemprop="time" itemid="<ms>"></span><p
  itemprop="preview">...</p></blockquote>`. The time `<span>` is **empty** as stored: the real client writes readable text
  into it at render time from `itemid` (the quoted message's own arrival time in ms), as `M/D/YYYY HH:MM` in local time,
  absolute rather than "Today"/"Yesterday". vtteams does the same when it draws one.
- Emoji: `<emoji alt=...>` or `<img itemtype=".../Emoji" ...>`.
- Images: hosted on `*.asm.skype.com` and, in newer tenants, on the async gateway `<region>-prod.asyncgw.teams.microsoft.com`
  (same `/v1/objects/<id>/views/...` paths); vtteams treats any https subdomain of `teams.microsoft.com` as a media host,
  fetched with `Authorization: skype_token <token>`. **(unverified)**: the fetch itself, and for the gateway.
- A pasted or shared picture or file is instead uploaded to the sender's OneDrive and arrives as a file attachment
  (`properties.files`, `fileType` the bare extension, `objectUrl`/`fileInfo.fileUrl` a `*.sharepoint.com` URL), not an inline
  `<img>`. Fetching it needs a Microsoft Graph bearer (a tenant may withhold that scope; vtteams's own refresh token already
  covers it, as for the spaces/CSA/Substrate scopes): Graph's `GET /v1.0/shares/{id}/driveItem` (`id` = `u!` + unpadded
  URL-safe base64 of the UTF-8 attachment URL) resolves it to metadata including `@microsoft.graph.downloadUrl`, itself
  already authenticated and sent no credential of ours. (The Teams web backend asks for a Graph-scoped token only when a
  message actually has such an attachment, and sends it with `Authorization` alone, none of the skypetoken, `Origin` or
  `Referer` headers its other calls carry, since Graph is a separate, official service.) Which hosts that URL points to is
  not known (and not listed in the client), so it is fetched only if it is `https`, names a host that is a DNS name (not an
  IP address, not `localhost` or another name meaning this computer or a private network), has no user name and the default
  port, and every redirect it follows has to be the same. Other open-source clients resolve a SharePoint attachment this way
  too, one of them as a fallback behind a more direct, more fragile internal SharePoint endpoint. **The Graph call itself is
  (unverified).** The driveItem answer is expected to carry `sharepointIds.tenantId`, which vtteams compares with the
  signed-in user's own tenant (the Teams session's stored tenant; for Graph, the `tid` claim of the access token) so that
  only pictures from the user's own tenant are drawn without being asked for **(unverified)**: that Graph returns that field
  and that it matches the sign-in's tenant. A tenant that withholds the Graph scope (as restricted clouds do) fails only that
  one picture.

### Code blocks

Not a logical element: the client writes HTML that it draws as a card, and the service stores what it is given.

- **What the Teams client writes**:
  `<p itemtype="http://schema.skype.com/CodeBlockEditor" id="x_codeBlockEditor-GUID">&nbsp;</p>` then
  `<pre class="language-rust language-was-manually-selected" itemid="codeBlockEditor-GUID"><code>...</code></pre>`,
  spaces `&nbsp;`, lines `<br>`, tokens in `<span class="hljs-keyword">` etc.
- **Rendering**: a card with the language in its header, line numbers and a copy button. Without the marker paragraph it is a
  numbered box with no header. A language Teams does not list (e.g. `objectscript`) shows as "Text". Mobile shows the card
  unrendered until tapped.
- **What the service rewrites**: `<codeblock class="x">` (the form Microsoft Graph's docs show) becomes a bare
  `<code>...<br>\r\n...</code>` with the class gone, even for `rust`. HTML comments and `class`/`data-*` attributes on `code`
  are removed. `<pre>` forms are stored as sent; ids get an `x_` prefix.
- **Colour**: the stylesheet colours six classes only: `keyword` indigo, `string` green, `function` red, `comment` grey,
  `addition` green, `deletion` red (a card of every class in highlight.js's scope list shows this). An inline
  `style="color:#rrggbb"` / `background-color` on a span is stored **and drawn**, beating the class; `<font color>` is stored
  but not drawn; bold does not show.
- **So vtteams sends its own colours**: `<span class="hljs-keyword" style="color:#a266df">` for each class that has one
  (`highlight::class_colour`; the class stays, as the client writes it). The palette has equal luminance, about 3.8 : 1
  against the light theme's white and the dark theme's `#292929`, the best one colour can do for both; variables, parameters,
  operators and punctuation stay plain. The language rides in the `pre` class, which the service keeps, so a card sent from
  here reads back as the same fenced block. It displays correctly in the Teams desktop app (Rust and ObjectScript cards) and,
  after a tap, on mobile (which first shows the card unrendered, as for blocks Teams itself sent). Dark mode is
  **(unverified)**.
- Teams' language menu (unverified, from another client): plaintext, bash, c, cpp, csharp, css, dart, dockerfile, dos, go,
  graphql, html, http, java, javascript, json, jsp, jsx, kotlin, kql, latex, lisp, markdown, objectivec, octave, perl, php,
  powershell, python, r, ruby, rust, scala, scss, shell, sql, swift, typescript, vbnet, vbscript, verilog, vhdl, xml, yaml.
- vtteams sends the card form with inline colours (`highlight::class_colour`) and reads any `<pre>`, `<codeblock>` and bare
  `<code>` containing `<br>` as a fenced block. See `convert.rs`.

## 3. Writing

| Operation | Request |
|---|---|
| Send | `POST {chat}/v1/users/ME/conversations/{id}/messages`, `Authorization: Bearer <IC3-scoped token>` only (no skypetoken, no `Origin`/`Referer`), with `{id: "-1", type: "Message", conversationid, conversationLink, from, fromUserId, composetime, originalarrivaltime, content, messagetype: "RichText/Html", contenttype: "Text", imdisplayname, clientmessageid, callId: "", state: 0, version: "0", amsreferences: [], crossPostChannels: [], properties: {mentions, formatVariant: "TEAMS", cards, files, links, importance: "", subject: "", title: "", onbehalfof: null, policyViolation: null, qtdMsgs?}}`; `qtdMsgs` only on a reply (see "Reply quotes" below). The answer carries `OriginalArrivalTime`. HTML is sent as given. |
| New 1:1 | The conversation need not exist: if the first send gets 403/404 it is created with `POST {chat}/v1/threads` (`members` with `role: Admin`, `properties: {threadType: "chat", fixedRoster: true, uniquerosterthread: true}`) and the send is retried. **(unverified)** |
| People search | `POST https://substrate.office.com/search/api/v1/suggestions?scenario=powerbar` with the Substrate token; `EntityRequests[{Query, EntityType: "People", Fields}]`. This works. vtteams also asks for `Phones` and `OfficeLocation`; whether the service returns them, and the shape of `Phones` (read as a list of numbers or of `{Type, Number}`), is **(unverified)**. A 400 to the request with them is taken as a refusal of those fields: asked again at once without them, and not for the rest of the session. |
| Edit | `PUT {chat}/v1/users/ME/conversations/{id}/messages/{messageId}` with `{content, messagetype, contenttype, skypeeditedid: <messageId>, imdisplayname}`. Answer: 200 with an empty/`null` body. See "Edits". |
| A message sent several times | Several messages in one chat from one sender can have identical content and one shared `clientmessageid`, each a record of its own: ids, `version`, `sequenceId`, `composetime` and `originalarrivaltime` all differ (arrival times can be tens of seconds apart). The Teams client shows it once. A client resending after a failure to hear back is the likely cause; the web client appears to show messages with one client message id once. vtteams shows the first and leaves the rest out (`without_resends`). |
| Reaction keys on a message you read | `properties.emotions`: an array of `{key, users: [{mri, time, value}]}` (a JSON string or the array itself). The key is one of the classic words (`like`, `heart`, ...), the code points in hex and a name (`1f37b_clinkingbeermugs`, `1f468-200d-1f4bb_manTechnologist`, decoded locally), or a **plain word that is an id in the emoji catalogue and cannot be derived from the emoji** (`salute` for 🫡, `fire` for 🔥, `launch` for 🚀; another id for the same emoji as a classic one, such as `yes` for 👍, also occurs). Shown as text until the catalogue turns it into an emoji. |
| Reaction add / remove | `PUT` / `DELETE` `.../messages/{id}/properties?name=emotions` with `{"emotions": {"key": ..., "value": <ms>}}` (key only on removal). Key: `like`, `heart`, `laugh`, `surprised`, `sad`, `angry`, or the id from Teams' public emoji catalogue. An id Teams does not know is stored and shown to everyone as a broken image, so vtteams refuses it first. Message id must be digits. **(unverified)** |
| Mark read | `PUT .../conversations/{id}/properties?name=consumptionhorizon` with `{"consumptionhorizon": "<ms>;<ms>;<message id>"}` (digit-only message id, the shape the service sends back). **(unverified)** |
| Delete | Not implemented. `properties.deletetime` marks a deleted message (unverified). |

### Edits

Editing a sent message:

- The `PUT` returns 200 with a `null` body. The message keeps its `id`, `originalarrivaltime` and `composetime`.
- `content` is replaced. `version` changes (it was the id; it becomes the edit time in ms), and
  **`properties.edittime` appears, a number of milliseconds since the epoch equal to the new `version`**. A message also
  gains the field `prioritizeImDisplayName`.
- A reaction changes `version` too but not `properties.edittime`. So "edited" is `properties.edittime`, not a version that
  differs from the id.
- Editing is by the sender only; what the service does for someone else's message is untried.
- **`properties.edittime` is not always a real, later edit**: a message that was never touched after sending can have
  `properties.edittime` set to a value a few minutes *before* its own `originalarrivaltime`/id, with `version` still equal to
  that same id (unlike a genuine edit, where `version` becomes the edit time). Such a message also has `properties.draftId`
  and a second, inner `properties.composetime` distinct from the outer, top-level `composetime`; `properties.edittime` lines
  up almost exactly with the inner one, both a few minutes before the outer `composetime`/`originalarrivaltime`, which
  suggests it is the time the draft was last saved before being sent, not a revision made afterwards. `parse_msg` therefore
  treats `edittime` as a real edit only when it is strictly after `created`, since an edit cannot happen before the message
  existed (`Msg::edited`); otherwise a never-edited message would be drawn with "(edited HH:MM)" and a time before its own
  timestamp.

### Reply quotes

A reply is a message whose content starts with the `schema.skype.com/Reply` blockquote (see "Message HTML as stored"). For
the Teams client to treat the quote as verified (no "This quoted message isn't verified by Teams" badge with an info icon),
the request has to match what the real client sends:

- The full `properties` shape every genuine message carries: `cards: "[]"`, `files: "[]"`, `formatVariant: "TEAMS"`,
  `links: "[]"`, `mentions: "[]"` (each present on every message, reply or not), plus `importance: ""`, `subject: ""`,
  `title: ""`, `onbehalfof: null`, `policyViolation: null`, and the top-level `callId: ""`, `state: 0`, `version: "0"`,
  `amsreferences: []`, `crossPostChannels: []`. A stored copy never shows some of these (the service strips or never stores
  them), so the stored copy of a message cannot be used to see what was sent: only the request can.
- **`properties.qtdMsgs`**, a second, structured reference to the quoted message, independent of the blockquote's own HTML
  attributes: `"qtdMsgs":"[{\"messageId\":\"<id>\",\"sender\":\"8:orgid:<guid>\",\"time\":<id as a number>}]"` (a JSON
  array embedded as a string, the same convention `properties.mentions` uses). `messageId` and `time` are both the quoted
  message's own id (Teams message ids are their arrival time in milliseconds, so `time` is numerically that id again);
  `sender` is the quoted message's sender's full MRI (`8:orgid:<guid>`). `teams::parse::qtd_msgs_json` builds it.
- **An IC3-scoped bearer.** The real client's own bearer for the chat service `POST` has `"aud":"https://ic3.teams.office.com"`,
  not `https://api.spaces.skype.com`, the audience `TeamsBackend::request`/`request_with` use for every other chat call. A
  token for that scope (https://ic3.teams.office.com/.default) is minted from vtteams's own stored refresh token (no new sign-in: same client id, already
  consented) and carries `given_name`/`family_name`; the service then adds `fromDisplayNameInToken`,
  `fromFamilyNameInToken` and `fromGivenNameInToken` to the stored message itself (they are never in a request body), as it
  does for a genuine message. The token alone as `Authorization` (no skypetoken `Authentication` header, no
  `Origin`/`Referer`) is accepted: `201 Created`. `send` does exactly this (`auth::IC3_SCOPE`, `TeamsBackend::ic3_token`,
  `TeamsBackend::send_message`), falling back to the ordinary bearer if an IC3-scoped token cannot be had at all. Other
  header combinations are accepted as well (a plain send with only the skypetoken `Authentication` header works), so no one
  of them is required by the service.
- The other headers `send_message` sends, matching the real client's: `Referer: https://teams.microsoft.com/`,
  `X-Ms-Request-Priority: 0`, `BehaviorOverride: redirectAs404`, `X-Ms-Test-User: False`, `X-Ms-Migration: True`, `DNT: 1`.
  The real client's `clientinfo` header (`os=...; osVer=...; proc=...; lcid=...; deviceType=1; country=...;
  clientName=skypeteams; clientVer=...; utcOffset=...; timezone=...`) is **not** sent: it is an explicit claim to be a
  particular other client and build, and the service does not need it. vtteams's own `User-Agent` is the honest
  `teams-client/<version>`.
- The quoted name is the sender's real name and id, including for your own message (your real name, not the "You" the
  transcript shows only to you).

**Notes-to-self never verifies.** In the notes-to-self conversation (`48:notes`, `threadType: streamofnotes`, no second
party and no `members`) the "isn't verified" badge appears on every reply, whatever the request looks like (the identical
badge shows in Teams for the web as in the desktop app). In an ordinary 1:1 or group chat a reply built as above shows no
badge. So reply quotes are never judged in that conversation. **Unverified:** whether a reply sent to notes-to-self still
shows the badge now.

## 4. Behaviours worth knowing

- Throttling is HTTP 429 with `Retry-After`; vtteams holds polling for that long (default 30 s, at most ten minutes).
- Message and list answers are large (about 600 kB for the full list), hence the incremental reads.
- The cloud is commercial only (authz host hard-coded).
- Unknown fields, types and message types are logged by name only (`Watch`), so protocol drift shows up as a log line.
- `--record-shapes DIR` writes the masked structure of every exchange: the way to see what a tenant really sends.
- **Connections.** A few times a day a request on a pooled keep-alive connection fails at once with a reset, after many
  successful requests on that connection. The service gives no warning: an unauthenticated request to
  `teams.microsoft.com/api/chatsvc/<region>/...` (HTTP/1.1, answered 401 by the Azure Front Door edge, which labels its
  answers `X-MSEdge-Ref` and `X-Cache`) carries no `Keep-Alive`, `Connection` or `Expires` header, and a second request
  reuses the first connection. `Cache-Control: no-store` is sent, which is about caching, not connection lifetime. Those are
  the edge's own 401s, not authenticated 200s: `--log-headers` shows the headers of real answers. No idle timeout, maximum
  connection age or request limit for the chat service is documented or measured; what other clients have are their own
  choices (a 30 s client-side idle age, Trouter pings every 25-30 s with `timeout=40` in the socket.io query) and, on
  Trouter, per-request answer deadlines (about 12 s and 117 s), which are not connection limits. Clients that retry after a
  transport failure retry reads only; a message create is not retried (no idempotency key, and the answer may be lost
  although the post was accepted). The failure shows as a poll that fails after about 13 ms (too fast for a new connection)
  on a connection that had carried a request every four seconds; on Windows it reads "An existing connection was forcibly
  closed by the remote host (os error 10054)". `ureq` 3.5 tolerates a close only while reading a body, not while sending the
  request or waiting for the first bytes of the answer.
- **Name lookups** (`fetchShortProfile`, for titling group chats and naming senders). When the directory is failing, the
  service's own Substrate backend answers 500 for **every id, even asked alone** (the error text names
  `IsVisibilityAllowedS2SRequest`), after its own 10 s limit (`500 in 10100 ms`); a batch with one such id fails as a whole,
  so halving a failing batch finds the ids that can be looked up. A lookup that has to split takes about a minute, which is
  why the application never waits for it. A **throttling** service answers every request of a burst with a refusal (four
  within 250 ms at start-up, and again on the next refresh), so a burst of parallel lookups is worse than one request first
  and the rest only if it was not refused.
- **Pastes and terminal replies on the Windows console** (not the Teams API; recorded here because `crossterm` hides it).
  Observed in Windows Terminal:
  - A paste of 1,003 characters arrives as 2,007 key events (a press and a release for each character, plus one stray
    release) in 245 pieces of 1-65 characters a millisecond or two apart (a line, an Enter, the next line). The
    bracketed-paste markers (`ESC [ 200 ~` / `ESC [ 201 ~`) are not among them, although Windows Terminal, ConPTY and conhost
    support bracketed paste once an application asks with `?2004h`, which the application does: the console does not hand
    them to a program that reads key records. crossterm tracks this (crossterm-rs/crossterm#737; PR #1030 may fix it). Other
    editors work around it by switching the console to virtual-terminal input, which needs a Win32 `SetConsoleMode` call
    (`unsafe`, which this repository forbids).
  - Replies to the start-up queries (`vtteams --terminal-report` shows them) arrive as console key events with the ESC of
    each reply lost or delivered as Ctrl+`[`; the 62 bytes of a full set of replies are rebuilt from them
    (`probe::bytes_from_keys`). Windows Terminal answers `CSI 16 t` (`6;20;10`, i.e. a 10x20 cell), `CSI 14 t`, `CSI 18 t`,
    DA1 (`?61;4;6;7;14;21;22;23;24;28;32;42;52`, attribute 4 = Sixel) and mode 2026 (`?2026;2$y`: supported, currently off)
    within 3-65 ms. It does **not** answer XTSMGRAPHICS (`CSI ? 2;1;0 S`, `CSI ? 1;1;0 S`). The console API's font size is a
    fixed 0x16 under ConPTY and says nothing about the real cell. Sixel pictures draw with `--images sixel --cell-pixels
    10x20`.

## 5. Microsoft Graph

Not explored with a session of its own. What is known is from Microsoft's documentation and other open-source clients, so
all of it is **(unverified)** against a live tenant:

- Code blocks: the docs show `<codeblock class="Json"><code>...</code></codeblock>` for sending and as read back, with the
  supported languages listed on the `chatMessage: post` page. A third-party Graph client reports that Graph also accepts and
  returns the editor-card structure above and keeps a `<codeblock>` fallback for tenants that reject it. vtteams sends
  `<codeblock>` to Graph (`CodeBlockStyle::Codeblock`); switching is a one-line change once someone can try it.
- Edits: `PATCH /chats/{id}/messages/{id}`, with `lastEditedDateTime` on the message.
- Reactions: `setReaction` / `unsetReaction` with `reactionType`, the classic six.
- Sign-in is the device-code flow with a configurable public client id; whether a tenant accepts the default id for
  `Chat.ReadWrite` is not known. Chats: `GET /me/chats?$expand=members,lastMessagePreview&$orderby=lastMessagePreview/
  createdDateTime desc` (with `viewpoint`) is expected to return what `parse_chat` reads; `Chat.updated` is the later of
  `lastUpdatedDateTime` and the newest message's time. Message HTML (`<at>`, hostedContents `<img>`) and the top-level
  `mentions` array are as documented. The id in Graph's `<at id>` is a small index, not an object id, so vtteams does not
  try to tell a mention of you from another (always the "someone else" colour on Graph).
- The same third-party client reports that Graph writes the code card as `class="language-X skipProofing"` plus
  `spellcheck="false"` on the `pre` and picks the language from Teams' menu names (`teams_language` maps short labels such as
  `ts`, `py`, `sh`).
- Search: Graph's Search API is global, with no documented way to restrict it to one chat, so there is no in-chat search
  through Graph.
- A SharePoint attachment is resolved with the Shares API using a token for the scopes the Graph sign-in asks for
  (`graph::auth::SCOPES`); Microsoft may require a Files or Sites permission for that, in which case those pictures stay a
  placeholder. The scopes are not widened, because asking for more could stop sign-in on tenants that cannot consent.
- Everything in vtteams's Graph path is covered by the scripted test server only.

Results of any live Graph session belong here, marked the same way.
