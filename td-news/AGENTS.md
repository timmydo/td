# AGENTS.md

This file provides guidance for coding agents working in this repository.

## Project Overview

**tn** (Timmy's News) is a Rust TUI news reader that fetches RSS/Atom feeds, caches articles
in a local database, and presents them in a terminal interface. It is modeled
after [tmc](../tmc) (Timmy's Mail Console) but reads news feeds instead of JMAP
mail.

The current RSS workflow it replaces:
- `feed2maildir` fetches RSS feeds into Maildir format
- `notmuch` indexes and tags them
- `rss_digest.py` generates an HTML digest of unread items

This project consolidates that pipeline into a single Rust binary with a TUI.
It also supports a non-interactive JSON-over-stdin/stdout CLI mode for scripting.

## Build / Run / Test

```bash
CC=gcc cargo build
CC=gcc cargo run
CC=gcc cargo run -- --help-cli
CC=gcc cargo run -- --cli --offline
CC=gcc cargo test
CC=gcc cargo clippy
cargo fmt -- --check
```

The `CC=gcc` prefix is required because the `ring` crate (via `ureq`) needs a C
compiler and the system does not have `cc` on `$PATH` (Guix provides `gcc`
instead).

Useful runtime flags:
- `--offline` — disable fetching and browse cached data.
- `--cli` — newline-delimited JSON protocol on stdin/stdout; implies `--offline`.
- `--help-cli` — comprehensive command/protocol docs for `--cli`.
- `--cache=PATH` (or `--cache PATH`) — use alternate redb file location.

## Configuration

Default path: `$XDG_CONFIG_HOME/tn/config.toml` (or `~/.config/tn/config.toml`).

```toml
[ui]
page_size = 100
scrolloff = 0                  # min context rows above/below selection
mouse = true
sync_interval_secs = 300           # how often to re-fetch feeds (default: 300; at least 30; 0 turns it off)

[theme]
bg = "#002b36"
fg = "#839496"
bold_fg = "#93a1a1"
selection_bg = "#073642"
selection_fg = "#eee8d5"
status_bg = "#586e75"
status_fg = "#eee8d5"
header_fg = "#268bd2"

[[feed]]
name = "Hacker News"
url = "https://news.ycombinator.com/rss"
```

## Architecture

Follows the same multi-threaded pattern as tmc:

```
┌──────────────────────────┐
│  TUI (main thread)       │
│  Input handling + render  │
└────────────┬─────────────┘
             │ mpsc channels
             v
┌──────────────────────────┐
│  Backend worker thread   │
│  - RSS fetch & parse     │
│  - Cache management      │
│  - Article storage       │
└────────────┬─────────────┘
        ┌────┴────┐
        v         v
   ┌────────┐ ┌────────┐
   │  HTTP  │ │  redb  │
   │ (ureq) │ │ cache  │
   └────────┘ └────────┘
```

### Source layout

- `src/main.rs` — CLI flags, config loading, mode dispatch (TUI vs CLI).
- `src/cli.rs` — newline-delimited JSON command handler for automation/integration use.
- `src/config.rs` — TOML config parsing for `[ui]`, `[theme]`, and `[[feed]]`.
- `src/backend.rs` — worker thread + `mpsc` command/response channels.
- `src/feed.rs` — RSS/Atom fetching and parsing (using `ureq` + XML parsing).
- `src/cache.rs` — local cache layer (redb): articles, feed metadata, read state.
- `src/tui/` — raw terminal setup, input parsing, view stack.
  - `tui/mod.rs` — event loop, terminal management.
  - `tui/screen.rs` — raw terminal, ANSI drawing.
  - `tui/input.rs` — key and mouse parser.
  - `tui/views/` — feed list, article list, article view, help.
- `src/keybindings.rs` — centralized keybinding definitions.
- `src/log.rs` — file logging.
- `tests/cli_integration.rs` — integration tests for `--cli` request/response behavior.

### Threading model

- UI loop runs on the main thread.
- Feed fetching and cache operations run on one backend thread.
- Communication over `std::sync::mpsc` using `BackendCommand`/`BackendResponse` enums.
- TUI applies optimistic updates for read/unread toggles.

### Data flow

1. Backend fetches each configured feed URL via HTTP.
2. Parses RSS/Atom XML into normalized article structs.
3. Deduplicates against existing cache entries (by link + title hash, similar to feed2maildir).
4. Stores new articles in redb.
5. TUI queries cache for display: feed list with unread counts, article list, article content.

### Cache (redb)

Location: `~/.cache/tn/tn.redb`

Override path: `--cache=PATH`

Tables:
- `articles` — keyed by hash(link+title), stores serialized article (title, link, description, content, published date, feed name, read flag).
- `feeds` — keyed by feed URL, stores feed metadata (title, last fetch time).
- `feed_index` — keyed by feed URL, stores ordered list of article hashes.

### Planned keybindings

- Feed list: `q` quit, `j/k/n/p/arrows` navigate, `Enter` open, `g` refresh, `u` mark all read.
- Article list: `q` back, `j/k/n/p/arrows` navigate, `Enter` open, `u` toggle read, `o` open link in browser, `g` refresh.
- Article view: `q` back, `j/k/n/p/arrows/Space/PgUp/PgDn` scroll, `o` open in browser, `u` toggle read, `n/p` next/prev article.
- Global: `?` help.

### CLI mode protocol (`--cli`)

- Transport: one JSON object per input line on stdin; one JSON object per output line on stdout.
- Reads and mutates cache only; no network fetching.
- Root-level folder listing includes virtual folders: `all`, `unread`, plus feed URL folders.
- Commands:
  - `{"cmd":"list_folders"}`
  - `{"cmd":"list_articles","folder":"all|unread|<feed_url>","offset":0,"limit":100}`
  - `{"cmd":"get_article","hash":"<article_hash>"}`
  - `{"cmd":"mark_read","hash":"<article_hash>","read":true}`
  - `{"cmd":"mark_folder_read","folder":"all|unread|<feed_url>"}`
  - `{"cmd":"search_articles","query":"<text>","folder":"all","offset":0,"limit":100}`
  - `{"cmd":"help"}`
  - `{"cmd":"quit"}`
- Response shape:
  - Success: `{"ok":true,...}`
  - Error: `{"ok":false,"error":"..."}`

## Constraints and Non-Goals

- RSS/Atom only (no JMAP, IMAP, Maildir).
- No built-in browser — `o` opens the system browser or `$BROWSER`.
- No feed discovery — feeds must be configured explicitly.
- HTML content converted to plain text for TUI display (similar to tmc's html2text).

## TODO

- [ ] Generate summary/digest file from cached articles (similar to `rss_digest.py` but reading from redb and producing a standalone HTML or text summary).

## Commit Policy

- Agent-created commits must include a `Co-Authored-By:` trailer.
- Run `cargo fmt` and wait for it to complete before committing changes.
