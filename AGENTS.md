# AGENTS.md - vtteams

This is the map of the project for a new contributor or coding agent: what it is, how it is organised, the rules to follow
when changing it, and the cautions that save a repeat of old mistakes. Wire-level facts about the services (endpoints,
shapes, what was seen live and how) are in `docs/teams-web-protocol.md`, user documentation is `README.md` (with `docs/details.md` for the fine print), and this file
describes the current state: update it when a decision or convention changes, and never add a changelog, a status list or
dated stories to it.

## What it is

A Rust + ratatui 0.30 terminal UI for Microsoft Teams chats and channels, over Microsoft Graph (device-code sign-in) or the
Teams web API (sign-in through the user's own Edge or Chrome). Left pane: recent chats, with a TEAMS tab for teams and
channels. Right pane: the conversation and the message box. Bottom row: a command panel. Standard (modeless) keys by
default, vim keys on request. Messages are rendered from Markdown (termimad), fenced code is syntax-coloured (tree-sitter
for ObjectScript, SQL, Python, JSON, Rust, TOML and the rest of its bundle, syntect for the other languages), and pictures
are drawn as block art or, where the terminal supports it, with real graphics. MIT licensed.

## How we got here (read before changing the backends)

- **Two backends, because neither is enough alone.** Graph is the official route, but some tenants do not allow it and
  reading channels needs permissions that take admin consent. The Teams web API (`--teams`) is what Microsoft's own web
  client uses: unofficial, undocumented, presented as that client, used at the user's own risk. Expect it to change.
- **The wording rule.** The README has a Disclaimer section right after its introduction (unofficial, unsanctioned, not recommended, own risk, Graph is
  the official way) and credits the projects the mode builds on; `--help`, the `backend` setting's doc, `SECURITY.md`, the
  crate README and the `teams-client` module docs carry the same wording. There is deliberately no runtime warning. Keep the
  wording, credit prior art, never copy code or data from AGPL projects (mautrix-teams is AGPL-3.0: read for behaviour
  only), and never add anything that hides the client from Microsoft or an administrator.
- **Why sign-in drives a browser, and what it costs.** Device code works for the Teams client id, but a tenant's Conditional
  Access can refuse to complete it, so it cannot be the only path; Safari and Firefox cannot be driven (no DevTools protocol;
  a WebDriver session starts clean and would lose silent renewal). So Edge, Chrome or Chromium is driven over DevTools on a
  private profile. Its debugging port (loopback) is unauthenticated while open and the pipe transport would need `unsafe`:
  the README says not to sign in this way on a shared computer.
- **The 24-hour cliff.** SPA refresh tokens live a fixed 24 hours (`AADSTS700084`) whatever rotation does. Silent renewal
  reopens the profile headless, lets Teams' SSO store a new refresh token, and trusts a token only after Entra accepts it (the
  profile's token is as dead as ours until SSO has run, so "different from ours" proves nothing). On failure the app says so
  and `F6` opens a visible browser. A refusal for one scope is not a dead sign-in: a tenant can withhold a single scope.
- **Browser quirks.** Chrome needs `--disable-features=ThirdPartyStoragePartitioning` (without it a sign-in completes while
  the page's `localStorage` stays empty); it is passed to every launch. The MSAL cache may be encrypted at rest (key in the
  `msal.cache.encryption` cookie); the page decrypts it with WebCrypto, so both forms work on any browser.
- **TLS is `rustls` + `rustls-platform-verifier`.** `native-tls` needs OpenSSL headers on Linux for no benefit; `rustls` with a
  bundled root list keeps trusting a CA the OS has since distrusted. The platform verifier asks the OS's own validator (Windows
  honours the Disallowed store). The `ring` provider is passed explicitly (never a process-global default) and the Trouter
  `wss://` wrap builds the same `ClientConfig`: one TLS stack, one trust policy.
- **Traps already paid for** (each has a test):
  - `isFavorite` on a team or channel is not the favourite flag; the "Favorites" conversation folder is (the old `favorites`
    user property is only a fallback). An edit of a channel post that does not restate `properties.subject` loses it.
  - A mention `<span>`'s attributes come in any order; matching the whole tag as one string silently dropped mentions.
  - Notes-to-self (`48:notes`) has no second party and shows "quoted message isn't verified" on every reply: never judge reply
    quotes there. `EntitySets[].IsPartial` is true on ordinary search answers. `properties.edittime` can precede a never-edited
    message's creation. A client that does not hear back resends with one `clientmessageid` and the service stores every copy.
  - The Windows console delivers a paste as typing and terminal replies as key events with the ESC lost; a throttling service
    refuses every request of a burst; a dead grant shows first in presence and sends while cached tokens keep reads working.

## Code map

Workspace: root package `vtteams` (the binary, `src/`) and `crates/teams-client` (the reusable library). `build.rs` sets
`VTTEAMS_BUILD_VERSION` (`git describe --always --dirty`, or `dev` without git or a commit) for the header and `--version` and
watches the git state.

### The binary

- `main.rs`: CLI (flags, `login`, `logout`, `config`), start-up, the event loop (Press events only; frames drawn with the
  cursor hidden inside a synchronized update), panic hook, `--dump`, `--terminal-report`. `workers.rs`: a fixed pool in
  two lanes (8 `vtteams-worker-N` background threads, 2 `vtteams-action-N` interactive ones) plus `spawn_cpu_task`
  (`Workers::inline()` for tests). `paste.rs`: key presses that are really a
  paste become one `Event::Paste`.
- `app.rs` coordinates polling, routing of background events (`Ev`), global key precedence (`App::on_key`), chat selection and
  send dispatch. Everything it uses of the machine comes in through `Desktop` (workers, `Clipboard`, `Opener`, image
  directory), so `App::for_test` replaces all of it.
- `app/` components each own their transitions: `chat.rs` (messages, pending sends and render caches are private; one
  `Option<Fetch>` is the request in flight), `chats.rs`, `reactions.rs`, `read.rs`, `reply.rs`, `edit.rs`, `post.rs`,
  `sign_in.rs`, `images.rs`, `favorites.rs`, `roster.rs`, `mention.rs`, `notify.rs`, `presence.rs`, `status.rs`, `search.rs`,
  `teams.rs` (the TEAMS tab), `gfx.rs`, `cache.rs`, `input.rs` (popup, pane, editor, command and mouse handlers). Integration
  tests: `app/tests.rs` and `app/tests/{gfx,posting,search,teams}.rs`.
- Pure policy: `cadence.rs` (polling), `status.rs` (own availability), `presence.rs` (how availability is drawn), `bell.rs`,
  `sign_in_modal.rs` (`plan` is the one layout for drawing and hit-testing).
- Drawing: `view.rs` (`layout()` is a pure function of the terminal size; `Geometry` is shared by drawing, scrolling and mouse
  hit-testing, so a click hits what was drawn), `ui.rs` (`ui::render` = `prepare`, the only code that changes drawing state,
  then `draw`, which takes `&App`), `transcript.rs` (messages to lines; per-message cache keyed by `message_key`), `render.rs`
  (Markdown body to lines; `ImageStore`), `theme.rs` (contrast-tested).
- Text: `convert.rs` (Teams HTML <-> Markdown), `inline_style.rs` (underline, colour, highlight as private-use marker characters
  that remote text can never contain), `fence.rs` (the one place that recognises a code-fence delimiter), `termtext.rs`,
  `highlight.rs` (code colouring and the colours sent to Teams), `emoji.rs`. `editor/` is a pure modal editor (no UI code;
  `standard.rs` is the modeless keymap; both keymaps have random-key invariant tests).
- Settings and security: `settings.rs` (pure: the `KEYS` table, `resolve`), `config.rs` (the only place that reads the
  environment and settings file or picks paths), `vault.rs` (seals credentials and cache), `cache.rs` (start-up cache format),
  `clipboard.rs` (`System`, `Terminal` via OSC 52, `Layered`), `opener.rs`, `title.rs`, `diag.rs` (`vtteams.log`), `testdir.rs`.
- Pictures: `imgart.rs` (block art) and `gfx/` (real graphics: `detect.rs` environment and the strict reply reader, `probe.rs`
  the start-up query, `encode.rs` Sixel/Kitty/iTerm2, `plan.rs`, `present.rs`, `report.rs`, `decoder.rs` tests only). `demo.rs`:
  the offline `Backend` for `--demo`; it builds Teams' markup by hand.

### The library (`crates/teams-client`)

`publish = false`, `#![forbid(unsafe_code)]`, `missing_docs` warned, never prints, takes no settings from the environment and
picks no paths (browser discovery consults `PATH`, skipping relative entries, and the standard install folders).

