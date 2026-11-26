# teams-client

A blocking Rust client for Microsoft Teams chats. [vtteams](../../README.md) is built on it, and any other project can use
it the same way. MIT licensed.

> **Disclaimer.** The `teams` backend is unofficial and unsanctioned. Microsoft has not published that API and does not
> support it, and this client presents itself to it as Microsoft's own Teams web client. Using it may breach Microsoft's
> terms and an organisation's policies and could get an account restricted. It is provided for interest and research, is
> not recommended, is used entirely at your own risk, and may stop working at any time. The `graph` backend is the
> official, supported way. The `teams` backend builds on [msteams-mcp](https://github.com/m0nkmaster/msteams-mcp) (MIT);
> see the [main README](../../README.md#acknowledgements).

Two backends implement one `Backend` trait:

| | `graph` | `teams` |
|---|---|---|
| Service | Microsoft Graph (documented, official) | The web API the Teams web app uses (**unofficial, unsanctioned**) |
| Sign-in | Device-code flow | The user's own Edge/Chrome, driven over DevTools |
| Needs | An Azure app registration the tenant allows | Nothing registered; a Chromium-family browser (Edge, Chrome or Chromium) |
| Stability | Stable | May change without notice |

The `teams` sign-in needs a Chromium-family browser because it reads the signed-in page's storage over the DevTools
protocol; Safari and Firefox cannot be driven that way. The browser only has to be installed (`BrowserOptions` can name
one), not to be the default. Why Safari was not built, and what it would cost, is in the
[protocol notes](../../docs/teams-web-protocol.md#safari-and-firefox-cannot-be-driven-considered-not-built).

Both cover: the signed-in user, recent chats, messages with paging, sending (HTML, mentions), directory search,
starting a 1:1 chat, reacting to messages, and downloading message images.

## Design

- **No policy.** The crate takes no settings from the environment, chooses no file locations and does not print
  (`clippy::print_stdout` is warned). Finding a browser for the Teams sign-in consults `PATH` and the standard install
  folders. The application passes a `CredentialStore` (`FileStore`, `MemoryStore`, or its own, e.g. an OS keyring) and
  the browser options.
- **Typed errors.** `Error` (with layered `AuthError`, `BrowserError`, `StoreError`) is `#[non_exhaustive]`, carries
  structured fields (HTTP status, the operation attempted, `Retry-After`), and keeps dependency errors as `source()`.
  `Error::is_transient()` and `Error::needs_sign_in()` say what a caller should do.
- **Public dependency types are explicit.** `chrono` and `image` types appear in the API (`DateTime<Utc>`, `RgbImage`),
  so both crates are re-exported (`teams_client::chrono`, `teams_client::image`). The stored credential formats and
  their serde implementations are private.
- **Credentials only go to their own service.** A backend refuses (`Error::UntrustedUrl`) to send a token to any host
  other than its service's, including paging links inside responses.
- **No redirects.** The HTTP clients never follow a redirect (a redirect answer from an API is an error); image
  downloads follow a few, each checked against the same media-host rule as the first URL.
- **Sanitised text.** Free text from the network (names, bodies, subjects, topics) has been through `clean`, which
  strips terminal control characters and bidi overrides. Identifiers, URLs and paging cursors are opaque and returned
  as sent.
- **Warnings, not silent failures.** Problems that do not fail a call (a renewed token that could not be saved, items
  in an answer that could not be read, failed name lookups) are returned by `Backend::take_warnings`.
- **No retries behind your back.** Throttling is `Error::Throttled { retry_after }`; the caller decides how to back
  off. After a failed silent renewal in the browser, no other is attempted for five minutes.
- **Bounded input.** Image downloads are capped at 10 MB and decoded within 8192 px / 128 MiB limits. The chat list is
  capped (Graph 100, Teams 300 by default and at most 500; `with_max_chats` raises or lowers it within the service's limit).
- **Logging through the `log` facade, never to a file or the terminal.** The crate installs no logger. It records
  requests (operation, method, host, status, time, size), whether a read was full or incremental and how many items came
  back, credential renewals, warnings, and the field names and JSON types of what the services send, so a change in a
  protocol shows up as a new name. Records never hold tokens, message text, names, URL paths or queries, or whole
  identifiers (`id_tag`, `Error::for_log`). Applications should log this crate's targets (`teams_client`) at the level
  they want and other crates, notably the HTTP client, which logs request and response bytes at trace level, at
  warnings only.
- **Changes only, after the first read (Teams web).** `messages` remembers each recently read chat's newest page and
  asks the service only for messages whose version is at or after the newest one seen (an edit, a reaction or a
  delete changes a message's version, so those come back too); `chats` follows the list's `syncState` links and
  merges what they return. The caller still gets the same whole page or list as a full read. Everything is read in
  full again every five minutes, whenever an answer may have been cut short, and always if the service refuses an
  incremental request (then increments are switched off with a warning). `with_incremental_reads(false)` turns them
  off.
- **Private directories.** `FileStore` and the browser profile are created with `create_private_dir` (also public):
  mode 0700 on Unix for the directories it creates; Windows relies on the per-user location's permissions.
- **Versioned storage.** Stored credentials carry a format version; a document written by a newer version is refused
  (`StoreError::UnsupportedVersion`) instead of being misread.

## Cargo features

| Feature | Adds |
|---|---|
| `graph` (default) | `graph::{GraphAuth, GraphBackend, GraphTokens, ..}` |
| `teams` (default) | `teams::{sign_in, TeamsBackend, TeamsSession, BrowserOptions, ..}` and the DevTools driver (`tungstenite`) |

The model types, `Error`, `CredentialStore` and the text helpers are always available. The examples in the crate docs
use both backends.

## Seeing what a service really sends

Attach a `ShapeRecorder` with `with_recorder` on either backend to write one JSON file per API exchange, containing the
operation, method, host, masked path, query keys, status and the *structure* of the request and response with every
value reduced to its character classes and secret-looking values redacted. Share the directory to show how a tenant
differs from what the parsers expect; no tokens, names or message text are in it. A failure to write a file is reported
by `ShapeRecorder::take_failure` (and as a warning) and never fails an API call.

## Testing

The request/response layer is tested against a scripted loopback HTTP server (see `src/testserver.rs`): status and
throttling mapping, bearer/skypetoken headers, 401 handling, paging, the new-chat creation fallback, token renewal and
rotation, and that credentials are never sent to untrusted hosts. Parsing is covered by unit tests built from the
documented shapes, and property tests check that arbitrary JSON never panics the parsers, that the HTTP layer's limits
hold for any status or body, and that the recorder's masking never keeps content.

## Status

Neither backend has been exercised against every tenant. `Backend::react` (adding and taking back a reaction; Teams files non-classic emoji under the ids of its public emoji catalogue, Graph accepts the classic six) is built
from the services' behaviour as understood and from scripted-server tests; it has not been run against a live tenant. The Teams web API is unofficial: its request shapes follow the
msteams-mcp project and observation, and may differ between tenants. Run the application with `--record-shapes` to
see where a tenant differs.
