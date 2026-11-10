# td-web

**Status: normative; every increment of §9 has landed.** The roadmap
(§10) is not normative; each item there is its own amendment.

td-web is td's own web browser: a td-ui program, built from source in
td's target graph, that reads documents on the web. It complements
Firefox and does not replace it today. Firefox stays td's browser for
the scripted web and stays the default handler for links other
programs open (`td-portal`'s OpenURI names it, §7). td-web is designed
so that it could later grow to replace Firefox; the decisions that keep
that open are stated where they are made, and the roadmap (§10) says
what a replacement would need.

## 1. Scope

td-web shows documents: HTML, plain text and images reached by
`http` and `https`. It runs no script. JavaScript is not in its scope
today and may be added later by its own design amendment; nothing here
forbids it, but every surface below is specified as if no script ran,
and any later script engine must be an interpreter: td-web never maps
executable memory and never carries a JIT (§2).

What td-web does not do, so a reader can say so: it runs no script,
plugin or WebAssembly; it does not speak to the bus, the portal or the
audio service; it keeps no cookie, cache, history or download on disk
in its first increments (§6); it has no GPU path (td-ui renders in
software, td-ui/DESIGN.md "The GPU path"); and it is not registered as
`BROWSER` or as the OpenURI target.

## 2. Trust and build

td-web is a zone-one program (AGENTS.md "Trust zones"): the `td-web`
crate is a standalone `td-*` roster crate built by td's Rust toolchain
in the Debian build root like every Cargo recipe. Its closure is `std`
and td's own crates by path: `td-ui` for the window, `td-html` for
markup, `td-fetch-client` for the network, `td-url` for URLs, and the
shared decoders (`td-png`, `td-jpeg`) for pictures. It adds no external
dependency and no `unsafe`: it declares `#![forbid(unsafe_code)]`; the
only raw syscall surface in its closure is td-ui's Wayland transport,
already recorded in UNSAFE.md, and td-fetch-client forbids `unsafe` as
td-web does. It follows AGENTS.md's production-code
rules (no panicking calls or indexing; bounded inputs).

Web content is hostile input. Every parser td-web feeds is bounded by
the limits below and by its crate's own (td-html's depth and element
caps and output budget, td-png's and td-jpeg's pixel caps), and a
refused or malformed document is shown as an error page, never a
crash.

**No executable memory.** td-web, and any later script support, never
asks for writable and executable memory, never maps a file executable
and never loads code at run time. A JIT would be a new UNSAFE.md surface
and an amendment to this section; it is out of scope.

## 3. Account and confinement

td-web runs under its own external service account, not as the login
user: the browsing it does, and the state a later cookie jar keeps, stay
outside the human's UID domain, as Firefox's do. Its registry name is
`web`, its account `tda65540`, its external UID 65540 (the first not
reserved; 65537-65539 stay reserved, APPLICATIONS.md §L), its home
`/var/lib/td/applications/65540`. Inside its jail it is UID 1000, as
every application is.

It is a `ShippedApplication` on `static-runtime`, packaged by the `web`
recipe from the `td-web` recipe's static binary with its debug
companion, like mail and news, but not run as the login user: it has no
`/bin` name and no terminal starts it. Its launcher card ("Web"; found
by web, browser, www or td-web; app id `td-web`) asks root to start
td-svc's `[web]` unit, which runs `td-authd application-start 1000 web
direct --`, ready once the compositor reports its window. Like the
other applications' units it starts at boot only on an oracle's (it
names `/run/td-applications-on-demand`), on the applications'
workspace. Nothing observes that start: no unit requires `[web]` and
no oracle reads a marker of it, so main's boot proves nothing of td-web
yet (§10, "Boot evidence"). "web" and "browser" are Firefox's search
terms too, so they list both cards, Firefox's first by label: Firefox
stays the browser a bare search opens. Its permissions keyfile grants:

```text
[Context]
sockets=wayland;fetch