- `lib.rs` public surface: `Backend` and model types (`model.rs`), errors (`error.rs`), `CredentialStore` with
  `FileStore`/`MemoryStore` (`store.rs`), `clean`/`html_to_plain`/`id_tag`/`preview_text` (`text.rs`), `graph::*`, `teams::*`.
  `http.rs`: shared `ureq` plumbing. `shares.rs`: SharePoint/OneDrive links through Graph's Shares API. `emoji.rs`: reaction
  keys. Diagnostics: `record.rs` (`ShapeRecorder`), `headerlog.rs` (`HeaderLog`), `watch.rs` (names fields, types and message
  types a service has not sent before).
- `graph/`: `auth.rs`, `mod.rs` (`GraphBackend`). `teams/`: `mod.rs` (`TeamsBackend`, the largest file), `auth.rs` (MSAL refresh
  token to spaces token to skypetoken and regional bases), `browser.rs`, `parse.rs` (pure JSON <-> model; `Parsed::{Item,
  Skipped, Malformed}`), `incremental.rs`, `favourites.rs`, `search.rs`, `teams.rs` (teams list), `channels.rs` (posting),
  `emoji_catalog.rs`, `trouter.rs` (push). `testserver.rs`: test-only scripted loopback server (and a self-signed TLS one).

Repository files: `docs/teams-web-protocol.md` (**update it whenever a live session teaches something about a
service; add Graph results when that is explored. Facts only, in the present tense: no dates, no account of how a fact was
found, no real names or ids; mark what is not observed `(unverified)`**), `README.md` (the overview: what it does, install, sign-in, keys,
settings, a few sentences per behaviour) and `docs/details.md` (the exact rules, numbers and diagnostic switches behind
each; a change of behaviour updates both), `SECURITY.md`, `CONTRIBUTING.md`, `deny.toml`, `.github/`.

## Architecture and design decisions

`teams_client::Backend` is the one seam: Graph, Teams web and the demo implement it and the app never sees service JSON (app
code imports from `teams_client` directly). Everything the app does to the machine is injected (`Desktop`, `Clipboard`,
`Opener`, `Workers`, `CredentialStore`, credential-store keys), so tests are deterministic. Drawing is `ui::prepare` (mutates)
then `ui::draw` (read-only) over the pure layout. `App` coordinates components that each own their transitions, so a change to
messages cannot forget to invalidate the transcript, request flags cannot overlap and retry policies have one owner.

| Decision | Reason |
|----------|--------|
| Teams web sign-in drives the user's Edge/Chrome over DevTools; Entra bearer tokens plus a skypetoken | Graph is unavailable on some tenants; no Node or Playwright |
| Remote images only from the signed-in service and only near the screen | Sender-chosen URLs leak the IP; bounded memory |
| Our own modal editor, not `tui-textarea` | Real vim semantics and Markdown/fence highlighting |
| `syntect` `default-fancy`; `rustls` with the platform verifier | No C builds (onig, OpenSSL); the OS decides who is trusted |
| All Microsoft connectivity in `teams-client`: typed errors, injected stores, no environment, paths or CLI text | Other projects can depend on it |
| Remote JSON stays untyped; unreadable items are counted and warned (`Parsed::Malformed`) | Typed structs would reject shapes that cannot be checked against every tenant |
| Fixed worker pool with panic containment; failing polls back off; throttling never triggers browser renewal | An in-flight flag must always clear; no retry storm |
| Modeless `standard` keymap beside vim, and it is the default | Most people expect a text box that behaves like every other |
| Logging records names, types and counts, never content | A service change shows as a new field name without recording anyone's messages |

## Conventions

### Build, gates and dependencies

- **Toolchain.** `rust-toolchain.toml` pins the latest stable and `rust-version` in `Cargo.toml` follows it; bump both
  together. Default rustfmt. Workspace lints: `unsafe_code = forbid`, `rust_2018_idioms`, `missing_debug_implementations`, and
  clippy `dbg_macro`, `todo`, `unimplemented`, `mem_forget`, `unwrap_used` (tests may unwrap).
- **Gates.** CI (`.github/workflows/ci.yml`; Linux, Windows, macOS; `--locked`, `-Dwarnings`) runs `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets`, `cargo test --workspace` and `cargo doc`; a second job builds, lints, tests and
  documents the library with no backend, `graph` only and `teams` only; a third runs `cargo deny check` through EmbarkStudios' action (pinned by commit; its version is the cargo-deny version, so a bump of the action needs `deny.toml` to pass under it first). Run the same locally
  before finishing.
- **Dependencies track the latest releases.** The ObjectScript highlighter crates are git dependencies pinned to a tag of
  `hkimura-intersys/syntax-color-objectscript` (MIT), the one git source allowed in `deny.toml` (it carries its diff grammar
  within itself). Move the tag forward as it releases. `deny.toml` ignores the `chrono` unmaintained advisory with a reason.
- **Editing files.** PowerShell 5.1 `Get-Content`/`Set-Content` corrupt UTF-8; use the editor tools or .NET
  `ReadAllText`/`WriteAllText` with UTF-8.

### Errors and failure handling

- **Typed errors.** The library returns `teams_client::Error` (`thiserror`, `#[non_exhaustive]`, layered `AuthError`,
  `BrowserError`, `StoreError`, dependency errors as `#[source]`, an `operation` field). Never `anyhow`, `Box<dyn Error>`,
  `Other(String)` or string-matching on messages (match `Error::Status { status, .. }`). 429 is `Throttled { retry_after }`;
  `is_transient()` and `needs_sign_in()` classify. `Unsupported`, `TooLarge` and `TooManyRedirects` are local decisions;
  `Unexpected` is for a well-formed answer that is wrong.
- **A write that may have been delivered.** `Error::delivery_unknown` is an exhaustive match, so a new variant forces the
  decision. `Error::NotSent` wraps a failure before the request was written (renewing credentials), so a dropped connection to
  the token endpoint reads as "send failed" with the text given back. A write is never repeated by the library.
- **Only a grant rejection triggers browser renewal** (`AuthError::is_grant_problem`), never throttling or a 5xx; a client
  misconfiguration (`invalid_request`, `unauthorized_client`) is an error, never read as "no session".
