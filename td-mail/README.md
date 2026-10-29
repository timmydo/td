# td-mail (Timmy's Mail Console)

`td-mail` is a Rust mail client (MUA) for reading and triaging email over JMAP, in a td-ui window: mailboxes, messages and threads as lists, a message read in td-ui's editor pane.

## Goals

- Fast, keyboard-first email workflow in a window on td's compositor: lists to move through, a document view to read in, an action bar for the pointer.
- Composition in place: a draft is edited in the window, in td-ui's editor pane, and retained as a file.
- Clear separation of concerns: `td-mail` reads/manages mail; message submission is the server's, through JMAP, so td-mail encodes no MIME and speaks no SMTP.
- Scriptable automation through a JSON-over-stdin/stdout CLI mode.

## What td-mail Does

- Connects to one or more JMAP accounts.
- Lists mailboxes and emails, opens message view, and shows threads.
- Supports read/unread, flag/unflag, move, archive, delete, and mailbox-wide mark-read.
- Supports compose/reply/reply-all/forward draft generation, and sends a draft through the account's JMAP server.
- Supports optional mail rules and retention policies.
- Provides `--cli` NDJSON mode for integrations and automation.

## Searching a folder

In a message list, press `s`, enter a search, then press Enter. Escape
cancels entry; Escape after searching clears the active search. The title
shows the active query. Searches go to the account's JMAP server with the
current mailbox and date filters, and pagination retains those filters.
Matching behavior depends on the server; td-mta supports case-insensitive
substring terms and quoted phrases across its documented searchable fields.
An empty result is distinct from a refused search: server errors such as
`storageWorkLimit` or `messageInterpretation` are shown with their reason.

## Reading attachments

Press A or click Attachments in a message to open its attachment list.
Each row shows the filename, content type and size. Use Up/Down (or
j/k or n/p), the mouse wheel, PageUp/PageDown, or Home/End to select any
attachment, including attachments numbered 10 and above. Enter or Open
downloads the selected attachment through the existing open/save flow;
clicking a row does the same. Escape, q or Cancel returns to the message.
Number keys do not open attachments.

## Local drafts

Compose, reply and forward retain `.eml` drafts in
`$XDG_STATE_HOME/td-mail/drafts`, falling back to
`$HOME/.local/state/td-mail/drafts`. Only absolute state/home paths are
accepted; an empty or relative state setting falls back to HOME. Runtime
storage is not used, so logout does not intentionally discard a draft.
Missing directories are created private; an existing draft directory must
already be private and must not itself be a symlink. Files are mode 0600.
These checks are not protection against a hostile ancestor-directory owner.

The retained file is then opened in the window, in td-ui's editor
pane, editable, with paragraphs filled as they are typed: C-s writes
what is in the window over the file, whole or not at all (a private
sibling is written and renamed over the draft, so a write that fails
leaves the draft as it was, and a symlink put at the path is replaced
rather than followed), and C-w closes the draft, asking first when it
has unsaved changes (y saves, n keeps the file as it was last saved,
Escape returns to it); the Save and Close labels do the same. Closing
the window with an unsaved draft asks the same question: a save or a
discard closes the window, and Escape keeps it, so nothing typed is lost
to the close and a save that fails is shown, not skipped. While the
window holds a draft the file is its own: an edit made to it elsewhere
is overwritten by the next save. A draft larger than the document view's
ceiling (16 MiB) is retained but not opened, and the log says so. Cut,
copy and paste are C-x, C-c and C-v (C-a selects all): a
selection made in any text, a message's view or an error's, is copied to
the system clipboard and kept in td-mail's own kill ring, a cut one too.
With no selection in a draft, Copy or Cut takes the caret's whole line;
read-only message and preview views still require a selection. A paste into a
draft takes the system clipboard's text when it has any, and the kill ring's
otherwise. The clipboard's text arrives a moment later, into the draft it
was asked for while that draft is still the one
being edited; closed, under its save question or replaced by another
draft, it goes nowhere. A refused copy or paste (a selection past the
clipboard's 1 MiB ceiling, a paste still arriving, a paste that failed
or was cancelled) is said in the status row until the next key; a held
key repeats the kill ring's copy or paste only, never a cut; and a compositor
without a clipboard leaves the kill ring as the whole of it, silently. Nothing
here deletes the draft or its attachment sidecar, and the log records
both paths. D on the mailbox list (the Drafts label) lists the drafts
retained and, after them, those sent to `sent` beside them, each newest
first, with its Subject, To and when it last changed. Return, or a
press, on a draft reopens it in the window to edit, with its attachment
sidecar when it has one, so Send and Attach work as on a new draft; on a
sent one it shows the file as it was retired, read-only. g reads the
directories again, as closing a view over the list does, so a draft sent
meanwhile is listed as sent, the selection kept on it. Only regular
files named `td-mail-draft-ID.eml` are listed, the newest 1000 of the
first 8192 names read from each directory (the status row says when it
cut one short), and a directory that cannot be read is named in the
status row. A draft with a send on record that has not settled (its
`.lost` record is beside it: an answer lost, or a send that went before
the retire) is marked `send unsettled`, and the status row says that its
next Send asks the server first and, when that send went, sends none of
the edits made since. The same file can also be opened with an editor or
file manager; delete it explicitly when no longer needed. The matching
`td-mail-att-ID` directory belongs to `td-mail-draft-ID.eml`: keep it
while the draft needs its attachments and remove it separately when
discarding that draft. Moving only the `.eml` file does not move or
rewrite attachment references. Nothing expires a draft.

