# td-web

**Status: design; increments in progress (§9).** This document is
normative for td-web once a section's increment has landed; a section
whose increment has not landed is the reviewed plan.

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
caps, td-png's and td-jpeg's pixel caps), and a refused or malformed
document is shown as an error page, never a crash.

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
recipe from the `td-web` recipe's static binary, started by
`td-authd application-start 1000 web direct --` from its launcher card
like any service-account application. Its permissions keyfile grants:

```text
[Context]
sockets=wayland;fetch

[Resources]
memory-high=...
memory-max=...
pids-max=...
cpu-max=...
```

No `shared=network`: td-web holds no network. No `[Filesystem]` grant:
the first increments keep nothing between runs, and a later one keeps
its state in the account's own home, which is private to it. No bus names. The resource numbers
are set by the packaging increment from measured use.

**Its own fetch service.** A service-account jail takes the fetch
socket only from its own runtime directory, owned by its own UID
(td-jail `sockets=fetch`). td-web therefore has its own `td-fetchd`,
run by td-svc as `tda65540` with its socket at
`/run/user/65540/td-fetch/socket`, as mail's and news's were before
they joined the login user. It shares nothing with the human's fetch
service: no connection, no credential and no TLS trust beyond the
service's compiled roots.

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
  (`to_rich`) at the page pane's width in cells, and rendered again when
  the width changes. Links, emphasis, code and preformatted text keep
  their looks; colours named by the document are not applied, so text
  is always the theme's ink on the theme's ground. An image with `alt`
  text shows that text as a link to the image; td-html drops one
  without. td-html keeps no element ids, so a link's `#fragment` is
  kept in the address but not scrolled to.
- `text/*` is shown as it is, wrapped at the pane's width.
- `image/png` and `image/jpeg` reached by navigation are decoded and
  shown fitted to the pane, never enlarged.
- Anything else is not shown: the page names the type and size and
  says that Firefox opens it. Downloads are a later increment (§10).

Text is decoded as UTF-8, invalid sequences replaced and a byte order
mark dropped; other charsets are a later increment. Markup past
`MAX_MARKUP` (2 MiB) and text past `MAX_TEXT` (4 MiB) are not rendered,
a rendering keeps at most `MAX_LINES` (100,000) lines, and the status
row says when either cut the page short. td-html builds a rendering
whole before td-web cuts it, so the markup bound is what bounds that
work: a hostile page of empty paragraphs at the bound costs about a
second and some 270 MB at each width (§10). A status other than `2xx` with a body is shown with its
status in the status row; one without a body is an error page.

Each character takes one cell: td-html's own stroke marks are dropped
from struck text, which td-web strikes itself, but a wide character or
a combining mark still takes one cell of its own, and a preformatted
line wider than the page is cut at its edge (§10).

The rendered page is td-web's own model, a list of lines of styled runs
with link targets, independent of how it was produced: the first
increments fill it from td-html's rich output, and the layout engine of
§10 replaces that producer without changing the view above it.

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
| `Ctrl+Q`, `Ctrl+W` | close |
| `?`, `F1` | the key list (`F1` is the window's own) |

A click on a link follows it, the pointer over one says where it goes,
and the wheel scrolls. A navigation's fetch and its decoding, a
picture's included, run on a thread of its own, so the window stays
responsive while a page loads, and `Escape` on the page stops waiting
for it; rendering at the page's width, again on a resize, is the
window's. A stopped fetch runs on until the service answers or times
out, so td-web keeps at most `MAX_WORKERS` (4) fetches out, stopped
ones included, and asks the person to wait past that; a navigation
overtaken by another is stopped. The history keeps at most 100 pages
each way, and moves only when a page arrives: a step back or forward
that is stopped, refused or overtaken leaves it as it was. A resize
keeps the focused link and what find marked. There is no text selection
in the page yet.

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
   units, its launcher card and its permissions (§3).

## 10. Roadmap (not normative)

What a document browser of NetSurf's or Dillo's reach needs, roughly in
order; each is its own design amendment when it starts:

- **Foundations.** An HTML5 tree builder in td-html exposing a document
  tree (the consumer td-mail and td-news later move to); a cookie jar
  kept in the account's home; forms; gzip through the fetch service;
  more concurrent fetches; legacy charsets.
- **Proportional text in td-ui.** td-ui lays text on a cell grid
  (td-ui/DESIGN.md "The pixel backend is a seam"); proportional layout is
  a remodel above that seam. With it, pinned Noto faces as upstream
  data (AGENTS.md "Pinned upstream data"): static Noto Sans and Noto
  Serif TTFs in four styles for Latin, Greek and Cyrillic, with
  JetBrains Mono kept for monospace and Unifont as the last fallback.
  Noto's CJK (CFF collections), colour emoji and variable fonts wait for
  td-ui's reader to take those forms (`td-ui/src/sfnt.rs`).
- **CSS and layout.** A `td-css` crate (syntax, selectors, cascade,
  computed values) and a layout engine (block and inline flow, tables,
  then flexbox) replacing the rich-output producer of §5.
- **Pictures.** Inline images through `public` subresource fetches;
  progressive JPEG in td-jpeg; GIF; then WebP and an SVG subset.
- **Downloads** to `~/Downloads` through the portal, which today admits
  Firefox alone.
- **Driving.** td-ui's control socket (td-ui/DESIGN.md), so a test or an
  agent can operate td-web through the verbs td-ui's other programs
  take, and an oracle can boot it in the image.
- **Text selection** in the page, and copying it.
- **Bounded rendering**: a line limit inside td-html's rich output, and
  rendering off the window's thread, so a hostile page costs neither
  the memory nor the pause §5 states.
- **Wide text**: horizontal scrolling for preformatted lines, and cells
  by character width (td-html already counts wide characters as two and
  combining marks as none).
- **Toward replacing Firefox.** Script (an interpreter, §2),
  bidirectional text and shaping, a per-site process model (which td-jail
  does not offer today: one jail, no nested namespaces), and becoming a
  link handler (§7).