- **Renewing credentials under lock.** `TeamsBackend::live` has two locks, taken in this order and never the other way
  round: `live_gate` is held across the whole renewal (token refresh, then the service's authorisation) so concurrent
  callers share one renewal, and `live` guards the cached `Live`, held only to read or replace it and never across a request
  or while another lock is taken. A caller takes the gate, looks at the cache again (another caller may have renewed while it
  waited) and renews only if still needed. A retrying caller passes `stale: Option<&str>`, the token it was just refused,
  and is never served that token (a bare `force` flag would rotate the refresh token once per caller). Renewal starts while
  credentials have minutes left, so a caller with no `stale` token whose cached credentials have not actually run out uses
  them at once while another thread renews, instead of queueing behind the network. Callers with a `stale` token, with run-out
  or missing credentials, and every caller once `sign_in_lost()` is true wait for the gate as before. The side-scope caches
  (CSA, presence, Substrate, Graph, IC3) hold their own mutex across their token request and share `TeamsAuth::renewing`
  (the refresh token rotates, so it is redeemed one at a time); `GraphBackend::token` still holds its lock while renewing.
- **Side calls never mean "signed out".** Only calls on the main chat credentials can declare the sign-in lost
  (`Ev::sign_in_error`: profile, chat list, messages, send, edit, open chat). A picture, people search, favourites, teams,
  reactions list, read marker, search, presence or status can each be refused on a permission of its own while chat reading
  works, so their errors are shown as they are (`describe_side`) and change nothing. `Backend::take_warnings` carries
  non-fatal problems to the status line and log. Errors become text only at the UI boundary (`app::describe`).
- **On the first evidence of a problem, try to fix it or tell the person; backing off is the last step.** A failure that
  degrades quietly and retries on a timer leaves a dead sign-in hammering the token endpoint and the person to find out by
  chance. Built so far: a read on a closed connection is sent again at once; name lookups send one request first; a refused
  scope asks the main scope, renews on the spot when it is dead, asks again, and only on failure marks the sign-in lost. Pauses
  (`REFUSAL_PAUSE`, `RENEWAL_PAUSE`) come after, never instead. When adding a failure path ask: what is the first thing that
  could repair this, and who is told if it cannot be repaired?

### Background work

- **Two lanes.** Every backend call runs on a worker, either `App::spawn_call` (background lane: 8 `vtteams-worker-N`
  threads, for polling, lookups, pictures from scrolling, presence, marking read, reporting activity, and anything that can
  run for minutes such as `sign_in_again`) or `App::spawn_action` (interactive lane: 2 `vtteams-action-N` threads with their
  own queue, so what a person has just asked for never waits behind slow background calls: send, post, edit, react, set
  favourite, set status, `find_people`, `open_chat`, searches, a picture loaded with Ctrl-L, and a message fetch with a spinner,
  see `Fetch::lane`). Only a call that ends after about one request belongs on the interactive lane, and a lane's threads are
  never added to. A panic in either lane is caught and delivered as an error result so request states always clear. Browser
  launches and cache writes run as `spawn_task` and picture encoding on its own thread (`spawn_cpu_task`), never behind backend
  calls. `Workers::in_flight()` counts the unfinished jobs of both lanes and `App::settled()` requires it to be 0.- **Quiet refreshes.** The spinner is for what a person asked for or waits on. A scheduled refresh of an already loaded list or
  conversation, an automatic older-page fetch and a seek run `quiet` and show nothing unless they fail. Every message fetch
  honours the service's hold-off and refuses while offline (`start_message_fetch`), not only the scheduled poll.
- **Shutdown.** `Workers` joins its threads on `Drop`, bounded by `SHUTDOWN_BUDGET` (a worker mid-call, such as an interactive
  sign-in, may not return for minutes), and `main` ends with `std::process::exit`: returning normally would make the runtime
  (on Windows `ExitProcess`) wait for stragglers and can hang or give an unrelated exit code.
- **Panic hook.** A panic on a thread other than the main one leaves the screen alone (workers, the clipboard thread and the
  HTML converter report an error instead); a UI-thread panic restores every terminal mode. It logs the thread and place, never
  the message, which can quote the text being processed.

### Security and untrusted input

- **Credentials go only to their own service.** Graph's bearer to `graph.microsoft.com`; the Teams bearer and skypetoken to
  hosts that pass `teams/auth.rs::trusted_url`; the skypetoken as `Authorization: skype_token` for images on Teams' media hosts (`*.asm.skype.com` and any https subdomain of `teams.microsoft.com`, such as the newer `<region>-prod.asyncgw` gateway; `teams::is_media_url`); the
  Substrate token only to `substrate.office.com`; Trouter hosts must be `https` and on an allowlist. Paging links and `authz`
  answers naming other hosts are `Error::UntrustedUrl`. Ids exposed to the app are bare Entra object ids (`8:orgid:` stripped);
  other ids, URLs and cursors are opaque; stored tenant and account ids pass `is_valid_id`.
- **No redirects, bounded bodies.** `http::agent` sets `max_redirects(0)` (ureq would forward every header but `Authorization`
  to an unchecked host); a 3xx from an API call is `Error::Status`. Every read goes through `http::read_decoded`, which caps
  decoded bytes (ureq's own limit counts compressed bytes). Image downloads follow up to three redirects themselves, only to
  URLs the backend's media-host rule accepts; a Graph-resolved download must pass `shares::is_download_url` (https, a DNS name,
  no userinfo, default port, no local names) at every hop. `http::local_agent` (DevTools on loopback) uses no proxy.
- **Remote content is hostile input.** All free text from the network passes `teams_client::clean` at the library boundary and
  `html_to_plain`/`convert::tidy`; do not render remote text without that. `inline_style::strip` removes the private-use marker
  range from HTML, text, subjects, code and anything copied or quoted, including attachment and card names
  (`convert::escape_name`, which also swaps literal brackets for look-alikes so a name cannot read as a second link). Remote
  text never reaches an escape sequence, with one exception: a link's address inside an OSC 8 hyperlink, and only one that
  passes `hyperlink::target` (see Links).
- **Hostile HTML cannot overflow the stack.** htmd/html5ever recurse per nesting level and an overflow cannot be caught (the
  Windows main thread has 1 MiB). Three layers: `nests_too_deeply`, `has_more_elements_than` (`MAX_HTML_ELEMENTS`; the first
  alone is fooled by comments and stray end tags) and the conversion running on its own large-stack thread (`CONVERT_STACK`)
  with the panic contained. A refused message is shown as plain text. Rendering is cut at `MAX_RENDERED_CHARS`.
- **Terminal replies are untrusted and read only at start-up**, by `gfx::detect::parse_replies` (exact numeric forms,
  everything else dropped), never by the key handler. The terminal's clipboard and old title are never queried either (a reply
  would let whatever set it type into the terminal); remotely `Layered::get` is `None` and Ctrl-V pastes `App.last_yank`.
- **No control character may reach a cell.** ratatui writes a cell's text to the terminal as it is, so one would be an escape sequence. After every frame `output::first_control_cell` checks the buffer and logs once (position and code point only) if it finds one: the tripwire for a remote string that missed `clean`. The only cells it lets through are links of exactly the form `hyperlink::mark` makes (`hyperlink::is_ours`).
- **Links.** `app::open_url` opens only `http(s)` and `mailto:` addresses without spaces or control characters, and drops a
  `mailto:`'s parameters. `render::number_links` numbers a label that looks like an address but names another; a `[text](...)`
  is a link only if the parentheses hold one address with a scheme (`render::link_destination`). The `[n]` list under a
  message is never cut: an address wraps under its number, its rows are `Transcript::link_rows` and a click on any of them
  opens the stored address (a sender's label is often a shortened address, and a terminal's own detection sees one row).
- **Terminal hyperlinks** (`hyperlink.rs`, the `hyperlinks` setting, default on; off for `--dump` and in tests unless set).
  A link's label in the text and each row of its `[n]` entry are marked with OSC 8, so Ctrl-click opens the whole address.
  The address must pass `hyperlink::target`: `http(s)`/`mailto:`, printable ASCII only (nothing can end the sequence),
  at most `MAX_TARGET` bytes, a `mailto:`'s parameters dropped; anything else is not marked. A label that reads as a
  different address is never marked (`render::label_may_link`; an address shortened with "…" counts as the same one), so a
  link never leads somewhere other than what it says; its `[n]` entry still is. Positions travel through termimad's
  wrapping as an `inline_style::Format::link` marker (`apply_linked`), become `Transcript::link_cells`, and `ui::prepare`
  plans the visible ones (`View.links`; none while a popup, overlay or the mention list is open, none under the hint
  labels). ratatui has no hyperlink type: `hyperlink::mark` puts the whole link in a run's first cell with
  `CellDiffOption::ForcedWidth`, one cell per run of one style, after the picture cells are marked `Skip`, and has last
  frame's link cells written again (`AlwaysUpdate`) where no link is now, so no character keeps a stale link.
