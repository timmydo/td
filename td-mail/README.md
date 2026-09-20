# td-mail (Timmy's Mail Console)

`td-mail` is a Rust mail client (MUA) for reading and triaging email over JMAP, in a td-ui window: mailboxes, messages and threads as lists, a message read in td-editor's document view.

## Goals

- Fast, keyboard-first email workflow in a window on td's compositor: lists to move through, a document view to read in, an action bar for the pointer.
- Composition in place: a draft is edited in the window, in td-editor's document view, and retained as a file.
- Clear separation of concerns: `td-mail` reads/manages mail; message submission is external.
- Scriptable automation through a JSON-over-stdin/stdout CLI mode.

## What td-mail Does

- Connects to one or more JMAP accounts.
- Lists mailboxes and emails, opens message view, and shows threads.
- Supports read/unread, flag/unflag, move, archive, delete, and mailbox-wide mark-read.
- Supports compose/reply/reply-all/forward draft generation.
- Supports optional mail rules and retention policies.
- Provides `--cli` NDJSON mode for integrations and automation.

## Local drafts

Compose, reply and forward retain `.eml` drafts in
`$XDG_STATE_HOME/td-mail/drafts`, falling back to
`$HOME/.local/state/td-mail/drafts`. Only absolute state/home paths are
accepted; an empty or relative state setting falls back to HOME. Runtime
storage is not used, so logout does not intentionally discard a draft.
Missing directories are created private; an existing draft directory must
already be private and must not itself be a symlink. Files are mode 0600.
These checks are not protection against a hostile ancestor-directory owner.

The retained file is then opened in the window, in td-editor's document
view, editable, with paragraphs filled as they are typed: Ctrl-S writes
what is in the window over the file, whole or not at all (a private
sibling is written and renamed over the draft, so a write that fails
leaves the draft as it was, and a symlink put at the path is replaced
rather than followed), and Ctrl-W closes the draft, asking first when it
has unsaved changes (y saves, n keeps the file as it was last saved,
Escape returns to it); the Save and Close labels do the same. Closing
the window with an unsaved draft asks the same question: a save or a
discard closes the window, and Escape keeps it, so nothing typed is lost
to the close and a save that fails is shown, not skipped. While the
window holds a draft the file is its own: an edit made to it elsewhere
is overwritten by the next save. A draft larger than the document view's
ceiling (16 MiB) is retained but not opened, and the log says so. Cut,
copy and paste (Ctrl-X, Ctrl-C, Ctrl-V) work within td-mail, so a
selection made in a message's view can be pasted into a draft; the
system clipboard is not yet connected. Nothing here deletes the draft or
its attachment sidecar, and the log records both paths. Reopen the
`.eml` file with an editor or file manager; delete it explicitly when no
longer needed. The matching `td-mail-att-ID` directory belongs to
`td-mail-draft-ID.eml`: keep it while the draft needs its attachments
and remove it separately when discarding that draft. Moving only the
`.eml` file does not move or rewrite attachment references. There is no
draft-list UI, automatic expiry or mail submission in this increment.

Attachment-bearing drafts require a UTF-8 storage path without quotes,
backslashes, angle brackets or control characters; unrepresentable MML
paths or content types fail explicitly instead of pointing elsewhere.
Descriptions replace control/attribute characters for display. Colliding
sanitized attachment names refuse before writing files.
On preparation failure, only that attempt's created files and sidecar
directory are removed; cleanup failure reports the remaining paths. This
is best-effort error cleanup, not recovery from process death or power loss.
Older runtime drafts are not moved or deleted automatically.

## Requirements

- Rust toolchain (stable) with Cargo. No crates at all: td-mail is `std` alone,
  and `Cargo.lock` lists one package, td-mail itself.