## Attaching

C-S-a, or the Attach label, opens a finder over the draft on the
folder a file was last attached from, else `$HOME`: Return opens a
folder or attaches the selected file, C-Return attaches it too, a
second press on a row attaches that file, Backspace on an empty filter,
M-Up and `^` go up, typed characters filter the names, and Escape
closes the finder with nothing attached. It lists the folders and
regular files td-mail can read (in its jail its state directory and the
Downloads grant, on a host everything), leaving out names beginning with
`.` until C-h shows them (C-h again leaves them out, the folder
listed again with the filter cleared and the selection kept, the status
row saying which; the choice holds for the next finder while td-mail
runs), folders first and each sorted; a folder of more than 4096 of them
shows the first 4096 in that order, and one of more than 65536 entries
is read that far. A file past the fetch service's 32 MiB request bound
is shown dimmed and cannot be chosen, since it could not be sent. The
file chosen is copied by the backend, off the window's thread, into the
draft's `td-mail-att-ID` sidecar, made private beside the draft when it
has none, read as the send reads an attachment (a regular file, not
blocking, to a byte past the bound at most). When connected the bound is
also what the account's server takes (its `maxSizeUpload`), so a file it
would refuse at send is refused at attach instead, the reason in the
status row and nothing copied; while the connection is still being made,
the copy waits for it, so that it knows the limit, and a copy queues
behind the backend's work in hand. While the copy is made the draft can
be edited but not sent, closed (the window's close included) or attached
to again, and when the tag goes in the caret and any selection stay with
the text they were on (a caret where the tag goes keeping to its own
line, and a range ending there staying before the tag). The copy keeps
the file's name, cleaned to one path component a tag can carry, with
`-2`, `-3`, … before the extension when that is taken, the tag then
naming it for the recipient as the file was. Its `<#part>` tag, the
media type from the name's extension (`application/octet-stream` for one
not known), goes on a line of its own when the copy is made: with the
caret in the body, at the caret's line (before it when the caret starts
it, else after it, and past the `<#/part>` of a tag the caret is on or
whose close it starts; of a selection, its lower end's line), and with
the caret in the headers at the end of the draft, unsaved as any edit
is. The CLI's `attach_file` appends it to a retained draft's end, saving
the tag into it; an answer carrying a `warning` took the tag but could
not confirm the save durable, and keeps the copy. The send reads the
copy, so a file changed or removed after it was attached does not
change the message, and the copy retires
with the draft to `sent`; a copy whose tag is deleted from the draft, or
undone, stays in the sidecar while the draft is one and is removed when
it is sent, so the retired sidecar holds what went. A copy is also named
for the recipient without a bidirectional control a name may carry, so
it cannot show another extension than it has. A tag written by hand
still attaches the file it names, read when the draft is sent. Nothing
is attached while a send is awaited or once the server has taken the
draft.

