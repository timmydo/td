# TODO

## Core
- [x] Implement RSS/Atom XML feed parser in `feed.rs`
- [x] Wire up backend thread to TUI in `main.rs`
- [x] Implement periodic background feed refresh (sync_interval_secs)

## TUI
- [x] Raw terminal setup and ANSI drawing (`tui/screen.rs`)
- [x] Key and mouse input parser (`tui/input.rs`)
- [x] Event loop and terminal management (`tui/mod.rs`)
- [x] Feed list view with unread counts
- [x] Article list view with search
- [x] Article view with plain text rendering (td's `html` renderer)
- [x] Help view with keybinding reference
- [x] Mouse support (click, wheel scrolling)

## Features
- [x] Open article link in `$BROWSER`
- [x] Mark read/unread per article and per feed
- [x] Offline mode (browse cached articles only)
- [x] Logging infrastructure (`--log`)

## Future
- [ ] Generate summary/digest file from cached articles (similar to `rss_digest.py` but reading from the cache and producing a standalone HTML or text summary)
- [x] CLI mode (NDJSON protocol, like tmc's `--cli`)
- [ ] Feed-specific refresh intervals
- [ ] OPML import/export
