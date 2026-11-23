# td-web

**Status: normative; every increment of §9 and §10 has landed,
through inline pictures (increment 9); increment 7's validation still
owes what it lists as not yet done.** §10, styled documents, is how
td-web makes, lays out and paints a page. The roadmap (§11) is not
normative; each item there is its own amendment.

td-web is td's own web browser: a td-ui program, built from source in
td's target graph, that reads documents on the web. It complements
Firefox and does not replace it today. Firefox stays td's browser for
the scripted web and stays the default handler for links other
programs open (`td-portal`'s OpenURI names it, §7); a program may ask
the portal for td-web instead, link by link. td-web is designed
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
plugin or WebAssembly; it speaks to the session bus only to hold its
link-intake name and take the calls by which the portal hands it a
link (§7), and calls no portal or audio service; it listens on no socket but td-ui's
control socket, and that only when run on a host with one (§8,
"Driving"); it keeps no
cookie, cache or history on disk in its first increments (§6), and
writes no file but one the person saves into
a Downloads the human granted it (§5, "Saving"); it has no GPU path
(td-ui renders in software, td-ui/DESIGN.md "The GPU path"); and it is
not registered as `BROWSER`, nor the OpenURI target unless a caller asks
for it (§7).

## 2. Trust and build

td-web is a zone-one program (TRUST.md "Trust zones"): the `td-web`
crate is a standalone `td-*` roster crate built by td's Rust toolchain
in the Debian build root like every Cargo recipe. Its closure is `std`
and td's own crates by path: `td-ui` for the window, `td-page` for
making, laying out and painting a page (§10) with `td-html` for
markup's tree, `td-css` for style sheets, `td-dom` for the tree's HTML
projection and `td-layout` for layout, `td-charset` for text in
other encodings, `td-fetch-client` for the network, `td-url` for URLs,
and `td-picture` for pictures, the bounded path over the shared
decoders (`td-png`, `td-jpeg`, `td-gif`, `td-webp`, `td-avif`), shared
crate code any page reader may take at its own limits; td-web's are a
document's body and `PICTURE_WORK` (`picture.rs`). td-charset's
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
the limits below and by its crate's own (td-html's tree limits,
td-css's sheet and rule bounds, td-layout's page and work ceilings, the
picture decoders' pixel caps), and a
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
`web`, its account `tda65540`, its external UID 65540 (mail uses 65537, news 65538, and 65539 remains
reserved, APPLICATIONS.md §L), the workload
account ledger's carried row (td-authd/DESIGN.md, "Workload account
ledger"), its home `/var/lib/td/applications/65540`. Inside its jail it is UID 1000, as
every application is.

It is a `ShippedApplication` on `static-runtime`, packaged by the `web`
recipe from the `td-web` recipe's static binary with its debug
companion, like mail and news: it has no
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

[Session Bus Policy]
org.td.web.*=own

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
home, and none of the human's files but what the human grants. One
bus name, under the `org.td.web.*` suffix grant, and one environment
entry, `WEB_LINKS_FROM=portal`, both the link intake's (§7); no
terminal.

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

**Memory.** The bounds hold td-web's own memory. A page, its tree,
styled document and layout with the decoded markup it keeps, is held to
§10's page budgets under one td-alloc root of `PAGES` (320 MiB), and
what td-alloc does not hold is a fixed term beside it: the window,
td-ui's atlas pages, the bodies of the fetches out and `WORKING` (§10,
"Memory" and "The jail"). Every heavy step runs on one render thread,
for every fetch thread, one job at a time; a navigation stopped or
overtaken before its turn takes none, and a document made for one
stopped while it was made is dropped there before the next reply is
taken. Measured on a development host, release build: td-web started
under a headless td-compositor with the pinned faces and the blank page
holds 23 MB resident at 1920x1200, 34 MB at 2880x1800 and 47 MB at
3840x2160, the largest output td supports, beside its window's
shared-memory buffers, 9, 20 or 32 MB each, of which td-ui keeps up to
three: 50, 95 or 144 MB in all, §10's window term. `memory-high` is
640 MiB (671 MB) and `memory-max` 768 MiB (805 MB); a test
(`the_page_budgets_fit_the_jail`) holds §10's sum at least 20 MB under
`memory-high`. Measured in-process over the pinned faces
(`app::tests::measure_window`, one frame buffer and no fetch client,
§10's "Increments"), the costliest page td-layout measures, 2 MiB of
blocks (115.9 MiB declared), shown with the same made beside it peaks at
171 MiB resident (VmHWM) at 1920x1200 and 193 MiB (202 MB) at
3840x2160: with td-ui's two further buffers, about 266 MB, under
`memory-high` by about 400 MB. A page's text is decoded (§5) on the render thread: sent
in valid UTF-8 it is its body, cut in place; sent in another encoding,
or in UTF-8 holding malformed bytes or ending unfinished, it is decoded
into a string of at most `MAX_MARKUP` or `MAX_TEXT` bytes, had at once,
and the body is dropped. The text then stands in its body's place among
the bodies; a markup page's is copied into the page's budget and
dropped before it is parsed (§10, "Pipeline and threads"), a text
page's once its page is made.

**Page admission and ownership.** The window's Renderer owns one shared
td-alloc root of `PAGES` plus sixteen budget control blocks. A reply
takes a child when its render job begins. Displayed and incoming pages
both count while both live; cancellation returns credit only when the
worker actually drops its work or result. Shared page references use
fallible BudgetShared ownership. The blank page needs no allocation.

The renderer's typed handoff has eight reusable request/result slots,
one for each fetch thread that hands it work, `MAX_WORKERS`
navigations' and `MAX_SUBRESOURCES` inline pictures' (§10, "Slots"),
in one BudgetShared mailbox admitted at startup.
Those slots include active, queued and completed-but-unclaimed requests;
further submitters wait for a free slot. A ticket retains that mailbox's
budget ownership until it is consumed or dropped. Short content types
(up to 128 UTF-8 bytes) fit inside a slot; longer ones use BudgetString
without truncation. Nested bodies and URLs retain their existing
fetch/metadata limits. Ordinary handoffs allocate no backing storage.
Admission/refcount refusal returns the static resource notice, and an
already cancelled reply allocates no handoff. The worker drops cancelled
rendered output before taking another request. Beside the slots, a
lane of `LANE` (4) items holds what the window posts without waiting, a
relayout (§10, "Relayout"), a page, a layout or a page's pictures let
go to be dropped on the render thread (§10, "Handover"), or a `data:`
picture to decode (§10, "Subresources"); the worker takes the lane's items
before the slots', and a post to a full lane is refused, never waited
for.

A dropped ticket abandons only its slot. Queued/running slots stay occupied
until the worker releases them; a completed result is released by its
caller. Ordinary rendering panics complete their ticket without a result
and leave the worker running. An unexpected worker exit completes its
active ticket and cancels queued tickets, wakes blocked submitters, and
the next submission restarts it. Dropping the renderer closes and joins
its worker; this can wait for its bounded active operation. Thread startup
still uses std/OS allocation machinery outside the owned-allocation
guarantee. The handoff module has a strict source guard against standard
heap owners, recognized unchecked allocation operations and std channels.

A navigated picture admits `PICTURE_WORK` (200 MiB) before decoding
and, decoded, keeps its actual RGBA capacity, Charge::shrink_to
releasing the excess without a gap. An inline picture (§10,
"Subresources") admits instead its own decoding's peak, reckoned from
its header by each decoder's layout (td-picture's `work`, at most
`PICTURE_WORK`), to its page's pictures term, and keeps its capacity,
made its own fit in place. `PICTURE_WORK` must cover the reckoning of
every picture td-web admits (a document's body, td-ui's pixel bound):
td-picture refuses a picture reckoned past its caller's cap, so a cap
below it, or an estimate grown past it, would refuse td-web's largest
pictures; `picture::tests::td_webs_costliest_picture_is_admitted` holds
the costliest, a 16-bit RGBA interlaced PNG, within it. A held save body owns its capacity
charge through the save worker, even after its Document is dropped. A
wrapper frees its value before releasing its charge.

A fitted picture admits at most 64 MiB before fitting, then retains its
actual buffer capacity. Overlapping owners keep both charges. A refused
fit paints a static resource notice, tried again after a resize or a
navigation; a page refused at every rung paints the same notice (§10,
"Cuts"), and a refused relayout keeps the old layout or steps down
(§10, "Relayout"); none aborts or panics. The text of td-web's own pages
is bounded to `MAX_OWN_TEXT` (4096) UTF-8 bytes.

Find owns its folded query, failure table, position ring and matches
under a 2 MiB child of the page's budget (`SEARCH_BYTES`), including
metadata, taken for each search and returned with its matches. Its
query is bounded to 256 Unicode scalar values and its matches to
10,000. A refused search keeps the old marks, current-match index,
viewport, focus and selection; Enter in find, or n/N on the page,
retries it. A relayout finds again on the new layout; refused there, it
keeps the old marks, whose places outlive a layout (§10, "Places").

Selection copies measure their UTF-8 pieces before allocating, refuse above
1 MiB, and retain their byte storage against the page's budget. Up to
80 bytes fit inline without a heap allocation, inside the window's bounded
source/send values; longer copies charge exact byte capacity and a shared owner. The window's clipboard source and each active send keep that ownership
after navigation drops the page; credits return only when the last payload
owner drops. Overlapping clipboard replacements count both owners. Address and
find selections use the same fallible payload against the window root;
cuts delete the entry only after
the clipboard accepts it. A refused copy preserves the clipboard and selection
and uses a static status notice. URL resolution/formatting and Wayland protocol
metadata still use their existing allocations; budgeted text delivery does not
claim to migrate that IPC machinery or incoming pastes.

PNG reserves once from its validated inflated size before decoding.
Bounds up to 16 MiB round up to a power of two for reuse; larger bounds
reserve exactly. The smaller case fits below 81 MiB with RGBA and row
scratch; the larger case avoids geometric spare capacity and old/new
inflater-buffer overlap. At
td-web's picture bounds, inflated rows plus RGBA and row scratch fit below
200 MiB; compressed IDAT growth (at most three times the 16 MiB input
while replacing a buffer) and IDAT capacity beside inflated rows are
smaller. JPEG's coefficient/plane peak is at most 192 MiB plus bounded
tables; WebP's working bound is below 145 MiB; AVIF's codec plus its at-most
16 MiB coded copy is below 96 MiB, or 88 MiB for YUV plus RGBA. Thus the
200 MiB allowance covers these decoder layouts, apart from allocator/RSS
bookkeeping. A raw RGBA or fitted pixel allocation is at most 64 MiB.

The picture allowance is an admission model, distinct from the exact
ownership of the parser, CSS and layout storage in §10. td-alloc does
not instrument font and raster ownership, fetch/window IPC or
URL/response metadata. Those source-inventory entries remain migration
debt; this is not a whole-process allocation-failure guarantee. The
root cap supplements the per-input/work limits and the jail's
process-wide limit; it does not replace them. The memory ownership
module has a strict guard against unchecked allocation operations and
standard heap owners.

The allocator (glibc's) gives a thread that allocates beside another an
arena of its own and keeps much of a step's peak in it after, for that
arena's next step to reuse, so heavy steps spread over the fetch
threads would keep that peak once per thread alive. So every heavy step
runs on the one render thread, a page's last drop happens there (§10,
"Handover"), and the fetch threads allocate only their requests and
bodies. A panic while a reply is made answers an error page and leaves
the thread to take the next (td-web's build unwinds: a `compile_error!`
refuses one that would abort on panic, by a profile's `panic` or a
recipe's `-C panic`), and a render thread found gone is started again.
`memory-high` throttles before `memory-max` ends the jail. The tasks
hold the window's thread, td-ui's, the render thread, `MAX_WORKERS`
fetches and `MAX_SUBRESOURCES` subresource fetches (§10,
"Subresources"), 11, and the link intake's thread with its passing
connect thread (§7), 13, with room, since td-svc does not restart a
program that exits on the cap. One CPU, twice news's, since a page is
made and laid out again on the render thread while the window's paints
(§10).

td-jail gives td-web an allowlist seccomp program in place of the
standard one, derived from its realized binary as mail's and news's
were (APPLICATIONS.md, "Per-application allowlists"): the list every
td static application shares, without td-kv's `fdatasync`, which mail
and news add and td-web does not link. What is off it answers `ENOSYS`;
`socket` is `AF_UNIX` alone and `ioctl` `FIONBIO` alone. A change that
reaches for another operating-system facility, in td-web or a crate it
links, derives the list again, or that path fails in the jail. The
control socket (§8, "Driving") is such a path: its `bind`, `listen`
and `accept4` are off the list, and the jail never starts td-web with
one. The link intake's bus connection (§7) is on it as it stands: an
`AF_UNIX` `connect` with read and write timeouts, then reads and writes;
it negotiates no descriptor passing and sends no signal. Saving (§5) is held to that list: it reads its mount table and
creates, writes, syncs, empties and unlinks a file (`openat`, `read`, `write`,
`fsync`, `ftruncate`, `statx`, `unlink`), calls td-ui's shared-memory
buffers and theme file already make, and renames and links nothing,
which would want `renameat2` or `linkat`.

**Its own fetch service.** A service-account jail takes the fetch
socket only from its own runtime directory, owned by its own UID
(td-jail `sockets=fetch`). td-web therefore has its own `td-fetchd`,
td-svc's `[web-fetch]`, run as `tda65540` with its socket at
`/run/user/65540/td-fetch/socket`, as mail and news each have their own
under 65537 and 65538; `[web]` requires it. It serves eight counted
requests at once (`td-fetchd run --workers 8`), td-web's `MAX_WORKERS`
navigations and `MAX_SUBRESOURCES` sheet fetches, stopped ones
included, so neither waits behind the other there (§10, "Slots");
`subresource.rs` asserts the sum at compile time against
`SERVICE_WORKERS`, and a recipe test holds the unit's line, that
constant and td-launch's `WEB_FETCH_WORKERS` equal, the eight td-launch
gives td-web's own service on a host (`./install-apps`). It runs on
every boot,
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
- A **subresource** is a request the document chose: a stylesheet, and
  later an inline image or a frame. It is always sent `public`, so a
  page on the internet cannot make td-web touch a device on the local
  network. td-web makes no request the person did not cause by
  navigating. A `<meta http-equiv=refresh>` is neither followed nor
  shown.

**Bounds.** A body is taken up to `MAX_DOCUMENT` (16 MiB), a picture's
too; a larger body is refused by the service and shown as an error. A
picture is shown within td-ui's bounds (8192 pixels an axis, 16 Mi
pixels): a larger JPEG is decoded at the largest of td-jpeg's scales
that fits, a larger PNG, GIF, WebP or AVIF is refused. Markup past `MAX_MARKUP` (2 MiB)
and text past `MAX_TEXT` (4 MiB), both counted in the UTF-8 they
decode to, are not rendered, and the page says so (§5). Requests carry
`accept: text/html, application/xhtml+xml, text/plain;q=0.9,
image/png;q=0.8, image/jpeg;q=0.8, image/gif;q=0.8, image/webp;q=0.8, */*;q=0.1` and td-web's
`user-agent`, a stylesheet's `accept: text/css,*/*;q=0.1` (§10,
"Subresources"). Every request names its limit, so the service asks for
gzip and deflate itself and hands td-web the decoded body, the limit
bounding its decoded bytes (APPLICATIONS.md, the fetch service).