## Sending

C-Return, or the Send label, sends the draft being edited: it is
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
creates it in Sent). Roles take precedence; otherwise td-mail uses a unique
root folder named Drafts or Sent without a role, case-insensitively. If
neither exists, a fresh send creates Sent and confirms it by listing the
folders again and matching the returned ID to a reusable Sent destination
before creating the message. Ambiguous names with no usable destination
refuse the send; they do not cause another folder to be created.
Retries check submission history first, then the folder IDs recorded for
the original attempt. Renaming folders cannot redirect this check. A missing
recorded folder, or a legacy record without folder IDs and without a matching
submission, refuses automatic retry and names the record for manual recovery.
Folder refusal leaves the local draft intact. A submission the
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
same draft asks the server first, in this td-mail or after a restart, by
the Message-ID the attempt carried and when it was made. The identity's
submissions around then (`EmailSubmission/query` by identity and time, a
day of slack either side for the clocks; the match is by Message-ID, the
window only bounds what is read; then `EmailSubmission/get`) are read
with the messages they sent, and the one carrying the Message-ID is the
answer, its mailbox read back. With none on record, the copies made in
that window in the mailbox the send creates in and in Sent
(`Email/query` by mailbox and time) say: one carrying the Message-ID and
no longer a draft where it was made (filed to Sent by a server that
keeps no submissions) went, and is the answer, "none on record" as its
submission; one still a draft where it was made never went, so it is
removed and the draft is sent afresh, as it is when no copy is there.
Only those two mailboxes are read, so a copy delivered to this account
is never touched. (A query by the Message-ID header would name the copy
directly, but servers answer that filter as they please: Stalwart
matches nothing by it, so nothing here rests on it.) The attempt is
recorded beside the draft before the request goes
(`td-mail-draft-ID.eml.lost`, private: the Message-ID, when, and the
identity) and removed when the server refuses it or when the draft,
sent, is retired to `sent` (a draft outside a `drafts` directory, which
is never retired, has its record removed once the server answers that
it went), so an answer lost to td-mail ending
mid-request, or td-mail ending before the retire, or a retire that
fails, is asked about too; a record that cannot be kept refuses the send
before anything goes, and one that cannot be read refuses it, named,
until the person, having checked Sent, removes it. So nothing is sent
twice for want of an answer, except on a server that keeps no
submissions and left the copy a draft, or when two td-mails send the
same draft at the same moment, which nothing guards. What is settled is
the attempt: a draft edited after its answer was lost and sent again is
answered with that attempt when it went, the edits not sent.

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
closes. Once both have moved, the retired draft names its attachments
where they now are: each tag whose `filename` is inside the sidecar,
however the path is spelled, is pointed into `sent` and the draft
replaced whole (a tag naming a file elsewhere, or climbing out of the
sidecar with `..`, is left as written); then a file in the sidecar no
tag names, which did not go, is removed, matched by what each tag
opens rather than by its spelling, and only when the draft reads back
as a send and every tag it names is found. Both are best effort and
logged, since the move has happened. The log records the message id,
the submission id, the mailbox the copy was kept in and the retired
path.
A sent draft whose name is already in `sent` is not overwritten: the
draft stays open, sent and still held, the status saying so; Send then
retries the move alone, never the send, and Close puts the draft away.
Nothing deletes a retired draft. The CLI's `send_draft` sends and
retires the same way.

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
  `sockets=fetch` grant provides it; elsewhere, the td-mail
  `./install-apps` installs serves one for its launch (below), or td-mail
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

As the XDG base directory specification says, a relative or empty
`XDG_*` value is ignored, and `HOME` counts only when absolute; with
neither, td-mail names no default configuration, keeps no cache, log or
spam model, and never falls back to the working directory or a shared
`/tmp` directory.

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
reply_from = "Me <me@work.com>"
archive_folder = "Old Mail"

