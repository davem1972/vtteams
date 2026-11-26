# vtteams in detail

The [README](../README.md) gives the overview; this is the rest: the exact rules, numbers and diagnostic switches, for
when you want to know precisely what vtteams does.

- [How updates arrive](#how-updates-arrive)
- [Where your sign-in and cache are kept](#where-your-sign-in-and-cache-are-kept)
- [Typing new lines](#typing-new-lines)
- [Links](#links)
- [People, presence and your own status](#people-presence-and-your-own-status)
- [Teams and channels](#teams-and-channels)
- [Searching a chat](#searching-a-chat)
- [Pictures](#pictures)
- [Clipboard](#clipboard)
- [Logging](#logging)
- [Showing what a service sends](#showing-what-a-service-sends)
- [Finding out what a service says about its connections](#finding-out-what-a-service-says-about-its-connections)
- [Rendering one frame (`--dump`)](#rendering-one-frame---dump)

## How updates arrive

vtteams polls. With the Teams web backend a background connection to Teams' push channel (the `push` setting, on by
default) also tells it when something has happened in a chat, which makes the next poll of that chat, or of the list,
happen at once instead of after its interval; the poll still fetches the update, so with `push = "off"`, or where the
connection cannot be made, everything below still holds. Graph has no such channel. The open chat is fetched every 8
seconds while you are using it, every 4 seconds for two minutes after a message is sent or received there (even when the terminal is
behind other windows), every 30 seconds once you have been idle for 5 minutes (or the terminal is behind other
windows), and every 2 minutes after 30 minutes without input. The chat list, which shows new messages and unread dots
for the other chats, is refreshed every 30 seconds (20 in a conversation, 60 idle, 3 minutes away). Bringing the
terminal back to the front refreshes both at once, in terminals that report window focus. With the Teams web backend a
poll after the first one asks only for what changed (a few kilobytes instead of a whole page or list). A failing
service is polled less often, up to 16 times less, and when a service says it is throttling vtteams waits as long as it
asks (up to 10 minutes).

## Where your sign-in and cache are kept

**What is encrypted.** Your stored sign-in (the refresh token, which is what keeps you signed in) and the start-up cache are
encrypted with XChaCha20-Poly1305. The key is one random 256-bit value held in your operating system's credential store:
Windows Credential Manager (kept for this computer and your account only, not roamed), the macOS Keychain, or the Secret
Service on Linux (GNOME Keyring, KWallet). The files carry a fresh random nonce each time they are written, and what each is
for (credentials or cache, which service, which account) is part of what is authenticated, so a file cannot be moved to
another account or purpose and still open. Only the key is in the credential store (those stores allow small entries); the
data is in the files.

**What that protects against, and what it does not.** It protects against someone who gets a *copy of the files*: a stolen
or lost disk, a backup, a roaming profile, a copied settings or cache folder, another account on the same computer. Such a
copy is useless without the key. **It does not protect against another program running as you:** Windows Credential Manager
and an unlocked Secret Service give the key to any process of your account, as they give it everything else they hold (the
macOS Keychain may ask first, but do not count on that). Treat a computer where untrusted programs run as you as not
protected.

**What is not encrypted.** The browser profile (`browser-profile/`) of the Teams web mode holds Microsoft single-sign-on
cookies for Teams and is **not yet protected**: anyone who copies that folder can sign in as you to Teams web until the cookies
expire or you run `vtteams logout`. The log (`vtteams.log`) holds counts, timings and error text but no message text or names
(see [Logging](#logging)) and is not encrypted. Images you opened are plain PNG files for a day.

**If there is no credential store.** With `credentials = auto` (the default) or `keychain`, vtteams needs the store. If it
cannot be used (a Linux server or container with no desktop session and so no Secret Service, a locked or missing keyring),
vtteams **stops at start-up, saying what failed, and writes nothing**; it will not quietly fall back to readable text. You
choose to: make the store work (on Linux, a running Secret Service such as `gnome-keyring-daemon` on a session D-Bus), or opt in
to a plain file with `credentials = "file"` (or `--credentials file`, or `VTTEAMS_CREDENTIALS=file`). `file` keeps
`token.json` / `teams-session.json` as readable text, readable by anything running as you and by anyone with a copy of the
folder (on Unix with mode 0600 in a 0700 folder). It is logged as a warning at every start and noted by `vtteams config`.
Switching between the two never converts a sign-in: you sign in again. A readable file left behind after switching back
to `keychain` is not used, and vtteams says at start-up that it is there, so you can delete it (`vtteams logout` does).

A sign-in whose key is not in the store (the settings folder copied from another computer, or the key deleted) is not
opened, and the message says so: run `vtteams logout` and sign in again.

**The cache** (`cache = on`, the default) is what lets the chat list and the Teams tab appear at once at start-up, marked
`cached`, while Teams is asked for the real ones, which then replace them. It holds, for chats: the id, kind, title, the people
in it (names and ids, which the titles and the presence marks need), when it last changed, your read marker, whether it is a
favourite and where, and for the newest message only *when* it was sent, *by whom* (an id) and whether a service posted it (which
is what "unread" is worked out from). For teams: the names, the channels and their flags, their order, which are favourites
and which are unread. It holds **no message text, no preview text, no subject, no attachment name and no link.** It is one file
per service in the cache folder, written in the background at most once a minute after a successful refresh, and thrown away
(and the normal start used) when it is older than seven days, from another version or account, damaged, or when the key store
cannot be read; this never stops vtteams starting. It is never written as readable text, so it needs the key: with
`credentials = file` it is only kept if the credential store works anyway, otherwise there is none. `cache = off` turns it
off and deletes it. It is not used or written by `--demo` or `--dump`.

**`vtteams logout`** deletes both stored sign-ins (encrypted and plain), the encryption key from the credential store, the
cache, and the browser profile, carries on past any failure and reports every one together.

## Typing new lines

Some terminals cannot tell `Shift-Enter` or `Ctrl-Enter` from `Enter` (a plain terminal sends the same
byte for all three; Windows Terminal locally, and kitty, WezTerm, foot, Ghostty, iTerm2 and other terminals that speak the
kitty keyboard protocol, which vtteams asks for, can, over SSH too). `Ctrl-j` (new line) and `Ctrl-s` (send) work everywhere,
and `Alt-Enter` works in most terminals; a newline is added inside an open code block either way. If you would rather
have `Enter` add a line and `Shift-Enter`/`Ctrl-Enter`/`Ctrl-s` send, set `enter = "newline"` (`--enter newline`,
`VTTEAMS_ENTER`). `Ctrl-v` is usually your terminal's own paste (which vtteams
receives as pasted text); the key itself is handled when the terminal passes it on.

## Links

A footnote shows the whole address, wrapped over as many rows as it needs, and clicking any of its rows opens all of it. Links are also marked as terminal hyperlinks (OSC 8), so Ctrl-click in Windows Terminal, iTerm2, kitty, WezTerm or a VTE terminal opens the whole address, even from a label a sender's client shortened with "…" (`hyperlinks = "off"` if your terminal prints them as stray characters).

## People, presence and your own status

- **Who is around** (Teams web): a coloured circle as Teams draws it (🟢 available, 🔴 busy, ⛔ do not disturb, 🟡 away or
  back soon, ⚪ offline, 🟣 out of office) in front of the name of each 1:1 chat, and in the conversation's header with its words. A
  one-column `●` instead if your font draws emoji badly (`presence = "dot"`), or none (`presence = "off"`).
- **Status messages and out of office** (Teams web): above the message box, a banner of up to two rows says what the
  people in the open 1:1, group or meeting chat have put up: a status message as `Name: words` in grey italics, and an
  out-of-office notice as `Name is out of office until <time>: words` in red (the time is local, with the day when it is
  not today). Out-of-office notices come first, then `+N more` when there are more than two. A note whose time is up
  leaves by itself; your own notes and anything in a channel are not shown. It is plain text, never a link, and
  `presence = "off"` hides it (and stops asking). Whether the Teams client shows a note to people who are not in your
  chat, and when exactly it does, is not known; this shows them to people of the chat you have open.
- **Who is in a group or meeting** (Teams web): the header of a group or meeting chat lists the other people in a muted
  line under the name, as many as fit, then `+N more` and how many people the chat has.
- **Your own status** (Teams web): while vtteams runs it tells Teams whether you are at the keyboard, so you show as
  available to other people and as away when you are idle, as the Teams app does (your own status is in the header).
  `F9` or `:status` opens a small dialog to choose Available, Busy, Do not disturb, Be right back, Appear away or Appear
  offline, or to reset it (`:status busy`, `:status reset`). The dialog's last row, or `a`, steps how long without a key before you show as away through 5, 10, 15, 30, 60 minutes and never, for the session.
  To keep it, set `away_after` (minutes, default 5, or `never` to be reported as at the keyboard all the time;
  `:status away-after 20` also changes it for the session), and `activity = "off"` stops vtteams reporting at all (it then does not make you appear
  online). Only keys pressed in vtteams count as being around.
- **A bell for new messages** (`bell = "background"` or `"always"`, off by default): a message from someone else, in the open
  chat or any other, rings the terminal's bell, which most terminals turn into a sound or a flashing tab or taskbar entry.
  `background` rings only while the terminal is behind other windows (it needs a terminal that reports focus: Windows
  Terminal, kitty, WezTerm, iTerm2 and xterm do). A burst of messages rings once.

## Teams and channels

With the Teams web backend the heading of the left pane has two tabs, `CHATS` and `TEAMS`, each with a count and, when
something is unread, a `●` and how many; the one showing is highlighted. `Alt-1` / `Alt-2` (standard keymap), `gt` / `gT`
(vim keymap), a click on a tab, or `:chats` and `:teams` switch. A service that cannot list teams has no tab and none of
these keys does anything.

The TEAMS tab starts with your favourite channels, one row each as `Team › Channel` (a team's General channel reads
`Team › General`), in the order your Teams "Favorites" folder has them, with a bullet when unread and a thin rule below
the last one, as in the chat list. They are not part of the team tree: they do not fold, and `Enter` or `Right` on one
opens the channel and leaves the tree as it was. `F8` (`F` in the vim keymap) hides them or brings them back, and the
`favorites` setting (`top` or `off`) chooses how vtteams starts, exactly as for chats; with none there is no section and
no rule.

Below that is a row for each team and, under an open team, each channel: its General channel, then the pinned
ones, then the others you have chosen to see, then a dim `+N more channels` row that shows the rest (hidden and archived
ones) when opened. Teams are in the order the service gives them, and archived teams are not listed at all. A team shows
`muted` if you muted it and a bullet and a count when channels in it are unread; a channel shows `private` or `shared`, a
bullet when unread, and is dimmed when archived or muted. Every team starts closed except the one holding the channel you
have open, and how you fold them lasts for the session. Typing (or `/` in the vim keymap) filters by team and channel
name, and a favourite by its `Team › Channel` text; a team with a match opens by itself while the filter is on, and a
match among the favourites opens nothing.

A team list is large (a person in dozens of teams gets megabytes), so it is asked for the first time you show the tab,
then every 10 minutes while the tab is in front and every 30 minutes otherwise; `F5` / `:refresh` asks at once while the
tab is showing. What is unread in a channel stays fresh without that: the conversation list vtteams already polls says so.
A failure is shown once and does not stop anything else, and a service that says the account has no teams removes the tab.

A channel opens like a chat (`Enter` or `Right` on it), with the title `Team › Channel`. Its messages are one feed with posts
and replies in the order they were sent: a post with a title shows it in bold under the sender's name, and a reply has a
dim line above its text saying `↳ reply to <sender>: <start of the post>`, or `↳ reply in a thread` when the post is
further back than what is loaded. Reading, scrolling back, searching and reacting work as in a chat, and an open channel is
marked read here and on the service.

**Posting to a channel.** What you write in an open channel's message box is posted as a new post, with the same keys as
in a chat (`Enter`, or `Ctrl-s`, or in the vim keymap `Enter` in normal mode, as the `enter` setting and the keymap say).
Everyone in the channel sees it, so **the first post or reply to each channel in a session asks first**: the status line
reads `Post to Team › Channel? Everyone in the channel will see it. Press Enter again to post.`, nothing is sent and what
you typed stays; send again as the very next key, not sooner than 0.4 seconds after the question appeared and within ten
seconds of it, and it is posted (a key held down, or repeating, is not an answer: the status line says it came too
quickly and the question stays asked). Any other key, a mouse click or movement, a paste, or waiting too long drops the
question and the next send asks again. After one post or reply to a channel it is not asked about again for that channel
until vtteams is started again.

- **A title.** `:subject <text>` gives the *next* post a title (up to 200 characters, one line); a `SUBJECT: <text>` pill
  on the message box's border shows it. `:subject` alone shows it, `:subject -` clears it, and `Esc` in an *empty* box
  clears it too (with text in the box `Esc` leaves the title where it is, as it leaves the text: a title is a draft). A
  title is used by the next post only, comes back with the text if the service refuses the post, and is dropped when you
  open another conversation. A reply has no title: `:subject` is refused while replying, and replying is refused while a
  title is set (`:subject -` first).
- **Reply in a thread.** `Ctrl-r` (standard keymap) or `r` (vim, in the conversation) on any post or reply in the channel
  marks that post's thread as where the next message goes; the box shows a `REPLYING IN THREAD` pill with the post's title
  or its sender. Nothing is quoted or pre-filled. `Esc`, opening another conversation or starting an edit cancels it. An
  event, a deleted message and a message still being sent cannot be replied to.
- **Edit.** `F4` (or `e` in the vim keymap, or `Up` in an empty box in the standard keymap) edits one of **your own
  posts**; the box shows `EDITING` and `SUBJECT kept`, because the service drops a post's title when an edit does not
  repeat it, so vtteams repeats it. **Editing a reply in a channel is not supported yet** (the request for it has not been
  tried) and says so. There is no delete.
- **If it fails.** A post the service refuses (a 4xx, throttling) gives its text, title and reply target back in the box.
  One that may have been delivered (the connection dropped, a 5xx) stays in the conversation as `not confirmed ... check
  the chat before sending again` and is not put back, so it is not sent twice by habit.
- **Mentions.** Typing `@` in the box of a channel offers **only the people who have posted in that channel** (in the
  messages vtteams has loaded, most recent poster first; not you, and not someone who only appears in an event or a
  deleted message), because the service gives no list of a channel's members. Tab or Enter chooses one, and the name
  becomes a real mention in a new post, in a reply and when you edit a post. Anyone else can be typed, but is sent as the
  plain text you typed, and so is anyone the list does not offer, such as someone who posted only before the messages
  loaded here. `@channel` and `@team` are not offered. Files and pictures cannot be attached.
- A service that cannot post to channels (not the Teams web backend) keeps the old behaviour: sending, replying and
  editing in a channel say so and send nothing, and the message box says so too.

Channels are shown as favourites as Teams has them, but cannot be made or taken off here yet (`f` says so).

## Searching a chat

`Ctrl-f` (standard keymap, from the conversation or the message box) or `/` (vim keymap, in the conversation) opens a
search of the open chat's messages; `:search <text>` opens it with the text already in. While you type, the list shows the
messages already loaded that contain the text (any case), newest first, and nothing is sent anywhere. `Enter` asks the
service about the whole chat (Teams web; the text is searched as one phrase, and a double quote or backslash in it counts as a
space): its hits come first, in the order it ranks them (not promised to be by date, so each
row shows when the message was sent), then loaded messages it did not give. The line under the text says how many of each.
`Down` past the last row loads the next page when there is one. `Enter` again opens the selected hit: the conversation
selects that message and scrolls to it, fetching older pages first if it is further back than what is loaded. `n`/`N` (vim)
or `F3`/`Shift-F3` then step to the next and previous hit, wrapping round. `Esc` closes the popup. A service that cannot
search (Graph) gets the loaded messages only, and the popup says so; a message sent a moment ago may not be found on the
service yet, but the loaded messages always are.

**Searching a whole team.** In a channel (Teams web), the popup is titled `Search Team › Channel` and `Tab` switches it to
`Search all of Team`, which asks the service about every channel of the team at once (the open one first, archived ones
included, at most 50; a team with more says so under the text) and back again with `Tab`. Switching starts the list over:
`Enter` asks, and while you type the list is still the open channel's loaded messages. In the team search each row also
shows the `#channel` the hit is in. `Enter` on a hit in another channel opens that channel (as the teams list does; the
left pane keeps its tab) and goes to the message, fetching older pages as it must; `n`/`N` or `F3`/`Shift-F3` step across
channels the same way. In the standard keymap `Ctrl-f` also works from the teams list while a channel is open (the vim
keymap's `/` there still filters the list). `:search <text>` always searches the channel alone. In a chat, or where the
service cannot search several conversations, there is no `Tab` and nothing changes.

## Pictures

With `images = "auto"` (the default) a picture is drawn with the terminal's own graphics when the terminal has them,
and as block art (below) when it does not. The protocols, and the terminals vtteams knows to speak them:

| Protocol | Terminals | Chosen when |
|----------|-----------|-------------|
| Sixel | Windows Terminal (1.22 and later), foot, xterm (started with `-ti vt340`), mlterm, WezTerm and others | the terminal lists Sixel in its device attributes, or `WT_SESSION` / `TERM=foot` name it |
| Kitty graphics | kitty, Ghostty, WezTerm | `TERM=xterm-kitty`, `KITTY_WINDOW_ID`, `TERM_PROGRAM=ghostty` or `WezTerm`, and the terminal answers the protocol's query |
| iTerm2 inline images | iTerm2 | `TERM_PROGRAM=iTerm.app` or `ITERM_SESSION_ID` |

These pairings come from the terminals' own documentation, and the code is tested against recorded replies and a decoder of its own output, not against the terminals themselves: please report what you see. How the choice is made: the environment names the terminal (the variables above), and when it is not obvious, or to
confirm it, vtteams asks the terminal once at start-up, before the first key is read, for its device attributes and the
size of a character cell in pixels (and, for terminals that say they are kitty-like, whether it speaks Kitty
graphics). It waits at most 0.3 seconds. The answers are read by a strict parser that accepts only a few exact
numeric replies and drops everything else (what a terminal sends back arrives as if typed, so it is never trusted
beyond a number). A terminal that does not answer is taken at the environment's word when that names a protocol. Inside
tmux or screen nothing is drawn with graphics, because those do not pass pictures on unless set up to; name a protocol
in `images` to try it there anyway. `--dump` and anything that is not a terminal never ask and never draw graphics.

A picture is drawn with graphics while any of it is on screen, and **a picture that is partly scrolled out is cut at the
edge, not given up**: the rows that are inside stay hi-res. It is cut at the transcript's edge only. The two labels the
transcript draws over its first and last row ("↑ gg loads older messages", "↓ G latest") do not cut it: the picture is
drawn under them and only the cells that hold the label's text show the label, written over the picture afterwards (for
Sixel and iTerm2, which put pixels into the cells; Kitty draws the picture under the text, so nothing is written). The
text shows on top of the picture, and the rest of the row stays hi-res. It is block art while it is not encoded yet (that happens on a
thread of its own, so typing never waits for it), and for as long as a popup or the list of people to mention is open,
which would otherwise show the picture through them. How a cut is made depends on the protocol, and nothing is encoded
again for it:

- **Sixel** is drawn in bands of six pixel rows, so the stream is the header (with its height changed) and just the bands
  that lie wholly inside; a cut leaves up to five rows of the picture's edge blank inside the cell at the cut, and the
  part cut at the top sits up to five pixels higher than it would uncut. A cut picture is written again each time the
  cut changes (every line it scrolls), a part of what the whole picture costs: about 108 KB for a 444x438 photograph, 78 KB
  for its top three quarters, 27 KB for its bottom quarter, at a microsecond a slice to make.
- **Kitty graphics** crops to the pixel row with a source rectangle on the placement (`x`, `y`, `w`, `h`), and moving or
  cutting it again sends nothing: the picture's data stays in the terminal. It is placed under the text (`z=-1`), so the
  label and everything else drawn over it is simply the screen's own text.
- **iTerm2** has no crop, so a partly visible picture there is block art until it is whole.

A picture that has not moved or been cut differently is drawn once and left alone, so a still screen writes nothing.
Pictures are made at the size of the space they take on the screen, never larger than the original and never more than
1600 pixels on a side.

To draw a picture with graphics vtteams has to know how many pixels a character cell is. In order: what you set with
`cell_pixels = "10x20"` (or `--cell-pixels 10x20`: the width and height of one cell in pixels, which the font and the
display's scaling decide), the pixel size of the window where the system gives it (not Windows), and what the terminal
answers to the questions asked at start-up. A program on Windows has no way of its own to read the size of a terminal's
cells, so in **Windows Terminal**, when none of those says, vtteams **assumes 9x18** and says so in the log (`pictures:
... ASSUMPTION ...`) and in `vtteams config`. That size is a little smaller than Windows Terminal's default font gives
on purpose: a picture drawn for cells smaller than the real ones leaves some blank space round it, while one drawn for
larger ones would cover the text beside it. **If pictures look the wrong size, set `cell_pixels` to your font's cell
size** (at 100 % scaling Cascadia Mono at 12 pt is about 9x19; at 125 % about 12x23; at 150 % about 14x28). In any other
terminal that gives no size, pictures stay block art, with a note in the log (`cell size unknown`). A size taken from the
system is read again when the window changes size; one from the terminal's answer, from an assumption or from you is
not, so restart after changing the font size.

An assumption is made only where a reply cannot say otherwise: an answer from the terminal, or `cell_pixels`, always
wins, and a terminal that answers the device-attributes question without listing Sixel gets block art.

**If a terminal is not detected, or pictures are the wrong size**, run `vtteams --terminal-report` in it. It asks the
same questions as start-up (then a few more, one at a time) and prints what the environment said, each question sent,
every byte that came back (escaped: `\x1b` is ESC, and nothing from the terminal is ever printed raw), how long the wait
took, which bytes the strict reader kept and which it dropped, on Windows the key events the console handed over, and the
decision with its reason. Paste its output into a report. It never starts the program's screen and puts the terminal's
settings back before it prints.

`How long a picture takes to appear is in the log at `--log debug` (times only, no content): `picture #N: wanted by the
frame ...`, `handed to a worker`, `encoding began on its own thread, X ms after ...`, `encoding result received by the
app`, `in the plan of a frame`, and `written to the terminal: B bytes took X ms to hand over`, each with the time since
the step before and since the picture was decoded. Encoding runs on a thread of its own, so it never waits behind
network calls, and the loop looks again every 10 ms while one is under way. A Sixel is a few hundred kilobytes, and
what a terminal does with them is slow compared with the encoding, so a picture is made with fewer colours (256, 128, 64,
32) until its Sixel is under 128 KiB; on Windows it is handed to the console in one write.

`:images <choice>` changes it while running (`auto`, `sixel`, `kitty`, `iterm`, or one of the art modes) and `I` steps
through them (graphics first when the terminal has them). Opening a full-size picture (`o`, `Ctrl-o`, a click) is unchanged.

**Trying the protocols by hand**, to see what your terminal does with them without the whole program:

```
cargo test --release write_sample_pictures -- --ignored --nocapture
```

writes three files and names them (in the temporary directory, `vtteams-graphics-samples`): `sample.sixel`,
`sample.kitty` and `sample.iterm`, each a picture of a colour wheel, and `sample-sixel-decoded.png`, what the Sixel says,
for comparison. Show the one for your terminal's protocol (`cat FILE` in a Unix shell, `Get-Content -Raw FILE` in PowerShell,
`type FILE` in cmd.exe: the files are plain ASCII). The picture appears at the cursor. If it does, `images = "auto"`
should work for you; if it shows as garbage text, your terminal lacks that protocol.

### Image modes

Block art is what a picture is when graphics are not used (and what a picture is while it is not whole on screen). It
is drawn from block characters. The default, `blocks`, uses glyphs every font has. `sextant` and
`octant` (the characters added in Unicode 16) show finer detail, especially small text, but only in a font that draws
them; where it does not, each such character shows as a box with a question mark. Try them with `I` (or
`:images octant`), and set `images = "octant"` in the settings file once they look right. `ascii` and `off` are also
available, and `--images <mode>` sets the starting mode.

Pictures are drawn to be made out, not read: whatever its shape, a picture takes about a thousand cells (a square
one is about 45 columns by 22 rows; a wide strip is up to 110 columns and a few rows; a tall screenshot is narrower and
up to 28 rows), and never more than the pane is wide. To see one properly, press `o` (vim keys) or `Ctrl-o` (standard
keys) on its message, or click it: the full-size picture opens in your browser.

## Clipboard

Copy and cut work in the message box with the keys of the keymap you use (standard: `Ctrl-C` / `Ctrl-X` / `Ctrl-V`;
vim: `y`, `d`, `p`), and on a selected message `Ctrl-C` (standard) or `y` (vim) copies it as Markdown. Where the copy goes
is the `clipboard` setting (`--clipboard`, `VTTEAMS_CLIPBOARD`):

- `auto` (default): this computer's clipboard. Over SSH, or on Linux with no display, and whenever the system
  clipboard cannot be written, vtteams asks your *terminal* to set its clipboard instead (an OSC 52 sequence), which is the
  clipboard of the machine you are sitting at. The status line then says "sent to the terminal's clipboard", because a
  terminal does not confirm it.
- `system`: only this computer's. `osc52`: only the terminal's. Windows Terminal, kitty, WezTerm, iTerm2 and most
  other current terminals accept it (some ask first); inside tmux, set `set -g set-clipboard on`. A copy over about 74
  KB is refused with a message, because terminals drop larger ones without saying so.

Paste (`Ctrl-V`, `p`) reads this computer's clipboard when vtteams runs locally. vtteams never *reads* the terminal's
clipboard: that is a query any program writing to your terminal could make, and would expose whatever you last copied
(a password, say). Over SSH, paste with your terminal's own paste (`Ctrl-Shift-V` in most), which vtteams receives as typed
text, or with `Ctrl-V` to paste the last thing copied inside vtteams.

## Logging

vtteams writes `vtteams.log` in its settings directory (see [Where things are kept](../README.md#where-things-are-kept)). `--log <level>` or `VTTEAMS_LOG` chooses how much: `off`,
`error`, `warn`, `info`, `debug` or `trace`. The default is `trace` in a debug build (`cargo run`) and `info` in a
release build. The level applies to vtteams and its client library; every other crate is held to warnings so the HTTP and
TLS libraries, which log request and response bytes when asked to, never reach the file. The file is rotated at 8 MB in
a debug build and 1 MB otherwise.

What the log shows, from `info` up to `trace`:

- **Every request** (`trace`): operation, method, host, status, time and size. Throttling and server errors are
  warnings.
- **How a read was done** (`debug`): a message page or the chat list read in full or as a change since the last read,
  how many items came back, how many were skipped on purpose and how many could not be read.
- **Polling** (`debug`): when the chat list and each open chat are polled, how long each took, failures in a row and
  throttling waits; focus changes; images fetched, decoded and fitted, with times.
- **Responsiveness** (`debug`/`trace`): frames slower than 25 ms and keystrokes that took 50 ms or more to reach the
  screen; at `trace` every keystroke's time to the screen.
- **What the services send** (`debug`, then `info`): the first time vtteams sees a kind of object (message, conversation,
  their properties, the answer itself) it records the field names with their JSON types as a baseline; afterwards any
  field or message type it has not seen before in the session is logged at `info`. An item that cannot be read is logged
  at `warn` as its structure with every value masked, the same masking `--record-shapes` uses. Together these show how
  a service has changed and what vtteams does not yet understand.

Records contain no message text, names, subjects, tokens or URLs with their paths and queries, and identifiers appear
only as a short tag (`19:abc..@thread.v2(31)`: first characters, domain, length). The error text a service itself sends
back can mention an identifier, so look through a log before attaching it to an issue.

To watch it while vtteams runs, in another terminal: `Get-Content -Wait -Tail 40 $env:APPDATA\vtteams\vtteams.log` (PowerShell)
or `tail -f ~/.config/vtteams/vtteams.log`. A record looks like this:

```
2026-10-28T14:59:40.064Z DEBUG [main] vtteams::app: chat 19:abc..@thread.v2(31): poll returned 7 message(s) in 162 ms
```

If typing feels slow, run with `--log debug` and look for `slow keystroke`, `slow frame` and `slow loop iteration`
records (the last says how long the program was busy and so not reading keys). At `trace` every keystroke's time from
being read to being on screen is logged; if those are a few milliseconds and typing still feels late, the delay is
outside vtteams, in the terminal. Debug builds run vtteams itself unoptimised, so `cargo run --release` is the fair test of
speed.

Over a slow link (SSH, a laggy remote terminal) vtteams sends each screen update as one write, wrapped in the terminal's
synchronized-update markers where it has them, so the terminal never shows a half-drawn screen, and sends nothing at all
while nothing on the screen changes. The header's colours drift a few times a second and stand still while the terminal is
behind other windows or you have been away. A `slow frame` record says where the time went: `render` is vtteams drawing,
`diff` is working out what changed, `graphics` is preparing pictures and `write` is the one write to the terminal, which
is the part a slow link makes long.

## Showing what a service sends

`--record-shapes DIR` (or `VTTEAMS_RECORD_SHAPES`) writes one JSON file per API exchange into `DIR`: the operation, method,
host, the URL path with identifiers masked, the query keys, the status, and the *structure* of the request and response.
Letters, digits and unusual characters become `a`, `A`, `9` and `x` (punctuation is kept, so an id's format and length
are visible); values under keys that look like secrets are replaced outright; JSON embedded in a string keeps its field
names. Tokens, names and message text never reach the files, so the directory can be shared to explain how a tenant
differs from what vtteams expects.

## Finding out what a service says about its connections

When a request fails with "An existing connection was forcibly closed by the remote host" (a service, or something
between you and it, closed a connection that vtteams was keeping open; vtteams sends a read again on a new connection,
but never a write), the response headers show whether the service says anything about how long a connection lasts.
`--log-headers NAMES` (or `VTTEAMS_LOG_HEADERS`) writes the response headers you name to `vtteams.log`, one `info` line
for every response, so it needs `--log info` or more and is for a diagnostic session, not for leaving on:

```
vtteams --teams --log info --log-headers "connection,keep-alive,via,x-cache,x-msedge-ref"
```

```
2026-11-18T23:19:20.641Z INFO  [vtteams-worker-3] teams_client::headerlog: response headers: GET teams.microsoft.com -> 200 HTTP/1.1: x-cache=CONFIG_NOCACHE, x-msedge-ref=Ref A: 5B2C... Ref B: TEB31EDGE0410 Ref C: 2026-11-18T23:19:20Z, connection=(absent), keep-alive=(absent)
```

`NAMES` is comma-separated and case-insensitive. `*` matches any run of characters (`x-ms*`, `*cache*`) and a leading `!`
excludes (`x-ms*,!x-ms-latency`). A name without `*` that a response lacks is reported as `(absent)`, because a service
that sends no `Keep-Alive` header is itself the answer. Only response headers are read. A header whose name suggests a
credential (`authorization`, `set-cookie`, anything with `token`, `secret`, `password`, `pass`, `session`, `key`, `csrf`,
`jwt`, `sig`, `sas`, `ticket`, `nonce` or `secure` in it, among others) is listed as `name=<withheld>` however it was
selected, so you can see it was sent without seeing it. Other values lose control characters, have addresses (with or
without a host: `/path?code=...` as much as `https://...`), email addresses and GUIDs masked and long opaque words
replaced, and are cut at 200
characters; still look through a log before attaching it to an issue. The push channel's WebSocket is not an ordinary
request and is not covered.

## Rendering one frame (`--dump`)

`--dump` renders one frame off-screen (plain text, or `--ansi` for 24-bit colour) after feeding optional key presses,
which is handy for checking layout without an interactive terminal. `--keys` takes plain characters and `<Name>` for
the others, with `C-`, `S-` and `A-` prefixes for modifiers: `<Enter>`, `<Esc>`, `<Tab>`, `<BS>`, `<Del>`, `<Space>`,
`<Up> <Down> <Left> <Right> <Home> <End> <PgUp> <PgDn>`, `<F1>` to `<F12>`, `<lt>`, and for example `<C-s>`,
`<S-Enter>`, `<C-S-Left>`. The standard keymap is the default, so add `--keymap vim` to feed vim keys.
