# td-web

**Status: normative; every increment of §9 has landed.** §10, styled
documents, is a plan: normative for the work it names, its increments
landing in order; until its cutover lands, §3's memory, §5's rendering
and §8's places describe td-web. The roadmap (§11) is not normative;
each item there is its own amendment.

td-web is td's own web browser: a td-ui program, built from source in
td's target graph, that reads documents on the web. It complements
Firefox and does not replace it today. Firefox stays td's browser for
the scripted web and stays the default handler for links other
programs open (`td-portal`'s OpenURI names it, §7). td-web is designed
so that it could later grow to replace Firefox; the decisions that keep
that open are stated where they are made, and the roadmap (§11) says
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
audio service; it keeps no cookie, cache or history on disk in its
first increments (§6), and writes no file but one the person saves into
a Downloads the human granted it (§5, "Saving"); it has no GPU path
(td-ui renders in software, td-ui/DESIGN.md "The GPU path"); and it is
not registered as `BROWSER` or as the OpenURI target.

## 2. Trust and build

td-web is a zone-one program (AGENTS.md "Trust zones"): the `td-web`
crate is a standalone `td-*` roster crate built by td's Rust toolchain
in the Debian build root like every Cargo recipe. Its closure is `std`
and td's own crates by path: `td-ui` for the window, `td-html` for
markup, `td-charset` for text in other encodings, `td-fetch-client`
for the network, `td-url` for URLs, and the shared decoders (`td-png`,
`td-jpeg`, `td-gif`, `td-webp`, `td-avif`) for pictures. td-charset's
tables are WHATWG data under BSD-3-Clause, so the `web` package carries
its notice, td-charset's `data/LICENSE-WHATWG` byte for byte, at
`files/share/licenses/td-charset/LICENSE-WHATWG`, copied from the
`td-charset-notice` recipe (td-charset/DESIGN.md). It adds no external
dependency and no `unsafe`: it declares `#![forbid(unsafe_code)]`; the
only raw syscall surface in its closure is td-ui's Wayland transport,
already recorded in UNSAFE.md, and td-fetch-client forbids `unsafe` as
td-web does. It follows AGENTS.md's production-code rules (no panicking
calls or indexing; bounded inputs).

Web content is hostile input. Every parser td-web feeds is bounded by
the limits below and by its crate's own (td-html's depth and element
caps and output budget, the picture decoders' pixel caps), and a
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
reserved; 65537-65539 stay reserved, APPLICATIONS.md §L), the workload
account ledger's carried row (td-authd/DESIGN.md, "Workload account
ledger"), its home `/var/lib/td/applications/65540`. Inside its jail it is UID 1000, as
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
workspace. There, under the autotest token, `[web-evidence]` requires
it and, a few seconds after its window reported ready, asks td-jail as
`tda65540`, whose delegated cgroup holds the jail, for td-web itself
still in its instance under the installed sandbox and for its stage 2
holding its allowlist's name; only then does it print `TD-WEB-RUNNING`,
which main's system boot requires. So a launch the workload ledger, the
jail or the account's fetch directory broke reds there. "web" and
"browser" are Firefox's search terms too, so they list both cards,
Firefox's first by label: Firefox stays the browser a bare search opens.
Its permissions keyfile grants:

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
home, and none of the human's files but what the human grants. No bus
names, no environment and no terminal.

