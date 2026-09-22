# Timmy's News

`td-news` (Timmy's News) is a Rust news reader for RSS/Atom feeds. It fetches
feeds, caches articles in a small key/value store, and shows them in a
keyboard/mouse-first window of its own: the feeds and the articles in
td-ui's lists, and an article in td-editor's document view, read-only.

## Dependencies

td-ui, td's dependency-free UI toolkit, and td-editor, whose document
view shows an article, both by path, and the Rust standard library: JSON,
TOML, XML, HTML rendering, the cache and dates are td's shared `std`
modules under `src/`, copied whole from one master each, and the window
is td-ui's widget window. Fetching is not done here at all —
`td-news` asks td's fetch service over the unix socket at
`$XDG_RUNTIME_DIR/td-fetch/socket`, which holds the TLS, the resolver and
the timeouts, and without it no feed can be fetched. The window needs a
Wayland compositor: `WAYLAND_SOCKET`, or `WAYLAND_DISPLAY` under
`XDG_RUNTIME_DIR`, as td-ui's clients find it.

## Build / Run

On a host, the one-word way is the repository root's entry script, which
builds td-news, serves td's fetch service for this launch alone, runs
td-news under your Wayland session and stops the service when td-news
exits (`td-builder host-run`, APPLICATIONS.md §X.7). It is not the jail:
td-news runs as you, with your whole privilege, and nothing confines it.
It needs cargo and a C compiler (`cc`, `gcc`, or `TD_CC_HOME`), and
crates.io the first time, for td-net's dependencies:

```bash
./news
```

Directly, this project needs `CC=gcc` for the linker; `cc` is not on
`PATH` in this environment, and the fetch service is yours to serve
(`net/target/release/td-net fetchd run --socket "$XDG_RUNTIME_DIR/td-fetch/socket"`
after `CC=gcc cargo build --release --manifest-path net/Cargo.toml`):

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
mouse = true
sync_interval_secs = 300   # at least 30; 0 turns the automatic refresh off
browser = "firefox {url}"  # optional; else $BROWSER, then xdg-open

[[feed]]
name = "Hacker News"
url = "https://news.ycombinator.com/rss"
```

`scrolloff = N` under `[ui]` keeps N rows shown past the selection on
each side of it as it moves through a list, as the terminal reader's key
did, so the items after the one being read stay in view; the window
stops at the list's ends, a margin of half the window's rows or more
centres the selection, and a press on a row inside the margin moves the
window as a key would. It defaults to 0, the least move that shows the
selection. The terminal reader's `page_size` and `[theme]` keys are read
as any unknown key is, and ignored: the lists page by what the window
shows and draw in the toolkit's palette.

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
- Feed list: `j/k/n/p` or arrows, `PgUp/PgDn`, `Home/End`, `Enter`, `u`
- Article list: `j/k/n/p` or arrows, `PgUp/PgDn`, `Home/End`, `Enter`, `u`, `o`, `/`
- Article view: `j/k`, `Space`, `PgUp/PgDn` scroll; arrows and `Home/End` move the caret; `n/p`, `u`, `o`, `b`
- Any text (an article, the log, the help): `Ctrl-A` selects all, `Ctrl-C` copies the selection to the system clipboard; the status row says whether it was taken
- Mouse:
- A click selects a list row; a click on an article opens it
- The action bar's labels are the view's keys
- The wheel moves the selection, or scrolls the article
- A drag in the article selects text, and `Ctrl-C` copies it

## Cache

- Database: `~/.cache/td-news/cache.tdkv`