[retention.work-archive]
folder = "Old Mail"
days = 730
account = "work"
```

An `[account.NAME]` section may set `[mail]`'s `reply_from`,
`archive_folder`, `deleted_folder`, `archive_mailbox_id` and
`deleted_mailbox_id` for that account alone; what it leaves unset is
`[mail]`'s, except that an account setting a folder or its mailbox id
takes no mailbox id for it from `[mail]`, since a mailbox id names a
mailbox on one server. The mailbox ids are read by `--cli` alone.

A `[retention.NAME]` policy with `account = "NAME"` is that account's
alone, and one without it applies to every account. A rule in
`rules.toml` takes `account = "NAME"` the same way. The legacy `[jmap]`
section's account is named `default`. A policy or rule naming an
account the configuration does not have is refused: the configuration
does not load, and the rules file is not used. So is an unknown key in
a rule, its actions or its triage, so a misspelled `account` cannot
leave a rule running for every account.

For a private HTTPS server, an account may set
`tls_ca_file = "/absolute/path/to/ca.pem"`. The file contains public CA
certificates in PEM format (1–65536 bytes), never a private key. td-mail
reads it when connecting the account; reconnect after replacing it.
For HTTPS requests to the origin of that account's `well_known_url`
(scheme, hostname and port), these certificates replace the public roots.
Certificate signatures, expiration and hostname checks remain enabled.
Other origins, including redirect destinations and separately hosted JMAP
API or attachment endpoints, use the normal public roots. Configure the
final discovery URL directly if a private server redirects to another
origin. Accounts without this setting are unchanged. An unreadable or
invalid bundle fails the connection; there is no fallback to public roots
for the configured origin and no certificate-verification bypass.

The file must be readable by td-mail itself; inside a jail, use a path
visible in that application's configuration directory. The fetch service
receives bounded certificate bytes, not a host pathname. This option does
not change its destination policy: the td image service still refuses
loopback. The host launcher installed by `./install-apps` permits local
mail-server requests while keeping public-content fetches off loopback.

Each account sets exactly one of `secret = "portal"` or `password_command`.
Legacy fallback is supported via `[jmap]` with `well_known_url`, `username`, and one of those;
its portal credential is mail/default.

The window opens before any account is connected, so the credential and
discovery round trips never hold it; the mailbox list loads once the
connection is decided. Every account is opened at start and stays open,
each with its own backend: one connection is made at a time, the shown
account's first. With two or more accounts the window keeps an account
tree at its left: "All inboxes" first, with every inbox's unread count,
then each account's heading, with its inbox's unread count (`...` until
its folders are listed, `offline` when the fetch failed), and its
folders under it. "All inboxes" opens the unified inbox: every
account's inbox in one list, newest first, each row naming its account.
Return reads a message on its own account (it is marked read there, a
reply or a draft from it is that account's, and `n`/`p` there step
through that account's messages); `a` and `d` move it to its own
account's archive or trash, `u` and `f` toggle read and flagged, each put
back if that account refuses; `g` asks every account again, and
returning from a message asks its account. A draft composed from the
unified inbox itself is from the account last shown, which the status
row names. Each account's inbox is fetched a page at a time, and the
list stops where an account with older mail still unfetched stops (the
count reads `n/m+`), so nothing older of another account is missing
above the last row; moving to the last row fetches that account's next
page. Tab moves the keys to the tree and back
(Escape or `q` give them back too, and any other key gives them back
and acts in the view); `n`/`p`, the arrows, the page keys, Home and End
move in it, and Return, Space or a press opens an entry in place of the
views: a heading opens the account's own page (its folder list, where it
is set up and reconnected), a folder opens that folder's mail over it,
so `q` returns to the account's page. The account already shown keeps
its page and what it cached; another's page starts from the folders the
tree holds. Opening is refused while a draft is open, and the tree never
has the keys while one is edited. The tree draws its cursor only while
it has the keys, and taking them scrolls it into view. `u` in the tree
marks every message in the selected folder read, as `u` (Mark read) on
an account's page does; the folder's list and the unified inbox show
them read when the account answers. The wheel scrolls whichever side
the pointer is over, the tree's rows without moving its selection,
whichever side has the keys; a press on the tree's scrollbar track
pages it and its thumb drags. The tree's right edge (its last few
pixels, beside the scrollbar) drags to resize it, and `<` and `>` in
the tree narrow and widen it, from 12 cells to half the window; a name
too long for the tree is cut short with `…` a cell before its unread
count. A window narrower than three default trees (72 cells) shows the
tree over the views only while it has the keys, and has no width to
change. Switching accounts connects nothing again; the tree's counts
follow each account's folder fetches,
and with `sync_interval` set, every idle interval asks every account,
whatever view is shown. If an account cannot be reached
(server down, network not up yet, placeholder credentials), td-mail lists
what its cache holds instead of exiting, and everything it cannot serve
from the cache says why the connection failed
(`... (offline mode: <reason>)`), as the log does; `R` in the mailbox list
asks for the shown account's connection again, ahead of any account still
waiting its turn; an account already connected goes on serving with its
connection meanwhile and refreshes its folders once the new one is made.
Every account's backend scores and trains the one spam model, so
training in one account is not lost to another's save. Each account
costs the window a thread, and the packaged jail caps td-mail's tasks at
32, so a configuration of more than about twenty accounts fails to
start there.

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

On a host, install it with the repository root's `./install-apps`, which
builds it with the other desktop programs and puts `td-mail` in
`~/.local/bin` as a link to td-net's launcher. Run that way, it serves
td's fetch service for this launch alone, runs td-mail under your
Wayland session and stops the service when td-mail exits
(APPLICATIONS.md §X.7). It is not the jail: td-mail runs as you, with
your whole privilege, and nothing confines it. It needs cargo and a C
compiler (`cc`, `gcc`, or `TD_CC_HOME`), and crates.io the first time,
for td-net's dependencies:

```bash
./install-apps
td-mail
```

Directly, with the fetch service yours to serve
(`net/target/release/td-net fetchd run --socket "$XDG_RUNTIME_DIR/td-fetch/socket"`
after `cargo build --release --manifest-path net/Cargo.toml`), and
`CC=gcc` on a host without `cc` on `PATH`, as the linker:

```bash
cargo run
```

In the window, F1, or `?` in a view, shows the window's key list over
the frame: the shown view's keys first, then every other view's and the
window's own; F1, Escape or a click closes it. The Help label on the
bars of the mailbox and email lists, a message, a thread, the drafts, a
sent draft and a draft being edited shows it too; in a draft, where `?`
types, the label opens the list without typing. Only a press of the
window's own pointer does: td-mail has no control socket. Its rows are
`src/keybindings.rs`'s tables, spelled as td-ui's keymap spells chords
(`C-S-a`, `PageDown`, `1..9`), which the CLI's `keybindings` command
exports too, so the two cannot drift apart.

For all command-line options, run:

```bash
td-mail --help
```

## Unreadable messages

A `serverFail/messageInterpretation` response from td-mta triggers bounded
single-message isolation. The unreadable message retains its stored identity,
filing and date. Its subject and body remain absent; the window shows an
unreadable notice and CLI JSON sets `unreadable: true`. Rules, spam annotation
and automatic triage exclude it, and a later online open retries its content.
Authentication, transport and other storage errors remain errors. A
`serverFail/storageWorkLimit` on a multi-message read splits its ID batch
in half, preserving the requested fields and order. Splitting stops at one
message: a singleton refusal is still an error, never an unreadable placeholder.
Each original batch has at most 50 IDs, so work-limit recovery performs at
most 99 read requests, including the original, and logs every split. Interpretation refusal during
recovery still uses the separate single-message isolation below. Only
read projections use this recovery; mutations are never replayed by it.

CLI JSON also exposes the original `blob_id`. Pass that value to
`download_attachment` with a unique `name` ending in `.eml` and
`content_type: "message/rfc822"` to save the exact original bytes. An existing filename is refused.

Each refresh currently repeats isolation: one unreadable message in a full
50-message batch costs 52 requests. This bounded linear recovery is the
initial read milestone, with no negative-result cache or bisection yet.
Ordinary message reads use batches of 50 IDs. Recovery adds at most one full
and one stored-only request per ID after the failed batch; additional header
properties are omitted from stored-only recovery. Thread reading uses
`Thread/get` membership followed by those batches, without a 500-message cut. Raw spam-training fetches still require UTF-8;
that pre-existing limitation is separate from byte-exact original export.

## Read-only server accounts

A connected account advertised as JMAP `isReadOnly` refuses mutation batches,
uploads and sending before transport. Automatic filing rules do not run, new
online mutations are not queued or projected into the cache, and previously
queued operations remain retained without replay. Reading, search, raw messages
and attachment downloads remain available. These client guards complement the
server's authorization checks; they do not grant authority.

The window title identifies a selected read-only account; the unified inbox
names its read-only accounts. Opening or navigating to unread messages leaves
them unread. Explicit write commands still report the read-only refusal.
The cache retains the last discovered account policy across offline restarts
and refuses offline queueing for a known read-only account. Discovery updates
that policy when reconnecting. A cache write failure is logged; a missing or
recreated cache has no retained policy. The server remains authoritative.
Use a separate configured account name/cache for a local replica.

## Development

```bash
cargo test
cargo clippy
cargo fmt -- --check
```

The window's one native case, `tests/control_process.rs`, launches
td-mail offline under the real headless td-compositor with its home,
state and configuration in a private directory, composes with `c`,
attaches with C-S-a, a filter and Return in the finder, saves
with C-s, and reads the draft, its tag and the sidecar's copy back
from disk; td-mail has no control socket, so the files are the oracle.
It is `#[ignore]`d for plain `cargo test`; `ready` runs it, and so does:

