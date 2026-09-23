# td-mail (Timmy's Mail Console)

`td-mail` is a Rust mail client (MUA) for reading and triaging email over JMAP, in a td-ui window: mailboxes, messages and threads as lists, a message read in td-editor's document view.

## Goals

- Fast, keyboard-first email workflow in a window on td's compositor: lists to move through, a document view to read in, an action bar for the pointer.
- Composition in place: a draft is edited in the window, in td-editor's document view, and retained as a file.
- Clear separation of concerns: `td-mail` reads/manages mail; message submission is the server's, through JMAP, so td-mail encodes no MIME and speaks no SMTP.
- Scriptable automation through a JSON-over-stdin/stdout CLI mode.

## What td-mail Does

- Connects to one or more JMAP accounts.
- Lists mailboxes and emails, opens message view, and shows threads.
- Supports read/unread, flag/unflag, move, archive, delete, and mailbox-wide mark-read.
- Supports compose/reply/reply-all/forward draft generation, and sends a draft through the account's JMAP server.
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
copy and paste are Ctrl-X, Ctrl-C and Ctrl-V (Ctrl-A selects all): a
selection made in any text, a message's view or an error's, is copied
to the system clipboard and kept in td-mail's own kill ring, a cut one
too, and a paste into a draft takes the system clipboard's text when it
has any and the kill ring's otherwise. The clipboard's text arrives a
moment later, into the draft it was asked for while that draft is still
the one being edited; closed, under its save question or replaced by
another draft, it goes nowhere. A refused copy or paste (a selection
past the clipboard's 1 MiB ceiling, a paste still arriving, a paste
that failed or was cancelled) is said in the status row until the next
key; a held key repeats the kill ring's copy or paste only; and a
compositor without a clipboard leaves the kill ring as the whole of
it, silently. Nothing here deletes the draft or
its attachment sidecar, and the log records both paths. Reopen the
`.eml` file with an editor or file manager; delete it explicitly when no
longer needed. The matching `td-mail-att-ID` directory belongs to
`td-mail-draft-ID.eml`: keep it while the draft needs its attachments
and remove it separately when discarding that draft. Moving only the
`.eml` file does not move or rewrite attachment references. There is no
draft-list UI or automatic expiry in this increment.

## Attaching

Ctrl-Shift-A, or the Attach label, opens a finder over the draft on the
folder a file was last attached from, else `$HOME`: Return opens a
folder or attaches the selected file, Ctrl-Return attaches it too, a
second press on a row attaches that file, Backspace on an empty filter,
Alt-Up and `^` go up, typed characters filter the names, and Escape
closes the finder with nothing attached. It lists the folders and
regular files td-mail can read (in its jail its state directory and the
Downloads grant, on a host everything), leaving out names beginning
with `.`, folders first and each sorted; a folder of more than 4096 of
them shows the first 4096 in that order, and one of more than 65536
entries is read that far. A file past the fetch service's 32 MiB
request bound is shown dimmed and cannot be chosen, since it could not
be sent. The file
chosen is copied into the draft's `td-mail-att-ID` sidecar, made
private beside the draft when it has none, read as the send reads an
attachment (a regular file, not blocking, to a byte past the bound at
most). The copy keeps the file's name, cleaned to one path component a
tag can carry, with `-2`, `-3`, … before the extension when that is
taken, the tag then naming it for the recipient as the file was. Its
`<#part>` tag, the media type from the name's extension
(`application/octet-stream` for one not known), goes on a line of its
own at the end of the draft, unsaved as any edit is. The send reads
the copy, so a file changed or removed after it was attached does not
change the message, and the copy retires with the draft to `sent`; a
copy whose tag is deleted from the draft, or undone, stays in the
sidecar unsent and retires with it all the same, so the sidecar holds
what was attached and the draft's tags say what went. A copy is also
named for the recipient without a bidirectional control a name may
carry, so it cannot show another extension than it has.
A tag written by hand still attaches the file it names, read when the
draft is sent. Nothing is attached while a send is awaited or once the
server has taken the draft.

## Sending

