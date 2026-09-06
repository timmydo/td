# td-mail (Timmy's Mail Console)

`td-mail` is a Rust terminal mail client (MUA) for reading and triaging email over JMAP.

## Goals

- Fast, keyboard-first email workflow in a terminal UI.
- Unix-friendly composition flow: drafts open in `$EDITOR`.
- Clear separation of concerns: `td-mail` reads/manages mail; message submission is external.
- Scriptable automation through a JSON-over-stdin/stdout CLI mode.

## What td-mail Does

- Connects to one or more JMAP accounts.
- Lists mailboxes and emails, opens message view, and shows threads.
- Supports read/unread, flag/unflag, move, archive, delete, and mailbox-wide mark-read.
- Supports compose/reply/reply-all/forward draft generation.
- Supports optional mail rules and retention policies.
- Provides `--cli` NDJSON mode for integrations and automation.

## Requirements

- Rust toolchain (stable) with Cargo. No crates at all: td-mail is `std` alone,
  and `Cargo.lock` lists one package, td-mail itself.
- A JMAP server/account.
- td's fetch service, listening at `$XDG_RUNTIME_DIR/td-fetch/socket`. td-mail
  opens no socket of its own: every request goes through the service, which
  holds the TLS trust, the resolver and the timeouts. Inside a td jail the
  `sockets=fetch` grant provides it; elsewhere, serve one there or td-mail
  reports that it is missing and starts from its cache.
- An editor available via `$EDITOR` (for compose/reply/forward flow).
- A password source per account: a non-interactive credential command for
  `password_command` (for example `pass`), or a file for `password_file`
  (read directly, no shell involved; keep it mode 0600).

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
editor = "nvim"
page_size = 100
scrolloff = 3
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
password_command = "pass show email/example.com"

[account.work]
well_known_url = "https://mx.work.com/.well-known/jmap"
username = "me@work.com"
password_command = "pass show email/work.com"
```

Each account sets exactly one of `password_command` or `password_file`.
Legacy fallback is supported via `[jmap]` with `well_known_url`, `username`, and one of those.

If the first account cannot be reached at startup (server down, network not
up yet, placeholder credentials), td-mail starts offline from its cache instead
of exiting; selecting the account again retries the connection.

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
