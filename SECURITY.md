# Security policy

## Reporting a vulnerability

Please report security problems privately, not in a public issue. Use the repository's **Security** tab and choose
**Report a vulnerability** (GitHub private vulnerability reporting). Include:

- what the problem is and which part is affected (`vtteams` or the `teams-client` library, and the file or function if you
  know it);
- how to reproduce it, or a description of what an attacker needs;
- what an attacker gains.

Never include real tokens, refresh tokens, message content or other people's data in a report. If you need to show what a
service returned, run vtteams with `--record-shapes DIR`: it writes only the masked structure of each exchange (no tokens,
names or message text) and the directory can be shared.

This is a volunteer-maintained project. Reports are read and answered on a best-effort basis; there is no guaranteed
response time. Please allow a reasonable period to fix a problem before disclosing it publicly.

## Supported versions

Only the latest commit on the default branch is supported. The project has not made a release.

## What is in scope

- The `vtteams` binary and the `teams-client` library in this repository.
- Anything that lets a remote party (another chat participant, or a service response) crash or freeze the client,
  inject terminal control sequences, make credentials leave their own service, or read local files.
- Anything that exposes stored credentials (`token.json`, `teams-session.json`, the browser profile) to another user.

## Known limitations

These are documented behaviour, not vulnerabilities:

- **The Teams sign-in browser has an unauthenticated debugging port** on the loopback interface while it is open, so other
  users of the same computer can reach it. Do not use the Teams sign-in on a shared computer. The Graph sign-in does
  not use a debugging port.
- **The Teams web API (`--teams`) is unofficial and unsanctioned** (see the README's disclaimer). Microsoft has not
  published or approved it, and vtteams presents itself to it as Microsoft's own Teams web client. It is used at the user's
  own risk and may be blocked, changed or breach an organisation's policies. Reports of a tenant blocking it, or of
  Microsoft changing it, are not vulnerabilities. The official, supported way is Microsoft Graph.
- Stored sign-ins and the start-up cache are encrypted with a key kept in the operating system's credential store
  (Windows Credential Manager, the macOS Keychain, or the Linux Secret Service). This protects copies of the files:
  backups, roaming profiles, other accounts. It does not protect against other programs running as the same user. The
  browser profile used by the Teams web sign-in and the log are not encrypted. `credentials = file` keeps the older
  plain-text files, readable by their owner only (mode 0700 directories and 0600 files on Unix; the per-user
  `%APPDATA%` permissions on Windows).

## Security design

The README's *Security notes* section describes what the client protects and the limits it enforces. `AGENTS.md`
records the conventions the code follows (typed errors, remote text always passed through `clean`, credentials sent
only to their own service, no redirects, bounded decoding).