Ctrl-Enter, or the Send label, sends the draft being edited: it is
saved first when it has unsaved changes, then handed to the account's
server through JMAP mail submission (RFC 8621 `EmailSubmission`), which
the server must list for the account (`urn:ietf:params:jmap:submission`
among its `accountCapabilities`; Stalwart, Cyrus, Fastmail and Apache
James do). td-mail reads the draft back as message-mode wrote it: the
headers to `--text follows this line--` (or, for a plain message pasted
in, to the first empty line), of which `From`, `To`, `Cc`, `Bcc`,
`Reply-To`, `Subject`, `In-Reply-To` and `References` are sent and any
other is refused by name; the text after it; and each MML
`<#part type="…" filename="/absolute/path" …>` tag as a regular file to
attach, uploaded as a blob (the `type` a media type, `kind/subtype`;
a line quoted `<#!`, as forwarded text is written so a message cannot
name a file to attach, is sent as text with one `!` fewer, as Emacs
sends it). `From` must be one address that one of the account's
identities sends as (the server's `Identity/get`, an exact identity
before a `*@domain` one), and `To`, `Cc` and `Bcc` must name a
recipient between them. The server assembles the message from these
parts (`Email/set` creates it in the Drafts mailbox, `$draft` and
`$seen`, in the same request as `EmailSubmission/set` sends it; on
success the server drops `$draft` and moves it to the Sent mailbox; an
account without a Sent mailbox keeps it in Drafts, one without Drafts
creates it in Sent, one without either cannot send). A submission the
server refuses, by a refusal or an error of its own, has the copy
`Email/set` made removed again; a message sent but left a draft by
the server, its filing refused, is reported so ("kept in Drafts, still
a draft: …") and is sent all the same. The Date and Message-ID are
td-mail's. Attachments are refused before any upload when one is not a
regular file or is larger than the server's `maxSizeUpload` or the
fetch service's 32 MiB request bound; each is read from the file
opened, to a byte past the ceiling at most.

A send whose answer never came (the fetch service or the server not
answering, an answer that could not be read, or a `serverPartialFail`)
may have gone: it is refused with that said, and the next send of the
same draft asks the server first, by the Message-ID the attempt carried
and when it was made. The identity's submissions around then
(`EmailSubmission/query` by identity and time, a day of slack either
side for the clocks; the match is by Message-ID, the window only bounds
what is read; then `EmailSubmission/get`) are read with the messages
they sent, and the one carrying the Message-ID is the answer, its
mailbox read back. With none on record, the copies made in that window
in the mailbox the send creates in and in Sent (`Email/query` by mailbox
and time) say: one carrying the Message-ID and no longer a draft where
it was made (filed to Sent by a server that keeps no submissions) went,
and is the answer, "none on record" as its submission; one still a draft
where it was made never went, so it is removed and the draft is sent
afresh, as it is when no copy is there. Only those two mailboxes are
read, so a copy delivered to this account is never touched. (A query by
the Message-ID header would name the copy directly, but servers answer
that filter as they please: Stalwart matches nothing by it, so nothing
here rests on it.) So nothing is sent twice for want of an answer, as
long as td-mail is the same process, except on a server that keeps no
submissions and left the copy a draft; after a restart the person checks
Sent before sending the draft again. What is settled is the attempt: a
draft edited after its answer was lost and sent again is answered with
that attempt when it went, the edits not sent.

While the answer is awaited the draft is held read-only, so the file
sent is the file retired: a second send, a save and a close, the
window's included, are refused in the status row until it comes. The
wait is bounded only by the fetch service's timeouts on each request
of the send (a minute for the head, five for the origin), since the
person asked to send and is waiting on it. A refusal (the server's,
the draft's, the connection's; offline there is no queue, the send is
refused at once) is the status row's and the draft is the pane's again
to be mended. Sent, the draft and its `td-mail-att-ID` sidecar are
moved together, or not at all, to `$XDG_STATE_HOME/td-mail/sent` (the
`sent` directory beside `drafts`, made private as it is) and the view
closes; the log records the message id, the submission id, the mailbox
the copy was kept in and the retired path. A sent draft whose name is
already in `sent` is not overwritten: the draft stays open, sent and
still held, the status saying so; Send then retries the move alone,
never the send, and Close puts the draft away. Nothing deletes a
retired draft. The CLI's `send_draft` sends and retires the same way.

A reply or forward is written so it reads back as the message it came
from: a display name with a comma, quote or bracket is quoted in the
header, a control character in a decoded subject or name is a space on
its one header line, and everything the message supplies below the
separator (the forwarded header block, the "wrote:" line, the text)
has each line beginning `<#` quoted `<#!`.

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
  `sockets=fetch` grant provides it; elsewhere, `./mail` from the checkout
  serves one for its launch (below), or td-mail reports that it is
  missing and starts from its cache.
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

On a host, the one-word way is the repository root's entry script, which
builds td-mail, serves td's fetch service for this launch alone, runs
td-mail under your Wayland session and stops the service when td-mail
exits (`td-builder host-run`, APPLICATIONS.md §X.7). It is not the jail:
td-mail runs as you, with your whole privilege, and nothing confines it.
It needs cargo and a C compiler (`cc`, `gcc`, or `TD_CC_HOME`), and
crates.io the first time, for td-net's dependencies:

```bash
./mail
```

Directly, with the fetch service yours to serve
(`net/target/release/td-net fetchd run --socket "$XDG_RUNTIME_DIR/td-fetch/socket"`
after `cargo build --release --manifest-path net/Cargo.toml`), and
`CC=gcc` on a host without `cc` on `PATH`, as the linker:

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
