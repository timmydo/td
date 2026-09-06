# AGENTS.md

This file provides guidance for coding agents working in this repository.

## Project Overview

**tmc** (Timmy's Mail Console) is a Rust TUI MUA that reads mail via JMAP.
It follows a Unix model: compose/reply opens `$EDITOR`; tmc does not submit mail.

## Build / Run / Test

```bash
CC=gcc cargo build
CC=gcc cargo run
CC=gcc cargo test
CC=gcc cargo clippy
cargo fmt -- --check
```

The `CC=gcc` prefix is required because the `ring` crate needs a C compiler
and the system does not have `cc` on `$PATH` (Guix provides `gcc` instead).

Runtime options:

```bash
tmc --help
tmc --log
tmc --cli             # JSON-over-stdin/stdout CLI mode (NDJSON)
tmc --help-cli        # print CLI protocol documentation
tmc --prompt=config   # print AI-friendly prompt for generating config
tmc --prompt=rules    # print AI-friendly prompt for generating rules
```

## Configuration

Default path: `$XDG_CONFIG_HOME/tmc/config.toml` (or `~/.config/tmc/config.toml`).

Supported config styles:

```toml
[ui]
editor = "nvim"
page_size = 100
scrolloff = 3
mouse = true
sync_interval_secs = 60

[account.personal]
well_known_url = "https://mx.example.com/.well-known/jmap"
username = "me@example.com"
password_command = "pass show email/example.com"

[account.work]
well_known_url = "https://mx.work.com/.well-known/jmap"
username = "me@work.com"
password_command = "pass show email/work.com"
```

Legacy fallback is still supported via `[jmap]` with the same three fields.

Credentials are fetched by running `password_command`; there is no interactive password prompt.

## Architecture

- `src/main.rs`: CLI flags (`--help`, `--log`, `--cli`, `--help-cli`, `--prompt=TOPIC`), config loading, first-account connect, TUI bootstrap.
- `src/config.rs`: `[ui]`, `[jmap]` and `[account.NAME]` read through `src/toml.rs`.
- `src/backend.rs`: single backend worker thread + `mpsc` command/response channels.
- `src/jmap/client.rs`: blocking JMAP client over td's fetch service (`src/td_fetch.rs`), discovery + mail operations.
- `src/jmap/types.rs`: JMAP models, read and written through `src/json.rs`.
- `src/tui/`: raw terminal setup through `src/term_sys.rs` (the size is polled per input tick, with no SIGWINCH handler), input parsing, view stack, mailbox/email/help views.
- `src/cli.rs`: JSON-over-stdin/stdout CLI mode (NDJSON protocol), alternative UI reusing the same backend thread.
- `src/keybindings.rs`: centralized keybinding dictionary (`KeyBinding` struct + `all_keybindings()`), used by CLI export and `--help-cli`.
- `src/compose.rs`: compose/reply/forward draft generation and secure temp draft files.
- `src/spam.rs`: self-contained Bayesian spam classifier (tokenizer + Robinson-Fisher scoring + on-disk model). No JMAP/TUI deps.
- `src/log.rs`: file logging and `--log` support.

### Dependencies

tmc is `std` plus shared td modules and nothing else: no `[dependencies]`,
no `[dev-dependencies]`, and one package in `Cargo.lock`. The crate root
denies `unsafe_code`; the only allowance is the syscall entry point inside
`src/term_sys.rs`, and that module's own test refuses a second one.

`src/json.rs`, `src/toml.rs`, `src/regex.rs`, `src/html.rs`, `src/kv.rs`,
`src/civil.rs`, `src/b64.rs`, `src/term_sys.rs`, `src/testing.rs` and
`src/td_fetch.rs` are copies of td's shared modules, and
`tests/mock_fetch.rs` is a copy of the shared test double. They are
byte-identical to their masters and are never edited here: a change one of
them needs is made upstream and re-copied, so tn and td carry the same text.

### Threading model

- UI loop runs on the main thread (TUI) or reads stdin line-by-line (CLI).
- JMAP operations run on one backend thread.
- Both TUI and CLI communicate with backend over `std::sync::mpsc` using the same `BackendCommand`/`BackendResponse` enums.
- TUI applies optimistic updates for some actions (read/unread, flag, move) before backend confirmation.
- CLI blocks synchronously on `resp_rx.recv()` for each command.

### Spam classification

- `src/spam.rs` holds a Bayesian classifier with a JSON model at `$XDG_DATA_HOME/tmc/spam-model.json` (data dir, not cache — it is user-trained and must survive cache clears). The model is owned by the backend thread.
- During sync, new **INBOX-only** messages (gated by the `inbox` mailbox role) are scored before `apply_rules`. The score is injected into `email.extra` as synthetic headers `X-Tmc-Spam-Score` / `X-Tmc-Spam-Verdict`, so the existing rules engine acts on them via rules.toml. The classifier never moves/deletes mail itself.
- Cold-start gate: until `[spam] min_training` messages of *each* class are trained, the verdict is always `unsure` and scoring is skipped entirely (no raw-message fetches).
- Training is driven by `J` (spam) / `H` (ham) in the email view via `BackendCommand::TrainMessage`, which fetches the raw message, updates the model, and persists it atomically.
- Tunables live in `[spam]` (see `tmc --prompt=config`); rule integration is documented in `tmc --prompt=rules`.

### CLI mode (`--cli`)

An alternative UI that speaks NDJSON (one JSON object per line) over stdin/stdout. It reuses the same backend thread and `BackendCommand`/`BackendResponse` protocol as the TUI, making it suitable for programmatic interaction and integration testing.

Supported commands: `list_accounts`, `connect`, `status`, `list_mailboxes`, `create_mailbox`, `delete_mailbox`, `query_emails`, `get_email`, `get_thread`, `mark_read`, `mark_unread`, `flag`, `unflag`, `move_email`, `archive`, `delete_email`, `destroy`, `mark_mailbox_read`, `get_raw_headers`, `download_attachment`, `compose_draft`, `reply_draft`, `forward_draft`, `train`, `keybindings`.

Response envelope: `{"ok": true, ...data}` or `{"ok": false, "error": "message"}`.

Context control for email viewing: `max_body_chars` (truncate body), `headers_only` (omit body/preview).

## Implemented User Flows

- Mailbox list (`Mailbox/get`) with role-aware sorting and unread counts.
- Email list (`Email/query` + `Email/get`) with per-mailbox search.
- Email view (`Email/get`) with plain text body rendering.
- Compose / reply / reply-all / forward via `$EDITOR` on temp draft files.
- Mark read/unread, flag/unflag, move to mailbox (`Email/set` variants).
- Multi-account switching (`a`) from mailbox view.
- Mouse support (click select/open, wheel scrolling) for list/help views.
- CLI mode (`--cli`): all of the above operations available via JSON commands over stdin/stdout.

## Keybindings (implemented)

- Global: `?` help, `c` compose.
- Mailbox list: `q`, `n/p`, `j/k`, arrows, `RET`, `g`, `a`, mouse click/wheel.
- Email list: `q`, `n/p`, `j/k`, arrows, `RET`, `g`, `f`, `u`, `m`, `s`, `Esc` (clear search), mouse click/wheel.
- Email view: `q`, `n/p`, `j/k`, arrows, `PgUp/PgDn/Space/Home/End`, `r`, `R`, `F` (forward as `message/rfc822` attachment, preserves HTML), `f` (forward as inline quoted text), `h` (toggle HTML/plain text body), `v`, `*` (flag), `u`, `c`, `a` (archive), `d` (delete), `m` (move), `J` (mark spam: train + move to Junk), `H` (mark not-spam: train + move to Inbox), `A` (attachments), `D` (expire).
- Help view: `q`/`?`/`Esc` close + navigation keys.

## Constraints and Non-Goals

- No IMAP/POP/mbox/Maildir support.
- No built-in editor.
- No HTML rendering beyond preview/plain-text fallback.
- No send path in tmc (submission is external).

## Commit Policy

- Agent-created commits must include a `Co-Authored-By:` trailer.
- Run `cargo fmt` and wait for it to complete before committing changes.
- This repo does **not** use pull requests. Commit straight to `master` and
  `git push origin master` (fast-forward). Do not open PRs or leave work on
  long-lived feature branches.

## Current Gaps (as of code in this repo)

- Some UI actions are optimistic and do not show explicit failure state on backend errors.