```bash
target/release/td-builder gate-crates native-compositor --manifest-path td-mail/Cargo.toml
```

Read requests retry HTTP 429 or 503 at most four times when Retry-After is a
decimal delay of one through five seconds. Each pause is at least the supplied
delay and backs off through one, two, four and five seconds, allowing a busy
mailbox read to finish. A 503 without Retry-After uses the same backoff.
The pauses total at most twenty seconds, in addition to the bounded HTTP
attempts. This covers authenticated read bursts and the handoff between API
reads and downloads.
GET discovery/downloads and explicit Mail read batches use this bound; mutation
batches, transport failures and other statuses remain errors, as do 429 without
a supported delay, 503 with an unsupported supplied delay, and continued pressure. Delays are synchronous; ordinary mailbox
reads wait on the backend thread.

Read/unread, star and mailbox move updates require success or `notFound` for
every requested message before the client retires the queued change. Per-
message refusals or missing outcomes keep it queued for reconnect replay.
Earlier remote successes remain applied; retrying these flag or destination
assignments is idempotent. The existing optimistic local display and offline
queue remain in effect. Mailbox-wide mark-read freezes message IDs before
queueing (the cached IDs when offline, which may cover only part of the
mailbox). An online ID-query failure returns an error before queueing, so the
user can retry that action. Batch progress removes confirmed IDs durably; a
per-message refusal does not prevent later batches from running. An explicit
`notFound` resolves a flag or move assignment because the message is gone. New
flag assignments atomically supersede older queued assignments to the same
flag and message. A new move atomically supersedes older queued destinations
for the same message, including pending rule actions. Older thread moves and
unplanned rule runs must resolve on reconnect before a new move can be queued. Other refusals, including a missing
move destination, remain queued under the existing stop-on-failure replay
policy. They are logged and can hold later queued work;
this flow does not silently discard user intent. Folder deletion and recovery
from a permanently removed destination require a separate queue-policy
increment.
Legacy mailbox-wide entries freeze their selections before any reconnect
replay effects; new seen assignments are rejected while such an entry still
has no selection. Before replay effects, rules persist their concrete action list in the older
parent queue entry. A separate durable cursor retires completed actions without rewriting the
plan after every action, so a move cannot
hide a later failed flag action and replay cannot supersede newer queued user
choices. New flag choices also prune matching pending rule actions.
Bulk mark-read uses the discovered `maxObjectsInSet`, capped at 256 messages
per request (also the default when absent). Zero, negative, and non-integer
limits refuse discovery. Other mutation flows keep their existing batching
and result policy.