**Downloads.** The human's files reach td-web only through the workload
grant table (WORKLOADS.md, "Grants"; td-authd/DESIGN.md, "Workload grant
table"), as Firefox's Downloads does, and td-web asks for no other way:
no portal, no prompt of its own. `web` is a declared application with an
account of its own, so root admits a directory row for it; td-software's
folders panel lists Flathub applications alone, so the human grants it
with `td-ctl grant application web rw Downloads`, and takes it back
with `td-ctl revoke application web Downloads`; an `ro` row is no grant
to save into. From td-web's next launch `application-start`
publishes the row as a detached idmapped view, nosuid, nodev and noexec,
listed in the account's manifest, and td-jail binds it at
`/home/td/Downloads`; a running td-web keeps what it was launched with.
That view is the one place td-web can reach of the human's: it saves
there (§5, "Saving") and nowhere else, and reads nothing there. td-jail makes `Downloads` in the account's own home as the
view's mountpoint and leaves it after a revoke, so td-web takes
`/home/td/Downloads` for the human's only while its own mount table
shows a mount there.

**Memory.** The bounds hold td-web's own memory; a body at
`MAX_DOCUMENT` (16 MiB) for each fetch out, held at its own length; one
rendering of markup within `HTML_LIMITS` (§5) or one decoding of a
picture; and what is shown, a page with the lines of it laid or a
picture with its copy fitted to the pane. Renderings and decodings run
on one render thread, for every fetch thread, one at a time; a
navigation stopped or overtaken before its turn takes neither, and a
document made for one stopped while it was made is dropped there before
the next reply is taken, so it never stands beside that one's rendering.
Measured on a development host, release build: td-web started under a
headless td-compositor with the pinned faces and the blank page holds 23
MB resident at 1920x1200, 34 MB at 2880x1800 and 47 MB at 3840x2160, the
largest output td supports, beside its window's shared-memory buffers,
9, 20 or 32 MB each, of which td-ui keeps up to three: 50, 95 or 144 MB
in all. The most it holds while a reply is made into a document beside
the one shown is then those 144 MB; 67 MB of bodies (`MAX_WORKERS` of
`MAX_DOCUMENT`, a save's among them, §5); on the render thread the
larger of td-html's rule for a rendering's peak, 100 bytes a byte of
input read and 1.25 a byte of budget (td-html/DESIGN.md, "Resident
rendering cost"), 252 MB at
`HTML_LIMITS` (238 MB at most measured), and the costliest decoding, a
16-bit RGBA PNG of 16 Mi pixels inflated at 8 bytes a pixel beside its
4-byte RGBA, 201 MB (201 measured; a JPEG of as many pixels some 112
MB); and the larger of what may be shown: a page, which holds no more
than td-html's output for it, so no more than the budget's 34 MB (at
most 19 MB measured), or a text page, whose tabs are expanded, at most
46 MB (eight bytes a byte of `MAX_TEXT` and 120 a line; 43 MB measured,
tabs 41 to a line), with the lines of it laid on the window's thread,
only those near the ones shown however long the page, 19.3 MB at most
measured and taken as 24 (§5), 70 MB; or a picture's 67 MB with its copy
fitted to the pane, 33 MB at 3840x2160, 100 MB; or a reply td-web does
not show, whose body it keeps for a save, at most `MAX_DOCUMENT` beside
a page of a few lines, 18 MB; the largest: 564 MB. That
is why `memory-high` is 640 MiB (671 MB), 107 MB above the sum, so that
pages as large as long Wikipedia articles render whole: under 512 MiB
(537 MB) with the same room the sum fits only with markup cut at some
1.5 MiB, the wrap no longer weighing in it; `memory-max` stays 768 MiB
(805 MB). A test (`the_html_limits_fit_the_jail`) holds the sum at least
20 MB under `memory-high`, the measured figures and the jail's copied by
hand and reviewed when `web.rs` or the limits change. A page's text is
decoded (§5) on the render thread before it is rendered: sent in valid
UTF-8 it is its body, cut in place; sent in another encoding, or in
UTF-8 holding malformed bytes or ending unfinished, it is decoded into a
string of at most `MAX_MARKUP` or `MAX_TEXT` bytes, had at once, and the
body is dropped before td-html or the text's lines are given it. While
it is decoded the body and the string are both alive, the string beside
a body the bodies already count; that is within the sum because the
render thread's term, a rendering or a picture's decoding, holds nothing
then: the decoding is that thread's work before its rendering starts.
After it, the text stands in its body's place and adds no term.

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
next (td-web's build unwinds: a `compile_error!` refuses one that would
abort on panic, by a profile's `panic` or a recipe's `-C panic`), and
a render thread found gone is started again. `memory-high` throttles
before `memory-max` ends the jail. The tasks hold the window's
thread, td-ui's, the render thread and `MAX_WORKERS` fetches with
room, since td-svc does not
restart a program that exits on the cap. One CPU, twice news's, since
a page is rendered on the render thread and laid out on the window's
(§8).

td-jail gives td-web an allowlist seccomp program in place of the
standard one, derived from its realized binary as mail's and news's
were (APPLICATIONS.md, "Per-application allowlists"): the list every
td static application shares, without td-kv's `fdatasync`, which mail
and news add and td-web does not link. What is off it answers `ENOSYS`;
`socket` is `AF_UNIX` alone and `ioctl` `FIONBIO` alone. A change that
reaches for another operating-system facility, in td-web or a crate it
links, derives the list again, or that path fails in the jail. Saving
(§5) is held to that list: it reads its mount table and creates,
writes, syncs, empties and unlinks a file (`openat`, `read`, `write`,
`fsync`, `ftruncate`, `statx`, `unlink`), calls td-ui's shared-memory
buffers and theme file already make, and renames and links nothing,
which would want `renameat2` or `linkat`.

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
form submission (GET, then POST) arrives with forms (§11).

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
that fits, a larger PNG, GIF, WebP or AVIF is refused. Markup past `MAX_MARKUP` (2 MiB)
and text past `MAX_TEXT` (4 MiB), both counted in the UTF-8 they
decode to, are not rendered, and the page says so (§5). Requests carry
`accept: text/html, application/xhtml+xml, text/plain;q=0.9,
image/png;q=0.8, image/jpeg;q=0.8, image/gif;q=0.8, image/webp;q=0.8, */*;q=0.1` and td-web's
`user-agent`. Every request names its limit, so the service asks for
gzip and deflate itself and hands td-web the decoded body, the limit
bounding its decoded bytes (APPLICATIONS.md, the fetch service).

## 5. Documents

A response is shown by its type, the `content-type` media type, else a
sniff: `<!doctype html` or `<html` at its start, after spaces and a
UTF-8 byte order mark, is HTML; a picture's signature a picture; a byte
order mark, or no byte of binary data in its first 1445 bytes, the
resource header (MIME Sniffing, "Rules for distinguishing if a resource
is text or binary"), text:

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
  body face changes; and only the lines near those shown (below).
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
  (§10) a pane shows no more. Find matches, and the pointer, the focus
  and the selection mark, only what of such a line is laid: a match
  past the edge is not found, and a link whose text is all past it is
  focused on its line with nothing marked, the status row naming where
  it goes; a selection spanning such a line copies its kept text whole
  (§8).
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
- `image/png`, `image/jpeg` (`image/pjpeg` included), `image/gif`,
  `image/webp` and `image/avif`
  reached by navigation use one `picture` dispatch module and are shown
  fitted to the pane, never enlarged. With no type or
  `application/octet-stream`, PNG, JPEG, GIF87a, GIF89a, RIFF/WEBP and AVIF file-type signatures select
  that same decoder. An explicit different type remains authoritative.
  JPEG includes progressive scans; GIF shows only its first image, with
  the transparent canvas and palette policy of td-gif/DESIGN.md. WebP
  supports lossy/lossless pictures and the first animation frame, under
  td-webp/DESIGN.md's canvas, alpha and color-conversion rules. AVIF uses
  td-avif/DESIGN.md's initial opaque eight-bit still subset; unsupported
  AV1 tools and container features report an error. The Accept header
  does not explicitly advertise AVIF while its codec coverage is partial.
  No animation runs. Encoded bytes and decoded dimensions are checked by the
  shared entry point before allocating picture pixels; decoding runs on
  the existing render thread and is skipped for an already stopped reply.
  Picture dispatch returns typed errors without formatting allocations.
  Allocator refusal, and codec limits that share that error category, produce
  a resource-failure document holding no allocated error page. Its notice
  paints directly from static text. A failed fit shows the same notice and
  is retried only after a resize or navigation. This covers picture decoding and fitting,
  not all allocations in fetching, navigation, fonts or window management;
  those remain separate migration work.
- Anything else is not shown: the page names the type and size and
  says that Firefox opens it, and, when the server answered `2xx`, that
  `Ctrl+S` saves it, its body kept for that (below, "Saving").

**Saving.** `Ctrl+S` saves the document shown into the human's
Downloads; nothing else td-web does writes a file. It saves the body as
the fetch service hands it, its content coding undone (§4), at most
`MAX_DOCUMENT` as every body is: a larger reply is refused by the
service for saving as for showing (§11). A reply td-web does not show,
answered `2xx`, keeps its body in its document for this, and the first
save takes it, so it is not fetched twice. A page, text or a picture,
whose body decoding consumes (§3), and a kept body once given up, are
fetched again as a reload fetches them, a navigation (§4), and saved
when that answer is `2xx`. A kept body is given up when its save
starts: a save that then fails, or whose thread could not start, leaves
the next to fetch it again rather than restoring it. Only what a server
answered with success is saved; a page that could not be fetched and
td-web's own pages are not. The save runs on a worker's thread of its
own, counted in `MAX_WORKERS` (§8), one at a time; the status row says
`saving in Downloads` while it runs and then what it saved, `saved N
bytes of HOST/PATH in Downloads as NAME`, its URL without the scheme and
cut to 60 characters so a save that ends during a navigation is not
read as the new page's, or why not.

A save fetching its body is stopped by `Escape` on the page when no
navigation is in flight (§8): it is let go at once, so a server that
stalls holds no later save back, and writes nothing, its worker keeping
its slot until its fetch returns. A save writing its file, which a kept
body does from the start, is not stopped, and `Escape` then clears as
it would with no save out; a save that has answered but not yet been
shown is shown first, so a failure reads as itself, not as stopped.
Closing the window (`Ctrl+Q`, `Ctrl+W`, the compositor's close)
likewise drops a save still fetching and waits for one writing: for the
write and both syncs, with no time bound, so a host Downloads on a
network filesystem that stalls holds the close.

Downloads is `$HOME/Downloads`; a `HOME` that is unset, empty or not
absolute is none. In td-web's jail `HOME` is td-jail's application
home, `/home/td`, the account's own, where the human's Downloads is
only the grant view (§3): td-web saves there only while its mount table
(`/proc/self/mountinfo`) shows one mount at `/home/td/Downloads`,
without `ro`. td-jail refuses a grant mountpoint the table repeats
(`grant_mount_rows`), so one listed twice is no view it made, and no
grant. With none, or a read-only one, nothing is fetched or written and
the status row says how to grant it, `td-ctl grant application web rw
Downloads`, from td-web's next start. Run on a host as the human (§7),
`HOME` is the human's and Downloads is its own; a host account whose
home is `/home/td` is taken for the jail and saves only into a mount
there. Either way Downloads is opened once, as a directory and without
following a link, before anything is fetched.

The name is the `content-disposition`'s `filename*` (RFC 8187, in UTF-8
or ISO-8859-1), else its `filename`, a quoted string's escapes undone,
else the last segment of the URL's path percent-decoded as UTF-8, else
`download`, each bounded already by the fetch service's 8 KiB head line
or td-url's URL; whichever is taken loses its control characters and
the format characters td's grant paths refuse (td-authd/DESIGN.md,
"Workload grant table"), has each `/` and `\` made `_`, drops dots and
spaces from its start, in any mix, until neither leads, and spaces from
its end, so it is one name in Downloads and no hidden file, `.` or
`..`, and is cut at a character to `MAX_NAME` (200) bytes, an extension
of at most 16 bytes kept however long the name; one with nothing left
falls to the next. A save never overwrites: the file is created
exclusively (`O_CREAT`, `O_EXCL`, `O_NOFOLLOW`, mode 0600, the human's
alone and every grantee's through its own idmapped view) under the
first free one of `NAME`, `NAME (2)` up to `NAME (100)` (`MAX_COPIES`),
the counter before the extension, in the directory opened, named
through `/proc/self/fd` so a link in its place is never followed or
replaced and a rename of Downloads meanwhile does not move the save;
all taken, nothing is saved. A name that is empty, `.`, `..` or holds a
`/` is refused there again, though none reaches it. The body is written
whole and synced, then the directory is synced so the name lasts too;
a directory that cannot be synced is said beside the save.

There is no rename into place, which would want `renameat2`'s
`RENAME_NOREPLACE` or `linkat` beside the shared allowlist (§3): the
file holds its final name while it is written, so until the write ends
it is visible there part written. The whole body is in memory before
the file is made, so that is the disk's time, not the network's. A
write or sync that fails empties the file through its own descriptor,
which a full disk may refuse, then removes it while its name still
names it, emptied or not, the descriptor held open until then so its
inode cannot be another file's; the status row says it was removed, or
left under the name, emptied or partly written, when it could not be,
or that something moved it away from the name first, naming no other
file as td-web's. The look at the name and the unlink are two steps: a
writer to Downloads that puts a file at the name between them loses it,
a narrow race only an unlink by descriptor would close, which Linux
lacks. td-web ended during a write, its jail killed or the machine
stopped, leaves the part written under the final name.

**Encodings.** Markup and text are decoded to UTF-8 on the render thread
before anything reads them, td-html and the title included, through
td-charset (td-charset/DESIGN.md) by the WHATWG Encoding and HTML
Standards' rules. A byte order mark decides first, whatever the label
says, and is dropped. Then the `content-type`'s `charset` parameter, its
first, when td-charset knows the label (ISO-8859-1 and ASCII meaning
windows-1252, as WHATWG has them). Then, for HTML alone, a `<meta
charset>` or a `<meta http-equiv=content-type>` complete in the first
1024 bytes (HTML's prescan, a UTF-16 one meaning UTF-8). Then, as the
HTML Standard's detection step allows, UTF-8 when every byte shown is
UTF-8; else windows-1252, the standard's default for a browser with no
locale. The detection is all or nothing, as validity detection such as
chardetng's is: one byte that is not UTF-8 anywhere in what is shown of
an unlabelled page makes the whole page windows-1252. A sequence the
bound cuts counts as UTF-8 when the bytes past it finish it, and one the
body ends in when UTF-8 beyond ASCII comes before it (a lead byte after
ASCII alone is as likely windows-1252's last letter); decoding makes the
latter one U+FFFD. Text takes the same order without the `<meta>`, as
browsers read text: Firefox prescans no `text/plain`, and the HTML
Standard's page-load model for text does not say.
`application/xhtml+xml`, and XML shown as text (`text/xml`, a
`text/*+xml` type), take XML's rules: the mark, the label, else UTF-8;
the XML declaration is not read. A label td-charset does not know (Big5,
EUC-JP, ISO-2022-JP) is passed over as if absent. Malformed bytes, and a
sequence the body ends in, are each U+FFFD and the page renders on: only
want of memory for its text fails a page, as an error page. `MAX_MARKUP`
and `MAX_TEXT` bound the text decoded, in bytes of UTF-8, not the bytes
sent, and the scalars a byte completes are left out whole when the bound
would cut them: td-html is handed at most `MAX_MARKUP` bytes whatever
the encoding, though a byte of windows-1252 or of halfwidth Shift_JIS
decodes to three, and the note saying a page was cut gives how many of
the bytes sent it shows: those before the first left out, none still
unfinished among them. A page's text sent in valid UTF-8 is its body,
cut in place; in another encoding, or UTF-8 holding malformed bytes or
ending unfinished, it is decoded into a string of at most the bound, had
at once, and the body dropped (§3). The status row names the encoding
when it is not UTF-8 (§8). Find, the selection and copying see the
decoded text alone. No encoding is chosen by hand (§11).

Control characters other than the tab (C0, DEL and C1), which prose
would show as U+FFFD, are dropped before the page is wrapped, a CR
before an LF among them. Markup past `MAX_MARKUP` (2 MiB) and text past
`MAX_TEXT` (4 MiB) are not rendered, a page keeps at most `MAX_LINES`
(100,000) logical lines, an unwrapped line at most `MAX_PRE_CHARS`
characters, and the status row says when any of these, or td-html's
output budget (below), cut the page short. The wrapped lines are not
counted or cut: however many a page wraps to, only those near the ones
shown are laid (below). A status other than `2xx` with a body is shown
with its status in the status row; one without a body is an error page.

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
render; a page whose own last line is that text is taken for one cut,
and one whose first block alone is past the budget (a paragraph of
some 2 MiB of words, say) renders empty but for that note. A page
td-web cuts at `MAX_LINES` before the notice never reaches it, so its
status row shows only the `MAX_LINES` note.

What runs on the window's thread is the wrap, and only of the lines
near those shown. Where the reader is, the top of the pane, is a place
in the logical lines (`Place`: a logical line, and a run and a byte in
it where one of its wrapped lines starts), as the history keeps it
(§8). The lines laid are consecutive wrapped lines, from a screenful,
or `STRIDE` (64) lines if more, above the top to as many past the
bottom, laid `STRIDE` at a time as the reader moves and forgotten past
that, so what the wrap holds is bounded by the pane, not by the page.
A logical line is wrapped from its start, or again from where one of
its wrapped lines starts, since a line's breaks depend only on its text
from there on; it is handed to `prose` a slice at a time, 4 KiB
(`FIRST_SLICE`) and twice that while a slice holds no whole line, at
most `MAX_ROW_BYTES` (64 KiB, with a byte more for each run), each
slice's last line laid again from its start by the next. A slice of
`MAX_ROW_BYTES` holding no whole line, none of its text overflowing
the width at a break, is one line, and the next starts at the first
character after it that is no space, as after any break, a line of
spaces alone being none. A line never breaks at a space, so when it is
spaces that run past the slice that is the line a wrap of the whole
logical line makes. A slice starting with spaces that its wrap drops
as a line of their own, as indentation or the text after a hang's
structure may, is laid again from the first character after them, the
wrap starting afresh there as the whole wrap does, so a word crossing
the slice's end is not cut. Only characters of no advance, which never
fill the width, end a line there that the whole wrap would not (a
test, `a_page_laid_a_slice_at_a_time_is_its_whole_lines_wrapped`,
holds words, runs of spaces and tabs, leading ones before a word
crossing a slice's end included, to the whole wrap). Moving down lays on
from the last line laid. Moving up lays a logical line from its start,
or from the nearest of the starts a walk along it last found, every
`STRIDE`th of its wrapped lines, kept for the last two logical lines
walked, keeping the lines before where it is going. `End` lays back
from the last logical line; a place found again after a resize, `F8`
or a history step, a link focused or a match shown lays its logical
line up to it from the nearest start remembered. So a resize or a jump
costs the lines shown and at most one logical line, not the page: the
costliest logical line, one word of `MAX_MARKUP` alone, in quotes or
in alternating bold runs of eight letters, takes 29 to 54 ms to walk
to its end, a screenful's scroll about half a millisecond, and a jump
back and forth between two logical lines of 1 MiB each, once walked,
0.06 to 0.2 ms. There is no count of the page's wrapped lines: the
status row says where the reader is in logical lines (§8).
Measured on a development host, release build, with Noto's metrics at
1920x1200 and 3840x2160 (scale one and two) and at 300 pixels, by what
the window's thread grew to: a word of `MAX_MARKUP` alone or inside 3,
8 or 38 quotes whose marks each continuation repeats, one-letter
paragraphs in 62 quotes, a paragraph of the document's own starting
with 100 quote marks, a list of eight-word items nested eight deep, a
table whose first cell pads every row past `MAX_PRE_CHARS`,
preformatted and plain lines of tabs, and 100,000 lines of text each
hold at most 1 MB of wrap; a paragraph of 2 MiB of combining marks,
whose 32 lines of 64 KiB are all near the top, 6.2 MB; paragraphs of
combining marks in runs of four to eight, bold and plain, bold and
italic, or bold, italic and plain, 13 to 18 MB; and the costliest, a
paragraph of combining marks in alternating bold and plain runs of
four, 19.3 MB, a piece for each run of the page in the lines laid
(taken as 24 MB, some 4.7 MB of room). That is the wrap the memory
bounds count (§3), and the page they count as at most the budget: a
run keeps its text at its own length, so a page held at most 19 MB of
the 32 MiB in the shapes tried, and a text page at most 46 MB, its
tabs expanded. A paint walks
only the lines shown, each up to the pane's edge, and draws at most
eight glyphs at one pen (`MAX_STACKED`), so a pile of combining marks
costs a walk rather than a glyph each (§11).

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
near the ones shown wrapped into prose lines whose pieces carry their
run's link as their tag. §10 replaces that model whole, the producer
and the places of the view above it with it.

## 6. State

A td-web window keeps, in memory only: its back and forward history
(each entry a URL and the scroll position it was left at), the current
document and its rendering. Nothing is written to disk in the first
increments, no cookie, cache, history or bookmark, but a file the
person saves into Downloads (§5, "Saving"), which is the human's, not
td-web's state. Closing the window forgets everything. A later
increment that keeps state writes it under the account's own home,
never in the login user's.

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
next key, click, wheel turn or paste, and what a save did, once it
ends, until the same; what is loading; that a save runs; where the link
under the pointer, else the focused link, goes; the response's status, the place in the page, the
encoding its text was decoded from when not UTF-8 (§5), and any cut.
The place, said only of a page not shown whole, is `N of M lines`: the
logical line the last line shown is part of, of the page's
logical lines, which needs nothing laid but the lines shown (§5). Its
app id is `td-web`; its title is the document's `<title>`, else its
URL, then ` - td-web`.

| Key | Does |
|---|---|
| `Ctrl+L`, `F6` | focus the address row, its text selected |
| `Enter` (address) | navigate to it: a text with no scheme is taken as `https://` |
| `Escape` (address) | restore the current URL and return to the page |
| `Tab`, `Shift+Tab` | focus the next or previous link, clearing the selection |
| `Enter` (page) | follow the focused link |
| `Alt+Left`, `Alt+Right` | back, forward |
| `Ctrl+R`, `F5` | reload |
| `Ctrl+F`, `/` | find in page, the current match selected; `Enter`/`Shift+Enter`, then `n`/`N`, next/previous |
| `Up`, `Down`, `PageUp`, `PageDown`, `Space`, `Home`, `End` | scroll |
| `Shift+Left`, `Shift+Right` (page) | move the selection's end a character; with `Ctrl`, to the word before's start or the word after's end |
| `Shift+Up`, `Shift+Down` (page) | move it to its logical line's start or end, from there the line before's start or the line after's end |
| `Ctrl+A` (page) | select the page |
| `Escape` (page) | stop loading, else a save still fetching (§5), else clear the focused link, find's marks and the selection |
| `Ctrl+C` | copy the address or find row's selection while it has focus, else the page's selection, else the focused link's URL, else the page's |
| `Ctrl+S` | save the document into Downloads (§5, "Saving") |
| `F8` | body text in Noto Sans or Noto Serif, for this run |
| `Ctrl+Q`, `Ctrl+W` | close |
| `?`, `F1` | the key list (`F1` is the window's own) |

A click on a link follows it at the button's release (below), the
pointer over one says where it goes, and the wheel scrolls; which link
a point is on is the piece of its
line under it, by pixel, never in the inset left of the lines or on a
line not shown whole. Find searches the page's logical lines, all of
them however few are laid (§5), and matches within one, across its
runs and the ends of its wrapped lines and ignoring case, looking at
each character once (Knuth, Morris and Pratt), so a search costs the
text searched whatever the query: 16 to 19 ms for a word of 2 MiB,
200-character queries included. Showing a match lays the lines around
it and shows it whole where the pane holds it, from the line its last
character is on up to the one its first drawn glyph is on, a space a
wrapped line ends with being dropped there; each line it is on marks
it by the pixels its glyphs span there. `Tab` and `Shift+Tab` go
through every link of the page, a link's place being where its text
starts on the logical line it starts on. A navigation's fetch runs on
a thread of its own, which hands the reply
to the render thread (§3) for its decoding, a picture's and a page's
rendering into logical lines included, and waits for it there, so
the window stays responsive while a page loads, and `Escape` on the
page stops waiting for it; laying out the lines near those shown at
the page's width, again on a resize or `F8`, is the window's. A
stopped fetch runs on until the service answers or times out, so
td-web keeps at most `MAX_WORKERS` (4)
fetches out, stopped ones included, a save among them (§5), and asks
the person to wait past that; a navigation overtaken by another is
stopped. The history keeps
at most 100 pages each way, each at the text its top line began with,
and moves only when a page arrives: a step back or forward that is
stopped, refused or overtaken leaves it as it was. A resize or `F8`
keeps the text at the top, inside a long paragraph too, and the
focused link; find's marks are made again on the new lines, the
current match's index kept as far as there are as many matches,
without moving to it. Coming back to a page opens it
at the text its top line began with, however it is wrapped now.

**Selection.** Text in the page is selected as two places in its
logical lines (§5's `Place`, here any character's, not only where a
wrapped line starts): where it was begun and where it is now, in either
order. So it is kept unchanged however far the page scrolls, the wrap
forgetting the lines far from those shown, and through a resize, `F8`
and anything else that wraps the page again, as the place at the top
is.

- **Pointer.** A press on the page and a drag select from the place
  between characters nearest the press, by the halves of the glyphs
  drawn there, to the one nearest the pointer: left of a line's text
  is its start, a continuation's repeated marks included; right of it,
  or in the inset right of the lines, its end, a line not wrapped
  ending where its logical line does, past what is laid of it; above
  the page, the first line shown's start; below its last line shown
  whole, the last's end; with no line shown whole, a pane shorter than
  a line, the first line shown's start. No place falls before a glyph
  of no width after one with width, so a combining mark is never parted
  from its base, while a line that begins with marks keeps its start.
  Once the pointer has moved from where it was pressed, held above the page or below that line it
  scrolls it once each `DRAG_SCROLL_MS` (50 ms), a line for each
  line's height it is past the edge and at most `DRAG_SCROLL_LINES`
  (4), so at most 80 lines a second, the selection following to the
  lines then shown, as it does when the wheel turns during a drag; a
  pane showing no line whole does not scroll so, and a scroll that
  moves nothing asks for no paint. A press clears the selection and
  selects nothing until the pointer goes further than `NEAR` (4 pixels
  at scale one) from it on either axis: a press and its release within
  that are a click, which follows the link under the press, and a
  press on a link dragged further selects and follows nothing. A
  second press within td-ui's `MULTI_CLICK_MS` (500 ms) of a click's
  release and within `NEAR` of it selects the word of letters and
  digits under it, across runs and wrapped lines but not past its
  logical line (off a word, nothing); a third selects the logical line
  whole; a fourth is a first again, and the drag of a double or triple
  click moves nothing. The window reports no click count, a pointer
  input carries no time, and inputs are handled before the turn's
  `poll` is told one, so td-web counts clicks on its own monotonic
  clock, read at each press and release. A
  click that follows a link, a drag, a Shift press and a press the
  pointer's leaving cancels are no click a later press pairs with. A
  Shift press moves the selection's end there from its anchor, and its
  drag goes on moving it; with nothing selected it begins there. A
  Shift press follows no link.
- **Keyboard.** td-web has no caret; the keys move the selection's
  end. Find's current match is the selection whenever find moves to
  one, as the query is typed, by `Enter` or by `n`, and a query that
  matches nothing leaves none. On the page `Shift+Left` and
  `Shift+Right` move the end a character, from a logical line's end to
  the next's start and back; `Ctrl+Shift+Left` and `Ctrl+Shift+Right`
  to the start of the word before or the end of the word after, within
  the logical line, and across a line's end a step, as a character;
  `Shift+Up` and `Shift+Down` to the logical line's start or end, and
  from there to the start of the line before or the end of the line
  after; the page scrolls as little as shows where the end went. With
  nothing selected they begin at the focused link's start, else the
  first line shown's. `Ctrl+A` selects the page.
- **One of a selection and a focused link.** Selecting lets the
  focused link go and `Tab` clears the selection, so `Ctrl+C` copies
  the one there is (the precedence in the table above). `Escape` on
  the page clears the selection with the focus and find's marks,
  unless it stops a load or a save still fetching. A page arriving, by any
  navigation, reload or history step, starts with none, a press held over the page before
  it is dropped, and the history keeps no selection.
- **Painting.** Each glyph of a line laid whose character lies between
  the two places is drawn on `SELECTED` in paper ink, as the focused
  link is. Find's current match is drawn so because it is the
  selection; once another selection replaces it, it is drawn on
  `INACTIVE_SELECTION` like every other match. Nothing is laid for it,
  so a
  selection reaching lines far from those shown costs nothing until
  they are shown; a continuation's repeated marks and the space a wrap
  drops at a line's end show no ground.
- **Copying.** `Ctrl+C` copies the page's text between the two places,
  each logical line's runs joined and the lines joined by a newline:
  the space a wrap drops is copied, and the marks a continuation
  repeats are not, being td-web's drawing rather than the page's text.
  A logical line's own leading structure, td-html's quote, heading and
  list marks, is copied with it, as its first wrapped line shows it and
  as td-html's text has it. A line not wrapped copies what td-web keeps
  of it, up to `MAX_PRE_CHARS`, past the pane's edge. A copy holds at
  most `MAX_COPY`, td-ui's clipboard ceiling (`clipboard::MAX_BYTES`,
  1 MiB), which the window keeps for its sends: a selection past it is
  not copied and the status row says so. The selection's text is
  measured before any of it is gathered and gathered into a string of
  exactly its size, so a copy never holds more than `MAX_COPY`, even
  `Ctrl+A` on the largest page, beside the memory §3 counts.
- **No primary selection.** td-web sets no primary selection and pastes
  none on a middle click: td-ui's widget window binds the primary
  selection for no handler and delivers no middle button (td-ui/DESIGN.md,
  "Widget window"), so that is td-ui's increment before it is td-web's
  (§11).

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

Increments 6 onward are §10's.

## 10. Styled documents

td-web moves from td-html's flat rendering (§5) to a document tree,
styled by td-css, laid out by td-layout and painted by td-web. This
section is the plan and, once its cutover (increment 7, below) lands,
the contract: that landing replaces §3's memory, §5's rendering and
§8's places with what this section says, so the document never
describes two producers.

### Pipeline and threads

A reply shown as a page is made on the render thread (§3), one job at
a time as now:

1. **Decode** (§5, "Encodings"), unchanged: at most `MAX_MARKUP` bytes
   of UTF-8, the body dropped. The decoded markup is kept until the
   page's first layout succeeds, since the text-only rung reads it
   (below, "Cuts"). td-web charges it at its length from decoding
   until the cascade; td-layout's styled document then counts it as
   its input, so td-web's own charge is returned, and td-layout's
   stays once the markup is dropped, an overcharge of at most
   `MAX_MARKUP`.
2. **Parse** with td-html's document parser (`parse_with_budget`,
   td-html/DESIGN.md "Document tree") into an immutable tree, its byte
   limit what the page's budget admits (below, "Memory"). Its storage
   retains the supplied tree budget until the last tree owner drops.
3. **Sheets**: the user-agent sheets and the page's own (below,
   "Sources") compiled into one `td_css::StyleSet` under td-css's
   admission ceiling, set to the sheets term the page has charged
   first, and dropped once the cascade is done: the styled document
   holds none and layout takes none. The environment is screen, the
   viewport's width and height in CSS pixels, the window's scale as
   its resolution, light, and reduced motion, since nothing in td-web
   moves.
4. **Cascade**: a `td_layout::StyledDocument`, parents before children
   through td-dom's projection, `style` attributes inline, computed
   styles shared in its pool, under td-layout's page-wide cascade work
   ceiling. td-web reports no element state (td-dom's default
   provider): hover, focus and checked are false, so neither the
   pointer nor the focus ever cascades a page again (below, "Links").
5. **Layout**: `td_layout::block::layout` of the whole page at the
   viewport's width, with no window: every line is kept.

Every job carries a cancel flag, which td-web sets when the job is
superseded or its navigation stopped, and which td-layout checks
between elements as it cascades and as it lays out (increment 6); so
`Escape` stops the cascade and the layout, not only the waiting for
them, and the render thread takes the next job. Decoding, parsing and
compiling are bounded by their limits but not cancelled: a stopped job
finishes the one it is in and then sees its flag.

The viewport, in CSS pixels, is the page pane less a cell's inset each
side, divided by the scale, and never narrower than `MIN_WRAP_EMS` ems;
a narrower pane shows the page's left and scrolls across it. The
render thread measures with its own clone of the faces the window
draws with, as td-web's window thread does now (td-ui/DESIGN.md,
"Measuring and wrapping"), and the window thread replays advances with
its own: the same faces at the same scale, so the two agree.

A page then holds the tree; its sheets' text, kept so a changed
environment can compile them again, but no compiled set until §11's
environment answer needs one kept; the styled
document, its pool and boxes; a table of each node's place in tree
order; and one layout with what td-web indexes over it, made for a
viewport size, a scale and a body family. It no longer holds the body,
td-dom's index or any cascade scratch, nor the decoded markup once the
first layout is in. td-web's own tables are td-alloc `BudgetVec`s in
the page's budget. The tree, sheets and styled document are immutable
and shared (`Arc`) between the window thread, which paints, hit-tests,
finds and copies from them, and the render thread, which lays them out
again; a layout is replaced whole, never edited.

**Handover.** The render thread sends a finished layout, or page, to
the window thread, which installs it on its next turn and sends the one
it replaces back to the render thread to be dropped there; a page let
go by a navigation goes the same way. So the last drop of a large page
happens on the render thread, whose arena holds it (§3), never in a
paint. When a relayout lets the old one go first (below), the window
thread decides so as it asks and gives up its references by sending
them with the request, and the render thread drops them before it
charges anything for the new one.

**Relayout.** What changed decides what is done again:

- `F8` changes the body family (below, "Faces"): a layout only.
- A width or a scale changes the environment: the set is compiled
  again from the sheets' text, the page cascaded again, then laid
  out. MDN's cascade takes 0.6 s (td-css/DESIGN.md §8) and its parse,
  compile and cascade 1.1 s (td-layout/DESIGN.md §6), on the render
  thread while the window paints the old layout. Once td-css can say
  whether a change alters which of a set's rules apply (§11), only a
  change that does cascades again.
- A height alone lays the page out again at that height, which
  td-layout gives the root as its definite height, without compiling
  or cascading: height and orientation media queries keep answering
  for the size the page was last styled at, until a width or scale
  change styles it again.
- A page arriving takes every step.

A recompiled set is charged to the sheets term while it lives, and the
new styled document and layout are charged beside the old ones, which
the window paints meanwhile. td-web decides that as it asks, from what
the old ones hold and the page's headroom, not by trying: when the new
ceilings do not fit beside the old, the old styled document and layout
are let go first (above, "Handover"), the pane showing paper and the
status row `laying out` until the new ones come. A relayout refused
then has no old layout to keep, and the page steps down from rung 2
(below, "Cuts").

td-layout measures the whole flow on every layout (td-layout/DESIGN.md
§6): a width change of 2 MiB of synthetic markup takes 105 to 194 ms
(medians, synthetic metrics) and the eight corpus pages 0.4 to 8.5 ms,
on top of the cascade above. So a relayout runs on the render thread,
never the window's, and a resize no longer costs only the lines shown,
as §5's wrap does. The window thread asks for one and goes on painting
the layout it has, at its own width, cut at the pane's edge or with
paper past it, the status row saying `laying out` until the new one
comes. Requests coalesce: a page has at most one waiting, a newer one
replacing it, and one running is cancelled by its flag when a newer
one is asked for, between elements of its cascade or its layout. The
text at the top of the pane is kept (below, "Places").

**Whole, not windowed.** td-layout can keep only the lines meeting a
window but measures the whole flow for it all the same (its windowed
and full work are identical), so a window would cost a whole pass at
every scroll past it. td-web keeps every line: scrolling is a paint,
and its memory is the layout's output, which the page's budget counts:
84.7 MiB for the costliest 2 MiB shape measured, sixteen-letter
paragraphs, and at most 37.8 MiB for a whole corpus page, tree and
styles included.

### Memory

Pages are held to td-alloc budgets (td-alloc/DESIGN.md, "Owned byte
budgets"): a root, held by the window's `App`, whose limit is `PAGES`
(320 MiB) and room for the budgets' own control blocks, which td-alloc
charges; under it, a budget for each page made, created when its reply
reaches the render thread and limited only by the root, so a page
takes what the others leave. The page shown and the page being made
are both charged while both live, the shown one kept until the new one
is installed rather than let go when a reply arrives, which would give
the new one all of `PAGES` but leave a blank pane when `Escape` stops
it. A page's charges are held in it and returned when it drops: charges
follow ownership. The body a reply td-web does not show keeps for a
save (§5, "Saving"), at most `MAX_DOCUMENT`, is charged to its page's
budget as that page's tree is, and returned when a save takes it, its
worker's body then one of the navigations' bodies below.

**Owned storage and declared charges.** td-html's document parser holds
its storage in td-alloc owners. The cutover passes a child of the page
budget to `parse_with_budget`, capped at `MAX_TREE_BYTES` plus its
control block; it takes no second reservation for that storage. The
parser's cumulative `charged_bytes` remains a separate complexity bound.

td-dom indexes likewise use the supplied page budget through
`HtmlTree::with_budget`; they keep both array charges until dropped.

td-css owns syntax, compiled rules, compute scratch and retained styles
through td-alloc. The cutover passes the sheets child to
StyleSet::new_with_budget and the page child to compute_with_budget and
StylePool::with_budget. Shared strings and custom tokens retain their
original child's admission until their last owner drops, including a
computed style that outlives its StyleSet. Dropping the set cannot return
that still-owned charge or move it to the styled term.

td-layout owns its arrays and scratch through td-alloc. The cutover passes
the page budget to StyledDocument::shared and a selected child to
block::layout_with_budget. The shared styled document owns the immutable
Document without a self-referential borrow. Its arrays and retained styles
keep their charges until dropped; the temporary DOM index returns its
actual admission when construction ends. Outputs retained across resize
count against their common ancestor automatically.

No external reservation covers these owned arrays. Limits::bytes and
Request::retained_output_bytes still govern the conservative layout
admission model, independently of the actual shared budget. The consumer
must own its input and its additional tree-order and viewport indexes
through td-alloc as well. Any refusal follows "Cuts" below.

| Term | Charged | Ceiling |
|---|---|---:|
| tree | td-html's owned allocation layouts, including parser scratch and growth overlap | 128 MiB, `MAX_TREE_BYTES`, plus budget metadata |
| styled | owned input, boxes, CSS pool and td-web tree-order table; excludes independently owned tree and DOM storage | the page's |
| index | td-dom's owned arrays, only while its index lives | `MAX_INDEX_BYTES`, within the page's |
| layout | owned geometry, lines, fragments and td-web's indexes over them | the page's |
| page | tree, styled and layout together | 224 MiB, td-layout's `MAX_PAGE_BYTES` |
| sheets | their text, compiled rules and scratch; referenced strings/tokens remain charged until their final owner drops | 48 MiB (estimate) |
| pictures | inline pictures' pixels and fitted copies (increment 9) | 48 MiB (estimate) |

The ceilings of page, sheets and pictures sum to `PAGES`, so a page
alone is refused only by its own terms. The sheets child bounds simultaneously live compilation layouts,
including scratch and replacement overlap. Source-size limits alone do
not guarantee admission: a sufficiently complex sheet can exhaust 48 MiB
before compilation finishes, and the page follows "Cuts" below.

A picture reached by navigation is a page of its own, charged its
decoding's peak before it is decoded, from the dimensions td-web's
shared picture dispatch reads first: 12 bytes a pixel for the
costliest, a 16-bit PNG (192 MiB at td-ui's 16 Mi pixels), shrunk after
to 4 bytes a pixel kept. Refused only because the page shown holds too
much, it lets the shown page go, the pane showing paper, and tries once
more. Its fitted copy is made on the window thread at its first paint
at a size, as now, the one heavy step that runs there, and charged to
the picture's budget there; a charge refused paints §5's static notice
in its place, tried again after a resize or a navigation, as a failed
fit is now. A
relayout's output is charged beside the one it replaces (td-layout's
`retained_output_bytes`, set from the old layout's `output_bytes`), or
the old one is let go first, as "Relayout" decides. Measured
(td-layout/DESIGN.md §6), a corpus page charges 1.1 to 37.8 MiB in all
and the costliest 2 MiB shape 167.4 MiB, so any two corpus pages fit
together, and a page is cut down (below, "Cuts") only when the page
shown leaves it too little.

**The jail.** What td-alloc does not hold is a fixed term:

- the window, 145 MB at 3840x2160, the largest output td supports:
  §3's measured 144 MB (47 MB resident and td-ui's three 32 MB
  buffers) rounded up, as its test takes it;
- `ATLAS`, td-ui's atlas pages for prose at sizes other than its own,
  8 MiB in all (estimate; td-ui's cap, increment 6);
- the navigations' bodies, 67 MB (`MAX_WORKERS` of `MAX_DOCUMENT`, a
  save's among them, §5), a page's decoded text standing in its body's
  place (§3);
- the subresources' bodies, 16 MiB (`MAX_SUBRESOURCES` of
  `MAX_SUBRESOURCE`, below, "Subresources");
- `WORKING`, td-layout's `WORKING_BYTES` (64 MiB): a cascade's
  candidate, wrapping scratch and the allocator's overhead, measured
  for layout and an estimate for parsing and compiling, for the one
  step the render thread runs at a time.

With `PAGES` that is 145 + 8.4 + 67.1 + 16.8 + 67.1 + 335.5 = 639.9
MB, 31 MB under `memory-high` (671 MB); `memory-max` stays 768 MiB,
and the jail's keyfile is unchanged. A test,
`the_page_budgets_fit_the_jail`, holds that sum at least 20 MB under
`memory-high`, the term ceilings summing to `PAGES` and the root's
limit at `PAGES` and its control blocks, its terms taken from the
crates' constants (`MAX_TREE_BYTES`, `MAX_PAGE_BYTES`, `WORKING_BYTES`,
the caller's sheets ceiling, the 192 MiB picture peak from td-ui's
`MAX_PICTURE_PIXELS`), and the window's term and the jail's copied by
hand and reviewed as §3's are. It replaces
`the_html_limits_fit_the_jail`: the terms that test sums,
td-html's rendering (252 MB), a page at its output budget, the wrap
(24 MB) and the text page (46 MB), go with the renderer they measure.

**What td-alloc does not bound**, and how it is kept: td-layout's
remaining std storage, whose external charge is a conservative admission
model; glibc's arenas and
fragmentation; td-ui's buffers, atlas pages, faces and measure caches;
and the fetch client's bodies. Every heavy step but a navigated
picture's fit runs on the one render thread, and a page's last drop
happens there, so one arena keeps their
peak (§3's reason); `WORKING` holds room for what the charges leave
out; the fixed terms are measured; and the cutover measures the
resident peak with a page shown and one made, both at their largest,
under the jail's limits. `memory-high` throttles before `memory-max`
ends the jail.

### Sources

A page's sheets, in cascade order:

1. td-css's `HTML_UA_CSS`, user-agent origin.
2. td-web's own sheet, compiled in (`td-web/src/page.css`), user-agent
   origin after it: td's look and what `HTML_UA_CSS` lacks. The root's
   ink is `INK`, links are `ACCENT`, `code`, `kbd` and `samp` lie on
   `CHROME`; the root's family is `"td-body"` (below, "Faces") at 16
   px, td-ui's `SIZE`; `[hidden]` is not displayed and `hr` is a block
   with a rule; a `plaintext` element (a text page's, below, "Cuts") is
   a block in monospace, `white-space: pre-wrap`. `PAPER` is the
   canvas's default, not the root's background, so a body's background
   reaches the canvas as CSS 2.2 §14.2 has it (below, "Painting").
3. The page's own: each `<style>` element in tree order whose `type`
   is empty or `text/css`, its `media` attribute, if any, its media
   condition (td-css, increment 6, rather than wrapping its text in
   `@media`, which a stray `}` in it would escape); then each element's
   `style` attribute, inline, as td-layout joins it. `<link
   rel=stylesheet>` and `@import` are subresources (increment 8);
   until then td-css skips `@import` whole and td-web fetches no sheet.

td-css takes no presentational attributes (`bgcolor`, `align`, a
table's `width`, `<font>`), so none applies; an image's `width` and
`height` are td-layout's. td-css's bounds hold (td-css/DESIGN.md §5):
at most 64 sheets in a set and 4 MiB of their text in all, the two
user-agent sheets included, 1 MiB each; a sheet past them is left out,
in tree order, and the status row says so.

**Colours.** The colours a page names are applied, unlike §5's: a page
looks as its sheets say. An unstyled page draws td-web's defaults,
`PAPER`, `INK`, `ACCENT` and `CHROME`, as td-ui's palette keys, which a
test holds equal to td-ui's constants, so td-ui's themes map them
(td-ui/DESIGN.md, "Themes"). Once a page's own sheets or attributes set
any colour, td-web draws those defaults in their fixed light values,
each moved off its key by one in its lowest byte so no theme maps it,
and moves the page's own colours off the keys the same way: a page
partly styled for light ground stays legible under a dark theme, as it
does wholly styled. td-web reports `light` to media queries, since
td-ui's theme map gives a handler no theme to report.

**Without the page's styles.** `F9` shows the page with the user-agent
sheets alone and no `style` attributes, cascading it again, for a page
whose styles hide or garble its text; `F9` again restores them. It
holds for that page until the next navigation, and the cutover adds it
to §8's key table.

**Faces.** A computed family list names one of td-ui's prose families
by its first entry td-web knows, an entry it does not know passed
over: `monospace` or `ui-monospace` the monospace face on the prose
line (`Mono`); `serif` or `ui-serif` Serif; `sans-serif`, `system-ui`
or `ui-sans-serif` Sans; `Noto Sans` and `Noto Serif` themselves. So
`Georgia, serif` is Serif. A list with no such entry, `"td-body"`
among them, is the body family: Sans, or Serif after `F8`. So `F8`
changes a layout's measurements, not a style, and lays the page out
again without cascading it; on text whose own family names a generic
it changes nothing. A weight of 600 or more is bold; italic and
oblique are italic; a size is its computed pixels times the scale,
rounded to a whole device pixel and clamped to td-ui's prose size
bound (increment 6), the same size in td-layout's measuring and in the
paint. Small caps, `text-transform` and `text-indent` are not applied
(td-layout keeps the last two unapplied); `letter-spacing` is, as
td-layout lays it.

### Subresources

A page's stylesheets (increment 8) are its first subresources (§4):
each `<link>` whose `rel` holds `stylesheet` and not `alternate`, with
an `href`, and each `@import` at the head of a sheet (td-css's syntax
keeps an at-rule's prelude), in the order the cascade takes them, an
import standing at its rule's place with its media list as its
condition, a link with its `media`; an import naming `layer()` or
`supports()` is skipped, as td-css skips layers. Every sheet is fetched
whatever its media, so what is fetched says nothing of the viewport.
Inline pictures (increment 9) are each `img` with a `src`, in tree
order, at most `MAX_PICTURES` (64, estimate) a page.

**Requests.** Each is a GET sent `public`, following redirects as a
navigation does (§4). A subresource from another site is taken, as
pages' sheets and pictures so often are; an `http` one under an
`https` page is refused at whichever hop it appears, as browsers
refuse mixed content; no cookie (td-web keeps none) and no `referer` is
sent, so a host learns no more of the page than the subresource's own
URL. A `data:` URL is taken within the same byte bounds, decoded by
td-web on the render thread with no request: it takes no slot, and its
decoded bytes are charged to the sheets or pictures term, whose
ceilings bound them, until the sheet is compiled or the picture
decoded. A reply other than `2xx` is dropped. A sheet
asks `accept: text/css,*/*;q=0.1`, and one whose type is not
`text/css` is not applied; its text is decoded as CSS Syntax §3.2
orders it, through td-charset: a byte order mark, the
`content-type`'s `charset`, an `@charset` rule, the page's encoding,
else UTF-8. Bounds: 1 MiB a sheet decoded (td-css's per-input bound,
asked of the service as the body's limit; a sheet in another encoding
whose decoded text passes it is left out after decoding, the status
row saying so), 4 MiB in all with the
user-agent sheets, at most `MAX_SHEET_FETCHES` (32, estimate) and
imports four deep; a picture's body at most `MAX_SUBRESOURCE` (4 MiB).

**Slots.** Subresources have a count of their own, `MAX_SUBRESOURCES`
(4) fetches out, stopped ones included until the service answers, as
navigations have `MAX_WORKERS`; so a stopped page's fetches never
take a navigation's slot, and the next page's sheets wait for one of
these, within their `STYLE_WAIT`.

**Waiting.** A page waits for its sheets, as browsers hold rendering
for them, rather than being shown and restyled. Once the render thread
has parsed its tree and found them, they are fetched on threads of
their own in those slots, then their text is handed to the render
thread for steps 3 to 5. Meanwhile the page shown stays and the status
row says `loading styles`. At `STYLE_WAIT` (3 s, estimate) after the
reply came, or once every sheet has settled, the page is styled with
the sheets that came, in their order; one later is not applied, the
status row says how many were not, and a reload asks for them again: a
late sheet is dropped rather than restyling a page being read.
`Escape` stops the navigation, as it stops any (§8), the page shown
staying. A page is cascaded once, never restyled by a sheet.

**Pictures** are fetched once the page is shown, decoded on the render
thread one at a time through the shared picture dispatch (§5), each
decoding's peak charged to the pictures term first. A decoded
picture's size is its intrinsic size for td-layout; arrivals coalesce
into one relayout at most every `PICTURE_SETTLE` (250 ms, estimate),
keeping the top's place, and a fitted copy at its laid size is made
with that layout and charged there. A picture refused (type, decoding,
budget) keeps its placeholder. `srcset`, `<picture>`'s sources, CSS
background images and SVG are not fetched.

### Places, find, selection and links

**Places.** A place in a page is a DOM position: a text node and a
byte offset in its decoded text on a scalar boundary, or before or
after a replaced element. Places are ordered by the tree-order table,
four bytes a node (1 MiB at td-html's `MAX_NODES`). td-layout lays
normal flow and tables, in tree order, so its lines and the fragments
in each are in tree order: relative offsets move what is drawn, not the
order. A **paragraph**, the unit find matches within and `Shift+Up` and
`Shift+Down` move by, as §5's logical line is, is a run of consecutive
lines one block container lays: text either side of a child block is
two paragraphs, and a preserved newline or a `<br>` ends one. td-web
reads containers from the styled document's box parents.

**Find** searches each paragraph's text as td-layout displays it,
spaces collapsed and a replaced scalar as U+FFFD, with the space a
wrap drops put back (a word split across lines gets none) and an
image's `alt` text in its place, in tree order and
not past a paragraph, ignoring case, each character looked at once
(Knuth, Morris and Pratt) as now, at most `MAX_HITS` matches. Text with
no fragment (`display: none`) or not painted (`visibility: hidden`) is
not found. Matches are places, so they outlive a relayout; showing one
scrolls its line into the pane, and each is marked by the pixels its
glyphs span, the advances replayed through the layout's metrics, since
td-layout keeps no glyphs.

**Selection** is two places, kept unchanged by scrolling, a relayout,
`F8` and a resize. A point of the pane falls on a line by its height
(an index of the layout's lines by height), on a fragment by its x,
and between scalars by the halves of their advances, replayed; left of
a line is its start and right of it its end, above the page the first
line's start and below it the last's end, as §8 says of wrapped lines.
§8's keys keep their meaning, a paragraph standing for a logical line,
scalars and words taken in tree order across fragments; a double click
selects a word and a triple click a paragraph.

**Copying** takes the text between the two places in tree order as it
is shown: fragments' displayed text, the space a wrap drops copied
(td-layout keeps its source bytes), a preserved newline a newline,
paragraphs joined by a newline, a replaced element its `alt` text when
the selection holds it whole; at most `MAX_COPY`, measured before it is
gathered, as now. A list item's marker is copied at its first
paragraph's start, as td-layout's marker kind sets it apart, and is
not found. What §5 copies and td-web no longer does: a quote's marks,
which nothing draws now; and a table's row, which copies a cell a line.

**Links** are `a` and `area` elements with an `href` (td-dom's rule),
joined against the page's base: the first `<base href>` in tree order,
joined against the page's URL, else that URL. A fragment's or replaced
box's link is its nearest such ancestor, and a point's link that of
the fragment or replaced box under it; an image with no link opens its
`src` on a click, as §5's image text does. `Tab` goes through the links
in tree order that laid a fragment or a replaced box, a link's place
being its first. td-web draws the focused link itself (below,
"Painting") and never tells the cascade, so `:focus` and `:hover` match
nothing and neither the pointer nor the focus costs a cascade; a page
that shows a menu or a focus ring by them shows neither. A URL's
`#fragment` scrolls to the element whose `id` it names, else to an `a`
whose `name` it names, td-html's tree keeping both, or, when that lays
nothing, to the next fragment after it in tree order; else to the top.
The title is the first `title` element's text, its spaces collapsed,
at most `MAX_TITLE` characters.

**Scrolling** is by device pixels within the layout's extent (its
overflow rectangle): `Up` and `Down` a body line (td-ui's prose line at
the scale, 22 pixels at scale one), `PageUp`, `PageDown` and `Space`
the pane less a line, `Home` and `End`, the wheel a line for each row
it turns; and, when the page is wider than the pane, `Left`, `Right`
and the wheel with `Shift` across it. Unwrapped lines are laid whole,
within td-layout's bounds, and scrolled to: `MAX_PRE_CHARS` goes. A
line that does not wrap (`pre`, `nowrap`) past td-layout's
`MAX_LINE_SCALARS` is cut there, td-layout marking the cut (increment
6), and the status row says so; `pre-wrap` wraps, so a text page's
lines never reach it.

**The top's place.** What the reader sees is kept as an anchor: the
place where the first line reaching below the pane's top begins, and
the CSS pixels from that line's top to the pane's. A relayout, `F8` or
a resize shows that line at that offset again. Each history entry
keeps its anchor; a reload or a history step parses the page anew, so
the anchor is used only when its node is a text node holding its
offset on a scalar boundary, as the same bytes give the same tree, and
otherwise the page opens at its fragment or its top.

**The status row** keeps §8's order. Its place is `N of M lines`: the
line of the layout the last line shown whole is, of the layout's lines
at this width. Its notes are the cuts below, the sheets not applied
and a paint cut short.

### Cuts

td-html and td-layout return no partial result, so a page is laid
whole or not at all. At the cutover this ladder replaces §5's cut
page, the lines rendered before td-html's budget ran out followed by
the status row's note that the rest is too large: no page is shown as
a prefix. A refusal, by a crate's limit or by the page's budget, steps
down, each rung on the render thread:

1. The page with its own sheets. A sheet td-css refuses alone, past a
   per-sheet bound, is left out and the set compiled again without it,
   the status row saying so; td-css's refusal names the sheet
   (increment 6).
2. On a refusal by td-css's compile ceiling, td-layout's cascade work
   ceiling or any other td-layout limit with the page's sheets, the
   page with the user-agent sheets alone and no `style` attributes;
   the status row says the page's styles were refused and names the
   limit.
3. On a refusal of the tree, or of rung 2, the page as text: what
   td-html's bounded extractor gives from the kept markup
   (td-html/DESIGN.md, "Bounded text extraction"), laid as a text page
   is; the status row says the page was too large to lay out and names
   the limit. It is a poor page and says so: no links, no structure,
   one space at each block's edge.
4. On a refusal of that, a notice painted from static text, as a
   picture's resource failure is (§5).

Markup past `MAX_MARKUP` is cut before it is parsed, as now, and the
status row says how much was shown. A relayout refused once a page is
shown, at a new width, scale or face, keeps the old layout, painted at
its own width, and the status row says the page could not be laid out
again; one whose old layout was let go first has none to keep, and the
page steps down from rung 2.

A **text page** takes the same pipeline: its text, its controls but
the tab dropped, follows `<!doctype html><plaintext>`, which td-html's
tokenizer ends markup at, so the page is one text node laid `pre-wrap`
in monospace, td-layout's tab stops every eight columns in place of
td-web's expansion. Its bound stays `MAX_TEXT` (4 MiB), the prefix
included, so it fits td-html's 4 MiB input, and td-layout takes it
under an input limit of its own for a text page (increment 6). td-web
cuts it before it is parsed at `MAX_TEXT_LINES` (200,000) lines of its
source, as `MAX_LINES` cuts it now, with the status row's note. A laid
line costs some 340 bytes of layout (measured in review), so at
ordinary widths, a source line a laid line, two layouts of a cut page,
the old beside the new, hold 136 MB beside its tree within the page
term; a narrow pane lays a long source line as several, and when two
layouts no longer fit, the old one is let go first (above,
"Relayout"). Rung 3's text takes the same bounds. td-web's own pages,
an error, the start page and the note for a type it does not show, are
markup it writes with their text escaped; want of memory for one
paints the static notice.

### Painting

The window thread paints from the page and its layout. td-ui gives the
raster: opaque fills, prose glyphs in Sans, Serif and Mono at a style
and a size, fitted pictures (`Image`), a clip on every draw, and the
theme's map. td-web decides what is drawn, in this order:

1. The canvas: the root's background colour, else the body's (CSS 2.2
   §14.2), else `PAPER`, over the whole pane.
2. Each box in tree order: its background colour over its border box,
   then each border side as a fill of its width, every style but `none`
   and `hidden` drawn solid.
3. Each line's content: the backgrounds of a fragment's inline
   ancestors behind its extent; the glyphs in the fragment's colour,
   face and size, at most `MAX_STACKED` at one pen as now; the
   underlines, overlines and line-throughs it takes from itself and its
   ancestors, as CSS propagates them, as fills across the fragment, a
   pixel thick at scale one; a replaced element's picture fitted to its
   box, else a frame in `BORDER` holding its `alt` text, clipped to it.
4. Over the page, as §8 paints them by the glyphs' extents: find's
   matches on `INACTIVE_SELECTION`, the selection and the focused link
   on `SELECTED` in paper ink.

`visibility: hidden` paints nothing of its box. `opacity` is not
applied. td-ui's fills are opaque, so a colour with alpha is blended by
td-web before it is drawn, over the colour of the nearest ancestor
with an opaque background, else the canvas's, which the paint knows in
tree order: a translucent tint shows as a tint, not as solid colour; a
glyph's ink is blended so over the ground it is drawn on, and a colour
whose alpha is zero draws nothing. Every draw is clipped to the pane
and the damage; a box whose `overflow` is `hidden` or `clip` clips its
descendants to its padding box, and `auto` and `scroll` draw as
`visible`: there is no scroll container
inside a page. A paint visits only the boxes and lines meeting the
damage, through an index by height made with each layout and charged
with it; td-web derives that index and the boxes' tree order from
td-layout's postorder geometry. A line's glyphs are drawn up to one em
of its largest size past the pane, td-ui's `OVERHANG` at that size
(increment 6). A paint draws at most `MAX_PAINT` (estimate) fills and
glyphs on the window thread; the rest is skipped and the status row
says so. Nothing animates: a paint is a function of the page, its
layout, the view and the damage, and no timer asks for one.

### Out, and the security posture

Not shown, each a later amendment: script (§1); forms beyond what
their elements lay out (a `button`'s or `textarea`'s text shows, an
`input` nothing, and nothing is submitted); positioning other than
static and relative (td-css computes no other, so `absolute`, `fixed`
and `sticky` stay in flow); floats, flex, grid and inline-block until
td-layout has them, laid in normal flow as td-layout specifies, a float
taking no exclusion, nothing hidden by it; table layout beyond
td-layout's automatic one (increment 6); pseudo-elements and generated
content, list markers aside; presentational attributes; web fonts
(`@font-face` is skipped); background images, border radii, shadows,
transforms and viewport units (td-css computes none); SVG and MathML,
outside td-dom's projection; frames, `object`, `embed`, `video` and
`audio`; and `:visited`, which td-css never matches.

**Time.** td-css bounds each element's matching (`MAX_MATCH_STEPS`)
and its `var()` substitution (`MAX_SUBSTITUTION_COMPONENTS` and
`_BYTES`), not a page's, and substitution costs no match steps. Review
measured 1.3 to 6 ms an element against 2,000 to 10,000 universal
`:not()` rules, 22 s for 5,000 paragraphs, and 1.7 to 3.1 ms an
element where every element takes a `var()` of 30,000 to 40,000
tokens, 33.7 s for 20,000 paragraphs: minutes at 2 MiB or at
`MAX_NODES`, again at every resize. td-layout's page-wide cascade work
ceiling (increment 6) sums both, each element's match steps and its
substitution charges as td-css's `ComputeStats` reports them, and with
its layout work ceiling
(`MAX_LAYOUT_WORK`) is sized so a page costs the render thread a few
seconds at most on the development host, measured at the cutover; a
page past it steps down (above, "Cuts") rather than being endured, and
the cancel flag ends superseded or stopped work between elements.

No new syscall: td-css, td-dom and td-layout are std-only computation
in td-web's process, so its allowlist (§3) is derived as before and
does not change. Nothing in a sheet fetches: td-css follows no URL; a
`url()` reaches td-web only from `@import` and `<link>`, above; td-css
computes no property holding one, and skips those that would (a
background image, an `@font-face` source, a cursor, a list-style
image); a custom property keeps a `url()` among its tokens, and nothing
fetches it. Every subresource request is `public` (§4) and of the
kinds above alone; a `data:` URL makes none. No executable memory (§2).

### Increments

6. **This design**, documentation only. Each prerequisite below is a
   landing of its own in its crate, with that crate's design and
   validation, before the cutover:
   - **td-ui**: prose at a size: `Metrics` and drawing at any whole
     device-pixel size within a named bound, `OVERHANG` an em at each
     size, and atlas pages for sizes but its own capped at `ATLAS` in
     all, the eldest remade when another is needed.
   - **td-layout**: a declared-charge split excluding independently
     owned tree, DOM and CSS storage (above, "Memory"); a styled document
     holding its document by shared
     ownership, so a page passes between threads, as td-web, which
     forbids `unsafe`, cannot hold a borrowed one beside its owner; a
     replaced element's intrinsic size from the caller, in place of its
     300 by 150 placeholder: its `alt` text's line, or nothing, until
     increment 9, its picture's after; list markers as a fragment kind
     of their own (td-layout/DESIGN.md §3); a basic automatic table
     layout, rows and cells with `colspan` and `rowspan` within bounds,
     so a page's tables line up as §5's do rather than stacking as
     blocks; an input limit a caller sets, up to 4 MiB for a text
     page's single text node, beside its 2 MiB `MAX_PAGE_INPUT` for
     markup; a page-wide cascade work ceiling in its `Limits`, summing
     each element's match steps and substitution charges, refused by
     name; a cancel flag checked between elements as it cascades and
     as it lays out; a line past `MAX_LINE_SCALARS` cut and marked
     rather than refused; tab stops every eight columns where white
     space is preserved; a cascade without `style` attributes; and a
     byte limit for each layout request, where today it is fixed when
     the page is cascaded.
   - **td-css**: the owned-budget API supplies compilation admission;
     remaining prerequisites are a refusal that names the sheet it arose in; a media condition for each sheet given to a set; the
     `background` shorthand's colour; and `rgb()`, `hsl()` and CSS
     Color 4's extended named colours.
   - **td-alloc** (available): `Charge::shrink_to`, which returns the difference
     between a charged ceiling and the actual, so td-web need not
     return the whole charge and charge the actual anew.
7. **The cutover**, one landing: the steps above behind `Document`,
   which stays the boundary between the render and window threads,
   with its page, picture and resource-failure bodies; the user-agent
   sheet, `<style>` and `style`; the budgets, places, find, selection,
   copying, links, scrolling, cuts and painting above, and `F9`; td-css,
   td-dom and td-layout joining §2's closure. It deletes td-web's use of
   td-html's rich renderer (`to_rich_within`, `HTML_LIMITS`), the page
   of logical lines, `Rendered` and its wrap, places in logical lines,
   the hang, `MAX_LINES`, `MAX_PRE_CHARS`, the cut-page notice, the
   title scan and the old jail test; amends §3, §5 and §8 to this
   section; and amends what says nothing consumes the new crates
   (td-layout/DESIGN.md §1 and §7, td-css/DESIGN.md's opening,
   td-html/DESIGN.md "Styling boundary", td-dom/DESIGN.md's opening),
   the memory comment in `recipes/src/recipes/web.rs`, and
   APPLICATIONS.md's list of crates whose change re-derives an
   allowlist. A staged cutover would put two producers behind one
   view, or a view over geometry its places cannot name, for as long
   as the stages took, which AGENTS.md's principle 4 forbids; what can
   land first without a second mechanism is the other crates' work in
   increment 6, which is why it is theirs. td-mail and td-news keep
   td-html's renderer, which this does not change. Validation: tests
   of each of the above in-process on the cell, §8's behaviours carried
   over, each rung of the cuts by a lowered limit, a cancelled job, and
   each step's charge refused and shrunk; td-web run under a headless
   td-compositor with the pinned faces at 1920x1200 and 3840x2160 over
   the eight corpus pages (td-dom/DESIGN.md's URLs), the real pages §5
   rests on (the WHATWG HTML and ECMAScript specifications cut to 2
   MiB, SANE's list of backends, long Wikipedia articles), td-layout's
   2 MiB shapes, td-html's hostile ones (one-letter paragraphs in 62
   quotes, list items in 62, a 2 MiB word), hostile sheets for time and
   memory (thousands of universal `:not()` rules, a `var()` of tens of
   thousands of tokens on every element, megabytes of custom
   properties) and many families and sizes on one page (the atlas
   reset every frame), all captured to scratch, never committed; its
   resident peak with a page shown and one made at their largest and
   its cascade, relayout and paint times recorded here in place of §3's
   figures, with the estimates measured: `ATLAS`, the sheets term,
   `WORKING` for parsing and compiling, the cascade ceiling's time,
   `MAX_PAINT`, and whether sizes rounded to device pixels move a
   page's lines visibly against an unrounded layout; the allowlist
   derived again and unchanged; and main's system boot still printing
   `TD-WEB-RUNNING`.
8. **Stylesheets**: `<link rel=stylesheet>` and `@import` as
   "Subresources" says. Validation through the injected fetcher: order,
   media, import depth, the count and byte bounds, mixed content at a
   redirect, a `data:` sheet, a `404`, a wrong type, a slow sheet past
   `STYLE_WAIT`, stopped fetches holding the slots, `Escape`, a reload;
   the sheets term, `STYLE_WAIT` and `MAX_SHEET_FETCHES` measured on
   the corpus with its linked sheets.
9. **Inline pictures**, as "Subresources" says. Validation: placeholders
   becoming pictures, one relayout per settle with the top kept, a
   refused picture keeping its placeholder, an unlinked picture opened
   by a click, mixed content at a redirect, `MAX_PICTURES` and the
   pictures term; the resident peak with a page of large pictures,
   `MAX_PICTURES`, `PICTURE_SETTLE` and the pictures term measured.

No request carries a cookie until a jar is designed (§11).

### Open questions

The estimates above (`ATLAS`, the sheets and pictures terms, `WORKING`
for parsing and compiling, `MAX_PAINT`, `STYLE_WAIT`,
`MAX_SHEET_FETCHES`, `MAX_PICTURES`, `PICTURE_SETTLE`) stand until the
increments that introduce them measure them. So does the value of
td-layout's cascade work ceiling, in match steps and substitution
charges together: no figure is set here, only that it hold a page to a
few seconds of the render thread on the development host, which the
cutover measures with the hostile sheets above.

## 11. Roadmap (not normative)

What a document browser of NetSurf's or Dillo's reach needs, roughly in
order; each is its own design amendment when it starts:

- **Foundations.** A cookie jar kept in the account's home; forms; more
  concurrent fetches.
- **Encodings.** A key choosing the encoding a page is decoded with
  (the HTML Standard's user override, kept with the page in the
  history); the XML declaration for `application/xhtml+xml`; Big5,
  EUC-JP and ISO-2022-JP in td-charset; and detection for unlabelled
  pages in a legacy encoding, which windows-1252 misreads.
- **More faces.** Kerning from `GPOS`, then Noto's CJK (CFF
  collections), colour emoji and variable fonts, each waiting for
  td-ui's reader to take that form (`td-ui/src/sfnt.rs`).
- **Layout beyond normal flow**, as td-layout gains it: float
  exclusion, inline-block, then flexbox and grid; pseudo-elements and
  generated content; element state (`:hover`, `:focus`) reported to
  the cascade.
- **Pictures.** `srcset` and `<picture>`; an SVG subset.
- **Downloads past `MAX_DOCUMENT`.** The fetch service already streams
  a reply in frames to a `stream` request, `public` ones included, so a
  larger save needs no protocol: td-fetch-client's `get_stream` taking
  `public`, a navigation whose reply td-web does not show handed to a
  save before its body is whole, written as its frames come on the
  save's worker, with progress in the status row and `Escape` or a key
  of its own to stop it, the partial file then removed as a failed
  write's is (§5).
- **Saving atomically**: a save written under a hidden name, then made
  its own by `linkat` (failing on an existing name) or `renameat2` with
  `RENAME_NOREPLACE`, so no partial file ever holds the final name: one
  of those calls added to td-web's allowlist, with the probe's witness
  (APPLICATIONS.md, "Per-application allowlists"; UNSAFE.md §9).
- **More of saving**: a `content-disposition` of `attachment` on a type
  td-web shows offered for saving rather than shown; a name and place
  chosen through td-portal's FileChooser, which today admits Firefox
  alone; saving the page as shown (its text) beside its source.
- **Evidence in the image.** A boot oracle that grants `web` a writable
  Downloads through `td-ctl`, starts td-web, saves a reply it does not
  show and a page, and checks from the human's side each file's owner
  (1000), mode (0600), name and bytes; then the same with an `ro` row
  and with the row revoked, each refused with nothing written and the
  status row naming the grant. Nothing yet exercises the jail's branch
  (the mount table, the view) or the realized static binary under its
  allowlist: §5's tests run on the host and fake the mount table.
- **Driving.** td-ui's control socket (td-ui/DESIGN.md), so a test or an
  agent can operate td-web through the verbs td-ui's other programs
  take, and an oracle can boot it in the image. Reading a page back
  needs td-ui's prose-aware read-back first (td-ui/DESIGN.md,
  "Proportional faces", "Not yet"): its seam reads the cell grid, and
  over proportional faces narrow glyphs share a cell, so the text it
  reads is not the text drawn.
- **Primary selection**: the page's selection offered as the primary
  selection, and a middle click pasting it into the address row, once
  td-ui's widget window binds it and delivers the middle button.
- **Environment changes**: td-css saying whether a changed width or
  scale alters which of a set's rules apply, so a resize that crosses
  no breakpoint lays a page out again without cascading it (§10).
- **Owned budgets** in td-css and td-layout, each replacing
  its declared charge in §10's memory with storage held in a td-alloc
  budget.
- **Toward replacing Firefox.** Script (an interpreter, §2),
  bidirectional text and shaping, a per-site process model (which td-jail
  does not offer today: one jail, no nested namespaces), and becoming a
  link handler (§7).
