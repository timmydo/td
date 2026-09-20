# Timmy's News

`td-news` (Timmy's News) is a Rust news reader for RSS/Atom feeds. It fetches
feeds, caches articles in a small key/value store, and shows them in a
keyboard/mouse-first window drawn in rows and columns of text, a td-ui cell
screen presented as a Wayland toplevel of its own.

## Dependencies

td-ui, td's dependency-free UI toolkit, by path, and the Rust standard
library: JSON, TOML, XML, HTML rendering, the cache and dates are td's
shared `std` modules under `src/`, copied whole from one master each, and
the window is td-ui's `screen_app`. Fetching is not done here at all —
`td-news` asks td's fetch service over the unix socket at
`$XDG_RUNTIME_DIR/td-fetch/socket`, which holds the TLS, the resolver and
the timeouts, and without it no feed can be fetched. The window needs a
Wayland compositor: `WAYLAND_SOCKET`, or `WAYLAND_DISPLAY` under
`XDG_RUNTIME_DIR`, as td-ui's clients find it.

## Build / Run

This project needs `CC=gcc` for the linker; `cc` is not on `PATH` in this
environment:

```bash
CC=gcc cargo build
CC=gcc cargo run
CC=gcc cargo test
CC=gcc cargo clippy
cargo fmt -- --check
```

## Configuration

Default config path:

- `$XDG_CONFIG_HOME/td-news/config.toml`
- or `~/.config/td-news/config.toml`

Example:

```toml
[ui]
page_size = 100
scrolloff = 0
mouse = true
sync_interval_secs = 300   # at least 30; 0 turns the automatic refresh off

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

`ui.scrolloff` controls the minimum number of list rows kept visible above and
below the current selection while moving in feed/article lists.

## Usage

- CLI executable: `td-news`

- Main feed list includes virtual views:
- `[All]` for all feeds combined
- `[Unread]` for unread-only combined
- Articles are sorted newest-first by date.
- Datetimes are normalized to local timezone and displayed as:
- `YYYY-MM-DD HH:MM:SS` (no timezone offset)

### Keybindings

- Global: `?` help, `q` back/quit, `g` refresh
- Feed list: `j/k/n/p` or arrows, `Enter`, `u`
- Article list: `j/k/n/p` or arrows, `PgUp/PgDn`, `Home/End`, `Enter`, `u`, `o`, `/`
- Article view: `j/k` or arrows, `Space`, `PgUp/PgDn`, `n/p`, `u`, `o`
- Mouse:
- Feed list click selects feed
- Article list click opens article
- Mouse wheel scrolls article list and article view

## Cache

- Database: `~/.cache/td-news/cache.tdkv`