Mailbox creation requires a matching Mailbox/set account and call ID, an
explicit created.newMailbox.id, and no refusal for that creation key before
it reports success. td-mta supports the name-only root folders this client
emits; Archive and Trash can be created through the client and selected by
their configured names. A duplicate name is reported as a refusal. Creation
is an online operation, not a durable queued assignment.

Permanent deletion confirms the matching Email/set call, account and each
requested destroyed ID (or explicit per-object notFound). A method error,
missing result or different account leaves durable intent queued; replay no
longer interprets words such as "unknown" as proof of deletion. Requests obey
the server's maxObjectsInSet. td-mta accepts pure destruction requests and
atomically removes relationships and unowned bodies; shared or leased bodies
remain until their last owner releases them.

For a local td-mta account, run `./install-apps` and then ordinary `td-mail`.
The installed launcher permits loopback for mail-server requests, including
local discovery and attachment downloads. Public-content requests retain
loopback refusal. Your account's `tls_ca_file` still scopes certificate trust;
no shell function or special launch command is needed. This host-only policy
does not change the td image's confined fetch service.

For an explicit diagnostic launch with the broader loopback exception:

```text
~/.local/lib/td/td-net launch --allow-loopback ~/.local/lib/td/td-mail --config=/absolute/path/to/mail.toml
```