## 5. Documents

A response is shown by its type, the `content-type` media type, else a
sniff: `<!doctype html` or `<html` at its start, after spaces and a
UTF-8 byte order mark, is HTML; a picture's signature a picture; a byte
order mark, or no byte of binary data in its first 1445 bytes, the
resource header (MIME Sniffing, "Rules for distinguishing if a resource
is text or binary"), text:

- `text/html` and `application/xhtml+xml` are made into a page as §10
  says: parsed into a tree, styled with td's sheets and the page's own,
  and laid out on the render thread (§3), then painted by td-web on the
  window's.
- `text/*` is a text page (§10, "Cuts"): its text laid `pre-wrap` in
  the monospace face, with tab stops every eight columns.
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
want of memory for its text fails a page, as the static resource notice
(§3). `MAX_MARKUP`
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

Markup past `MAX_MARKUP` (2 MiB) and text past `MAX_TEXT` (4 MiB) are
not parsed, and the status row says how much of the reply is shown. A
status other than `2xx` with a body is shown with its status in the
status row; one without a body is an error page.

What a page is and how it is shown is §10's: the sheets that style it
("Sources"), its faces ("Faces"), its places, find, selection, copying,
links, title and scrolling ("Places, find, selection and links"), what
is shown of a page too large to lay out, a text page cut by the lines
it lays among it ("Cuts"), and its paint ("Painting"). Without the
proportional faces (none installed, `TD_UI_FACE=bitmap`, every
in-process test) td-ui measures every glyph a cell and the page draws
on the cell. Text is unkerned and unshaped (td-ui/DESIGN.md,
"Proportional faces").

## 6. State

A td-web window keeps, in memory only: its back and forward history
(each entry a URL and the place its top was left at), the current
document and its layout. Nothing is written to disk in the first
increments, no cookie, cache, history or bookmark, but a file the
person saves into Downloads (§5, "Saving"), which is the human's, not
td-web's state. Closing the window forgets everything. A later
increment that keeps state writes it under the account's own home,
never in the login user's.

## 7. Other programs

Firefox remains `BROWSER` for td's applications and the OpenURI
target by default (`td-portal/src/open_uri.rs`). td-web is started from
its launcher card, as every service-account application is (it has no
`/bin` name); the program takes an optional URL argument, `td-web
[--control-socket PATH] [URL]`, which hosts and tests use (the socket is
§8's "Driving").

A program may ask for td-web link by link: `td-open --with web LINK`
sets the OpenURI request's `td.handler` option to `web`, and the portal
hands the link to td-web rather than Firefox (APPLICATIONS.md §E row 4).
td-web's side is its link intake (`links.rs`), on only when
`WEB_LINKS_FROM=portal`, which the `web` application's manifest sets, so
td-web run on a host takes nothing from that host's bus. The intake
connects to the session bus on a thread of its own and takes
`org.td.web.p<pid>` under the `org.td.web.*` grant, else, held already,
`org.td.web.u<unique>` from the unique name the bus gave it, which the
bus never gives twice. The jail's pid namespace makes the first nearly
constant, so a same-uid process can hold it first; one that foresaw the
second too would keep the intake off, which the login user's own
processes can do to td-web in many ways. It serves `OpenURL(s)` at
`/org/td/web` on interface `org.td.web`, and nothing else. The portal authenticates td-web by its broker credentials
before calling; td-web in turn takes a call only from the present owner
of `org.freedesktop.portal.Desktop`, which the broker reserves to
td-portal, asking the bus for that owner on each call, so neither
another application (which the broker would not route) nor the login
user's own unjailed processes (which it would) can hand td-web a link.
The link must be `http` or `https`, with no whitespace or control
character, within the portal's 8192-byte bound, as the portal admits
one; the window opens it as a typed address, which td-url encodes or
refuses further, replacing the page shown, which stays in history
behind it. At most four links wait for the window, and a fifth is
refused as busy; up to four calls that arrive while the intake asks the
bus something wait to be answered after, and a fifth is refused as
busy. A frame larger than 64 KiB, which no portal call is, is read past
and dropped; a failure stops the intake, said once on stderr, and
td-web runs on. While the intake runs an idle window looks for links
every quarter second. A burst of traffic or a slow bus while the intake
asks who the portal is leaves that call refused, to be asked again, and
the intake running; the bus's late answer is passed over.
Like Firefox, td-web is found, not started: a link for a td-web that is
not running fails, and the caller is told so (APPLICATIONS.md §W.9).
td-web opens no file, so `OpenFile` stays Firefox's.

## 8. Window and keys

One td-ui window: an address row on top, the page below, a status row
at the bottom. The status row says, first that applies: what the last
input made happen (a link not followed, a refusal, a copy), until the
next key, click, wheel turn or paste, and what a save did, once it
ends, until the same; what is loading, `loading styles of` a URL while
its page waits for its sheets and `loading pictures, K of M shown`
while its pictures come (§10, "Subresources"); that a save runs;
where the link
under the pointer, else the focused link, goes; the response's status, the place in the page, the
encoding its text was decoded from when not UTF-8 (§5), and any cut.
The place is `N of M lines`: the last line shown whole, of the
layout's lines at this width (§10, "The status row"). Its app id is
`td-web`; its title is the document's `<title>` (§10, "Links"), else
its URL, then ` - td-web`.

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
| `Left`, `Right` (page) | scroll across a page wider than the pane |
| `Shift+Left`, `Shift+Right` (page) | move the selection's end a character; with `Ctrl`, to the word before's start or the word after's end |
| `Shift+Up`, `Shift+Down` (page) | move it to its paragraph's start or end, from there the paragraph before's start or the one after's end |
| `Ctrl+A` (page) | select the page |
| `Escape` (page) | stop loading, else the page's pictures still to come (§10, "Subresources"), else a save still fetching (§5), else clear the focused link, find's marks and the selection |
| `Ctrl+C` | copy the address or find row's selection while it has focus, else the page's selection, else the focused link's URL, else the page's |
| `Ctrl+S` | save the document into Downloads (§5, "Saving") |
| `F8` | body text in Noto Sans or Noto Serif, for this run |
| `F9` | the page without its own styles, or with them (§10, "Without the page's styles") |
| `Ctrl+Q`, `Ctrl+W` | close |
| `?`, `F1` | the key list (`F1` is the window's own) |

A click on a link follows it at the button's release (below); the
pointer over one says where it goes and marks it as the focused link
is marked. The wheel scrolls up and down, a line for each row it turns,
and across, four cells for each column it turns, as `Left` and `Right`
do a press. Which link or place a point is on, what find
matches, how a match or a link is shown and how far each key scrolls
are §10's "Places, find, selection and links". A navigation's fetch
runs on a thread of its own, which hands the reply to the render thread
(§3) for its decoding and its making into a page or a picture, and
waits for it there, so the window stays responsive while a page loads,
and `Escape` on the page stops waiting for it; laying the page out
again at a new width, scale or face, or for `F9`, is the render
thread's too (§10, "Relayout"). A stopped fetch runs on until the
service answers or times out, so td-web keeps at most `MAX_WORKERS` (4)
fetches out, stopped ones included, a save among them (§5), and asks
the person to wait past that; a navigation overtaken by another is
stopped. The history keeps at most 100 pages each way, each at its
top's place (§10, "The top's place"), and moves only when a page
arrives: a step back or forward that is stopped, refused or overtaken
leaves it as it was. A relayout keeps the text at the top, as shown
when the new layout comes, and the focused link, by its element; with
find's marks shown it finds again on the new layout and marks the
matches, the current one's index kept as far as there are as many,
nothing moved to or selected. A relayout that steps down to another
rung's tree (§10, "Cuts") lets the selection, the marks and the focused
link go, places of the old tree naming nothing in the new.

**Selection.** Text in the page is selected as two places (§10,
"Places"): where it was begun and where it is now, in either order. So
it is kept unchanged however far the page scrolls, and through a
relayout that keeps the page's tree, as the place at the top is.

- **Pointer.** A press on the page and a drag select from the place
  nearest the press to the one nearest the pointer, as §10's
  "Selection" places a point. Once the pointer has moved from where it
  was pressed, held above the page or below its last line shown whole
  it scrolls it once each `DRAG_SCROLL_MS` (50 ms), a line for each
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
  release and within `NEAR` of it selects the word of letters, digits
  and `_` under it, across fragments and lines but not past its
  paragraph (off a word, nothing); a third selects the paragraph whole;
  a fourth is a first again, and the drag of a double or triple click
  moves nothing. The window reports no click count, a pointer input
  carries no time, and inputs are handled before the turn's `poll` is
  told one, so td-web counts clicks on its own monotonic clock, read at
  each press and release. A click that follows a link, a drag, a Shift
  press and a press the pointer's leaving cancels are no click a later
  press pairs with. A Shift press moves the selection's end there from
  its anchor, and its drag goes on moving it; with nothing selected it
  begins there. A Shift press follows no link.
- **Keyboard.** td-web has no caret; the keys move the selection's
  end. Find's current match is the selection whenever find moves to
  one, as the query is typed, by `Enter` or by `n`, and a query that
  matches nothing leaves none. On the page `Shift+Left` and
  `Shift+Right` move the end a scalar, an image whole or the white
  space a wrap left, from a paragraph's end to the next's start and
  back; `Ctrl+Shift+Left` and `Ctrl+Shift+Right` to the start of the
  word before or the end of the word after; `Shift+Up` and
  `Shift+Down` to the paragraph's start or end, and from there to the
  start of the paragraph before or the end of the one after; the page
  scrolls as little as shows where the end went. With nothing selected
  they begin at the focused link's start, else the top's place.
  `Ctrl+A` selects the page.
- **One of a selection and a focused link.** Selecting lets the
  focused link go and `Tab` clears the selection, so `Ctrl+C` copies
  the one there is (the precedence in the table above). `Escape` on
  the page clears the selection with the focus and find's marks,
  unless it stops a load or a save still fetching. A page arriving, by
  any navigation, reload or history step, starts with none, a press
  held over the page before it is dropped, and the history keeps no
  selection.
- **Painting.** Each glyph between the two places is drawn on
  `SELECTED` in paper ink, as the focused link is (§10, "Painting").
  Find's current match is drawn so because it is the selection; once
  another selection replaces it, it is drawn on `INACTIVE_SELECTION`
  like every other match. A selection reaching lines far from those
  shown costs nothing until they are shown.
- **Copying.** `Ctrl+C` copies the page's text between the two places
  as §10's "Copying" gathers it. A copy holds at most `MAX_COPY`,
  td-ui's clipboard ceiling (`clipboard::MAX_BYTES`, 1 MiB), which the
  window keeps for its sends: a selection past it is
  not copied and the status row says so. The selection's text is
  measured before any of it is gathered and gathered into budget-owned
  storage retained through delivery: at most 80 inline bytes per owner,
  otherwise exact capacity and a fallible shared owner. Allocation refusal
  copies nothing, preserves the previous source and selection, and reports
  a static notice. Copy and cut feedback shares the ordinary status message:
  a later save result replaces it. Pending messages also occupy the Find
  row until the next input restores its entry. Even `Ctrl+A` on the largest
  page cannot gather more than `MAX_COPY`; this storage is included in
  §3's root.
- **No primary selection.** td-web sets no primary selection and pastes
  none on a middle click: td-ui's widget window binds the primary
  selection for no handler and delivers no middle button (td-ui/DESIGN.md,
  "Widget window"), so that is td-ui's increment before it is td-web's
  (§11).

**Driving.** td-web takes td-ui's driven seam (td-ui/DESIGN.md,
"Driving" and "The semantic seam"), so a test or an agent operates the
window through the verbs td-ui's other programs take. `td-web
--control-socket PATH` serves it on a private socket td-ui's binder
admits, bound before the display is connected; each turn answers at
most `CONTROL_PER_TURN` (4) requests and, while the socket is served,
waits at most `LOADING_WAIT_MS` (50 ms). `td-web --help actions`
prints the action table.

- **Actions.** `open HEX_ADDRESS` goes to an address as the address
  row's `Return` takes it. Every other action is one chord of the keys
  above, delivered through the key path, so it means what that key
  means where the focus is: `address`, `back`, `forward`, `reload`,
  `stop`, `next-link`, `previous-link`, `follow`, `find`, `page-down`,
  `page-up`, `top`, `bottom`, `select-all`, `copy`, `save`,
  `body-face`, `page-styles` and `close`. The seam's inputs reach no
  clipboard, so a copy through it is refused and the status row says
  so; a paste is not one of its inputs.
- **Inputs.** A key, a press and a release go through the window's own
  paths, as td-ui's window delivers them: a press holds the button and
  tells the window the pointer no longer hovers, a move while it is
  held is the drag's and otherwise says where the pointer is, and a
  release lets the button go and says where it is. The key list opens
  from the person's keyboard and pointer only (td-ui/DESIGN.md, "Key
  list"), so a `?` through the seam leaves it closed. A press carries
  no Shift, `focus` changes nothing, the roles `held` name nothing, and
  `tick MS` is the window's poll at that time on the window's own
  clock, taking what has come. An input answers `quit` once the window
  closes and `changed` when it must paint again, which every key,
  press, release, wheel turn and resize the window takes asks, whether
  or not it moved anything; a move with no button held answers
  `changed` only when the hover moved, and `focus`, `held` and a tick
  that took nothing `ignored`; a press while one is held and a release
  with none are dropped, `ignored`, as the window drops them. On the
  live window the display sets the surface, so its next paint undoes a
  `resize`.
- **State.** `KEY=VALUE` fields, a value's controls each a space: `url`;
  `title`, the window's; `focus`, `address`, `page` or `find`;
  `address`, the row's text; `loading`, `none`, `page`, `styles` or
  `pictures`; `body`, `blank`, `page`, `picture` or `refused`;
  `status`, the response's; `back` and `forward`, the history's
  counts; `link`, the focused link's address, and `hover`, the one
  under the pointer; `selection`, `some` or `none`; `query` and
  `matches`, find's; `styles`, `page` or `none`; `face`, `sans` or
  `serif`; `saving`, `1` while a save runs, else `0`; and `line`, the
  status row's text, which find's row replaces while find has the
  focus and no message is shown (its text is then `query`).
- **What it shows.** `text`, `frame` and `frame-page` read one paint of
  the window, the one its raster takes, recorded in order into storage
  charged to §3's root, at most `MAX_RECORDED` draws (the page's
  `MAX_PAINT` and 65,536 for the two rows), and replayed; a recording
  refused answers `limit`. The page's glyphs come after its fills and
  its spaces are drawn (§10, "Painting"), as the seam's read-back of
  prose assumes. td-ui paints the seam's frame with its pinned cell
  face alone and its default theme, so with the prose faces loaded the
  page's text is drawn there in the bitmap face at the pens the prose
  laid, and in that theme; the draws are the window's. The key list
  td-ui's window draws over the page is not in it, and td-ui draws a
  composition's picture at its fitted size, so a picture the window
  draws at a factor (at scale two, one at its own size, §10,
  "Subresources") is drawn smaller in `frame`; `text` reads no picture.
- **On a host only.** td-jail's `web` allowlist admits no `bind`,
  `listen` or `accept4`, which answer `ENOSYS` there, so a socket asked
  for in the jail fails before a window appears, and the application's
  entry asks for none: a jailed td-web is never driven (§3). The seam
  serves td-web run on a host (`./install-apps`), its socket in a
  directory the person keeps private (mode 0700), with the person's
  authority over the window.

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

Increments 6 onward are §10's; 6 to 9 have landed.

## 10. Styled documents

A page is a document tree, styled by td-css, laid out by td-layout and
painted. Increments 8 and 9 (below), stylesheets and inline pictures
fetched as subresources, have landed; this section is the contract.
The making, laying out and painting, and the places over a layout, are
td-page's (td-page/DESIGN.md), a crate of their own so another program
can show a page, and this section governs them; the fetching, the
threads and the window are td-web's.

### Pipeline and threads

A reply shown as a page is made on the render thread (§3), one job at
a time:

1. **Decode** (§5, "Encodings"): at most `MAX_MARKUP` bytes of UTF-8,
   the body dropped. A markup page's decoded markup is copied into the
   page's budget, charged at its length, and kept, shared, for the
   page's life, so the text-only rung (below, "Cuts") can read it after
   a relayout that let the old layout go first; td-layout's styled
   document also counts it as its input.
2. **Parse** with td-html's document parser (`parse_with_budget`,
   td-html/DESIGN.md "Document tree") into an immutable tree, its byte
   limit what the page's budget admits (below, "Memory"). Its storage
   retains the supplied tree budget until the last tree owner drops.
3. **Sheets**: the user-agent sheets and the page's own (below,
   "Sources"), its linked and imported ones gathered first (below,
   "Subresources"), compiled into one `td_css::StyleSet` under td-css's
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
finishes the one it is in and then sees its flag. td-layout tests its
limits before the flag, and a flag set during the last element or after
layout's last read can still return the result, so td-web tests its own
flag after each step, before taking a result or stepping down on a
refusal: a step refused by a limit after its job was stopped gives the
job's cancellation, never a lower rung. td-web's own walk over a new
layout, up from each box to its nearest ancestor with geometry, reads
the flag at its first step and every 4,096 after (`WALK_INTERVAL`),
since a deep nest of elements without geometry over many boxes makes
that walk long.

The viewport, in CSS pixels, is the page pane less a cell's inset each
side, divided by the scale, and never narrower than `MIN_WRAP_EMS` ems;
a narrower pane shows the page's left and scrolls across it. The
render thread measures with its own clone of the faces the window
draws with (td-ui/DESIGN.md, "Measuring and wrapping"), and the
window thread replays advances with
its own: the same faces at the same scale, so the two agree.

A page then holds the tree; a markup page's decoded markup (step 1);
its sheets' text, kept so a changed environment can compile them
again, but no compiled set until §11's environment answer needs one
kept; the styled document, its pool and boxes; a table of each node's
place in tree order; and one layout with what td-web indexes over it,
made for a viewport size, a scale, a body family and whether the
page's own styles apply (`F9`). It holds no body, td-dom's index or
cascade scratch. td-web's own tables are td-alloc `BudgetVec`s in
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
charges anything for the new one. The same holds for everything else
the window lets go: a relayout's answer it no longer wants (a
navigation's page arriving first), the page a refused let-go relayout
leaves, and a navigation stopped or overtaken, whose answer channel,
with whatever it holds or is yet sent, is handed over whole; a fetch
thread whose answer the window no longer takes lets the document go
there too. A let-go the full lane refuses is held, `PENDING` (8) of
them, and posted at the window's next poll; only past those is one
dropped on the window thread, which takes a burst of let-goes while
one relayout runs. A post to a render thread that was lost starts
another, as a reply's submission does, and a relayout whose job ends
without answering, as one that panicked does, is answered refused, so
the window never waits on it. The window cancels a relayout it lets go
and takes its answer under the answer's lock, under which the render
thread writes it: a page made after that is written refused and
dropped there.

**Relayout.** What changed decides what is done again:

- `F8` changes the body family (below, "Faces"): a layout only.
- `F9` cascades the page again with or without its own sheets and
  `style` attributes (below, "Without the page's styles"), then lays it
  out.
- A picture arriving (below, "Subresources") changes its image's
  intrinsic size, which td-layout takes with each layout request: a
  layout only.
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
- For a text page, a change of face or scale counts its text again in
  the least viewport (below, "Cuts") before it is laid out, and cuts it
  further where it now passes: a round trip through a denser face can
  leave less of it shown, until it is loaded again.

A recompiled set is charged to the sheets term while it lives, and the
new styled document and layout are charged beside the old ones, which
the window paints meanwhile. td-web decides that as it asks, from what
the old ones hold and the root's headroom, not by trying: it predicts
the new output as the old output times the old viewport's width over the
new one's, at least once and at most 64 times, plus the old styled
document when it cascades again, and the new ones fit beside the old
when the tree, the old styled document, the old output and that
prediction come to at most `MAX_PAGE_BYTES` and the prediction is within
the root's headroom (`fits_beside`). When they do not, the old styled
document and layout are let go first (above, "Handover"), the pane
showing paper and the status row `laying out` until the new ones come. A
relayout refused beside the old ones is refused whole: the old layout
stays shown and the page on its rung. One refused after the old ones
were let go has no old layout to keep, and the page steps down from its
rung (below, "Cuts").

td-layout measures the whole flow on every layout (td-layout/DESIGN.md
§6): a width change of 2 MiB of synthetic markup takes 54 to 67 ms
(medians, synthetic metrics; 105 to 194 ms under a heavier load) and the
eight corpus pages 0.4 to 8.5 ms, on top of the cascade above. Measured
on the cell (a development host, release build, td-page's
`pipeline::tests::measure_pages`), seven of the corpus pages (GitHub's
not fetched) are made, parsed, compiled, cascaded and laid out at 400
CSS px, in 1.7 to 225 ms, and laid out again at 1200 px with the old
layout kept in 1.4 to 207 ms. Over the pinned faces, through the whole
window (`app::tests::measure_window`, a process a page and output),
they are made in 2.5 to 249 ms at 1920x1200 and 2.8 to 251 ms at
3840x2160, and laid out again two thirds as wide, the old layout kept,
in 2.0 to 225 ms and 2.1 to 229 ms, The Guardian's front page the
slowest; td-layout's 2 MiB shapes are made in 78 to 587 ms and laid out
again in 59 to 519 ms at 1920x1200, and in 82 to 646 ms and 89 to 521
ms at 3840x2160, blocks the slowest. So a relayout runs on the render thread,
never the window's, and a resize costs a layout of the whole page, not
of the lines shown. The window thread asks for one and goes on painting
the layout it has, at its own width, cut at the pane's edge or with
paper past it, the status row saying `laying out` until the new one
comes. Requests coalesce: a page has at most one waiting, a newer one
replacing it, and one running is cancelled by its flag when a newer one
is asked for, between elements of its cascade or its layout. The text at
the top of the pane is kept (below, "Places").

**Whole, not windowed.** td-layout can keep only the lines meeting a
window but measures the whole flow for it all the same (its windowed
and full work are identical), so a window would cost a whole pass at
every scroll past it. td-web keeps every line: scrolling is a paint,
and its memory is the layout's output, which the page's budget counts:
68.6 MiB for the costliest 2 MiB shape measured, sixteen-letter
paragraphs, 168.2 MiB for 4 MiB of 43-letter ones at the narrowest
pane, and at most 37.8 MiB for a whole corpus page, tree and styles
included.

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
its storage in td-alloc owners. td-web passes a child of the page
budget to `parse_with_budget`, capped at `MAX_TREE_BYTES` plus its
control block; it takes no second reservation for that storage. The
parser's cumulative `charged_bytes` remains a separate complexity bound.

td-dom indexes likewise use the supplied page budget through
`HtmlTree::with_budget`; they keep both array charges until dropped.

td-css owns syntax, compiled rules, compute scratch and retained styles
through td-alloc. td-web passes the sheets child to
StyleSet::new_with_budget and the page child to compute_with_budget and
StylePool::with_budget. Shared strings and custom tokens retain their
original child's admission until their last owner drops, including a
computed style that outlives its StyleSet. Dropping the set cannot return
that still-owned charge or move it to the styled term.

td-layout owns its arrays and scratch through td-alloc. td-web passes
the page budget to StyledDocument::shared and to
block::layout_with_budget. The shared styled document owns the immutable
Document without a self-referential borrow. Its arrays and retained styles
keep their charges until dropped; the temporary DOM index returns its
actual admission when construction ends. Outputs retained across resize
count against their common ancestor automatically. Its declared charges
come by owner (`StyledDocument::charges`, td-layout/DESIGN.md §2): the
tree's is the document's own, the same in every styled document holding
it, and taken once; the styled term takes `styled` alone, the input
among it; td-dom's `index` and construction's `scratch` are admitted
only while construction runs. A layout's admission starts from tree and
styled (`held()`), and its `page_bytes` adds any
`retained_output_bytes` and the output; it sees no other styled
document, so one sharing the tree, or kept beside it, is the caller's
to fold into `retained_output_bytes`.

No external reservation covers these owned arrays. Limits::bytes, or a
layout request's own Request::bytes, and Request::retained_output_bytes
still govern the conservative layout admission model, independently of
the actual shared budget. The consumer
must own its input and its additional tree-order and viewport indexes
through td-alloc as well. Any refusal follows "Cuts" below.

| Term | Charged | Ceiling |
|---|---|---:|
| tree | td-html's owned allocation layouts, including parser scratch and growth overlap | 128 MiB, `MAX_TREE_BYTES`, plus budget metadata |
| styled | owned input, boxes, CSS pool and td-web tree-order table; excludes independently owned tree and DOM storage | the page's |
| markup | a markup page's decoded markup, at its length, for the page's life; `styled` declares it again as td-layout's input | `MAX_MARKUP` |
| index | td-dom's owned arrays and construction's per-node maps, only during construction | `MAX_INDEX_BYTES` and 80 bytes a node, within the page's |
| layout | owned geometry, lines, fragments and td-web's indexes over them | the page's |
| page | tree, styled and layout together | 224 MiB, td-layout's `MAX_PAGE_BYTES` |
| sheets | their text, linked and imported sheets' kept for the page's life, compiled rules and scratch; referenced strings/tokens remain charged until their final owner drops | 48 MiB |
| pictures | inline pictures' pixels, their fitted copies, the peak of the one decoding and their bookkeeping | 48 MiB |

The ceilings of page, sheets and pictures sum to `PAGES`, so a page
alone is refused only by its own terms and what the root holds beside
the pages: the render thread's reusable slots and lane, the faces'
handle, each relayout's cancel flag and answer slot, the paint order's
scratch (4.5 MiB taken at the first paint, grown to at most 18 MiB by
a paint that fills it, "Painting" below) and the window's measure
caches, a few MiB in all and about 20 once that scratch is grown,
which come out of `PAGES`; and, only while td-ui's seam reads the
window on a host (§8, "Driving"), its recording of one paint, 64
bytes a draw, at most 20 MiB and 36 MiB while it doubles the last
time, so a page made meanwhile can be refused for it. The sheets child
bounds simultaneously live compilation layouts,
including scratch and replacement overlap. Source-size limits alone do
not guarantee admission: a sufficiently complex sheet can exhaust 48 MiB
before compilation finishes, and the page follows "Cuts" below. A
page's gathered sheets (below, "Subresources") hold at most 4 MiB of
text, their entries, at most twice `MAX_SHEET_FETCHES` and never
grown, and their fetches' URLs, each at most `MAX_URL`,
`gather::MAX_HELD`, in a child of the page's budget, and the
compilation's child is the sheets term less what they hold. Measured
(`subresource::tests::measure_sheets`, the corpus of
increment 8 with every sheet it links and imports, the least child
that compiles them found by halving), a page's sheets need at most
27.2 MiB of the term, GitHub's 23 sheets of 3.6 MiB of text, which
compile to a set of 13.7 MiB; The Guardian's inline sheets 11.7 MiB;
every other page 0.2 to 5.5 MiB. The term stays 48 MiB, its room
over the corpus for hostile sheets, which it refuses.

A page's pictures (below, "Subresources") are held in a child of its
budget, the pictures term: each decoded picture's pixels at its
decoded size, the copies fitted to boxes that are not a whole multiple
of that, the working peak of the one being decoded, charged before it
begins and returned once it is kept, and their bookkeeping (each
picture's entry, its inbox, at most `MAX_PICTURES` places, and a
fetch's URL). A copy being made holds its pixels and the one pass
between its two in the term too. A layout's list of its fits is the
page's, charged to its budget with the rest of the layout. The
reckoned peak is the largest of the decoder's phases, each holding
what it holds once the last let go, not their sum: a PNG's gathered
stream (three times its body while it is grown), that stream beside
its inflated rows, then those rows beside RGBA; a JPEG's sample
planes, padded to whole MCUs, beside RGB, then RGB beside td-web's
RGBA, a progressive frame's whole-size coefficients beside its planes
before them (td-jpeg's `Header::progressive`; a baseline frame holds
none); WebP's two 32-bit images or its macroblock planes beside RGBA;
AVIF's coded copy beside the codec's frames, in 64-pixel superblocks,
so a picture one pixel tall is reckoned at 64, then its planes beside
RGBA. The decoding's scratch is the pictures term's, not the window's
working allowance: the term's ceiling, inside `PAGES`, bounds both. Over
a corpus generated on the host (`cjpeg`, `cwebp`, `avifenc`, `ffmpeg`;
baseline, progressive and progressive 4:4:4 JPEG to 4000x3000, PNG,
GIF, lossy and lossless WebP and AVIF to 2048x1536, and each 8192x1 and
1x8192, AVIF's thin one as 8192x64, the same superblock row), the
resident peak's growth decoding each was 0.08 to 0.99 of its reckoning,
a 4000x3000 baseline JPEG and its progressive 4:4:4 transcode the
closest, as their RGB beside RGBA is all they hold; each format's
reckoning is pinned by
td-picture's `tests::each_estimate_is_its_decoders_largest_phase`. Measured
(`app::tests::measure_window` over the corpus below, with every
picture each page names, and `pictures::tests::measure_decoding`, a
process a picture), and before block-level and floated pictures were
laid, which the corpus's pages hold few of, a page's inline pictures
hold at most 25.3 MiB at 1920x1200, Debian's eight, seven of
them copies its sheets size to its 420 px banner, and the Guardian's
108 21.3 MiB at both outputs, drawn at their size or twice it with no
copy; at 3840x2160, where copies are four times the pixels, Debian's
take 41.1 MiB and the United States article's 47.8 MiB, which the term
refuses five of its 64 pictures for room, Debian one of its eight,
each keeping its placeholder and counted in the status row. The term
stays 48 MiB: a larger one would come out of the sheets' term, which
the corpus needs 27.2 MiB of, or of the jail's margin under
`memory-high`, and a page past it loses pictures, not its styles. The
corpus's 271 pictures (173 WebP, 67 JPEG, 29 PNG, 4 GIF, to 1282x777)
each decoded within 48 ms; the 4000x3000 JPEGs above take 0.4 to 0.7
s. Within the 48 MiB term a baseline JPEG to about 7 MP is decoded,
a progressive one to about 5.5 MP, a page's first. A
page of 24 large PNGs, twelve 1600x1200 and twelve 800x600, shows 12
and refuses 12 for room, the term holding 44.0 MiB, the process
peaking at 77.6 MiB resident at 1920x1200 and 100.4 MiB at 3840x2160.

A picture reached by navigation is a page of its own, charged the shared
decoder's conservative 200 MiB peak before decoding, including the bounded
table/row overhead above its 192 MiB bulk ceiling, then shrunk to the
retained RGBA capacity. One whose working charge the root refuses is
handed back to its fetch thread with its body, which asks the window for
room: the window lets the page or picture shown go on the render
thread, the pane showing paper that holds the page's history entry,
and answers once that drop has left it for the lane (a let-go the full
lane holds is posted first; a render thread lost and not started again
takes none, and the room is then answered refused); the fetch thread
then has the picture made once more, behind that drop, and one refused
again shows §5's static notice, as one does at once when nothing was
shown to let go. Only the root's refusal of that charge asks for room,
not a failure to keep the decoded picture. The entry the paper holds
is the page's for history: a navigation that overtakes the picture's
puts it on Back, a reload loads it, and stopping the picture's shows
the page again by loading it. That reload gives the entry back to the
paper if it is stopped or overtaken before its page arrives, so a
second stop leaves the paper, saying so, its entry still held. The body
never leaves its fetch thread's slot, so `MAX_WORKERS` bodies still
bound it. Its fitted copy is made on the window thread at its
first paint at a size, the one heavy step that runs there, and charged
to the picture's budget there; a charge refused paints §5's static
notice in its place, tried again after a resize or a navigation. A
relayout's output is charged beside the one it replaces (td-layout's
`retained_output_bytes`, set from the old layout's `output_bytes`), or
the old one is let go first, as "Relayout" decides. Measured
(td-layout/DESIGN.md §6), a corpus page charges at most 37.8 MiB in all
and the costliest 2 MiB shape 115.9 MiB, so any two corpus pages fit
together, and a page is cut down (below, "Cuts") only when the page
shown leaves it too little or its own layout is past td-layout's
`MAX_PAGE_BYTES`, which 4 MiB of markup can be (up to 222.4 MiB
measured at the narrowest pane, others refused by name, not in order of
density). On the cell (above, "Relayout"), the seven corpus pages hold
0.3 to 14.6 MiB of td-alloc's owned storage at their peak and declare
0.6 to 35.9 MiB (`page_bytes`); 2 MiB of sixteen-letter paragraphs is
made in 641 ms, holds 78.9 MiB at its peak and declares 131.6 MiB, so a
relayout does not fit beside it and lets the old layout go first.

**The jail.** What td-alloc does not hold is a fixed term:

- the window, 145 MB at 3840x2160, the largest output td supports:
  §3's measured 144 MB (47 MB resident and td-ui's three 32 MB
  buffers) rounded up, as its test takes it;
- `ATLAS`, td-ui's pages for prose at sizes other than its own, 13.5
  MB: at most td-ui's `prose::PAGES` (8) atlas pages of 1 MiB with
  their key maps and shelves, and the coverage scratch they share
  (td-ui's bound, increment 6; 7.1 MiB measured drawing every glyph of
  the three families at the least and greatest sizes);
- the navigations' bodies, 67 MB (`MAX_WORKERS` of `MAX_DOCUMENT`, a
  save's among them, §5), a page's decoded text standing in its body's
  place (§3);
- the subresources' bodies, 16 MiB (`MAX_SUBRESOURCES` of
  `MAX_SUBRESOURCE`, below, "Subresources"), a sheet's at most 1 MiB
  and a picture's 4 MiB (`MAX_PICTURE_BODY`), each held by its fetch
  thread, and so by its slot, until the render thread has decoded it;
- `WORKING`, td-layout's `WORKING_BYTES` (64 MiB): a cascade's
  candidate, wrapping scratch and the allocator's overhead, for the one
  step the render thread runs at a time, measured for layout and, for
  parsing and compiling, the resident peak's growth over a step past
  what td-alloc holds after it (`measure_sheets`, the peak reset before
  each step): at most 0.3 MiB parsing a corpus page or 2 MiB spec, the
  heap reusing what reading it freed, and at most 8.8 MiB compiling,
  GitHub's sheets.

With the root's `PAGES` (not td-ui's `prose::PAGES`) that is 145 +
13.5 + 67.1 + 16.8 + 67.1 + 335.5 = 645.0 MB, 26 MB under
`memory-high` (671 MB), with `memory-max` 768 MiB, §3's keyfile. A
test,
`the_page_budgets_fit_the_jail`, holds that sum at least 20 MB under
`memory-high`, the term ceilings summing to `PAGES` and the root's
limit at `PAGES` and its control blocks, its terms taken from the
crates' constants (`MAX_TREE_BYTES`, `MAX_PAGE_BYTES`, `WORKING_BYTES`,
the caller's sheets ceiling, the 200 MiB picture working allowance checked
against td-ui's `MAX_PICTURE_PIXELS` and the decoder bounds), and the
window's term and the jail's copied by hand and reviewed when
`web.rs` or the limits change.

**What td-alloc does not bound**, and how it is kept: td-layout's
remaining std storage, whose external charge is a conservative admission
model; glibc's arenas and
fragmentation; td-ui's buffers, atlas pages, faces and measure caches;
and the fetch client's bodies. Every heavy step but a navigated
picture's fit runs on the one render thread, and a page's last drop
happens there, so one arena keeps their
peak (§3's reason); `WORKING` holds room for what the charges leave
out; and the fixed terms are measured. The resident peak with a page
shown and one made, both at their largest, is §3's: 193 MiB at
3840x2160 with one frame buffer, about 266 MB with td-ui's three. Each
page alone peaks at 28 to 68 MiB at 1920x1200 and 51 to 90 MiB at
3840x2160 for the corpus, and at most 141 and 164 MiB for 2 MiB of
blocks, which holds 71.9 MiB of td-alloc's owned storage and declares
115.9 MiB. `memory-high` throttles before
`memory-max` ends the jail.

### Sources

A page's sheets, in cascade order:

1. td-css's `HTML_UA_CSS`, user-agent origin.
2. td-web's own sheet, compiled in (`td-page/src/page.css`), user-agent
   origin after it: td's look and what `HTML_UA_CSS` lacks. The root's
   ink is `INK`, links are `ACCENT`, `code`, `kbd` and `samp` lie on
   `CHROME`; the root's family is `"td-body"` (below, "Faces") at 16
   px, td-ui's `SIZE`; `[hidden]` is not displayed and `hr` is a block
   with a rule; a `plaintext` element (a text page's, below, "Cuts") is
   a block in monospace, `white-space: pre-wrap`. `PAPER` is the
   canvas's default, not the root's background, so a body's background
   reaches the canvas as CSS 2.2 §14.2 has it (below, "Painting").
3. The page's own, in tree order: each `<style>` element whose `type`
   is empty or `text/css`, and each `<link>` naming a stylesheet (below,
   "Subresources"), its `media` attribute, if any, its media
   condition (td-css's `Sheet::with_media`, rather than wrapping its text in
   `@media`, which a stray `}` in it would escape), each sheet's imports
   before it; then each element's `style` attribute, inline, as
   td-layout joins it. td-css skips `@import` whole: td-web reads a
   sheet's imports itself and gives the sheets they name to the set.

td-css takes no presentational attributes (`bgcolor`, `align`, a
table's `width`, `<font>`), so none applies; an image's `width` and
`height` are td-layout's. td-css's bounds hold (td-css/DESIGN.md §5):
at most 64 sheets in a set and 4 MiB of their text in all, the two
user-agent sheets included, 1 MiB each, a sheet's media lists counted
with it; a sheet past them is left out,
in cascade order, and the status row says so.

**Colours.** The colours a page names are applied: a page looks as its
sheets say. An unstyled page draws td-web's defaults,
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
holds for that page until the next navigation, which starts with the
page's own styles. It is refused, the status row saying so, on a text
page, td-web's own pages, a picture and a page already on rung 2 or 3
(below, "Cuts"), none of which has styles of its own to leave out.

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
bound, `prose::MIN_SIZE` to `MAX_SIZE` (6 to 192, `Size::clamped`,
increment 6), the same size in td-layout's measuring and in the paint.
Small caps, `text-transform` and `text-indent` are not applied
(td-layout keeps the last two unapplied); `letter-spacing` is, as
td-layout lays it.

### Subresources

A page's stylesheets (increment 8) are its first subresources (§4):
each `<link>` whose `rel` holds `stylesheet` and not `alternate`
(ASCII case-insensitively), with an `href` not blank, no `disabled`
attribute and a `type` empty or of MIME essence `text/css` (a
`<style>`'s `type` empty or exactly `text/css`), each in the preferred
style sheet set: a `<style>` or such `<link>` with a `title` applies
only when it is the first such title in tree order, an untitled one
always, as no alternative set can be chosen; and each `@import` at the
head of a sheet, in the
order the cascade takes them, an import standing at its rule's place
with the media lists of its chain as its conditions, all of which must
match: the `<style>`'s or `<link>`'s `media`, then each import's list,
outermost first (a `<link media=print>` importing `x screen` applies in
neither). td-web reads a sheet's head itself (td-page's `gather.rs`), as CSS
Syntax tokenizes it, comments, strings and blocks kept whole: an
`@import` counts after `@charset` and `@layer` statements and before any
other rule, an `@namespace` or an unknown statement among them; a
string a newline breaks is the tokenizer's bad string, its statement
going on from the newline to its `;`, and an import holding one is
malformed and skipped; its URL is a string or `url()`, escapes
undone. An import naming `layer()` or `supports()` is left out and
counted not applied, as td-css skips layers; td-css's `@supports`
evaluator could later decide a `supports()` condition, which would then
apply it. Every sheet is fetched whatever its
media, so what is fetched says nothing of the viewport. A `<link>`'s
URL is joined against the page's `<base href>`, else the page's URL;
an import's against the URL its sheet answered from, after redirects.
Inline pictures (increment 9) are each `img` whose `src`, ASCII white
space trimmed, is not blank, in tree order, at most `MAX_PICTURES`
(128) a page, those past it keeping their placeholders.

**Requests.** Each is a GET sent `public`, following redirects as a
navigation does (§4). A subresource from another site is taken, as
pages' sheets and pictures so often are; an `http` one under an
`https` page is refused at whichever hop it appears, as browsers
refuse mixed content; no cookie (td-web keeps none) and no `referer` is
sent, so a host learns no more of the page than the subresource's own
URL. A `data:` URL is taken within the same byte bounds, decoded by
td-web on the render thread with no request: it takes no slot; its
decoding's scratch, at most its URL's length, is charged to the page's
budget, as the page's text it comes from is, and dropped once decoded,
and its decoded bytes are charged to the sheets or pictures term,
whose ceilings bound them, until the sheet is compiled or the picture
decoded. A `data:` sheet is parsed as WHATWG's `data:` URL processor
has it, its fragment from `#` left out, percent-decoded and, under
`;base64`, forgiving-base64-decoded; its type must be `text/css` and
its `charset` is its label; one that does not decode is a sheet not
applied; it counts toward `MAX_SHEET_FETCHES`, and since it has no URL
to join against, its imports are taken only when absolute. A fetched
sheet's URL is kept, as text in the sheets term, for joining its
imports and refusing cycles, at most `MAX_URL` (td-url's 2,048 bytes), one
longer not applied. A reply other than `2xx` is dropped. A sheet asks
`accept: text/css,*/*;q=0.1`, and one whose type's essence is not
`text/css` is not applied, with no quirks-mode exception, since td-web
has no quirks mode; one with no `content-type` is taken, as Fetch
takes a style sheet whose type is absent. Its text is decoded as CSS
Syntax §3.2 orders it, through td-charset: a byte order mark, the
`content-type`'s `charset`, an `@charset` rule exactly as CSS Syntax
has it (the first 1024 bytes beginning `@charset "`, a label holding
no `"` or `;`, then `";`; a UTF-16 one read as UTF-8), the
environment's encoding, the page's for a `<link>` and the
importing sheet's for an import, else UTF-8. Bounds: 1 MiB a sheet
decoded (td-css's per-input bound, asked of the service as the body's
limit; a sheet in another encoding whose decoded text passes it is left
out after decoding, the status row saying so); 4 MiB in all of the
gathered sheets' text and their imports' media lists, a sheet arriving
past it left out; at most `MAX_SHEET_FETCHES` (32) sheets linked and
imported, `data:` ones among them, counted as they are found, the
`<link>`s and `<style>` imports in tree order with the page, each
sheet's imports as it arrives, one past it left out; and imports four
deep (`MAX_IMPORT_DEPTH`), a fifth level left out, as is an import of a
URL in its own chain, which would import itself. The set then cuts
them with the `<style>` elements, in cascade order, at td-css's 64
sheets and 4 MiB with the user-agent sheets (above, "Sources"). Every
sheet not applied, a failed fetch, a wrong type, mixed content, a
`data:` URL that does not decode, a URL past `MAX_URL`, one left out,
is counted in the status row's `N of the page's style sheets were not
applied`. A page naming no sheet to gather, no `<link>` and no
`<style>` that imports, holds no gathered sheets at all. Measured on
the corpus of increment 8 (below), a
page links and imports at most 23 sheets, GitHub's, holding 3.6 MiB of
text, the nearest the corpus comes to either bound; the others 0 to
17 sheets and at most 0.24 MiB. A picture asks `accept:
image/png,image/jpeg,image/gif,image/webp,*/*;q=0.1`, AVIF decoded
but not named while td-avif covers only part of it (§5), its body at
most `MAX_PICTURE_BODY` (4 MiB, `MAX_SUBRESOURCE`), a `data:` one's
decoded bytes the same, and each hop the `deadline` of what is left of
its `PICTURE_WAIT` (30 s from its ask).

**Slots.** Subresources have a count of their own, `MAX_SUBRESOURCES`
(4) fetches out across the window, stopped ones included until the
service answers, as navigations have `MAX_WORKERS`; so a stopped page's
fetches never take a navigation's slot, and the next page's sheets wait
for one of these, within their `STYLE_WAIT`. Each is a thread of its
own, its slot held by the thread and given back when the service has
answered and the thread ends, whether or not anyone still waits for the
answer. A sheet's body stays on its fetch thread until it is handed to
the render thread with its slot, and one no longer waited for is
dropped there, as a stopped navigation's is.

A sheet's answer, whether it came or was refused, holds its slot until
its page takes it, so a page never has more answers waiting than its
mailbox has places, one a slot, however fast its sheets are refused (a
page on a LAN device, whose sheets the service refuses at once, as
`public`); a page that has gone on without them drops each as it comes
and those left as it goes, so their slots free at once.

The service's workers are counted the same way: `[web-fetch]` serves
`SERVICE_WORKERS` (8), `MAX_WORKERS` and `MAX_SUBRESOURCES` together
(§3), as does the service td-launch starts for td-web on a host, and
td-web never has more requests out than that, so a navigation is served
at once whatever a page's sheets do. A sheet's fetch asks
each hop with what is left of its page's `STYLE_WAIT` as the request's
`deadline` (APPLICATIONS.md, the fetch service), which the service
holds its exchange with the origin to; and before each hop it looks
whether its page has gone on without it, stopped, styled or past its
wait, and asks no further hop if so. So the worst a hostile page can do
is hold all four slots, and so four of the service's workers, until
its own `STYLE_WAIT` has passed, 3 s after its reply, plus whatever a
name lookup takes, which no deadline bounds (the service's recorded
gap), the resolver's own timeouts aside. A sheet's fetch waits that out
with its slot, as the client's reply wait lets it: given its slot back
sooner, with the service's worker still held by the lookup, td-web
could have more requests at the service than it has workers, and a
navigation could wait. The sheets of pages after it wait for a slot
that long at most,
and come too late for their own wait if it runs out first. Its
navigations are never held: measured with a model of the service's
eight workers and a page of six sheets whose origins never answer
(`a_page_of_tarpit_sheets_never_holds_a_navigation_back`), the next
navigation is served at once and its page shown within the test's
1 s, the slots free at the tarpit's wait; at the four workers the
service had before, the same navigation waited 1.5 s, the whole of that
test's wait. A sheet slower than `STYLE_WAIT` is never applied, since
its fetch is given up there; a reload asks again. The cost is the
service's: four more of its threads at most, outside td-web's jail
(§3), each counted request's memory bounded as the service bounds it
and a sheet's decoded body by td-web's 1 MiB limit.

Pictures take at most `PICTURE_SLOTS` (2) of the four slots at once,
counting sheets' among them: a picture is asked only while fewer than
two slots are out, so a page's pictures leave the next page's sheets
two however long they take, and its navigation, served by its own
workers, none to wait for. While no slot is free nothing is joined or
copied for the next picture.
A picture's fetch holds its slot until the render thread has decoded
its body, or it was refused, and it ends: its thread hands the body to
the render thread in one of the handoff's slots (§3), which has one for
each fetch thread, navigations' and pictures', so a handoff never waits
behind another's. Its worst case: a page whose pictures' origin never
answers holds two slots, and two of the service's workers, for
`PICTURE_WAIT` (30 s) after each ask, plus a name lookup, and asks
no other once it is left: shown, a page of `MAX_PICTURES` such
pictures holds them two at a time for up to 32 minutes. The next
page's sheets are fetched two at a time meanwhile
(`a_page_of_tarpit_pictures_holds_back_no_navigation_or_sheet`, its
pictures waiting 20 s, the next page styled within 10 s and no picture
past the two asked). `PICTURE_WAIT` gives a 4 MiB picture a little more than
1 Mbit/s. Measured by replaying the corpus's captured answers at their
captured times (below), the Guardian's 108 pictures are all laid 9.1 s
after its page is shown at two slots, 6.3 s at three, 4.9 s at four and
17.7 s at one, Wikipedia's World War II and United States articles' 62
and 64 in 4.4 s at two and 3.1 s at three; two keep half the slots for
sheets, the bound the paragraphs above give them, at the cost of a
picture-heavy page's last pictures, which come last in tree order.

**Waiting.** A page waits for its sheets, as browsers hold rendering
for them, rather than being shown and restyled. The render thread
decodes and parses the page, finds its sheets and decodes its `data:`
ones (`gather::Sheets`); when none is to be fetched it styles the page
in the same job. Otherwise the page, its tree and sheets go back to the
navigation's fetch thread, which asks for each sheet wanted, in order,
as slots free, on threads of their own, none once its wait has passed;
the answers come since the last job are handed to the render thread in
one job, which decodes them into the page's sheets and finds their
imports, asked for in turn. The fetch thread holds the page between
jobs and does not drop it: stopped, it hands the page to the render
thread to drop, as every let-go goes (above, "Handover"), held when the
lane is full and dropped on the fetch thread only past `PENDING` of
those. Meanwhile the page shown stays and the status row says `loading
styles of` the URL. At `STYLE_WAIT` (3 s) after the reply came to the
fetch thread, or once every sheet has settled, the render thread styles
the page with the sheets that came, in their order; one later, or never
asked for want of a slot, is not applied, the status row says `N style
sheets did not come in time; Ctrl+R asks again`, and a reload, a
navigation like any, asks for them anew: a late sheet is dropped rather
than restyling a page being read. Its fetch holds its slot until the
service gives it up at that deadline (above, "Slots"). `Escape` stops
the navigation, as it stops any (§8): the fetch thread sees it within
`TICK` (20 ms), tells the page's sheet fetches, and gives the page back
to be dropped, the page shown staying; each fetch holds its slot until
the hop it has out is answered and asks no other. A page is cascaded
once, never restyled by a sheet.
Its gathered sheets are kept with it, so a relayout that cascades again
(a resize, `F9`) compiles them again; rungs 2 and 3 (below, "Cuts")
drop them. Measured by replaying the corpus's captured answers, each
taking the time it took from the development host (2026-11-17, a fresh
connection each), every page's sheets were gathered and the page styled
within 1.3 s of its reply (Debian's, a chain of imports, and the WHATWG
specification's, whose styling alone takes 0.8 s), so none came late at
3 s; `STYLE_WAIT` stays 3 s, a slower network's margin.

**Pictures** are fetched once their page is shown, a markup page on
rung 1 or 2 (rung 3's text lays none), and asked for only while no
navigation is out, so the next page's sheets come first; those already
out are taken. Each `src` is joined against the page's `<base href>`,
else its URL, as a link is; one whose path ends `.svg` or `.svgz`
(ASCII case-insensitively, before its query) is not fetched, td-web
drawing no SVG, nor one of a scheme other than `http`, `https` and
`data`. A `data:` one is known as such before it is joined, whatever
its length, td-url's `MAX_URL` notwithstanding: its bound is td-html's
1 MiB attribute (`MAX_ATTRIBUTE_BYTES`), base64 to about 768 KiB
decoded, a longer `src` dropping the page to its text (rung 3), so
`MAX_PICTURE_BODY` does not bind it. td-web keeps no cache: each `img`
is its own fetch, the same `src` twice fetched twice (in the corpus at
most five of a page's 81), and one its layout lays no box for,
`display: none`, is fetched all the same, the layout not consulted.
Each picture is fetched on a thread of its own, which hands a `2xx`
reply's body to the render thread; a `data:` picture is decoded by the
render thread from the page's tree, with no request and no slot, one on
its lane at a time. Each picture asked owes its page an answer: one
ended any other way, a panic or a lost render thread among them, is
answered failed as its fetch or decoding drops, so none stays out. There
it is sniffed as MIME Sniffing sniffs an image, by its signature
whatever its type, except that one whose type's essence is XML's
(`text/xml`, `application/xml` or `…+xml`, SVG's among them) is
refused; decoded one at a time through the shared picture dispatch
(§5), its decoding's peak charged to the pictures term first ("Memory")
and its body dropped there once decoded; and kept as its own fit,
premultiplied in the raster's order, in place (td-ui's
`Picture::into_fitted`). It is put in its page's inbox, a place a
picture, so no answer waits for another to be taken or is lost however
fast they come, and the window takes the inbox each time it polls. A
picture whose fetch fails or is refused (an answer other than `2xx`,
mixed content, past its bound or its wait), whose body is not a picture
td-web decodes or fails to decode, or which the term has not the room
for, keeps its placeholder, its frame and its `alt` text. A JPEG
decoded at a reduced scale keeps its frame's size as its intrinsic
size.

A picture taken lays the page again, not at once: those come since the
last settle are laid together by one relayout, a settle waiting until
`PICTURE_SETTLE` (250 ms) has passed since the page's last relayout
came, and while one runs, the first at once. A settle's relayout is a
layout alone ("Relayout" above), each picture's size its intrinsic
size for td-layout, a pixel a CSS px, and it keeps the reader's place
as any relayout does; a relayout for another cause lays the pictures
settled by then and no others, so what a layout shows is what its
settle counted. Each layout fits each picture it lays to its box in
device pixels, each edge rounded as a paint places it: a box that is a
whole multiple of the picture's size, as one laid at its own size is
at any scale, draws the picture itself, each pixel a square that
multiple on a side (td-ui's `Raster::image_scaled`), with no copy; any
other takes a copy fitted to it exactly (`Fitted::fit_exact`), made on
the render thread with the layout, charged to the pictures term and
shared by a later layout that lays it at the same size; a copy refused
keeps the box's placeholder. Inline, block-level and floated pictures,
in a link or not, are laid and drawn alike. A copy is made an axis at
a time, in the cheaper order, so its cost is about its source's pixels
plus its own (`Fitted::fit_exact_cost`): a 1x8192 picture laid
8192x1 takes 16 Ki steps, not 64 Mi. A layout's copies take at most
`FIT_WORK` (32 Mi) steps, 0.13 to 0.25 s at the host's measured 131 to
246 M steps a second, a cancel read at each row; a copy past it keeps
its placeholder, is counted not shown and noted, so a layout's fitting
is bounded whatever its pictures and boxes. A relayout that keeps the
old layout shown beside it while it is made holds that layout's
copies too; one whose copies were refused beside them is laid once
more, once a want, with the layout it made kept shown beside and its
copies reused: the old one was let go to the render thread's lane
ahead of it, so the room it held is had. Painting
charges a picture its pixels within the clip as a fill is, `BLEND` (5)
times them unless every pixel is opaque: td-ui copies an opaque one
and fills an opaque scaled pixel's run at about its fill rate (1.1 to
2.3 Gpx/s on the host, sixteen overlapping 1x1 pictures scaled to
2160x2160 in 32 ms), and blends a translucent
one at about a fifth of it (0.4 Gpx/s). An opaque picture scaled by two
or three draws at 1.1 to 1.6 Gpx/s and is charged its pixels alone, so
a paint at its bound of them takes up to about 50 ms (22 pictures of
720x720 pixels drawn at 1440x1440, 49.7 ms), the slowest paint the
bound admits. Measured by replaying the corpus, a
settle's relayout takes 3 ms on average for the Guardian's front page
and 17 ms for the World War II article, the slowest, so the settles
take a few per cent of the render thread while pictures come, and 250
ms keeps a page's pictures appearing in a few batches a second.

While a navigation is out, pictures that come are taken but neither
asked for nor laid: its page is made first, and a settle waits for it
to end. A navigation started cancels a relayout of the page shown
that is still running beside the layout shown, so its page's job never
waits behind it on the render thread, and the page shown is laid again
once no navigation is out, if the navigation ends without a page; it
is laid for no other cause meanwhile. One that let the layout go first
runs on, so the page is never left unlaid while the navigation is out.
A relayout asked during a navigation when none was running at its
start runs ahead of the navigation's page on the render thread, as
before. A page of `MAX_PICTURES` 1x8192 pictures laid 8192x1
is laid in a few ms, and a navigation from it is shown in 26 ms
(`a_navigation_cancels_the_relayout_of_the_page_shown`, release
build).

`Escape` stops a navigation out first, then the page's pictures still
to come (§8): those not come are let go, their fetches asking no
further hop and what they answer dropped where it is put; those come
stay. The status row says `pictures stopped`, then `N of the page's M
pictures are not shown; Ctrl+R asks again`, and a reload, a navigation
like any, asks for them anew. While pictures come it says `loading
pictures, K of M shown`, and once all are settled `N of the page's M
pictures are not shown` for those past `MAX_PICTURES`, refused or
failed, or whose copy the layout shown refused (`the page's picture is
not shown` when it names one); one with no box counts as shown. A page
let go, overtaken, reloaded or stepped down to rung 3 lets its pictures
go on the render thread, their fetches told first, as its layouts go
(above, "Handover"). An unlinked picture opens its `src` on a click as
its placeholder did (below, "Links"). `srcset`, `<picture>`'s sources,
CSS background images and SVG are not fetched.

### Places, find, selection and links

**Places.** A place in a page is a DOM position: a text node and a
byte offset in its decoded text on a scalar boundary, or before or
after a replaced element. Places are ordered by the tree-order table,
four bytes a node (1 MiB at td-html's `MAX_NODES`). td-layout lays
normal flow and tables, in tree order, so its lines and the fragments
in each are in tree order: relative offsets move what is drawn, not the
order. A **paragraph**, the unit find matches within and `Shift+Up` and
`Shift+Down` move by, is a run of consecutive
lines one block container lays: text either side of a child block is
two paragraphs, and a preserved newline or a `<br>` ends one. td-web
reads both from td-layout, each line naming the block that laid it
and how it ended (`Line::block`, `Line::end`, td-layout/DESIGN.md §4,
"Line ends"), not from gaps between lines.

**Find** searches each paragraph's text as td-layout displays it,
spaces collapsed, a tab taken as a space, as in the query, and a
replaced scalar as U+FFFD, with the white
space a `Wrap` end leaves between lines put back as Copying puts it
(none at an edge, nor across a `Split`) and an image's `alt` text in
its place, in tree order and
not past a paragraph, ignoring case, each character looked at once
(Knuth, Morris and Pratt), so a search costs the text searched whatever
the query, at most `MAX_HITS` matches, which do not overlap. Text with
no fragment (`display: none`) or not painted (`visibility: hidden`) is
not found, nor is a list marker, which is no fragment's text. Matches
are places, so they outlive a relayout; showing one
scrolls its line into the pane, and each is marked by the pixels its
glyphs span, the advances replayed through the layout's metrics, since
td-layout keeps no glyphs.

**Selection** is two places, kept unchanged by scrolling, a relayout,
`F8` and a resize. A point of the pane falls on a line by its height
(an index of the layout's lines by height, which cannot be a binary
search of their order inside a table, where line order is not y order,
td-layout/DESIGN.md §5, "Tables": each run of 64 lines keeps the span
its boxes cover, which bounds how near any of them is, so the nearest
run's lines are scanned by y and then x, and then only those of runs
no further), on a fragment by its x,
and between scalars by the halves of their advances, replayed through
td-layout's `inline::replay` with the layout's metrics and the style of
the block that laid the line (`Line::block`, td-layout §3), which sets
the tab stops: it gives a tab
the advance to its stop and its own byte, and refuses a fragment whose
advances do not sum to its width, an untabbed one only after reporting
its scalars, so the window thread gathers a fragment's scalars before
it acts on them; left of
a line is its start and right of it its end, above the page the first
line's start and below it the last's end.
§8's keys move by scalars, words and paragraphs taken in tree order
across fragments; a double click selects a word and a triple click a
paragraph. A scalar laid with no advance, a combining mark, is never
parted from the scalar before it: no place falls between them, a point
past the base's middle falls after its marks, the keys step over the
two as one and a word holds its letters' marks. Text not painted
(`visibility: hidden`) is not selected: the keys and a word step over
it, and a copy leaves it out. A line laying nothing (a text page's last
newline, a paragraph's last `<br>`) holds no place: a place at the end
of the line before it is on that line.

**Copying** takes the text between the two places in tree order as it
is shown: fragments' painted text, neither `visibility: hidden` text
nor a hidden item's marker; after a `Wrap` end the white
space laid between its line and the next line with a fragment, read
from the source between them, or across two text nodes from the
first's tail when that is white space, else from the next's head, as
laid: one space where spaces collapse, the preserved run in pre-wrap,
none at an edge, and past a cut only what td-layout dropped as space,
never the tree's bytes between, which can hold hidden text or runs
that collapse. A line of spaces alone has no fragment and no place, so
that gap is copied once, however many `Wrap` ends lie in it. After a
`Split` nothing is copied; after each paragraph one newline, whether a
newline, a `<br>` or its block ended it; and for a replaced element its
`alt` text when the selection holds it whole; at
most `MAX_COPY`, measured before it is gathered. A list item's
marker, td-layout's `MarkerFragment` (generated text naming its item and
no byte, td-layout/DESIGN.md §5, "List markers"), is copied as its
`text()` shows it at the start of the paragraph its line begins, when
the selection holds that start, an outer item's before an inner one's
where they share a line; one whose item laid no line, an empty item, is
copied as a paragraph of its own, its trailing space dropped, when the
selection spans its item in tree order (it has no place of its own),
before the line after it; no place lies before the empty items ahead of
the page's first place, or after those past its last, so only a
selection of the whole page, from its first place to its last, holds
them. A marker is no
place: a point on an outside marker, left of its line, falls on the
line's start, and one on an inside marker, before the line's first
fragment, on the same start, and the keys step over it. A table is
copied as rows of cells: within a row, a cell's paragraphs are joined by
a space, whatever ended them, `<br>` included, and cells by a tab, and
each row ends in a newline, so a copied table pastes into a spreadsheet
a row a line. An empty cell lays no line, so td-web takes a row's cells
from the box tree (its `table_role` children) rather than from lines,
and an empty one between two laid cells still gives its tab; empty
cells before a row's first laid cell give none. Cells come in tree
order, as td-layout lays them (row by row, each row's cells left to right,
td-layout/DESIGN.md §5, "Tables"), and td-web tells a cell's and a row's
boundaries from each line's `Line::block` and its box parents, by
`StyledDocument::table_role`. A row or cell CSS left anonymous has no
box (td-layout/DESIGN.md §5, "Anonymous objects"): an anonymous cell's
lines name the box holding its run, so a CSS table's anonymous ones give
no cell boundary and their content is copied as the paragraphs it lays,
and cells in a row CSS left anonymous are one row by the box holding
them, so the cells of two CSS tables are two rows;
a selection that starts or ends inside a table and holds more than one
cell copies the cells it holds, partly held ones from or to its place,
with the same separators, and one within a single cell copies it as
ordinary paragraphs, joined by newlines.

**Links** are `a` and `area` elements with an `href` (td-dom's rule),
joined against the page's base: the first `<base href>` in tree order,
joined against the page's URL, else that URL. A fragment's or replaced
box's link is its nearest such ancestor, and a point's link that of
the fragment or replaced box under it; an image with no link opens its
`src` on a click, and the status row names it while the pointer is
over it. `Tab` goes through the links
in tree order that laid a fragment or a replaced box, from the
focused link, else from the screen: `Tab` the first link on a line
shown or after it and `Shift+Tab` the last on a line shown or before
it, each going round the page; a link's place
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
it turns; and, when the page is wider than the pane, `Left` and `Right`
four cells across it, as the wheel does for each column td-ui reports
it turning. Unwrapped lines are laid whole, within td-layout's
bounds, and scrolled to. A
line that does not wrap (`pre`, `nowrap`) past td-layout's
`MAX_LINE_SCALARS` is cut there, the rest of it to its end not laid,
td-layout counting the cut lines and placing the first (`Layout::cuts`,
td-layout/DESIGN.md §4, "Long lines"), and the status row says so;
`pre-wrap` breaks there instead, so a text page is never cut.

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
at this width. Its notes are the cuts below, the sheets not applied,
those that did not come in time, the page shown without its styles
(`F9`) and
a paint cut short.

### Cuts

td-html and td-layout return no partial result, so a page is laid
whole or not at all: no page is shown as a prefix of itself. A
refusal, by a crate's limit or by the page's budget, steps
down, each rung on the render thread:

1. The page with its own sheets. A sheet past a per-sheet bound of
   td-css's is left out in the one compilation, as if never given
   (`from_sheets` lists it in `Refused`), and the status row says how
   many were.
2. On a set-wide refusal by td-css (its compile ceiling and its
   set-wide totals, such as `MAX_COMPILED_RULES`, even when one sheet
   alone exceeds them; td-web validates the environment it passes, so
   `InvalidContext` never arrives here), td-layout's cascade work
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

Markup past `MAX_MARKUP` is cut before it is parsed, and the status
row says how much was shown. A relayout refused once a page is
shown, at a new width, scale or face, keeps the old layout, painted at
its own width, and the status row says the page could not be laid out
again; one whose old layout was let go first has none to keep, and the
page steps down from its rung: from its own sheets to rung 2, from
rung 2 to rung 3, and from a text page, rung 3's among them, or
td-web's own to rung 4's notice.

A **text page** takes the same pipeline: its text, its controls but
the tab dropped, follows `<!doctype html><plaintext>`, which td-html's
tokenizer ends markup at, so the page is one text node laid `pre-wrap`
in monospace, td-layout's tab stops every eight columns in place of
td-web's expansion. Its bound stays `MAX_TEXT` (4 MiB), the prefix
included, so it fits td-html's 4 MiB input, and td-layout takes it
under a raised input limit (`Limits::input` at td-layout's `MAX_INPUT`,
4 MiB). td-web cuts it by the lines it lays at its least width, not
by its source lines: where it would lay past `MAX_TEXT_LINES`
(262,144) lines in the 128 px viewport `MIN_WRAP_EMS` sets, counted by
td-layout's own sizing and breaking with the faces it is measured with
(`text::fit`, td-layout/DESIGN.md §6, "Text pages"). td-web parses and
styles the text whole within `MAX_TEXT`, counts it, and if the count
passes, builds the page again from its text up to the cut, which falls
within the source line that passes, where its first laid line past the
count would start, or, when that line's first laid line passes, at the
end of the line before. The status row's note says how many of its
source lines are shown, and that the last is shown in part when it is.
The cut is an offset in the page's text node, and
td-web's text, its newlines made LF and its controls but the tab
dropped as td-html would have them, is that node's byte for byte; it
meets `text::fit`'s precondition, no control scalar a line could not
lay as one fragment. The count is of the lines laid and of each indent
a line drops before a word that moves: a wider pane keeps such an
indent beside its word and can need one more line for it, and with
those counted no wider viewport lays more (td-layout §6), so the page
cut at the least width fits at every width. A change of face or scale,
which can change advances, counts the text shown again and cuts it
further if it now passes (above, "Relayout"); a new width needs no
count. Why 262,144: a layout's lines are charged in capacity steps
(td-layout/DESIGN.md §6, "Text pages"), 68 MiB up to 262,144 laid
lines, 136 MiB to 524,288, and refused past that, and a text page's
line holds one fragment at most, its tabs coalescing with its text. So
two layouts of a cut page, the old beside the new, charge at most 136
MiB at every width, beside a tree and styled document of at most 17
MiB measured, within the page term, and a relayout never has to let the
old one go first; at 262,145 lines one layout would charge 136 MiB and
two would not fit. Measured in the least viewport with 0.5 and 0.6 em
monospace faces (td-layout §6), the count takes 44 to 83 ms for a full
4 MiB page at 11 or 14 columns, 33 for 200,000
`aa\tbb\tcc\tdd\tee` lines, 28 for 200,000 `a\tb\t…\tj` lines, 58
to 73 for a 4 MiB page of 46-byte lines and 57 to 72 for a licence
banner and a 4 MiB minified line, half to two thirds of laying what it
keeps. Rung 3's text takes the same bounds, its controls but the tab
dropped as a text page's are. td-web's own pages, an error and the
note for a type it does not show, are markup it writes with their text
escaped, laid on the thread that makes them, a fetch thread or the
window's, being a few lines; want of memory for one paints the static
notice.

### Painting

The window thread paints from the page and its layout. td-ui gives the
raster: opaque fills, prose glyphs in Sans, Serif and Mono at a style
and a size, fitted pictures (`Image`), a clip on every draw, and the
theme's map. td-web decides what is drawn: every fill first, in this
order, then every glyph.

1. The canvas: the root element's background colour, else that of the
   HTML body element, the root's first `body` or `frameset` child (CSS
   2.2 §14.2), else `PAPER`, over the whole pane.
2. Each box in tree order: its background colour over its border box,
   then each border side as a fill of its width, every style but `none`
   and `hidden` drawn solid.
3. Each line's content: the backgrounds of a fragment's inline
   ancestors behind its extent, as one fill, since all lie over that
   extent: the innermost opaque one with the translucent ones inside it
   blended over it, however deep; the grounds of what is marked, by the
   glyphs' extents, find's matches on `INACTIVE_SELECTION` and the
   selection and the link under the pointer, else the focused link, on
   `SELECTED`; the underlines, overlines and line-throughs it takes
   from itself and its ancestors, as CSS propagates them, as fills
   across the fragment, a pixel thick at scale one; and a replaced
   element's picture once its layout has fitted it, an `Image` held
   back in the fills' order and charged its pixels as a fill is (it is
   not opaque, so it covers no fill, and a later fill covering it
   leaves it undrawn), framed in `SELECTED` when its link is marked;
   else a frame in `BORDER` (`SELECTED` when its link is marked)
   holding its `alt` text, clipped to it.
4. The glyphs: each fragment's in its colour, face and size, marked
   ones in the theme's pairs, `PAPER` on `SELECTED` and `INK` on
   `INACTIVE_SELECTION`, which no page colour moves, so a mark is as
   legible on a dark page as on paper; at most `MAX_STACKED` at one pen
   in a line, across the fragments td-layout splits a run into;
   an `alt` text's, its first `MAX_ALT_SCALARS` scalars; and the list
   markers', their text in their item's colour, face and size at their
   baseline, those without a line with their item's box. Each scalar
   is drawn, white space (a preserved tab and a no-break space among
   it) as a space, which every face draws blank, so td-ui's seam reads
   a page's words apart (§8, "Driving"). They are drawn
   ordered by family, then size, then the order they came in, so a
   later box's background never covers an earlier box's glyphs.

`visibility: hidden` paints nothing of its box, and a hidden item's
marker, drawn in its item's style, is not painted. `opacity` is not
applied. td-ui's fills are opaque, so a colour with alpha is blended by
td-web before it is drawn, over the colour of the nearest ancestor
with an opaque background, else the canvas's, which the paint knows in
tree order: a translucent tint shows as a tint, not as solid colour; a
glyph's ink is blended so over the ground it is drawn on, and a colour
whose alpha is zero draws nothing. Every draw is clipped to the pane
and the damage; a box whose `overflow` is `hidden` or `clip` clips its
descendants to its padding box, and `auto` and `scroll` draw as
`visible`: there is no scroll container inside a page. A paint visits
only the boxes, lines and markers meeting the damage, through an index
by height made with each layout and charged with it: each run of 64
entries keeps the span its ink covers, and of a run meeting the damage
each entry is looked at again, a line by its box widened by the
greatest reach of its run (below), and one that does not meet it is
passed by. td-web derives that index and the boxes' tree order from
td-layout's postorder geometry. A
line's glyphs are drawn up to one em of its largest size past its clip
(the pane, or the box that clips it), td-ui's `prose::overhang` at that
size (increment 6). A line of several sizes puts each piece's glyphs
on its baseline with `Metrics::top_at`; td-ui's `draw` alone
top-aligns them. Ink so passes a line's box vertically: each glyph is
drawn down td-ui's line box at its size from that top, which a tight
or overlapping `line-height` makes taller than td-layout's, and its
ink passes that by `Metrics::reach_at` at the size, so the index by
height takes for each line, and each list marker, the span of its box
and its glyphs' ink so placed, a device pixel more each way for the
pen's rounding, and culls by it. A marker is drawn on its own
baseline, td-layout's absolute `MarkerFragment::baseline`.

A paint is bounded three times on the window thread. It takes at most
`MAX_PAINT` (262,144) fills, glyphs and visits, each box, line and
marker meeting the damage counting one, so that ones laid on one
another end it though they draw nothing; it charges at most `PANES`
(8) times the pane's area in pixels: a fill what it covers within its
clip, a glyph its em square within its clip, a space nothing, as it
inks nothing; and it takes at most
`MAX_PAINT_WORK` (1,048,576) steps of other work: each entry of the
index passed by, each fragment and replaced box looked at, each scalar
replayed or measured, a tabbed fragment's bytes, which td-layout scans
whole before replaying it, and each ancestor stepped through for a
ground. Past any bound it stops, walks no further box, line or
marker, and the status row says the page was painted in part.
`MAX_PAINT` is four times the glyphs a 3840x2160 pane of 8 px cells
shows, about 62,600, so a pane of text, its lines' visits and its
fills are painted whole. `MAX_PAINT_WORK` is four steps a draw:
td-layout lays a run of more than `MAX_FRAGMENT_SCALARS` (1,024)
scalars as several fragments (td-layout/DESIGN.md §3), so a line
passes by their x those wholly before its clip and replays at most
1,024 scalars to find it, however far it is scrolled; a pane of text
meets `MAX_PAINT` first, and what meets this bound is lines laid on one
another, each replaying to its clip, at about 20 ns a step, 20 ms. The
grounds a paint blends over, the nearest
opaque background under a box, a line's block or a marker's item, and
the inline backgrounds behind a fragment, are kept down the path from
the root to the last asked for, so a paint, asking in tree order,
steps through each ancestor once rather than once an item, and a box
walks to its ground only for a translucent colour.

Eight panes is room for what a page shows: a pane of text charges
about two (ems overlap at their advances), the canvas and each nested
background one, find's and the selection's grounds one; the corpus
charges 1.0 to 3.2. It holds a paint to td-ui's fill rate over eight
panes, tens of milliseconds even at 3840x2160, where without it 2,000
boxes laid over one another filled 2,000 panes, 7.4 s a paint. A fill
a later fill wholly covers is not drawn: the last 16 fills are held
back, the oldest drawn when a seventeenth comes, and each new fill
drops, and gives back the charge of, those it covers, so boxes stacked
at one place, borders included, cost one pane. A fill refused by the
bound drops nothing.

td-ui remakes a page of another size whenever more than `prose::PAGES`
family and size pairs interleave, at up to 47 µs a glyph at the largest
sizes, which ordering the glyphs avoids. Their order's scratch,
`MAX_UNORDERED` (65,536) entries of 72 bytes, 4.5 MiB, is taken from
the root at the first paint, grown by doubling as a paint fills it, to
`MAX_PAINT` entries, 18 MiB, and kept for every later paint; where it
is refused or not grown, the glyphs past it are drawn unordered, as
they come, the fills held back drawn before each, and a paint draws at
most `MAX_UNORDERED` so and is cut past them. Pixels do not bound that:
a glyph whose em square misses its clip, its pen within an em of it,
charges none, and td-ui makes a glyph's page before it clips the
glyph, so each of those could remake a page, about 65,500 × 47 µs, 3.1
s, at worst.

On the cell (above, "Relayout") a paint of a whole 1920x1200 pane of a
corpus page takes 66 to 157 µs and at most 1,651 draws, measured
before each space was drawn as a glyph, which adds a draw a space and
no pixel charge. Over the pinned
faces, the whole window (its rows included) is drawn into its buffer,
the pinned faces rasterised by td-ui's `Raster`, at 1920x1200 in 2.1 to
5.7 ms once its glyphs are in the atlas and 2.8 to 9.2 ms at the first
paint, for every corpus page and shape but many families and sizes on
one page, 27.4 ms (38.1 ms first), whose sizes overrun td-ui's atlas
pages so that glyphs are covered again every frame; at 3840x2160 8.0
to 16.4 ms (8.6 to 25.7 ms first) and many families and sizes 35.4 ms
(56.8 ms first). The hostile shapes the bounds meet take at most, first
paint, at 1920x1200 and at 3840x2160 (scale 2), medians of three on a
host under a load average of 15 (2026-11-16): 2,000 boxes laid over
one another, each a pane, 4.3 and 30.0 ms (150,000 of them, cut at
`MAX_PAINT`, 11.2 and 35.4 ms); boxes each a row lower than the last,
or their top borders, cut by pixels, 12.0 and 58.6 ms; 500,000 lines
of one glyph at `line-height: 0`, cut at `MAX_PAINT` by their visits
and glyphs, 39.4 and 87.0 ms (35.8 and 69.6 ms once in the atlas);
255,000 empty boxes in 100 nested boxes, 12.2 and 36.8 ms, or 150,000
empty list items there, cut by their visits, 22.0 and 52.4 ms, where
walking each one's ancestors for its ground took 93 and 113 ms; glyphs
of 200 px on lines 4 px apart, cut by pixels at 1920x1200, 13.4 and
37.2 ms; 20,000 fragments in 200 nested spans, 15.8 and 44.3 ms, where
walking each one's spans took 76 and 102 ms; and 2,000 lines of 1,024 scalars laid
on one another, scrolled to their ends, cut at `MAX_PAINT_WORK`, 30.8
and 50.0 ms. A fragment is replayed only to its clip's right edge
(td-layout's `replay_until`) and from the first fragment meeting its
clip, so 30 lines of 60,000 scalars at 6 px take 6.2 and 28.9 ms
unscrolled and 6.4 and 28.0 ms scrolled to their right ends, tabbed
7.0 and 30.8 ms and 7.9 and 29.9 ms, none cut, where with each line one
fragment the scrolled and tabbed ones were cut at the bound; 60 lines
of 20,000 scalars scrolled to their ends 7.6 and 35.6 ms; and a
`pre` of 464-column lines at 3840x2160 and scale one, 95 lines in the
pane, 41.6 ms, and scrolled 64 lines down 45.2 ms, whole, where
before the lines of a run above the pane spent `MAX_PAINT` first. The
worst paint is so about 90 ms at 3840x2160 and 40 ms at 1920x1200.
Nothing animates: a paint is a function of the page, its layout, the
view and the damage, and no timer asks for one.

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
ceiling (`MAX_CASCADE_WORK`) sums both, with the declarations, custom
names, inherited custom entries copied and attribute scans each element
costs, as td-css's `ComputeStats` reports them; the costliest page it
accepts cascades in about 3.5 s on the development host, and its
costliest element, which bounds how late a cancel flag is seen, takes
17 to 21 ms (td-layout/DESIGN.md §3 and §6). With its layout work ceiling
(`MAX_LAYOUT_WORK`) it is sized so a page costs the render thread a few
seconds at most; a page past it steps down (above, "Cuts") rather than
being endured, and the cancel flag ends superseded or stopped work
between elements. On the cell (above, "Relayout"), 5,000 universal
`:not()` rules over 5,000 paragraphs are refused by `MAX_CASCADE_WORK`
and the page laid on rung 2, 1.58 s in all (1.9 to 2.5 s through the
window over the faces). A `var()` of 30,000 tokens on every element of
5,000 paragraphs is refused by `MAX_CASCADE_WORK` too, rung 2 in 1.2 to
1.3 s; 1.5 MiB of custom properties in one sheet is left out by
td-css's `MAX_INPUT_BYTES` (1 MiB a sheet) and the page made in 69 ms,
and 0.9 MiB of them, inherited by every element, applied in 61 to 64
ms. td-html's hostile shapes step
down to text: one-letter paragraphs in 62 quotes by `MAX_NODES` in 212
to 235 ms, list items in 62 by `MAX_WORK` in 269 to 282 ms.

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

6. **This design**, documentation only. Landed, with each
   prerequisite below a landing of its own in its crate, with that
   crate's design and validation, before the cutover:
   - **td-ui** (available): prose at a size (td-ui/DESIGN.md,
     "Sizes"): a glyph's `prose::Size` (`GlyphStyle::with_size`,
     `Size::clamped`), 6 to 192 device pixels per em
     (`prose::MIN_SIZE`, `MAX_SIZE`); `Metrics::advance_at`, `width_at`
     and `line_at`; `prose::overhang`, an em at a size; `reach_at`, ink
     past a line box above and below, and `top_at`, a glyph's top for a
     baseline; and at most `prose::PAGES` (8) pages of other sizes, a
     new size covering into the storage of the one drawn from longest
     ago, within `prose::ATLAS` (13.5 MB, pages, key maps and shared
     scratch; 7.1 MiB measured drawing every glyph at the least and
     greatest sizes). A frame interleaving more than eight family and
     size pairs glyph by glyph remakes a page per glyph (4 µs each at
     20 pixels per em, up to 47 µs at 192), so a paint orders its
     glyphs by family and size.
   - **td-layout** (available): a declared-charge split excluding
     independently owned tree, DOM and CSS storage (above, "Memory";
     `StyledDocument::charges`, td-layout/DESIGN.md §2, "Whole-page
     memory"), each charge named by the owner holding it, so the tree
     term takes the document's once and the styled term none of it or
     of construction's index and scratch, though the pool's charge
     conservatively includes strings a computed style shares with its
     sheets; a styled document holding its
     document by shared ownership (`StyledDocument::shared`), so a page
     passes between threads, as td-web, which forbids `unsafe`, cannot
     hold a borrowed one beside its owner; a text page's lines counted
     at a width through layout's own sizing and breaking, without laying
     it out, and the page cut where they would pass a cap
     (`text::fit`, td-layout/DESIGN.md §6, "Text pages"); a basic table
     layout, so a page's tables line up rather than stacking as
     blocks: the automatic and fixed
     width algorithms, rows and cells with `colspan` and `rowspan`
     within HTML's bounds, refused past `MAX_TABLE_COLUMNS`, border
     spacing with collapsed borders approximated, cells' vertical
     alignment and captions, a box's part read by
     `StyledDocument::table_role`; lines stay in tree order, row by row
     and cell by cell left to right, which is not y order inside a
     table, so the index of lines by height must take each table's
     contiguous run of lines apart, by y and then x
     (td-layout/DESIGN.md §5, "Tables"); list markers as a fragment kind of their own
     (`Layout::markers`, each a `MarkerFragment` naming its item, its
     ordinal and type, its line and its box, and no DOM byte), outside
     and inside, the ordinals HTML gives `ol`, `ul` and `menu` items
     (`start`, `reversed` and `value`, clamped to `i32`), in disc,
     circle, square, decimal, decimal with a leading zero, roman and
     alphabetic, roman and alphabetic falling back to decimal outside
     their ranges (td-layout/DESIGN.md §5, "List markers"); a replaced
     element's intrinsic size from the
     caller (`Request::intrinsic`: a width, a height and a ratio, each
     optional), used after its `width` and `height` attributes in place
     of its 300 by 150 placeholder. td-web gives as that intrinsic size
     its `alt` text's line, measured in the image's own font from the
     style the lookup is asked with, before the layout or with its own
     metrics since layout holds its metrics meanwhile, or nothing (0 by
     0), until increment 9, and its picture's after. A ratio, the
     picture's or the attributes' (their own proportions where both are
     given), is kept through `min-` and `max-` sizes, and a changed size needs a layout and no cascade
     (td-layout/DESIGN.md §5, "Intrinsic sizes"); a page-wide cascade
     work ceiling in its
     `Limits` (`MAX_CASCADE_WORK`), summing each element's weighted
     match steps, declarations, custom names, inherited custom copies
     and substitution charges, refused by name; a cancel flag (`Limits::cancel`,
     `Request::cancel`) read between elements as it cascades and every
     4,096 work units as it lays out, refusing with `Cancelled`; an
     input limit a caller sets (`Limits::input`), up to `MAX_INPUT`
     (4 MiB) for a text page's single text node, beside its 2 MiB
     `MAX_PAGE_INPUT` for markup; a cascade without `style` attributes
     (`Limits::style_attributes`); a byte limit for each layout
     request (`Request::bytes`), in place of the one fixed when the
     page was cascaded (td-layout/DESIGN.md §3 and §6); tab stops
     every eight columns where white space is preserved (`pre`,
     `pre-wrap`, `break-spaces`), a tab coalescing with its text so it
     costs no fragment, its advance and byte replayed by
     `inline::replay`; and a line past `MAX_LINE_SCALARS` cut and
     marked (`Layout::cuts`: the lines cut and the first one's place)
     where it does not wrap, broken there where it does, rather than
     refused (td-layout/DESIGN.md §4, "Tab stops" and "Long lines").
   - **td-css** (available): the owned-budget API supplies compilation
     admission; `rgb()`, `hsl()`, CSS Color 4's named colours and the
     `background` shorthand's colour; `list-style-position` and the
     `list-style` shorthand; and `StyleSet::from_sheets`, media
     conditions for each sheet given to a set, and one compilation that
     leaves out each sheet a per-sheet bound refuses and names it
     (td-css/DESIGN.md §3, §4 and §5). Whether a changed environment
     changes which rules apply is §11's.
   - **td-alloc** (available): `Charge::shrink_to`, which returns the difference
     between a charged ceiling and the actual, so td-web need not
     return the whole charge and charge the actual anew.
7. **The cutover**, one landing. Landed: the steps above behind
   `Document`, which stays the boundary between the render and window
   threads, with its page, picture and resource-failure bodies; the
   user-agent sheet, `<style>` and `style`; the budgets, places, find,
   selection, copying, links, scrolling, cuts and painting above, and
   `F9`; td-css, td-dom and td-layout joining §2's closure. td-mail and
   td-news keep td-html's renderer. A staged cutover would have put two
   producers behind one view, or a view over geometry its places cannot
   name, which AGENTS.md's principle 4 forbids; so the other crates'
   work came first, in increment 6. Validation: tests of each of the
   above in-process on the cell, §8's behaviours carried over, each
   rung of the cuts by a lowered limit, a page's and a let-go
   relayout's, and a relayout kept beside refused whole; a job
   cancelled at each step, at td-layout's reads and between rungs; and
   each step's charge refused, the last reservation of its own refused
   in turn (`td_alloc::with_reservation_limit`) and every charge
   returned, and shrunk once the step is done to what the page keeps;
   the cell measurements above (in "Relayout", "Memory", "Painting" and
   "Time"); the allowlist derived again and unchanged; and main's system boot still printing
   `TD-WEB-RUNNING`. Measured on the host over the pinned faces
   (`./install-fonts` into a scratch `XDG_DATA_HOME`), through the
   whole window in-process (`app::tests::measure_window`, release
   build, a process a page and output) at 1920x1200 and 3840x2160:
   seven of the eight corpus pages (td-dom/DESIGN.md's URLs, captured
   to scratch 2026-11-16, never committed; GitHub's not fetched),
   td-layout's 2 MiB shapes, td-html's hostile ones (one-letter
   paragraphs in 62 quotes, list items in 62, a 2 MiB word), hostile
   sheets (5,000 universal `:not()` rules, a `var()` of 30,000 tokens
   on every element, 1.5 and 0.9 MiB of custom properties) and many
   families and sizes on one page; the make, relayout and paint times,
   the owned storage and the resident peaks above ("Relayout",
   "Memory", "Painting" and "Time", and §3). Measured with increment 8
   (below): the WHATWG HTML and ECMAScript specifications cut to 2 MiB,
   SANE's list of backends and long Wikipedia articles, the sheets term
   and `WORKING` for parsing and compiling. Run as a process under a
   headless td-compositor at 1920x1200 and 3840x2160, each at scale one,
   the headless compositor's only scale (`tests/control_process.rs`,
   the gate's native case, over a mock td-fetch service): the window
   fills the output but for td-compositor's frame, fetches a page and
   its picture, reads back as its words over the control socket, and
   the compositor's capture of it has the digest of the seam's frame of
   it, both on the cell face (`TD_UI_FACE=bitmap`), with the picture's
   pixels on it; the seat's Tab and Return follow a link, and Ctrl+Q
   closes the window. The state first read the page loaded 343 and 500
   ms after launch in a release build on the development host; in the
   gate's debug build a 3840x2160 `frame` takes about a second and the
   compositor's capture about two and a half, so the case asks both
   again past their five-second exchange bounds on a loaded host. Not
   yet done, and still this increment's
   though it has landed: the 3840x2160 output at scale two, which needs
   a headless output of that scale; and whether sizes rounded to device
   pixels move a page's lines visibly against an unrounded layout.
8. **Stylesheets**: `<link rel=stylesheet>` and `@import` as
   "Subresources" says. Landed. Validation through the injected
   fetcher, in-process: cascade order of links, `<style>` elements and
   imports; media chains, every sheet fetched; imports four deep and the
   fifth left out, a cycle left out; `MAX_SHEET_FETCHES` and one past
   it; a sheet past the service's 1 MiB, one decoded past td-css's, the
   4 MiB of gathered text; mixed content at a redirect, never asked; a
   `data:` sheet, asked of no one; a `404` and a wrong type, noted; a
   slow sheet past `STYLE_WAIT`, the page shown meanwhile, then a reload
   applying it; stopped fetches holding the slots past the next page's
   `STYLE_WAIT`; `Escape` while styles load, the page shown staying and
   the root's charges returned exactly, the page dropped on the render
   thread within a tick, both while its sheets are out and between its
   jobs; a stopped page's sheet asking no hop past the one out; a page
   whose sheets never answer, against a model of the service's eight
   workers, the next navigation served at once (red at four); each
   request's `Ask` (public, its accept, 1 MiB, its deadline); a link's
   `disabled`, `type` essence and the preferred set; sibling imports in
   order and imports joined against a redirect's URL; a link's
   fallback to the page's encoding and a label beating `@charset`; the
   head ending at another statement and a bad string; a `data:` URL's
   fragment; a reply with no type; the compilation's child the sheets
   term less the gathered sheets, and rungs 2 and 3 letting them go;
   no answer lost however fast sheets are refused (32 refused at once,
   and a good sheet among twenty refused, each page styled at once), and
   a page gone freeing its answers' slots; a stopped page handed for its
   styles dropped on the render thread; a page naming no sheet holding
   none; imports naming `layer` or `supports()` counted; td-launch giving
   td-web's service eight workers; and, in td-fetchd over its socket,
   `--workers 2` serving two at once while a third waits, each given up
   at the `deadline` it asked of an origin that never answers.
   Measured on the host (`subresource::tests::measure_sheets` and
   `app::tests::measure_window` over a `fetch::tests::Corpus`, a
   process a page and output, release build, the pinned faces): the
   corpus of increment 7, GitHub's included, with the WHATWG HTML and
   ECMAScript specifications cut to 2 MiB, SANE's list of backends and
   Wikipedia's World War II and United States articles, each asked for
   at its own URL with every sheet it links and imports, 84 answers
   captured to scratch 2026-11-17 and never committed, sheets found by
   td-web's own gathering; the sheets term, `WORKING`, `STYLE_WAIT` and
   `MAX_SHEET_FETCHES` as "Memory" and "Subresources" say. Styled by
   their sheets, the thirteen pages are made in 4.7 to 814 ms at
   1920x1200 and 4.3 to 795 ms at 3840x2160, the WHATWG specification
   the slowest, laid out again two thirds as wide in 3.3 to 758 ms and
   3.1 to 762 ms, and painted whole in 4.2 to 6.3 ms warm (9.7 to
   13.1 ms first) and 11.3 to 20.6 ms (30.7 to 43.5 ms first), under
   a load average of about 7; a page holds 0.4 to 31.1 MiB of td-alloc's
   owned storage and declares 0.9 to 56.0 MiB, and a process peaks at
   42 to 87 MiB resident at 1920x1200 and 65 to 110 MiB at 3840x2160.
9. **Inline pictures**, as "Subresources" says. Landed. Validation
   through the injected fetcher, in-process: placeholders shown first
   and becoming pictures, drawn by pixel at their own size, at twice it
   at scale two with no copy, and as a copy where the page sizes them;
   pictures come together laid by one relayout at `PICTURE_SETTLE`,
   none before it, the top kept, a fitted copy kept by the next layout
   at its size, and a relayout for another cause laying only those
   settled; a refused picture keeping its placeholder and its `alt`
   text, noted (no route, a `404`, an XML type, a body that does not
   decode, SVG and another scheme never asked, a picture sniffed
   whatever its type); an unlinked picture opened by a click, a linked
   one following its link; mixed content at a redirect, never asked, and
   an `http` page's `http` picture shown; `MAX_PICTURES` and two past
   it, the first in tree order each asked once; the pictures term
   refusing a decoding and a copy, each kept as a placeholder; a
   decoding charged its whole reckoned peak first, refused a byte short,
   then exactly its pixels, and a copy exactly its own; a `data:`
   picture, asked of no one; each request's `Ask` (public, its accept,
   4 MiB, within its wait); `Escape` stopping a navigation before the
   pictures and the pictures after, those not come never shown, then a
   reload asking again; a picture not asked while a navigation is out;
   a page overtaken while a picture is out letting its pictures, decoded
   pictures and fitted copies go on the render thread and returning
   every charge; each answer put once and none after its page has
   gone, an untaken one going with its pictures, an owed one answered
   failed as it drops; a page of pictures whose origin never answers
   holding two slots, the next page's sheet fetched and styled at once;
   a layout's copies within its work, the rest placeholders noted; a
   navigation cancelling the page shown's relayout, the page laid again
   once it is stopped; block-level, floated and linked pictures laid
   and drawn, a refused one framed with its `alt`; copies refused beside
   the old layout's made again with it let go, at every resize; a
   `data:` picture past `MAX_URL` decoded; a slot held until its
   picture is decoded; a picture in the marked link framed; no settle
   while a relayout is out; a step down or a refusal letting the
   pictures go, their fetches told at once; a relayout stepped down to
   rung 2 laying its pictures, rung 3 none; a picture charged its
   pixels, `BLEND` times them when translucent, and knowing it; a
   navigation leaving a relayout that let the layout go running; a
   copy no room holds laid again once a want; `data:` pictures on the
   lane one at a time; a JPEG decoded at half scale laid at its frame's
   size; and `alt` text drawn inside its box. Measured on
   the host (`app::tests::measure_window` and
   `pictures::tests::measure_decoding` over a `fetch::tests::Corpus`, a
   process a page, output or picture, release build, the pinned faces):
   increment 8's pages but the specifications and SANE's list, every
   picture each names, 342 answers captured to scratch 2026-11-17 and
   never committed, replayed at their captured times, and a page of
   large pictures; the pictures term, `MAX_PICTURES`, `PICTURE_SETTLE`,
   `PICTURE_SLOTS` and the decoding's reckoned peak as "Memory",
   "Subresources" and "Slots" say. With their pictures, a process
   peaks at 44 to 97 MiB resident at 1920x1200, the Guardian's the
   most, and 67 to 141 MiB at 3840x2160, the United States article's.
   `MAX_PICTURES` is 128, the Guardian's front page naming 108, where
   64 left 44 of them placeholders.

No request carries a cookie until a jar is designed (§11).

### Open questions

td-layout's cascade work ceiling, in match steps and substitution
charges together, has no figure set here, only that it hold a page to
a few seconds of the render thread on the development host, which the
cell measurement above ("Time") holds for the `:not()` sheet and
increments 7's and 8's runs for the corpus, the slowest page made in
0.8 s.

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
- **Driving in the image.** td-web takes td-ui's seam on a host (§8,
  "Driving"); an oracle booting it driven needs td-jail to bind a
  private socket directory into the `web` jail and `bind`, `listen` and
  `accept4` in its allowlist (APPLICATIONS.md, "Per-application
  allowlists"; UNSAFE.md §9), with the probe's witness. td-ui's
  composition naming a picture's factor would let the seam's `frame`
  draw one as the window does.
- **HTML mail.** td-mail shows a message's HTML through td-page
  (td-page/DESIGN.md §4), styled, fetching nothing, its links opened in
  the configured browser with their real target shown first, or with
  Shift held in the other one, which td's mail package makes td-web
  through the portal's handler option (§7). To come: `cid:` pictures
  from the message's own parts;
  remote content only when asked for; and its pages made off the
  window's thread.
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