[Resources]
memory-high=671088640
memory-max=805306368
pids-max=32
cpu-max=100000 100000
```

No `shared=network`: td-web holds no network. No `[Filesystem]` grant:
td-web keeps nothing between runs, and a later increment that does keeps
its state in the account's own home, which is private to it. In
WORKLOADS.md's terms td-web is already the target shape: an account of
its own (its reserved row carries into the workload ledger), a private
home, and no grant of the human's files. No bus
names, no environment and no terminal.

**Memory.** The bounds hold td-web's own memory; a body at
`MAX_DOCUMENT` (16 MiB) for each fetch out, held at its own length;
one rendering of markup within `HTML_LIMITS` (§5) or one decoding of a
picture; and what is shown, a page with its wrap or a picture with its
copy fitted to the pane. Renderings and decodings run on one render
thread, for every fetch thread, one at a time; a navigation stopped or
overtaken before its turn takes neither, and a document made for one
stopped while it was made is dropped there before the next reply is
taken, so it never stands beside that one's rendering. Measured on a
development host, release build: td-web started under a headless
td-compositor with the pinned faces and the blank page holds 23 MB
resident at 1920x1200, 34 MB at 2880x1800 and 47 MB at 3840x2160,
the largest output td supports, beside its window's shared-memory
buffers, 9, 20 or 32 MB each, of which td-ui keeps up to three: 50,
95 or 144 MB in all. The most it holds while a reply is made into a
document beside the one shown is then those 144 MB; 67 MB of bodies
(`MAX_WORKERS` of `MAX_DOCUMENT`); on the render thread the larger of
td-html's rule for a rendering's peak, 100 bytes a byte of input read
and 1.25 a byte of budget (td-html/DESIGN.md, "Resident rendering
cost"), 252 MB at `HTML_LIMITS` (238 MB at most measured), and the
costliest decoding, a 16-bit RGBA PNG of 16 Mi pixels inflated at 8
bytes a pixel beside its 4-byte RGBA, 201 MB (201 measured; a JPEG
of as many pixels some 112 MB); and the larger of what may be shown:
a page, which holds no more than td-html's output for it, so no more
than the budget's 34 MB (at most 19 MB measured), with its wrap on the
window's thread, 77 MB at most measured at its peak and taken as 85
(§5), 119 MB, or a picture's 67 MB with its copy fitted to the pane,
33 MB at 3840x2160: 582 MB. That is why `memory-high` is 640 MiB
(671 MB), 89 MB above the sum, so that pages as large as long
Wikipedia articles render whole; `memory-max` stays 768 MiB (805 MB).
A test (`the_html_limits_fit_the_jail`) holds the sum at least 20 MB
under `memory-high`, the measured figures and the jail's copied by hand
and reviewed when `web.rs` or the limits change. Wrapping only the
lines near those shown (§10) would take most of the wrap's term away
and bring the sum back under 512 MiB.

The allocator (glibc's) gives a thread that allocates beside another an
arena of its own and keeps much of a rendering's peak in it after, for
that arena's next rendering to reuse. Renderings on the fetch threads
kept it once per thread alive at the time: the worst page rendered in
turn on three fetch threads alive together peaked at 243, 325 and
417 MB within `HTML_LIMITS` (364, 567 and 659 MB within td-html's
defaults), where on the one render thread four peak
at 243 to 250 MB. So the sum counts one rendering or decoding, and the
fetch threads allocate only their requests and bodies. A panic while a
reply is made answers an error page and leaves the thread to take the
next (td-web's Cargo build unwinds; it sets no `panic=abort`), and a
render thread found gone is started again. `memory-high` throttles
before `memory-max` ends the jail. The tasks hold the window's
thread, td-ui's, the render thread and `MAX_WORKERS` fetches with
room, since td-svc does not
restart a program that exits on the cap. One CPU, twice news's, since
a page is rendered on the render thread and wrapped on the window's
(§8).

td-jail gives td-web the standard seccomp program, not an allowlist:
mail's and news's lists were derived from their realized binaries
(APPLICATIONS.md, "Per-application allowlists"), none has been derived
from td-web's, and a compiled program beyond the five UNSAFE.md §9 names
is an amendment there.

**Its own fetch service.** A service-account jail takes the fetch
socket only from its own runtime directory, owned by its own UID
(td-jail `sockets=fetch`). td-web therefore has its own `td-fetchd`,
td-svc's `[web-fetch]`, run as `tda65540` with its socket at
`/run/user/65540/td-fetch/socket`, as mail's and news's were before
they joined the login user; `[web]` requires it. It runs on every boot,
not on demand: a unit skipped by `unless-exists=` settles ready, which
would satisfy `[web]`'s requirement with no socket behind it.
td-firstboot prepares the account's home and its `/run/user/65540`, as
it does for every application with a service account. The service runs
in the account's `td-app-65540/session` leaf, where exec-service-as puts
it, beside td-web's jail rather than in it, so td-web's limits above do
not count it, as the human's fetch service sits in `td-user-1000`. The service
shares nothing with the human's: no connection, no credential and no
TLS trust beyond the service's compiled roots.

## 4. Network

Every byte td-web reads from the network comes through its fetch
service (`td-fetch-client`), which holds TLS, name resolution, the
timeouts and the body ceilings; td-web holds a unix socket. It speaks
`http` and `https` only, and asks for GET in the first increments;
form submission (GET, then POST) arrives with forms (§10).

**Redirects are td-web's.** Every request asks the service to follow
none (`redirects 0`), and td-web follows a `3xx` with a `location`
itself, at most `MAX_REDIRECTS` (5) hops, joining each location against
the URL that answered. Following them itself is what lets td-web know
the URL a document came from, which its relative links resolve
against; a later cookie jar needs it too, to see each hop's
`set-cookie`.

**Reach.** The fetch service always refuses loopback, link-local,
multicast and unspecified destinations. td-web further splits requests
by who chose them:

- A **navigation** is a request the person chose: an address typed or
  pasted, a link activated, a reload, a history step. It is sent as an
  ordinary request, so it may reach a host on the local network (a
  router's page, a printer's).
- A **redirect hop** keeps its navigation's reach only when it stays on
  the same host as the URL that redirected, whatever its scheme or port
  (`td_url::Url::same_host`): a device's `http` page sending the browser
  to its own `https` is still that device. A hop to another host is
  sent `public`, and so is every hop after it.
- A **subresource** is a request the document chose: an inline image,
  and later a stylesheet or a frame. It is always sent `public`, so a
  page on the internet cannot make td-web touch a device on the local
  network. td-web makes no request the person did not cause by
  navigating. A `<meta http-equiv=refresh>` is not followed; td-html
  drops `<meta>`, so neither is it shown.

**Bounds.** A body is taken up to `MAX_DOCUMENT` (16 MiB), a picture's
too; a larger body is refused by the service and shown as an error. A
picture is shown within td-ui's bounds (8192 pixels an axis, 16 Mi
pixels): a larger JPEG is decoded at the largest of td-jpeg's scales
that fits, a larger PNG is refused. Markup past `MAX_MARKUP` (2 MiB)
and text past `MAX_TEXT` (4 MiB) are not rendered, and the page says
so (§5). Requests carry
`accept: text/html, application/xhtml+xml, text/plain;q=0.9,
image/png;q=0.8, image/jpeg;q=0.8, */*;q=0.1` and td-web's
`user-agent`; the service sends `accept-encoding: identity` itself.

## 5. Documents

A response is shown by its type, the `content-type` media type, else a
sniff of its first bytes for `<!doctype html` or `<html`:

- `text/html` and `application/xhtml+xml` are rendered through td-html
  (`to_rich_within`, within `HTML_LIMITS`) once, unwrapped (width 0),
  on the render thread (§3), into the page's logical lines, the runs
  td-html gives each word joined where they look and link alike.
  td-web wraps those by
  pixel width with td-ui's `prose::wrap_at_most` at the page pane's
  width less a cell's inset each side, but never narrower than
  `MIN_WRAP_EMS` (8) ems, 128 pixels at scale one, a narrower pane
  showing each line's start, cut at its edge; measured with the faces
  the window draws with, and again when the width, the scale or the
  body face changes.
  Body text is in Noto Sans, or Noto Serif after `F8` (not kept between
  runs); `<strong>` and `<em>` in its bold and italic styles; code and
  preformatted text in the monospace face on the prose line (td-ui's
  `Mono`), code on a ground of its own. A line holding preformatted
  text, and one of a table's rules or rows (a line with a run of
  box-drawing characters alone, U+2500 to U+257F, as td-html draws a
  table's rules and column bars), is never wrapped: it is cut at the
  pane's edge and kept, when td-web renders it, to its first
  `MAX_PRE_CHARS` (4096) characters, a link left with no text no link,
  and laid out only as far as the pane shows it, its edge and td-ui's
  `OVERHANG` past it, again at each width: without horizontal scrolling
  (§10) a pane shows no more. Find matches, and the pointer and focus
  mark, only what is laid: a match past the edge is not found, and a
  link whose text is all past it is focused on its line with nothing
  marked, the status row naming where it goes.
  Without the proportional faces (none installed, `TD_UI_FACE=bitmap`,
  every in-process test) td-ui measures every glyph a cell and the page
  draws on the cell. Links keep their looks; colours named by the
  document are not applied, so text is always the theme's ink on the
  theme's ground. An image with `alt` text shows that text as a link to
  the image; td-html drops one without. td-html keeps no element ids,
  so a link's `#fragment` is kept in the address but not scrolled to.
- `text/*` is shown as it is, its tabs expanded every eight
  characters, wrapped at the pane's width in the monospace face on
  prose lines.
- `image/png` and `image/jpeg` reached by navigation are decoded and
  shown fitted to the pane, never enlarged.
- Anything else is not shown: the page names the type and size and
  says that Firefox opens it. Downloads are a later increment (§10).

Text is decoded as UTF-8, invalid sequences replaced and a byte order
mark dropped; other charsets are a later increment. Control
characters other than the tab (C0, DEL and C1), which prose would show
as U+FFFD, are dropped before the page is wrapped, a CR before an LF
among them. Markup past `MAX_MARKUP` (2 MiB) and text past `MAX_TEXT`
(4 MiB) are not rendered, a page keeps at most `MAX_LINES` (100,000)
logical lines and a rendering lays out and keeps as many wrapped ones
however narrow it is, an unwrapped line at most `MAX_PRE_CHARS`
characters, and the status row says when any of these, or td-html's
output budget (below), cut the page short. A status other than `2xx`
with a body is shown with its status in the status row; one without a
body is an error page.

td-html builds a rendering whole before td-web cuts it, so
`HTML_LIMITS` (td-html/DESIGN.md, "Resident rendering cost") bound that
work, which runs on the render thread (§3), one rendering or picture
decoding at a time: td-web gives td-html the first `MAX_MARKUP` bytes,
cutting them itself so td-html's notice (below) means only its budget,
and td-html stops once it has built 32 MiB of runs and lines, 16 bytes
a byte of that. Measured at width 0, real pages spend 2 to 11 bytes of
budget a byte (HTML manuals and specifications, Wikipedia articles,
the first 2 MiB of the WHATWG HTML and ECMAScript specifications at
some 14 MiB), and dense tables 16.4 (SANE's list of backends, 11 MiB
at its 670 KiB), so every real page tried, cut to `MAX_MARKUP`,
renders whole, and only 2 MiB of tables that dense would be cut;
2 MiB at 11 bytes a byte needs 22 MiB. td-html composes the marks
of the quotes and lists around a line into it once, as it emits the
line, but as a run per level, so its output still grows with how
deeply they nest, and the budget cuts such a page short. Measured on a
development host, release build, through td-web's rendering: 2 MiB of
one-letter paragraphs left open inside 62 quotes, the costliest of
td-html/DESIGN.md's hostile shapes, takes 0.35 s and peaks at 238 MB,
keeping some 8,000 of their lines; inside 8 quotes 236 MB, keeping
54,000; in none 222 MB, which the budget does not cut; list items
inside 62 quotes 198 MB; 2 MiB of empty paragraphs 48 MB. td-html
ends a rendering it cut short
with a line of notice (`td_html::CUT_NOTICE`), which td-web leaves out
of the page, saying in the status row that the rest is too large to
render; a page whose own last line is that text is taken for one cut.
A page td-web cuts at `MAX_LINES` before the notice never reaches it,
so its status row shows only the `MAX_LINES` note. What runs on the
window's thread is the wrap, at each width and body face, at most
`MAX_LINES` lines laid out. Measured there with Noto's metrics, by
what the window's thread grew to at the wrap's peak, at widths from
128 to 3820 pixels: one word of `MAX_MARKUP`, alone or inside 3, 8 or
38 quotes whose marks each continuation repeats, is the costliest,
77 MB at most (inside 8 quotes at 300 pixels or 3 at 200, some
100,000 lines), in under a second; a paragraph of the document's own
starting with 100 quote marks 55 MB; a list of eight-word items nested
eight deep 39 MB; a pile of combining marks 36 MB; one-letter
paragraphs in 62 quotes, each line repeating 62 marks, 29 MB; a
table whose first cell pads every row past `MAX_PRE_CHARS`, and
preformatted lines of tabs,
3 MB at most, laid only to the pane's edge. That is the wrap the
memory bounds count (§3), and the page they count as at most the
budget: a run keeps its text at its own length, so a page held at most
19 MB of the 32 MiB in the shapes tried. A
paint walks only the lines shown, each up to the pane's edge, and
draws at most eight glyphs at one pen (`MAX_STACKED`), so a pile of
combining marks costs a walk rather than a glyph each (§10).

Each character takes the advance of the face that draws it: Noto's
for body text, the monospace face's one advance for code, preformatted
and plain text and for a scalar Noto lacks, a cell's for one neither
has (drawn from Unifont), and a combining mark its own, zero in Noto's
faces. Text is unkerned and unshaped (td-ui/DESIGN.md, "Proportional
faces"). td-html's own stroke marks are dropped from struck text, which
td-web strikes itself over each glyph's extent.

td-html lays the page out unwrapped and in characters. td-web hangs a
wrapped line from what td-html starts it with, as it does a list, a
nested list, a quote, a heading or a definition: whole runs, at most
256 characters, each holding only spaces, a quote's mark (`> `), a
heading's (one to six `#` and a space) and a list's marker (`*`, `-`,
`+`, `•`, `◦` or `▪` and a space, or digits, after a `-` or not, then
`.` or `)` and a space), in any order. The rest of the line is wrapped
at the width less that prefix's, measured as the line is, and every
line of it starts that far in: the first after the whole prefix, each
continuation after the quote's and heading's marks alone, drawn where
the first line has them, and no list marker, one piece for each run
of them however deeply they nest. A prefix wider than half
the width does not hang, so the text keeps room. Marks that share a
run with text, as every line of plain text does, do not hang; but
td-html's rich output gives each word of a paragraph a run of its own,
so a paragraph of the document's own that begins `> `, `# ` or `- `
hangs as td-html's structure does, which nothing in that output tells
apart. What else td-html lays out by width does not carry into pixels:
a table's columns, padded with spaces between box-drawing rules, line
up only on the cell, which is why its lines are not wrapped, and a
rule is three dashes (§10).

The rendered page is td-web's own model, independent of how it was
produced: a page of logical lines of styled runs with link targets,
filled once from td-html's rich output, and its rendering, those lines
wrapped into prose lines whose pieces carry their run's link as their
tag. The layout engine of §10 replaces that producer without changing
the view above it.

## 6. State

A td-web window keeps, in memory only: its back and forward history
(each entry a URL and the scroll position it was left at), the current
document and its rendering. Nothing is written to disk in the first
increments: no cookie, cache, history, bookmark or download. Closing
the window forgets everything. A later increment that keeps state
writes it under the account's own home, never in the login user's.

## 7. Other programs

Firefox remains `BROWSER` for td's applications and the OpenURI target
(`td-portal/src/open_uri.rs`). td-web is started from its launcher card,
as every service-account application is (it has no `/bin` name); the
program takes an optional URL argument, `td-web [URL]`, which hosts
and tests use. Making td-web a handler for links, or offering a choice
of handler, is a later decision and an amendment to td-portal's design.

## 8. Window and keys

One td-ui window: an address row on top, the page below, a status row
at the bottom. The status row says, first that applies: what the last
input made happen (a link not followed, a refusal, a copy), until the
next key, click, wheel turn or paste; what is loading; where the link under the pointer, else the
focused link, goes; the response's status, the place in the page and
any cut. Its app id is `td-web`; its title is the document's `<title>`,
else its URL, then ` - td-web`.

| Key | Does |
|---|---|
| `Ctrl+L`, `F6` | focus the address row, its text selected |
| `Enter` (address) | navigate to it: a text with no scheme is taken as `https://` |
| `Escape` (address) | restore the current URL and return to the page |
| `Tab`, `Shift+Tab` | focus the next or previous link |
| `Enter` (page) | follow the focused link |
| `Alt+Left`, `Alt+Right` | back, forward |
| `Ctrl+R`, `F5` | reload |
| `Ctrl+F`, `/` | find in page; `Enter`/`Shift+Enter`, then `n`/`N`, next/previous |
| `Up`, `Down`, `PageUp`, `PageDown`, `Space`, `Home`, `End` | scroll |
| `Ctrl+C` | copy the address or find row's selection while it has focus, else the focused link's URL, else the page's |
| `F8` | body text in Noto Sans or Noto Serif, for this run |
| `Ctrl+Q`, `Ctrl+W` | close |
| `?`, `F1` | the key list (`F1` is the window's own) |

A click on a link follows it, the pointer over one says where it goes,
and the wheel scrolls; which link a point is on is the piece of its
line under it, by pixel, never in the inset left of the lines or on a
line not shown whole. Find matches within a line, across its runs and
ignoring case, and marks each match by the pixels its glyphs span. A
navigation's fetch runs on a thread of its own, which hands the reply
to the render thread (§3) for its decoding, a picture's and a page's
rendering into logical lines included, and waits for it there, so
the window stays responsive while a page loads, and `Escape` on the
page stops waiting for it; wrapping at the page's width, again on a
resize or `F8`, is the window's. A stopped fetch runs on until the
service answers or times out, so td-web keeps at most `MAX_WORKERS` (4)
fetches out, stopped ones included, and asks the person to wait past
that; a navigation overtaken by another is stopped. The history keeps
at most 100 pages each way, each at the text its top line began with,
and moves only when a page arrives: a step back or forward that is
stopped, refused or overtaken leaves it as it was. A resize or `F8`
keeps the text at the top, inside a long paragraph too, and the
focused link; find's marks are made again on the new lines, the
current match's index kept as far as there are as many matches,
without moving to it. Coming back to a page opens it
at the text its top line began with, however it is wrapped now. There
is no text selection in the page yet.

## 9. Increments

1. **This design** and its AGENTS.md route.
2. **`td-url`**: td-agent's strict `Url` and `Destination` move into a
   shared crate, td-agent reading them from there, with the browser's
   reference resolution (an `href` joined against a base, its
   fragment kept, spaces and non-ASCII characters percent-encoded,
   non-`http(s)` schemes classified rather than fetched).
3. **The `td-web` crate**: §4-§8 over td-html's rich output, runnable on
   a host through `./install-apps` with a private fetch service
   (`td-net launch`), tested without a network through an injected
   fetcher. Inline pictures, which are subresources, wait for §10.
4. **Packaging**: the `td-web` and `web` recipes, the `web`
   `ShippedApplication` at 65540, its fetch service and application
   units, its launcher card and its permissions (§3). Landed.
5. **Proportional text**, after td-ui's increments 38 and 39
   (td-ui/DESIGN.md, "Proportional faces"): the page in prose as §5
   says, hit testing, find and link focus by pixel, `F8` between Sans
   and Serif (not kept), and the faces loaded at startup beside the
   monospace one, a missing one said once and the page drawn on the
   cell. Landed.

## 10. Roadmap (not normative)

What a document browser of NetSurf's or Dillo's reach needs, roughly in
order; each is its own design amendment when it starts:

- **Foundations.** An HTML5 tree builder in td-html exposing a document
  tree (the consumer td-mail and td-news later move to); a cookie jar
  kept in the account's home; forms; gzip through the fetch service;
  more concurrent fetches; legacy charsets.
- **More faces.** Kerning from `GPOS`, then Noto's CJK (CFF
  collections), colour emoji and variable fonts, each waiting for
  td-ui's reader to take that form (`td-ui/src/sfnt.rs`).
- **CSS and layout.** A `td-css` crate (syntax, selectors, cascade,
  computed values) and a layout engine (block and inline flow, tables,
  then flexbox) replacing the rich-output producer of §5.
- **Pictures.** Inline images through `public` subresource fetches;
  progressive JPEG in td-jpeg; GIF; then WebP and an SVG subset.
- **Downloads** to `~/Downloads`, through a directory grant of
  WORKLOADS.md ("Grants") or td-portal's FileChooser, which today admits
  Firefox alone; td-web's account owns no file of the human's.
- **Driving.** td-ui's control socket (td-ui/DESIGN.md), so a test or an
  agent can operate td-web through the verbs td-ui's other programs
  take, and an oracle can boot it in the image. Reading a page back
  needs td-ui's prose-aware read-back first (td-ui/DESIGN.md,
  "Proportional faces", "Not yet"): its seam reads the cell grid, and
  over proportional faces narrow glyphs share a cell, so the text it
  reads is not the text drawn.
- **Boot evidence**: a `[web-evidence]` unit, as news's, that probes
  the running jail's process token and filter as tda65540 and prints a
  marker main's system boot requires, so a launch path broken by the
  ledger, the jail or the fetch directory reds there.
- **An allowlist**: derive td-web's seccomp program from its realized
  binary, as mail's and news's were (APPLICATIONS.md, "Per-application
  allowlists"); it parses the most hostile input of td's programs.
- **Text selection** in the page, and copying it.
- **Bounded rendering**: the rendering is off the window's thread and
  one at a time, but td-html still builds it whole, up to
  `HTML_LIMITS`, before td-web keeps `MAX_LINES` of it, so a hostile
  page costs the render thread the memory and time §5 states; a line
  limit inside its rich output at `MAX_LINES` would bound that
  further. The window's thread wraps the page at each width up to
  `MAX_LINES` lines, an unwrapped line only to the pane's edge, and
  grows by some twice the bytes the wrap itself holds (a wrap of
  100,000 lines holding 31 MB grew it 55 MB, 62 at its peak), the
  memory bounds' largest term after the baseline and the render
  thread's (§3): wrapping only the lines near those shown would bound
  that pause and that memory further, and bring td-web's worst back
  under a `memory-high` of 512 MiB.
- **Wide text**: horizontal scrolling for preformatted lines, with
  the rest of a line past `MAX_PRE_CHARS` kept for it; table columns
  and rules laid out in pixels rather than td-html's characters, until
  the layout engine above replaces it.
- **Toward replacing Firefox.** Script (an interpreter, §2),
  bidirectional text and shaping, a per-site process model (which td-jail
  does not offer today: one jail, no nested namespaces), and becoming a
  link handler (§7).