- **Credentials at rest are sealed** (see Stored sign-in). Credential, profile, image and log directories are created private
  (`teams_client::create_private_dir`: 0700 on Unix; Windows relies on `%APPDATA%`'s ACL). `config::config_dir` ignores an
  environment variable that is empty or not absolute, so nothing lands relative to the current directory.

### Logging

- Both crates log through the `log` facade; only the binary installs a logger (`diag::init`, `vtteams.log`). The library never
  prints. Default level `trace` in debug builds, `info` in release; vtteams's own targets get the level and every other crate
  warnings only (`ureq` would log message bytes; a `diag` test covers it).
- **Records carry no content:** no tokens, message or subject text, names, or URLs beyond the host. Ids go through
  `teams_client::id_tag`, errors through `Error::for_log` (drops the URL or value an error is about; keeps status, operation,
  OAuth codes and the service's own explanation with addresses, emails and GUIDs masked), unreadable items appear as
  `watch::masked` shapes, `Watch` logs field names and JSON types only, remote text in a record is one escaped line.
- `the_log_describes_what_happens_and_what_changed_without_any_content` plants content in every field and asserts none reaches
  a record: extend it when adding a log line that touches remote data. Timings and counts are fine.

### Settings and keymaps

- **Settings.** Every user-facing option that is not a diagnostic is a row of `settings::KEYS` and can be a flag, a
  `VTTEAMS_*` variable and a key in `config.toml` (`--config` / `VTTEAMS_CONFIG`). Precedence: flag, environment, file,
  default; blank values do not count. A missing default file is fine; a missing explicit one, an unknown key or a bad value
  stops start-up naming the file, line and column. The file holds no secrets.
- **To add a setting:** a `KEYS` row, a field in `Document`, a case in `resolve`, and the flag in `Args::flags`; it then shows
  in the starter file and `vtteams config` by itself, and a test resolving every setting fails on a forgotten case. `--demo`,
  `--dump` and helpers are flag-only; `--record-shapes`, `--log-headers` and `--config` also read a variable but are not
  settings rows. A flag's value is never another option (`--browser --teams` is an error).
- **Keymaps.** The `keymap` setting selects `editor::Keymap` (default standard). Help (`VIM_HELP`/`STANDARD_HELP`), footer
  hints, the message-box placeholder and the command-panel prompt follow the keymap in `ui.rs`: **when adding a key, update
  that keymap's help, hints and the README table together.** The standard keymap has its own pane handlers and no bare-letter
  commands; vim's `Ctrl-h`/`Ctrl-l` and `^W` pane keys are handled centrally in `App::on_key`.
- **AltGr** is reported by Windows as Ctrl+Alt: `input::types_text` and `ctrl_shortcut` read Ctrl+Alt as text and Ctrl alone as
  a shortcut. Pasted text goes to the open popup (ignored), else the command panel, the chat filter, then the editor.

### Tests

- **App tests are deterministic.** `App::for_test(DemoBackend::instant())` runs jobs on the calling thread, delivers results by
  `on_tick`, and uses an in-memory clipboard and a recording `Opener`. Nothing sleeps or touches the real clipboard, browser,
  config directory, OS credential store or terminal. Tests that need a folder use `testdir::TestDir` (the library has its own, `crates/teams-client/src/testdir.rs`) and leave nothing in the
  temp directory; credential tests use `vault::MemoryKeys`, never `OsKeys`.
- HTTP tests use the scripted loopback server (`testserver.rs`; `{origin}` in a body is its address;
  `teams::auth::trust_for_tests` and `allow_download_origin_for_tests` let the backends accept it; `Reply::hang_up()` closes
  without answering). The browser is replaced by `recover_hook`.
- **Mutation-check what you add:** break the rule and see its test fail. Ignored measurements and previews are run by hand
  (`frame_cost`, `preview_a_picture`, `write_sample_pictures`, `sixel_slice_cost`; see their doc comments).

### Files and platform

- Settings and credentials live in `config::config_dir` (`%APPDATA%\vtteams`, `$XDG_CONFIG_HOME/vtteams` or
  `~/.config/vtteams`; never the current directory), with `images/` and `vtteams.log`; the start-up cache lives in the
  non-roaming `config::cache_dir` (`%LOCALAPPDATA%\vtteams\cache`, `~/Library/Caches/vtteams`, `$XDG_CACHE_HOME`).
  `vtteams logout` (`config::forget_all`) deletes credentials, browser profile, cache and key and reports every failure.
- **Windows.** No `unsafe` anywhere, so no `SetConsoleMode`: a paste arrives as key events (`paste::Coalescer`; delete
  `paste.rs` if crossterm ever delivers `Event::Paste` there) and terminal replies as key events with the ESC lost
  (`probe::bytes_from_keys` puts it back; it and its helpers are compiled for Windows and for tests only, so a Linux build has no unused code). Mouse capture is on, so terminal text selection needs Shift. The wheel (and a trackpad's two-finger slide) moves the view, never the selection: `wheel_list` scrolls the chat or teams list and `View.list_wheeled`/`team_wheeled` stop `ui::prepare` pulling it back to the selection until the selection moves; `wheel_transcript` scrolls the conversation, follows again at the bottom and loads older messages at the top.

### Sending safety

- An ambiguous send failure (transport failure, unreadable answer, a 5xx or 408) leaves the message in the transcript as "not
  confirmed ... check the chat" and does not restore the text; a refusal the service itself issued gives the text back.
- The first post or reply to a channel each session asks first, and a held key never confirms (`POST_CONFIRM_GAP`,
  `POST_CONFIRM_WINDOW`); `F6` on a sign-in that is fine asks first.
- A full name shared by two different ids is never sent as a mention; it stays text and the person is told.
- Nothing is dropped silently when copying or editing: yank copies `message_markdown_in_full`, and editing a message holding
  private-use characters says they will be lost on saving.

## Features: how they work and what to watch for

### Sign-in and renewal

- **Teams web.** Only the refresh token and identity are persisted. On a grant refusal for the spaces scope `TeamsAuth::token`
  calls `recover` (headless browser on the private profile; `working_session` redeems every refresh token in the page's cache
  issued to the Teams client id, the signed-in account's first, remembering refused ones in a `dead` set). A failed renewal
  pauses silent renewals (`RENEWAL_PAUSE`).
- **A refusal for any other scope** goes through `token_locked`: ask the main scope at once. Fine: only that scope is
  withheld and the app shows it as a side call. Refused too: renew now, ask again, and on failure mark the sign-in lost
  (`sign_in_lost`) so `TeamsBackend::live` fails as `AuthError::Expired` at the next poll. Refusals are remembered per scope
  (`TeamsAuth::refusals`) and not asked about until a growing pause ends; success forgets them and `adopt` clears all. Only a
  refused grant is remembered, never throttling, a 5xx or a configuration problem.
- **The app takes over** when an `Ev` fails with `needs_sign_in()` while `Backend::can_sign_in_again()`: `SignIn::Expired` stops
  polling, picture fetches and marking read, the footer shows a banner and `Overlay::SignIn` opens by itself (keys other than F6
  and clicks are ignored for 700 ms after it opens, since it may appear mid-typing; it never opens over another popup).
- **`F6` / `:signin`** runs `Backend::sign_in_again` on a worker (`TeamsAuth::reauthenticate`: visible browser, same account
  only, waits for a different refresh token, `INTERACTIVE_TIMEOUT`). While it runs `token()` fails at once with
  `SignInInProgress`; `Ev::SignedIn` resumes everything. The editor is never touched, so a draft survives.
- **Browser hygiene.** A browser left on the profile makes the next exit at once (Chromium "profile in use"):
  `browser::close_leftover` asks it to quit through the port in `DevToolsActivePort` (a launch must not delete that file first).
  `wait_for_sign_in` ends when the window closes (`BrowserError::Closed`). Graph cannot sign in again while running, and shows
  a device-code address only if it is `https`.

### Polling and cadence

- `cadence.rs` is policy only: time since input, terminal focus and time since the last exchange give an `Engagement`
  (conversation, active, idle, away) and from it the message and list intervals (seconds when engaged, minutes when away).
  `Schedule` times one resource: last fetch, a fetch-at-once flag, failures (back-off doubling, capped) and a hold-off when the
  service asked to wait (`Throttled`, default 30 s, capped at 10 minutes; it beats a back-off or a refresh-now). Only the open
  chat is polled; `App::set_focused(true)` asks for an immediate refresh. "Away" looks at input only.
- **An unchanged poll does no UI work.** `merge_messages` reports whether anything changed (`Merged::changed`: a new message, or a replaced copy that differs; `Msg` is `PartialEq` for this) and `ChatState::receive_page` bumps the transcript `version` only then, so the layout is not rebuilt for a page the app already holds. The transcript key also carries the local date, so `Today`/`Yesterday` labels still refresh after midnight. The repeat diagnostics in `TeamsBackend::watch_repeats` return at once unless info logging is on (they only feed a log line; `without_resends` is separate and always runs).
- A scheduled list refresh only moves the selection off a chat that left the list when nothing of the person's is at risk
  (empty draft, focus off the editor), so a silent refresh cannot retarget a send. The TEAMS tab asks nothing until shown, then
  rarely (a large answer); `:refresh`/F5 asks at once.

### Push (Trouter)

- A background connection (`teams/trouter.rs`): register, socket.io handshake, WebSocket dial on the host the answer names,
  registrar POST per app, ping, reconnect with backoff from registration (`failures_after`; `HEALTHY_AFTER` resets the
  backoff, `SILENCE_LIMIT` replaces a silent connection). Queue, frame size and timeouts are bounded by named constants.
- **It is only a hint, never a second way to apply an update.** `Backend::poll_pushes` is drained every tick and a
  `PushHint::Chat(id)` or `Unknown` only calls `refresh_soon()` on the existing schedules, so the tested poll-and-merge path
  fires sooner. A frame's body is JSON inside a string; `extract_chat_id` tries `conversationid` and `conversationLink` on
  `resource` and the body, and `trailing_id` cuts `;messageid=` so a channel reply names the channel. An unrecognised shape
  broadens to `Unknown`. `handle_frame` is pure.
- `connect_push(self: &Arc<Self>)` is called once from `main`; the thread holds only a `Weak`, so it cannot keep the backend
  alive. The `push` setting off never starts it; `--dump` starts none.

### Incremental reads (Teams web)

- After the first full read the backend asks only for what changed and returns the same whole page or list
  (`teams/incremental.rs`). Messages: `startTime` = newest version seen minus a margin; a message's `version` changes on edit,
  reaction and delete, so those come back; the remembered page is trimmed to `KEEP` and a trim forces a full read. Chat list:
  follow the `syncState` link one step per call and merge by id (`ChatIndex`).
- A full read happens on a schedule (`FULL_REFRESH`), when an answer has 50 or more items (it may be cut short), and after a
  4xx to an incremental request. Incremental reads are given up for the session only after three such refusals in a row
  (`MAX_INCREMENT_REFUSALS`); `with_incremental_reads(false)` turns them off.

### Unread and marking read

- `Chat::is_unread` (shared rule `model::unread_after`): the newest message is after the read marker, is not yours and is not
  `Preview.automated` (a service posting under a person's id). A chat with no marker is unread unless it is a meeting chat. A
  channel has no read marker here, so no "new" divider (`ReadState::channel_unread`).
- While the open chat is loaded, the terminal is focused and the person is not away, `mark_current_read_on_the_service` calls
  `Backend::mark_read` once per newest non-local sent message. The local override applies at once; a failure is logged, not
  shown, and pauses marking. `App.read_only` (`--dump`) tells the service nothing and starts no push channel, so a diagnostic
  frame cannot change what other devices show as unread.
- A chat holding too few real messages (joins, calls and recordings arrive as system messages) gets older pages fetched by the
  app itself, quietly and within a budget enforced only when a request starts (`AUTOFILL_*`).

### Messages, mentions and replies

- **Conversion.** `Msg.body` reaches the app already normalised by the library (mention spans to `<at id>`, code blocks to
  fences), so `convert.rs` has one mention tag to recognise. `message_markdown` is what is drawn, `message_markdown_in_full`
  what yank copies. `termtext::normalize_emphasis` runs first (termimad's emphasis is looser than CommonMark): do not hand-craft
  `_..._` italics elsewhere. `Chat::title` never claims you are alone when people are merely unknown. The chat list's preview is introduced by
  `ui::preview_prefix`: "You: ", a group sender's first name, and nothing for a 1:1 chat, a cached list or an unknown
  sender (never a bare ": ").
- **Mentions.** `Msg.mentions` and `Msg::mentions_me` come from the service's structured data, never from text. A mention of
  you is drawn bold red (`theme::MENTION_ME`), anyone else's link blue, per `<at>` tag; a message mentioning you gets an `@`
  badge. Graph's `<at id>` is not comparable to a real id, so a Graph mention is always the "someone else" colour. Typing:
  `App::mention_suggestions` filters `App::mention_candidates` (a chat's roster; for a channel the people who posted in its
  loaded messages); at send `convert::extract_mentions` matches `@Full Name` and an unmatched name stays text. The message box colours by the same rule (`convert::mention_ranges`, through `ui::MentionPeople`, whose fingerprint keys the editor's caches), so what is drawn as a mention while typing is what is sent as one; an `@word` that matches nobody is coloured as one word, as before. Shortcodes and
  mentions skip code, inline code and addresses.
- **Reply quotes.** Reading fills the empty time `<span>` of Teams' reply `<blockquote>` from `itemid` (`quote_time`). Sending:
  `r` / `Ctrl-R` marks a message (`App.replying`, the box is untouched); `App::send` hands the backend a plain `ReplyTarget` and
  only `TeamsBackend::send` builds Teams' markup (`reply_quote_html`), plus `properties.qtdMsgs`, the full `properties` shape
  every genuine message carries, and an IC3-scoped bearer (`send_message`). The quoted name is the sender's real one, not "You".
  Replying to a `local-` message is refused; the pending reply is restored with the text on a refused send. Graph ignores it.
  While a reply is being written `ui::draw_reply_connector` frames the message replied to in the box's border colour (`╭─╮` on the blank row above it, `├─╯` on the one below; the right side in the transcript's last column, which every message row leaves free: text, headers and reaction rows) and runs the left side, in the column between the sender bar and the text, on down to a `┴` on the box's top border; `↑` at the top when the message is above the screen, nothing when it is below it or not loaded.
- **Code blocks** are not a logical element: Teams draws HTML it is given as a card. Each backend gets its own form
  (`Backend::code_block_style`): Teams web `EditorCard` (a marker paragraph plus `<pre class="language-x">`; the service
  rewrites every other form), Graph `Codeblock` (documented, unverified). Teams colours only six classes but stores inline
  `style="color:#rrggbb"`, so `convert::code_body` sends an equal-luminance palette (`highlight::class_colour`). Reading accepts
  `<codeblock>`, any `<pre>` and a bare `<code>` holding `<br>`; `highlight::guess_language` colours an untagged block as
  ObjectScript on strong signs. Every fence decision goes through `fence.rs`.
- **Attachments.** `convert::is_image_attachment` judges from `Attachment::content_type`, deliberately "content type or file
  extension" (Graph says `image/png`, Teams web the bare `png`). A picture pasted into Teams is on the sender's OneDrive, where
  neither backend's bearer works: `shares.rs` resolves it through Graph's Shares API.
- **Only pictures from the signed-in tenant are drawn automatically** (`sharepointIds.tenantId` against the session's tenant; a
  missing id or mismatch is `Error::UnverifiedContent`); others show a placeholder and `Ctrl-L` / `L` loads one explicitly
  (`App.explicit_loads`, `Backend::image_from_outside`). Failed pictures are retried on `:refresh`/F5, not on a timer.
- **A message sent again is shown once** (`TeamsBackend::without_resends`, before parsing, full and incremental): the same
  sender, `clientmessageid` and content is one message and the first copy is kept; a shared id with different words, or no id,
  is never dropped.

### Reactions

- `Ctrl-E` (standard), `+` in the vim conversation pane and `F2` open `Overlay::Emoji`; focus decides the target (the box
  inserts, the conversation reacts to the selected non-system message). A reaction applies locally at once
  (`App::apply_reaction`), `Backend::react` runs on a worker and `Ev::Reacted` confirms or withdraws it; choosing one already in
  `mine` removes it; changes are serialised per message.
- **Two lists, one picker, one load.** Reacting takes only what the service accepts (`Backend::reaction_emoji`, the classic
  six until it arrives). Writing takes that same list followed by the shortcode table's other emoji (`emoji::insertable`,
  both built by `emoji::merge`; just the table, 72 entries, until the list arrives), so everything that can be reacted with
  can be written. Opening either picker asks for the list (`Reactions::start_catalog`) and a failed load is said only
  when someone was reacting (the table still serves writing). An emoji the Teams catalogue lacks is refused before any
  request, because an unknown id is stored and shown to everyone as a broken image. Teams' own list has over 1,500 entries,
  so the picker's title counts the list (and the matches of what is typed), `PgUp`/`PgDn` move `EMOJI_ROWS` rows, and a
  particular emoji is found by searching its name or keywords (`turkey`, `xmas`), not by scrolling.
- **Reading.** A reaction key is made drawable by `emoji::shown`. A plain word id (`salute`, `fire`) cannot be derived from the
  emoji, so `parse::word_or_glyph` keeps it whole and `TeamsBackend::resolve_reaction_ids` looks it up in the catalogue
  (downloaded once, only when needed) in full and incremental reads, merging two ids for one emoji without double counting.

- **Who reacted.** `Reaction::names` runs parallel to `users`: the backend's display name, or empty. Teams web fills it from its name cache at the end of every read (`fill_reactor_names`, full and incremental) and hands ids it has never asked about (not you) to the `teams-names` helper (the same one-at-a-time flag and pauses as the titles' lookups; each id asked once), then queues `PushHint::Chat(chat)` only if a name was learned; not attached, nothing is asked. Graph uses `displayName` when the answer has one. The transcript (`transcript::reactor_names`) draws, dim after each chip, "You" first, then first names (full names when two share one), at most `REACTORS_SHOWN`, then "+N" (named but not shown, unknown, or counted without a name); names missing from the backend come from the chat's people and loaded senders (`App` passes them as `known_names`); nothing when nobody can be named. Rows break between reactions only, and one too wide for a row has its names cut. The formatted names are part of `message_key`, so a name that arrives later redraws the message.
### Editing

- `F4`, vim `e`, and `Up` in an empty standard-keymap box call `begin_edit` (`app/edit.rs`): yours, sent and not deleted
  (`why_not_editable`), saving the box's text as `Editing.draft`, loading the message as Markdown and focusing the box. `Enter`
  is `finish_edit`: unchanged is "no change", empty is refused, else `Ev::Edited` and `ChatState::apply_edit` (which leaves
  `Msg::modified` alone so the service's own timestamp is not treated as stale). A refusal keeps the text. Esc, `select_chat`
  and `show_chat` call `cancel_edit`; a second edit while one is open is refused.
- **Cancelling an edit does not cancel its request.** A second edit of the same message gets its own token
  (`App.edit_tokens`) and `on_edited` drops a confirmation whose token is not the latest, so a late answer cannot revert it.
- **`Msg::edited`** comes from `properties.edittime` / `lastEditedDateTime`, never from `modified`, and only when strictly
  after `created`. Formatting the box cannot hold (`<u>`, colour, highlight, images, tables) is lost on saving and the status
  line says so. Graph cannot edit (`can_edit` false).

### Search

- `Backend::can_search_messages` / `search_messages` (Teams: Substrate `searchservice/api/v2/query`, `teams/search.rs`; the
  default and Graph answer `Unsupported`, so the popup then searches loaded messages only) and `search_conversations` for the
  channels of a team. The popup (`app/search.rs`): vim `/`, standard `Ctrl-F`, `:search <text>`. While typing the rows are loaded
  matches and nothing is sent; `Enter` with changed text searches the server once, `Enter` on a current list opens the hit;
  `Tab` switches chat and team scope in a channel; `n`/`N` and `F3`/`Shift-F3` step through hits.
- **The phrase is untrusted and no backslash escape is relied on.** `normalise_phrase` treats every `"` and `\` like whitespace
  and collapses runs, so the query's fixed text cannot be extended. Refused as `InvalidInput` before any request: nothing left,
  over 200 characters, a control character, and a chat id that is empty, long or holds a quote, backslash or control
  character. Relax only on a clear "escapes HONOURED" from a live, read-only probe.
- **A hit must belong to the scope asked about** (`Source.ClientThreadId`, else `ClientConversationId` up to `;messageid=`);
  other hits are skipped, counted and warned once. A result without a usable digits-only message id or time is left out.
  `HasParseException`, no `EntitySets`, or a result set with neither `Results` nor a zero `Total` is `Error::Unexpected`, never
  "no hits"; **`IsPartial` is ignored**. At most `MAX_SEARCH_CONVERSATIONS` ids are searched.
- **Results race.** `Ev::Searched` carries chat, scope and a sequence number and is used only if all match the request
  `pending` waits for. It is a side call (never "signed out"); `Unsupported` turns server search off for the session. Going to
  a hit (`ChatState::seek`) pages older messages quietly until it is loaded or `MAX_SEEK_PAGES` is reached.

### Favourites

- Teams: a chat is a favourite when its id is in the "Favorites" conversation folder of the chat service aggregator (its own
  bearer scope, `Bearer::Csa`), falling back to the old `favorites` user property only for an account with no folder. A read
  that failed for the moment is an error, so a hiccup never flips the remembered source (`favourite_source`). Read with each
  `chats()` (`add_favourites`) and carried as `Chat::favorite`; favourites older than the page are fetched singly and kept when
  the list is cut to `max_chats`.
- **Writing:** when the property is the source `set_favorite` reads the whole property, changes the map and puts it back
  (`favourite_write` serialises calls); when the folder is the source it refuses (`folder_write_unsupported`; the folder's write
  call is unexplored) and `can_favorite` says so.
- App: `f` (vim) and `F7` call `toggle_favorite` (one change per chat in flight; applied at once, put back on refusal);
  `F`/`F8` toggle pinning (`favorites = top|off`); `view::list_row` / `list_capacity` are the one definition of the list rows
  that drawing, scrolling and clicking share.

### Teams and channels

- **Reading:** `Backend::can_list_teams` / `teams()` / `channel_activity()` (`teams/teams.rs`: the aggregator's `v3` list, big
  and slow, so callers ask rarely). A team's id is its General channel's. Teams are in the person's own order from the
  `teamsOrder` user property (the rest alphabetically, **a guess**); failing to read it keeps the service's order with one
  warning. Archived teams are hidden but their data stays loaded. Favourite channels come from the Favorites folder in the same
  answer (never `isFavorite`). `ChannelKind` and `Channel::shown` are guesses. `chats()` records what the plain conversation
  list says about channels as `ChannelActivity` (bounded, replaced by each read) for freshness at no extra request.
- **A channel is opened like a chat** by id (`open_channel` -> `select_chat`), so polling, read tracking, search and reactions
  are unchanged. `current_chat()` is `None` for it and `current_channel()` is the check, so roster, presence and attendee
  paths do nothing (a test fails if `members` is asked: a channel has no roster to read). A channel's feed mixes posts and
  replies: `Msg::root_id` is set only for a reply and only a post has a `subject`.
- **Rows** (`Teams::rows(filter, pin)`): favourite channels first as `Team › Channel` (they never fold), then the teams tree;
  `view::teams_row`, `teams_capacity` and `teams_scroll` share the pinned-divider logic with the chat list. Moving the cursor
  opens nothing (only Enter/Right on a channel does). `Ev::Teams` is a side call and `Unsupported` hides the tab.
- **Writing** (`Backend::post_to_channel` / `edit_post`, `teams/channels.rs`; `app/post.rs`): a post is the chat send with
  `properties.subject`; a reply goes to `{channel};messageid={root}` with no quote; `edit_post` always restates the subject. Ids,
  thread, subject (1-200 characters, not on a reply) and mentions are checked before any request. `:subject <text>|-` sets the
  next post's title (a `SUBJECT` pill, restored on a refused send); editing a reply is refused; there is no delete.

### Presence and your own status

- `Backend::can_presence` / `presence(ids)`; the app asks (`app/presence.rs`) for the open chat's person, the 1:1 chats the
  list shows and you, about every minute (never when away), at once for someone new, backing off like any poll. It is a side
  call: a failure is said once and never means signed out; `Unsupported` stops it. `AvailableIdle` is drawn as away and
  `BusyIdle` as busy, as the Teams client shows them. Group chats show nothing; names line up whether or not a chat has a mark.
- **Own status.** `Backend::can_set_status` / `set_status` / `report_activity`. **Away is the service's idea, from what the
  program reports:** `status::Reporter` decides when to say "active" from the keys pressed in vtteams (it cannot see other
  windows, hence `away_after`, `never` and `:status away-after`). The `activity` setting off means vtteams does not make you
  appear online at all; `--dump` reports and sets nothing. Choose a status with `F9` (`Overlay::Status`) or
  `:status busy|dnd|brb|away|offline|available|reset`.

- **Status messages and out of office.** `presence.note` (a status message) and `presence.calendarData.outOfOfficeNote` are `{message, expiry, publishTime}`; `message` can hold HTML. `StatusNote::new` makes it plain, one line, at most 300 characters and never empty, private-use characters out. `parse::presence(answer, now)` drops a note that has expired, treats an expiry over about five years away as none, and reads the out-of-office note only when `isOutOfOffice` is true. `Presence` is not `Copy`. `app/presence.rs` also asks for every person of the open group or meeting chat (`GROUP_PEOPLE`, none in a channel); `App::banner_entries(now)` lists whoever has a current note, out-of-office first, never you, and nothing when `presence = off`.
- **The banner** (`banner.rs`, pure): at most two rows between the transcript and the message box, `Name: words` in grey (`SUBTLE`) and `Name is out of office until <time>: words` in red (`theme::OUT_OF_OFFICE`), `+N more` in the colour of the line it ends, plain text only. `view::layout` takes a banner closure and fills `Geometry.banner`; `ui::prepare` decides the rows and `draw` only draws them, so clicks and scrolling agree. Not verified: whether the Teams client shows a note to people outside your chat, and when; the "show when people message me" choice is not in the answer.
### The bell, notes to self, new chats

- **Bell** (`bell` setting: `off`, `background`, `always`): a message rings when it is a person's other than yours and newer than
  anything seen in that chat (`Bell::observe`, one watermark counting every sender), after a silent start (the first list, a
  chat's first page). A burst rings once; `background` rings only while unfocused. Fed by `bell_for_list` and `bell_for_chat`;
  `main::event_loop` writes BEL after the tick.
- The chat with yourself is `48:notes` (`streamofnotes`, no members): `parse_conversation` accepts exactly that stream as "Notes
  to self" (the other `48:` streams are feeds) and `open_chat` for your own id returns it. Graph has none.
- `:new <name|email>` (`Backend::find_people` + `open_chat`) opens one match directly or shows a picker (`Overlay::People`, at
  most nine). Each person takes up to three rows (`ui::person_rows`): name, availability and status message (the picker's
  people join `wanted_presence`, asked for at once when it opens); email and phone; job and office. `Contact.phone` and
  `office` come from Graph's `businessPhones`/`mobilePhone`/`officeLocation` and Substrate's `Phones`/`OfficeLocation` (unverified;
  a 400 falls back to the old fields for the session). Details are `SUBTLE`, `TEXT` on the highlight (contrast-tested). Who
  someone reports to and their time zone are not shown: they need services not explored yet. A chat with no messages is not in the server's list, so `ChatList` keeps it as pending. Teams creates the
  conversation with `POST .../v1/threads` if the first send gets 403/404; Graph uses `/users?$search` + `POST /chats`.

### Stored sign-in and the start-up cache

- One random 256-bit key lives in the OS credential store (`OsKeys` over `keyring-core`: Windows Credential Manager with local
  persistence, macOS Keychain, Secret Service); the library never sees it. `vault::seal`/`open` are XChaCha20-Poly1305 with a
  fresh nonce per write and associated data naming format, purpose and identity. Threat model: copies of the files, backups,
  roaming profiles, other accounts; **not** malware running as the same user. The browser profile and the log are not encrypted.
- **Fail closed.** The `credentials` setting (`auto` = `keychain`, or `file`) is resolved in `config::open_vault`; an unusable
  store stops start-up with a message offering `file`. `config::open_credentials` opens the sealed file (`teams-session.enc`,
  `token.enc`), or the plain one with `file`. Nothing is converted between them: a plain file found while encrypting is not
  used and is named in a start-up note.
- **Cache** (`cache` setting, default on): one file per service in the cache dir with our own structs and **no message text,
  preview, sender name, subject, attachment name or link**; parts carry read times and are ignored after a week, on a
  version/account/seal mismatch or when oversize; a cache that cannot be used is a log line and a normal first load. Never
  written without the key; not used for `--demo` or `--dump`; `cache = off` deletes it. `App::use_cache` shows cached lists
  marked `cached` until the first real answer replaces them; nothing is selected or fetched from it and the bell never sees it.

### Pictures

- **Block art** (`imgart.rs`) is the transcript's rendering and the fallback; size comes from the picture's shape and a cell
  budget (`imgart::budget`), the default set is `Blocks` because a font without a glyph draws a box, and fits are cached
  (`ImageStore::art`): rebuilding the transcript must never refit glyphs. `o` / `Ctrl-o` or a click opens the full-size picture
  in the browser (saved under `images/`, since the source URL needs our credentials). Only pictures near the viewport are
  queued (`App::layout_transcript`); decoded ones are evicted beyond a cap; downloads and decodes are bounded.
- **The code highlighter is warmed at start-up.** The shared tree-sitter highlighter lives in `highlight::Shared`, built under its lock, so concurrent first users share one construction. An interactive run warms it from `main` through `highlight::warm_in_background` (`vtteams-warm` thread: one line each for Rust, Python, SQL, JSON, ObjectScript and Go, which also loads syntect's engines; a panic is contained and the time is logged at debug); `--dump` stays lazy (`warms_at_start`). The cold first block costs about 0.3 s. Measure with `cargo test --release highlight_first_use_cost -- --ignored --nocapture` and `highlight_after_warm_up_cost`, each in its own process.- **Real graphics** (`gfx/`): `ui::prepare` plans (`plan_graphics`), `ui::draw` marks covered cells `CellDiffOption::Skip`, and
  `main::event_loop` presents after the flush inside the synchronized update. A picture is drawn with graphics only inside the
  free transcript rows and with no overlay, popup, mention list or hint row above it; otherwise block art. Never query the
  terminal outside start-up; every graphics command carries `q=2`. `--dump` never queries or draws graphics.
- **A cell size is required** (setting `cell_pixels`, the window's pixel size, or the terminal's answer; crossterm cannot give it
  on Windows), else block art; for Sixel in Windows Terminal only, a labelled assumption (`ASSUMED_WINDOWS_TERMINAL_CELL`)
  stands in and never overrides a told size. The `images` setting is `auto`, `sixel|kitty|iterm` (forced) or an art mode; tmux
  and screen disable detection unless a protocol is named; an answering terminal is believed over the environment.
- **Cropping never encodes again** (Sixel is cut by whole 6-pixel bands, Kitty crops with a source rectangle, iTerm2 cannot, so
  a partly visible picture stays block art there). A changed `rect` or `rows` is a new placement; an unchanged one writes
  nothing. The transcript's hint labels do not cut a picture (Sixel/iTerm2 redraw the label cells over it, Kitty places it
  under text). Streams are bounded (`MAX_STREAM_BYTES`); encoding runs on `vtteams-encode`; `present::emergency_cleanup` runs
  on every exit and from the panic hook. `vtteams --terminal-report` prints what a terminal answers, escaped (`report::escape`).

### The editor and keymaps

- `editor/` is pure. The buffer stores `char`s but Left/Right, vim's `h`/`l` (so `x`, `dl`, ...) and Backspace/Delete move and
  erase by whole grapheme cluster (`cluster_before`/`cluster_after`); word motions and undo stay per-`char`. `wrap_line` and
  `ui::grapheme_col_widths` measure by cluster for the same reason (a variation selector changes a base character's width).
- **Standard** (`editor/standard.rs`): no modes (the editor stays in `Mode::Insert`), Shift+movement selects. **Enter:** the
  `enter` setting says whether a plain Enter sends or adds a line and the modified Enter does the other; `Ctrl-S` always sends
  and `Ctrl-J` always adds a line where a terminal cannot tell a modified Enter apart (`main::enhance_keys` asks for the Kitty
  keyboard protocol). Enter inside an open fence keeps only the line's indentation.
- **The message box's layout is cached between frames.** `ui::refresh_editor_rows` keeps the rows in `View.editor_cache` (`view::RowsCache`): rows of lines without cursor or selection are cached per line for (editor version, width), and `view.editor_rows` is made again only when `RowsKey` (version, width, cursor, selection) changes; a cursor or selection change lays out only the lines it touches. `build_editor_rows` (test-only) is the forced rebuild the kept rows are tested against, and neighbouring characters of one style share a span, so a row's span count means nothing. `md_cells` finds closers through `Closer`, which remembers its last answer, so a failed search for `](`, `)` and the like is not repeated for each opener (differential test against `reference_md_cells`; `POSITIONS_LOOKED_AT` bounds the work at 8n for 32,000 characters). Measure with `cargo test --release editor_cost -- --ignored --nocapture`.- **Up and Down go by screen rows** (`ui::build_editor_rows` records what each drawn row shows, `view::RowSpan`;
  `App::move_editor_by_rows`), because the editor only knows text lines.
- A paste is inserted as it is, with no auto-indent (`Editor::paste`). Ctrl-V asks the clipboard, falling back to the last
  copy; `--dump` keeps the plain system clipboard so no escape sequence lands in the dump. The bottom row is a permanent
  command panel (`:` or click) running `App::run_command`; the editor's own vim `:` line shows above the hints.
- **Terminal title** (`title.rs`): on start `CSI 22;0t` pushes the old title on the terminal's own stack, then OSC 0 sets
  "⌨ vtteams"; exit and the UI-thread panic hook pop it (the `title` setting turns it off). The old title is never read back.

### Logging and diagnostics

- **One write a frame (`output.rs`).** `Display::frame` draws through `FrameBackend`, which collects cell updates in memory and only notes the cursor; `assemble` adds the picture bytes (`Presenter::stage`), the synchronized-update markers and the cursor, and everything leaves in one `write_all` plus one flush, so a slow link never sees a half-built frame. A frame with no changed cell, no picture bytes and no cursor or shape change writes nothing (decided from the collected bytes, not from a dirty flag, so no source of change can be missed); a cursor-only change writes just its bytes. Nothing is written until the frame is complete, so a failed draw leaves no update open; a failed write discards the staged pictures (`Presenter::discard`) and draws everything again next frame. Rendering still runs every loop pass (about 0.5 ms); only the terminal output is skipped. The panic hook, `emergency_cleanup` and teardown still write straight to stdout. Windows: frames use std stdout (UTF-16 conversion); only an all-ASCII frame of at least 16 KiB goes through the duplicate console handle (`TerminalOut`, `fits_whole_write`), because that handle writes bytes in the console's code page.
- **Animation.** `App::tick` is a clock that moves at most once per 50 ms. The header gradient (`header_phase`) steps once per 4 ticks and stands still while the terminal is unfocused or the person is away (`App::animate`); spinners step every 2 ticks and keep running. Measure with `cargo test --bin vtteams output_cost -- --ignored --nocapture` (compares against the old separate-writes path kept in `output/tests.rs` as `OldPath`).
- **Slow frames.** The loop logs `slow frame: N ms (render, diff, graphics, one write of N bytes)` for a frame over 25 ms: `render` is our work, `diff` is ratatui's, and `write` is the one write, which is where a slow terminal or link shows, so read the split before assuming drawing is slow. A steady frame costs about 0.5-1 ms in debug and 0.3-0.6 ms in release with 300 chats, 240 channels and a 200-message conversation (`cargo test --bin vtteams frame_cost_by_part -- --ignored --nocapture`, synthetic catalogue in `src/ui/frame_cost.rs`); every message laid out again (a width change) costs about 80 ms in debug, and fitting block art for a large picture about 240 ms in debug (26 ms in release) on the UI thread. `transcript::laid_out()` (test-only) counts messages laid out and a frame in which nothing changed must lay out none; `ImageStore::evict` must not bump `version` when it drops nothing.
- **`--record-shapes DIR`** (or `VTTEAMS_RECORD_SHAPES`) writes one masked JSON file per API exchange: operation, method, host,
  masked path, query keys, status and the structure of request and response (letters and digits become `a`/`A`/`9`,
  secret-looking keys redacted). Files are created new, never written through a link. Use it to check the unverified
  assumptions below.
- **`--log-headers <names>`** (or `VTTEAMS_LOG_HEADERS`) adds one `info` line per response with the selected **response**
  headers (`headerlog::HeaderLog`: comma list, `*` globs, `!name` excludes); it never reads request headers. A name that
  suggests a credential (`headerlog::WITHHELD`) is logged as `<withheld>` however it was selected; values are cleaned and
  masked. Extend `WITHHELD` and its tests when a credential-bearing name turns up.
- **Rosters for titles are read off the list's path when attached.** `chats()` returns at once with what it has: an
  untitled group whose roster is not known yet has no members and reads "(untitled chat)". The rosters (`fetch_rosters`: the
  first alone, then `MEMBER_LOOKUPS_AT_ONCE`, at most `MEMBER_LOOKUPS_PER_READ` a job; a 429 pauses, a failure is remembered in
  `failed_members`) are read on the `teams-names` thread (`start_roster_job`, the same one-at-a-time flag as the name lookup;
  `Running` frees it even after a panic), then the names of everyone the list holds. When rosters were read it queues
  `PushHint::Unknown`, once after the rosters and once more when the thread is free, so the app refreshes at once and the next
  read starts the next job. A backend that is not attached reads them in the call and queues no hint. The app keeps a
  group's known members until the real list has a roster for it (`ChatList::receive`), so the start-up cache's titles are not
  downgraded.
- **People the directory no longer lists.** `fetchShortProfile` answers 200 and leaves some ids out (see the protocol
  doc): an id asked about in an answer that listed anyone is put in `TeamsBackend::unlisted` (taken out again when an
  answer lists it; an empty answer says nothing; the signed-in person never), and `TeamsBackend::member` gives it as
  `Member::not_in_directory`. The chat list's lookups (`resolve_names(.., named_too: true)`: the list, rosters,
  favourites) ask about every member once per session even when a name is known from messages, since a departed
  person's name usually is; the reactor-name lookups ask only about unknown names. When the set changes the helper thread
  queues `PushHint::Unknown` (`marks_changed_since`), so the app reads the list again and redraws. Such a person has no
  availability: `App::partner_of` skips them, so it is neither asked for nor shown. `Chat::title_parts` says which pieces of a title are such names; the chat list, the chat
  header (a 1:1 also says "no longer in the directory") and the sender line of their messages draw them with
  `ui::unlisted_style` (dim, italic, not bold). The set is part of `TranscriptKey` and of the cached member
  (`not_in_directory`, absent in older caches). Whether everyone left out is a deleted account is not verified.
- **Name lookups** (`resolve_names`) split a batch that fails with a 5xx in halves for a bounded time (`SPLIT_BUDGET`), write an
  id off only if some other lookup in the call was answered, and never hold up the chat list (`TeamsBackend::attach` runs them
  on one `teams-names` thread). **Reads are repeated, writes never:** `http::call_json` (GET) and `fetch_image` resend up to
  `RECONNECTS` times on `connection_dropped` (not a refusal, DNS failure or timeout), logged at info as the only trace.

## Not verified against a live service or real terminal

- A real Microsoft Graph session: that the default public client id is accepted for `Chat.ReadWrite`, the `/me/chats` query and
  its shape, the message HTML dialect (`<at>`, hosted-content images), the `<codeblock>` form and a switch to the editor card,
  and whether the SharePoint path needs more scopes than sign-in asks for.
- A genuine 24-hour sign-in expiry end to end (silent renewal, then `F6`), and that a later run renews from the persisted
  session.
- Marking a chat read (the `consumptionhorizon` PUT), sending a reaction (`emotions`; Graph's `setReaction`), and mention
  markup in a **sent** message (reading one is verified).
- Resolving a SharePoint/OneDrive attachment (that Graph returns the tenant id the check compares) and image fetch with
  `Authorization: skype_token` (including the `asyncgw` host).
- Presence (shape, scope) and your own status (`forceavailability`, `endpoints`, `reportmyactivity`; visible to others, so
  compare with the real client's requests first). Push-based presence is not tried.
- Message search: the URL and scope spelling a tenant accepts, whether the Substrate token carries the search scope, 1:1 and
  meeting chats, `Total`, page limit, indexing delay, rate limits, escapes, paging a team-wide search, over seven channels.
- `:new` (people-search shape, whether a computed 1:1 id is created on first send, the `threads` fallback body) and Graph's
  `/users?$search` + `POST /chats`.
- Teams and channels: the `channelType` mapping and what `isFavorite` on a channel means, whether `isFavorite` on a team means
  "not hidden", `unread = !isMessageRead` against the official client, marking a channel read, the push frame for a channel
  reply, how the real client draws a channel mention, mentions in edits, `@channel`/`@team`, and whether a reply's answer id
  equals its stored id.
- Drift logging on a real session (baseline noise) and that the service heartbeats an idle push connection often enough for
  `SILENCE_LIMIT`.
- Terminals: the standard keymap (Shift/Ctrl-Enter, Ctrl-V, Alt/Ctrl+arrows, Ctrl-Backspace), the Kitty keyboard protocol,
  the bell, the title restore (expected in xterm, VTE, kitty, WezTerm, iTerm2, foot; unknown for Windows Terminal and tmux),
  OSC 52 in remote setups (tmux `set-clipboard`, SSH end to end), arboard's Wayland feature, and the interactive event loop and
  panic-hook restore (rendering is tested through `TestBackend`).
- Graphics on real terminals: Kitty and iTerm2 output, mode 2026 with graphics, the Unix raw reply read, a cut Sixel stream in
  Windows Terminal, Kitty crop keys, text replacing an image slice (assumed for Windows Terminal, foot, xterm, iTerm2) and Kitty
  honouring `z=-1`, scrolling speed, and the assumed Windows Terminal cell size.
- macOS and Linux credential stores and `cfg(unix)` paths (CI is the first check, never manual runs), and dark mode of code
  cards sent to Teams.

## Known limitations

- Commercial cloud only (the `authz` host is hard-coded). The chat list is capped (Graph 100; Teams 300 by default, at most 500
  because the service refuses 1000; favourites come on top).
- Not implemented: delete (messages and posts), editing through Graph, channels through Graph, group-chat creation, file
  upload, search across all chats, desktop notifications, avatars, an assistant chat, setting your own status message, work location. Graph
  cannot mark chats read, search within a chat or quote a reply.
- Channels: no `@channel`/`@team` mentions, attachments or editing a reply; a person who has not posted in the loaded messages
  cannot be mention-completed there.
- Safari and Firefox cannot do the Teams web sign-in. A headless Linux with no Secret Service must use `credentials = file`
  (a passphrase unlock is planned, not built).
- Known and accepted: `sign_in_lost()` can stay true while a spaces-scope refusal entry exists, so each poll retries a token
  (bounded by back-off); Graph resolves a link to another tenant's SharePoint file with the reader's token before the tenant
  comparison; DNS resolution in the push dial has no timeout.
- Known and accepted, graphics: a pre-1.22 Windows Terminal that does not answer is trusted by `WT_SESSION` and would print
  garbage (use `images = "blocks"`); a terminal reply arriving after the start-up wait would be typed into the editor; a cell
  size from a reply or `cell_pixels` is not refreshed after a font-size change; selecting a message with a graphics picture
  does not tint its rows; a stale encode result can be accepted for a re-queued picture key; the Windows console code page can
  garble multibyte overlay labels.