Add `--cli` for the JSON-line interface. The launcher preserves the Wayland
connection and stops its fetch service when td-mail exits. The option permits
loopback for this application's requests; certificate verification
and the account's `tls_ca_file` origin scope still apply. Prefer the ordinary
`~/.local/bin/td-mail` launcher, which keeps public-content loopback refusal.

### Uncached mailbox loading

Mailbox lists outside the configured automatic-rule folder scope fetch headers
and stored metadata first,
without downloading previews, MIME part lists or full message bodies. After
the interactive listing is delivered, the backend downloads missing bodies
for offline reading, one message between checks for queued commands. Opening
a message fetches its body on demand. Loaded bodies remain cached when
later summary refreshes update flags and folder membership. Offline lists can
show cached summaries, but opening a summary whose body was never downloaded
reports that the message is not cached instead of displaying an empty body.
Background caching is best effort: it keeps at most 1024 pending IDs per
account, dropping the oldest pending IDs when full, and stops on a fetch or
cache-write failure until another listing schedules work. Closing the client
discards pending work. A command arriving during a body request waits for that
request and its existing retry/deadline policy; background reads do not mark
messages read, run rules, or replace local flags and folder membership.
Automatic rules retain full-message input in matching folders, even for
`headers_only` queries. Thread and keyword lookups use at most 256 IDs per call.
CLI queries retain full bodies unless
`headers_only` is requested.

An empty, unfiltered interactive folder view shows its first summary batch
(up to 50 messages) while later batches and thread counts are still loading.
This preview does not advance pagination or mark the folder refreshed; the
final response replaces it. Existing rows and filtered views keep their
normal refresh behavior. CLI commands still receive one complete response.
The diagnostic log records first-summary count and elapsed milliseconds.


### Diagnostic logs

`$XDG_STATE_HOME/td-mail/td-mail.log` (normally
`~/.local/state/td-mail/td-mail.log`) records JMAP method names, HTTP attempt
latency, status, request/response byte counts, total transport latency and
retry waits. Status zero means a transport error. Timings include fetch-service
work and network/server time; they do not identify server stages by themselves.
Routine transport timing entries omit API payloads and credentials. Other
messages can include account/server names, message IDs, paths, browser URLs
and provider or password-command error text; the log is not a complete
redaction boundary. The file is created with mode 0600, and opening an older
log corrects its permissions to 0600.

Writers serialize with a process-shared file lock, including warnings and
errors; contention waits instead of silently skipping an entry. Disk/lock
failures report entries to stderr, including entries after failed startup
initialization. Each message is capped at 16 KiB before its timestamp and
level are added.
At 16 MiB, in-place compaction retains up to the most recent 8 MiB of complete
lines and adds a marker. This also bounds an oversized old log on its next
write. Compaction keeps the inode shared by running writers; it is not an
audit archive or crash-durable storage. Copy logs before reproducing a problem
if older history needs preservation.