- A JMAP server/account.
- td's fetch service, listening at `$XDG_RUNTIME_DIR/td-fetch/socket`. td-mail
  opens no socket of its own: every request goes through the service, which
  holds the TLS trust, the resolver and the timeouts. Inside a td jail the
  `sockets=fetch` grant provides it; elsewhere, serve one there or td-mail
  reports that it is missing and starts from its cache.
- A password source per account: td's credential portal for
  `secret = "portal"` (the secret stored as mail/NAME for `[account.NAME]`,
  read through the `/app/bin/td-secret` helper packaged beside td-mail, so
  inside a td jail only; submit it from the human session with `td-secret set mail/NAME < file`, then press Ctrl+Alt+Esc and W, verify the target and touch the enrolled token), or
  a non-interactive credential command for `password_command` (for example
  `pass`).

## Build

```bash
cargo build
```

Release build:

```bash
cargo build --release
```

## Install

From this repository:

```bash
cargo install --path .
```

Or use the compiled release binary directly:

```bash
./target/release/td-mail
```

## Setup

Default config path:

- `$XDG_CONFIG_HOME/td-mail/config.toml`
- Fallback: `~/.config/td-mail/config.toml`

Example config:

```toml
[ui]
page_size = 100
mouse = true
sync_interval_secs = 60

[mail]
archive_folder = "Archive"
deleted_folder = "Trash"
# Optional: override From used for draft generation
# reply_from = "Me <me@example.com>"

[account.personal]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "me@example.com"
secret = "portal"

[account.work]
well_known_url = "https://mx.work.com/.well-known/jmap"
username = "me@work.com"
password_command = "pass show email/work.com"
```

Each account sets exactly one of `secret = "portal"` or `password_command`.
Legacy fallback is supported via `[jmap]` with `well_known_url`, `username`, and one of those;
its portal credential is mail/default.

The window opens before the first account is connected, so the credential and
discovery round trips never hold it; the mailbox list loads once the
connection is decided. If the account cannot be reached (server down, network
not up yet, placeholder credentials), td-mail lists what its cache holds
instead of exiting, and everything it cannot serve from the cache says
why the connection failed (`... (offline mode: <reason>)`), as the log does;
`a` in the mailbox list, which selects the next account and with one
account reopens it, retries the connection.

Optional rules file path defaults to `rules.toml` next to your config; override with `--rules=PATH`.

## Pattern dialect

`mail.rules_mailbox_regex`, `mail.my_email_regex` and every `regex` in
`rules.toml` are **POSIX Extended Regular Expressions over bytes**, with GNU's
`\w \W \b \B` and an optional **leading** `(?i)` for case folding.

Also accepted: `\d \D \s \S` as the ASCII classes, `(?:…)` as a plain group,
`[[:word:]]` and `[[:ascii:]]`, and `\. \[ \] \( \) \| \+ \? \* \{ \} \^ \$
\\ \/ \-` as the literal character (`\t \n \r` as the byte).

Refused, each naming its byte offset in the pattern: `\uXXXX`, `\x…`,
`\p{…}`/`\P{…}` and any other unknown escape; a backreference; lookaround
(`(?=` `(?!` `(?<=` `(?<!`); a named group; a comment group; an inline flag
group that is not the leading `(?i)`; a non-greedy quantifier (`*?` `+?` `??`
`{n,m}?`); `&&` inside a bracket expression; a negated shorthand inside one
(`[\S]`); a non-ASCII character inside one; an unbalanced paren or bracket;
and a pattern over 4 KiB. A refused pattern is a configuration or rules
error naming the pattern and the reason.

Matching is POSIX **leftmost-longest**, so `x|xy` matches `xy`. `.` is one
byte and matches a newline. `(?i)`, `\w`, `\b` and the named classes are
ASCII: `(?i)é` does not match `É`. A pattern too expensive to decide counts
as no match, so a rule that cannot be evaluated does not fire.

## Run

```bash
cargo run
```

For all command-line options, run:

```bash
td-mail --help
```

## Development

```bash
cargo test
cargo clippy
cargo fmt -- --check
```
