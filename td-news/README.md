# Timmy's News

`td-news` (Timmy's News) is a Rust news reader for RSS/Atom feeds. It fetches
feeds, caches articles in a small key/value store, and shows them in a
keyboard/mouse-first window of its own: the feeds and the articles in
td-ui's lists, and an article in td-ui's editor pane, read-only.

## Dependencies

td-ui, td's dependency-free UI toolkit, whose editor pane shows an article,
and td's shared library crates, all by path: td-json and td-toml for JSON
and TOML, td-html for HTML rendering, td-kv for the cache, td-civil for
dates and td-fetch-client for the fetch service. With the Rust standard
library and the XML reader under `src/`, that is the whole closure, and
the window is td-ui's widget window. Fetching
is not done here at all — `td-news` asks td's fetch service over the
unix socket at `$XDG_RUNTIME_DIR/td-fetch/socket`, which holds the TLS, the
resolver and the timeouts, and without it no feed can be fetched. The window
needs a Wayland compositor: `WAYLAND_SOCKET`, or `WAYLAND_DISPLAY` under
`XDG_RUNTIME_DIR`, as td-ui's clients find it.

## Build / Run

On a host, install it with the repository root's `./install-apps`, which
builds it with the other desktop programs and puts `td-news` in
`~/.local/bin` as a link to td-net's launcher. Run that way, it serves
td's fetch service for this launch alone, runs td-news under your
Wayland session and stops the service when td-news exits
(APPLICATIONS.md §X.7). It is not the jail: td-news runs as you, with
your whole privilege, and nothing confines it. It needs cargo and a C
compiler (`cc`, `gcc`, or `TD_CC_HOME`), and crates.io the first time,
for td-net's dependencies:

```bash
./install-apps
td-news
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

As the XDG base directory specification says, a relative or empty
`XDG_*` value is ignored, and `HOME` counts only when absolute; with
neither, td-news names no default configuration, keeps no log, and
stops, saying why, rather than open its cache in a directory relative
to where it was started.

Example:

```toml
[ui]
mouse = true
sync_interval_secs = 300   # at least 30; 0 turns the automatic refresh off
browser = "firefox {url}"  # optional; else $BROWSER, xdg-open; no shell

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

- Global: `?` or `F1` shows the window's list of keys, the view's
  first (`?`, `q`, `Escape`, `F1` or a click closes it); every action
  bar's `Help`, the link picker's too, shows it as well; `q` quits, or
  goes back; `g` refreshes
- Feed list: `j/k/Up/Down`, `n/p`, `PageUp/PageDown`, `Home/End`, `Return`, `u`
- Article list: `j/k/Up/Down`, `n/p`, `PageUp/PageDown`, `Home/End`, `Return`, `u`, `o`, `/`
- Article view: `j/k`, `Space/PageDown`, `PageUp` scroll; arrows and `Home/End` move the caret; `n/p`, `u`, `o`, `b`, `1..9`
- Any text (an article, the log): `C-a` selects all, `C-c` copies the selection to the system clipboard; the status row says whether it was taken
- Mouse:
- A click selects a list row; a click on an article opens it
- The action bar's labels are the view's keys
- The wheel moves the selection, or scrolls the article
- A drag in the article selects text, and `C-c` copies it
- A `C-click` on a link in the article opens it in the browser;
  holding Control over one underlines it

## Memory

td's jail lets td-news grow to 128 MiB before it throttles it
(`memory-high`) and to 192 MiB before it ends it (`recipes/src/recipes/news.rs`).
td-news renders an article's HTML on the window's thread, one rendering
at a time, within `HTML_LIMITS` (`src/ui/mod.rs`): the first 256 KiB of
the HTML, and a 6 MiB output budget (td-html/DESIGN.md, "Resident
rendering cost"). An article past 256 KiB ends with td-html's notice that
the rest is not shown; ordinary HTML of 256 KiB needs under 3 MiB of the
budget, so the budget cuts only hostile markup, such as one-letter
paragraphs nested in quotes. The links the picker lists come from the
same part of the HTML: its `href`s, and the URLs in its rendering within
the same limits, so neither a link past the cut nor a long article of
distinct links costs more than its rendering.

Measured on a development host, release build: td-news started under a
headless td-compositor with the pinned monospace face and no articles
holds 19 MB resident at 1920x1200 and 30 MB at 2880x1800, beside its
window's shared-memory buffers, 9 or 20 MB each, of which td-ui keeps up
to three: 46 to 91 MB in all. The costliest hostile shape measured
through an article's path (the link scan, the pane's rendering at 80 or
200 columns and the pane's copy) is one-letter paragraphs left open
inside 62 quotes: it peaks 33 MB above the test's 1 MiB article, so 79 to
124 MB in all, under 128 MiB at either size, which leaves 10 to 55 MB for
the articles held. A test holds `HTML_LIMITS` to td-html's rule (100
bytes a byte of input, 1.25 a byte of budget) beside the larger baseline
and the pane's last text; those figures and the jail's are copied by
hand, and reviewed when `news.rs` or the limits change. A feed is fetched
whole, to the fetch service's 64 MiB ceiling, and its articles are held
whole; these limits bound what rendering one costs, not what fetching or
holding them does, nor what a search does: it lowercases a copy of each
article's title, description and content in turn
(`filtered_article_indices`), so it costs some three times the largest
article beside the articles held.

## Cache

- Database: `~/.cache/td-news/cache.tdkv`
