# td-ui

td-ui is the dependency-free Rust toolkit that td-owned graphical programs
share. Today it carries the pinned Unifont face and wire codec, the XKB
keyboard translation, repeat policy and pointer decoding, the clipped XRGB
raster with its palette and scrollbar geometry, the Wayland client
connection over its own raw descriptor transport, and the client over it
(object table, registry, one toplevel surface with its buffers and frame
callback, the seat with its keyboard and pointer, the clipboard's data
device with its offers and sources, the pointer image and the turn loop),
all moved out of td-editor, with the chrome bands (`chrome`: the menu bar
and its panel, the wrapped text block, the tab strip and the status row)
that td-editor draws, the paged list `List` built on the panel's row
painter and the single-line text entry `TextEntry`, the new widgets no
scene drew. td-editor is its first consumer; the installer front end
`td-setup` (its welcome page landed) and td-portal's file chooser follow.
This document is the component contract and the starting point for
successive agents; the root `AGENTS.md` and `DEVELOPMENT.md` still govern
changes and submission.

## Status and scope

The crate exists and carries, unchanged in behaviour, what has moved out of
td-editor so far: the bounded XKB text-v1 keymap compiler (`keyboard`,
`xkb`), the explicit-clock held-key and repeat policy (`repeat`),
`wl_pointer` event decoding with axis-frame accumulation (`pointer`), the
core data-device decoding with the offer record and its budgets (`data`),
and the clipped, allocation-free XRGB painter over the pinned face
(`raster`): rectangle and glyph primitives, the integer scale, the warm
palette, scrollbar geometry, the text-run painter and the `Raster` that
writes a `Composition`'s draws into a caller-owned buffer, with the face's
provenance and licence texts embedded in `notices`; and the Wayland client
connection (`wayland`): display endpoint resolution from explicit
environment values, the bounded connect, request framing with at most one
descriptor per send, the receive path that owns every delivered right until
an event's consumer takes it, the startup and write deadlines, the unlinked
private pool file and the pointer image, over the private raw module `sys`
that `UNSAFE.md` §19 records; and the client over that connection
(`client`): the object table with its fixed and dynamic ids, the registry
with its budgets, the three globals every consumer binds, one toplevel
surface with its SHM buffers and frame callback, the seat with the keyboard
and pointer its capabilities give (the keymap right consumed and compiled,
focus, held keys, the modifier snapshot and repeat timing applied, pointer
events decoded and the pointer image shown on enter), the clipboard's data
device with its offers, barriers and sources (the send right consumed and
handed on), and the turn loop that drives a consumer's `App` under the
startup deadline; and the chrome bands over the raster (`chrome`: the menu
bar with its drop-down panel, the wrapped text block, the tab strip and the
status row), which td-editor's scene composes and paints. It re-mounts the
compositor's `font`, `font_data`, `wire` and `filter` sources exactly as
td-editor did, so there is still one Unifont face and one wire codec in the
tree, and it owns the 8x16 cell constants every consumer lays text out on.
td-editor depends on it by path and uses those modules through the crate's
public surface.

Newly built: `td-setup`, the second consumer, is a new crate whose
`welcome` page renders from the toolkit and whose Wayland turn loop
presents it as a live `App`, proven under the native compositor harness
(increment 6). Moved: td-portal's file chooser, the third consumer, renders
from the toolkit's raster and chrome bands (increment 7(c)) and its private
transport is now a live `App` over the shared client (increment 7(d)), the
old hand-rolled Wayland transport deleted. Its dialog is the first `App` to
own a Wayland object of its own, the privileged `td_portal_manager_v1` it
binds through its `Tag`. td-editor's window is the first `App` and
td-setup's the second; each of those owns no Wayland objects of its own.

Moved (increment 8(a)): the transport of td-editor's driven UI, the layer
an agent or a test operates a program through. `control` carries the
length-prefixed frame, the tab-separated ASCII envelope, the scalar and
byte codecs and the two response lines; `control_socket` publishes the
private Unix listener under td-editor's path and ownership contract;
`control_worker` is that transport's bounded thread, now generic over the
consumer's request type behind `Parse`; and `replay::run` is the
consecutive-frame runner behind a headless `--replay`. td-editor's control
socket, worker and replay run on them, wire-compatible with its
CONTROL.md; what a request means, and whether it may read or act, stays
the consumer's. Newly built (increment 8(b)): `driven`, the semantic
half, a `Controller` a consumer implements once over its own action
table, the generic verbs routed over the envelope, the text read back
from the draw stream and the painted frame, digested and paged; and the
raster's one XRGB-to-PPM writer, which td-setup's and td-editor's
previews now use. td-photo consumes the seam in its next increment.

Newly built (increment 11): the cell screen under "Cell screen" below.
`screen` is the styled grid a program that draws in rows and columns
paints, a `Composition` over the raster, with the key vocabulary such a
program reads and the translation of the keyboard's chords into it;
`screen_app` is the window that presents it, one more `App` shape,
which drives a program's `Handler` with translated presses, clicks and
wheel travel on cells, the grid laid out again on configure, focus and
the close request, and polls the handler each turn under a bounded wait
so work arriving on a channel from another thread is served without a
descriptor of its own in the loop. td-news and td-mail drew in it
until increment 12; nothing does now, and it goes with td-mail's
composing increment.

Newly built (increment 12): the widget window under "Widget window"
below. `window` is the same loop for a program that lays toolkit widgets
and an embedded document pane out over its surface instead of a grid:
its handler paints into a raster over the surface and reads the
keyboard's chords, the left button's press, drag and release in surface
pixels with Shift, wheel travel, the surface on configure, focus and the
close request. td-news and td-mail have moved onto it, with their
lists and td-editor's pane (APPLICATIONS.md §W.8, "Reworked again");
the cell screen and its window go with td-mail's composing increment.

## Purpose and trust position

td-ui is target-zone source: it ships only inside the programs that embed
it, as a Cargo path dependency resolved offline from the checkout. It is not
a runtime library, a plugin host, a theme system or a general Wayland
toolkit, and it does not claim third-party toolkit compatibility. It carries
no foreign payload and no external crate; its lock lists exactly its own
package, and its confinement tests pin that its manifest declares no
dependency at all.

A consumer names it as `td-ui = { path = "../td-ui" }`. That is the one
sibling-dependency spelling `builder/src/affected.rs` admits, and the
consumer's lock then lists exactly its own package plus td-ui. A program
that depends on td-ui is built by a cargo recipe that stages sibling source
trees (`local_source_trees`, the td-net shape); a flat-staged direct-rustc
recipe cannot link a second crate. td-portal, td-taskmgr, td-editor,
td-news and td-mail are built that way: each stages `td-ui`, and
`td-compositor` because td-ui mounts the font and wire modules from it,
beside its own tree (td-portal stages further siblings of its own), so a
toolkit edit moves each consumer's source-digest row and selects each
consumer's realized-output check.

## Public surface

The crate's `pub` items are the whole contract. Consumers use them through
`td_ui::` paths and nothing else; a consumer's confinement tests pin which
of its own files may name each module.

- `CELL_WIDTH`, `CELL_HEIGHT`: the 8x16 bitmap cell. `font::pinned` is held
  to them by a test.
- `font`: the compositor's PSF2 reader and pinned Unifont face, unchanged.
  Provenance and licences stay in `td-compositor/assets`.
- `wire`: the compositor's Wayland framing codec, unchanged.
- `filter`: `MAX_QUERY_BYTES`, `insert` and `matches`, the compositor's
  bounded ASCII query rule shared by the launcher and td-portal's chooser,
  mounted from `td-compositor/src/filter.rs` unchanged; the finder takes
  its `insert` and bound and repeats its match rule with the name folded
  at the comparison.
- `keyboard`: `Keymap::parse` over an XKB text-v1 map, `Modifiers`,
  `Stroke`, `InputError`, `Selected`, and translation from evdev keycodes
  plus a compositor modifier snapshot to logical chords. No display,
  descriptor, environment, action execution or clock is accessed. The
  bounded lexical envelope and the compatibility target are the ones
  td-editor/DESIGN.md records under "Implemented keyboard compiler"; that
  text remains the behavioural specification and moves here with the next
  documentation increment.
- `xkb`: `TypeCatalog`, `VirtualBinding`, `Selection`, `ResolvedType`,
  `Diagnostic`, the type-table half of the compiler that the keyboard
  compiler builds on.
- `repeat`: `Input`, the held-key set and repeat policy over an explicit
  millisecond clock: focus with held keys, modifier snapshots, timing,
  key press and release, arming, repeat and next-wake computation. The
  client owns one for its keyboard and applies the lifecycle half (map,
  focus, snapshot, timing, presses) itself, lending a consumer a read of
  the state and the repeat half: `arm`, `repeat`, `wait_ms` and
  `cancel_repeat`.
- `pointer`: `Event`, `decode` for `wl_pointer` v5 through v7 events and
  `Wheel`, which accumulates axis, axis-discrete and axis-value120 input
  into whole cell rows and columns per frame.
- `data`: `DeviceEvent`, `SourceEvent`, `device`, `source` and `offer`, the
  exact core data-device v3 schemas; `Offer`, the record the client keeps
  per server offer, with `mime`, its preferred supported text type, and
  `announce` under the budgets; `UTF8` and `PLAIN`, the two text MIMEs a
  consumer offers and accepts; `OFFER_LIMIT`, `ANNOUNCEMENTS` and
  `MIME_BYTES`.
- `raster`: `Rect`, `Scale` (1 through 4), `Weight`, `GlyphStyle`, `Primitive`,
  `Draw`, `Surface`, the `Composition` trait, `Scrollbar`, `text_run`, `Raster`,
  `Error`, the axis and frame-byte ceilings, the palette constants, and `rgb`
  and `ppm`, a painted frame as tight RGB rows and as a binary PPM. A
  composition reports the surface it was laid out for and streams the draws
  inside a damage rectangle; `Raster::new` validates surface, font, stride and
  buffer before any write, and `Raster::paint` refuses a composition laid out
  for another surface. The behavioural contract (clipping, the medium fringe,
  scrollbar proportions and drag rounding) is the one td-editor/DESIGN.md
  records under "Implemented reference-renderer contract"; that text moves here
  with the documentation increment.
- `chrome`: `Bar` with its `Panel`, `Block`, `Strip`, `Button` and
  `Buttons`, `Status`, `List` and `TextEntry`, the `Row` a panel paints,
  the `Item` a list paints, the `Field` a text entry paints and `step`,
  the bands, the paged list and the text entry a td-owned window shares,
  over `raster` and independent of any scene. The bar, a panel's rows,
  the tab strip, the button strip and the status row are each `ROW` (24)
  reference-renderer pixels tall, of 8x16 cells, scaled by the surface;
  the text block wraps in 16-pixel cell rows. `Bar` fills the first row
  and lays its labels from cell one, three cells apart, and answers a
  header hit. Its `Panel`,
  under a header and clamped to the right edge, is `PANEL_WIDTH` (320)
  pixels wide with one row per entry, at most `PANEL_ROWS` (13), and
  exists only with a status row's height below it; it paints each `Row`
  with the selected one highlighted, a disabled one dim, a checked one
  prefixed and the shortcut at the right edge, and `step` walks the
  enabled rows with wrap. `List` shares that row painter over a
  scrolling window the caller owns: `ROW`-tall rows filling a
  rectangle, the same highlight, dim and prefix, a marked row starred
  and an optional right-aligned `Item` column, empty rows below left
  chrome, and a scrollbar in a `SCROLL_GUTTER` (16)-pixel gutter at its
  right; `reveal` keeps the selection shown and `hit` maps a point to a
  row. `Block` wraps a caption at its columns, the
  width less a cell each side, over up to `BLOCK_ROWS` (15) rows, a
  newline starting the next, at most `BLOCK_SCALARS` (`BLOCK_ROWS` * 73)
  scalars visited. `Strip` lays `TAB_WIDTH` (160)-pixel tabs from the left
  keeping the active one in view and clipping a tab that a narrower
  surface cannot hold, painting the active one paper and the rest chrome
  under a top and right border, a dirty tab starred and a `CLOSE_WIDTH`
  (24)-pixel close mark at its right; with no tabs it still fills its row.
  Document tabs retain that default. `with_close_buttons(false)` gives
  resource tabs the close-mark space for their labels and removes the
  close hit region. `hit` returns typed `Select`/`Close` intents only on
  the visible surface, including when a tab is partly clipped.
  `selection` handles `Previous`, `Next`, `First` and `Last`, wrapping
  previous/next and returning no selection for an empty strip. The caller
  stores the returned active index and chooses keyboard bindings; the
  strip's overflow layout keeps that active tab visible.
  `Status` shows one line in whole cells, the width less a cell each side
  and at most `STATUS_COLUMNS` (512), a control scalar blank and the last
  cell an ellipsis when the line is longer, under a top border; `frame`
  paints the row without a line for a consumer laying out its own status.
  `TextEntry` is one `ROW`-tall paper field the caller drives with a
  `Field`: the text from the first shown column, an optional selection
  filled `SELECTED` focused or `INACTIVE_SELECTION` not with the ink
  flipped over it, a one-pixel `INK` caret the caller blinks, a dim
  placeholder when empty, and a masked mode drawing a fixed mask glyph
  per character; `reveal` keeps the caret shown and `hit` maps a point to
  a caret column. Each streams its fills and glyphs inside a damage
  rectangle and reads nothing but its inputs; a draw-stream oracle pins
  each, and the status band, the list and the text entry are rasterized
  whole to pixels. td-editor's `Geometry` and `Scene` compose the bands
  and its `--preview` stays byte-identical.
- `notices`: `FONT_PROVENANCE`, `FONT_COPYING` and `FONT_LICENSE`, the
  texts beside the face in `td-compositor/assets`, embedded at compile
  time for a program's `--font-license` output.
- `wayland`: `Endpoint` and `endpoint` (from the `WAYLAND_SOCKET`,
  `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` values a consumer passes),
  `connect`, `Connection` (`new`, `send` with at most one borrowed file,
  `words`, `take`, `read_more`, `budget`, `pop_descriptor` with the
  pending `descriptors` count, and the `wait` and `startup_deadline`
  accessors), the budgets `READ_BYTES`,
  `PENDING_BYTES`, `DESCRIPTORS`, `WRITE_DEADLINE`, `CONNECT_DEADLINE` and
  `IDLE_WAIT`, `backing_file`, `CURSOR_WIDTH`, `CURSOR_HEIGHT` and
  `cursor_pixels`, and `peer`, the test support: `drain` reads the far
  end of a socket pair a consumer's tests own (giving a blocking end the
  idle wait as its read timeout when it has none), and `push_descriptor`
  queues a right under the reader's bound without a socket; both are
  public because a consumer's tests are another crate, and `drain` is
  held to the connection's byte and right budgets. The FIFO itself is
  never lent out: a consumer pops rights one at a time in arrival order
  and cannot reorder or extend them. Errors are strings under the
  module's own `Result` alias, as the wire codec's are; a consumer that
  glob-imports the module shadows `std::result::Result`. Its behavioural
  contract is the transport invariant below. `sys`, the raw module
  beneath it, is private to the crate.
- `client`: `Client<T>` over one `Connection`, with the consumer's own
  object kinds as `T: Tag` (`retired` says which of them wait for
  `delete_id`) inside `Kind<T>`, beside the client's own (the fixed slots,
  pools, buffers, frames, the pointer image's, the seat, keyboard and
  pointer, and the data-device manager, device, sources and sync barriers,
  with their retired states); the fixed ids `DISPLAY` through `TOPLEVEL` and
  the budgets `OBJECTS`, `GLOBALS`, `NAME_BYTES`, `BUFFERS`,
  `MESSAGES_PER_TURN` and `INITIAL_DEADLINE`; `new`, the table (`allocate`
  and `set_tag` for the consumer's own objects, `kind` for any slot), the
  registry (`find_global`, `global_name`, `globals` for a consumer that must
  assert the compositor advertised an exact global set, `bind`, `required`,
  `is_required`, `forget_global`), the toplevel (`set_title`, `set_app_id`,
  `commit`,
  `acknowledge`, `close`), presentation (`can_present` and `present`, which
  refuses an extent the raster could not paint, then paints through the
  caller's closure into the reused raster and submits under the three-buffer
  rule; `buffers`, `pixels`, `frame_callback`), the devices (`seat`,
  `keyboard`, `pointer`, `entered` for the pointer's enter serial, `input`
  for the keyboard's state, and the repeat half of that state a consumer
  drives: `cancel_repeat`, `arm`, `repeat` and `wait_ms`), the clipboard
  (`clipboard` for a live data device, `selection` and `selection_mime` for
  the seat's selection and its preferred text type, `receive` to ask it for
  its text over a consumer's endpoint, `offer_selection` to offer text at a
  serial, and `source` for the live source whose text the consumer keeps),
  the pointer image's `cursor`, the state accessors `bound`, `configured`
  and `closed`, `words`, `send` and `pop_descriptor` with the pending
  `descriptors` count, `needs_descriptor` for the keymap and send rights the
  client consumes, `connection` for the schedule inputs, and `handle`, which
  takes the consumer's clock, consumes what is the client's in an event and
  returns `Handled`: `Done`, `Bound`, `Configure`, `CloseRequested`,
  `FrameDone`, `GlobalRemoved`, `Capabilities`, `SeatRemoved`, `Keyboard`
  with a `KeyboardEvent` (`Keymap`, `Focus`, `Ready`, `Key` with its serial,
  key and `Stroke`, `Refused`), `Pointer` with the decoded `pointer::Event`,
  `Clipboard` with a `ClipboardEvent` (`Selection`, `Send` carrying the
  right to write the offered text to, `Cancelled`, `Released`), or
  `Unhandled` for the consumer's own objects. `App` (`client`,
  `needs_descriptor` for the consumer's own rights, `descriptor_wait`,
  `tick`, `event`, `end_turn`, `draw`) is what `run` drives: the registry
  request and initial sync under the startup deadline, then, until the
  client is closed, at most `MESSAGES_PER_TURN` events per turn with one
  event parked while its right has not arrived (the client's keymaps and
  sends and the consumer's rights alike, cancelling repeat), the consumer's
  end of turn and draw, and the transport's wait. `unconfigure` and
  `input_mut` are test support, public because a consumer's tests are
  another crate and hidden from the crate's documentation.
- `control`: `MAX_FRAME`; `Error` (`Protocol`, `Limit`) with its `code`,
  and `Result`; `ErrorCode`, the one method a consumer's error type gives
  so its codes can travel, and `valid_code`, the grammar of a code;
  `Refusal<E>` (`id`, `error`) with `response`, the error line, and `ok`,
  the success line; `Parse`, the request type a worker parses off the
  wire; `Decoder` (`push`, `payload`, `finish`) and `frame`; `Envelope`
  (`id`, `name`, `args`) and `envelope`, generic over the consumer's error
  so its parser can `?` through; and the codecs `decimal`, `size`,
  `boolean`, `hex` and `unhex`.
- `control_socket`: `Socket` with `bind`, `accept` and `close`, the
  private listener publication under "Private socket publication" below.
- `control_worker`: `CONNECTIONS`; `Worker<R>` (`start` for an `R: Parse`,
  `try_request`, `close`) and `Job<R>` (`request`, `is_live`,
  `respond_with`, `respond`), the bounded transport under "Bounded
  worker" below.
- `replay`: `run`, the consecutive-frame runner over a consumer's handler.
- `driven`: `KEY_BYTES`, `ARGUMENTS`, `PAGE_BYTES` and `WHEEL_LIMIT`;
  `PointerPhase`, `Input` and `Outcome` with its `word`; `Binding`, one
  row of a consumer's action table, with `check`, `bound` and `help`; the
  `Controller` trait (`bindings`, `action`, `input`, `state`,
  `compose`, `request`); `VERBS` and `request`, the generic router;
  `Payload`, the worker's request type for a driven consumer; `text`,
  the read-back of a composition's draw stream; `paint` and the `Frame`
  it returns with its `ppm`; and `fnv1a64`, the frame digest.
- `screen`: `PAPER` and `INK`; `Style` (`new`, `bold`, `reversed`)
  and `Cell`; `Screen` (`new`, `resize`, `surface`, `rows`,
  `columns`, `ground`, `clear`, `clear_row`, `put`, `write`, `cell`,
  `line`, `hit`), a `Composition`; `Key`, `Press` (`plain`,
  `plain_char`) and `press`, the chord translation; and `Input`, the
  vocabulary a screen program reads, under "Cell screen" below.
- `screen_app`: `DEFAULT_WIDTH` and `DEFAULT_HEIGHT`; `Flow`; the
  `Handler` trait (`app_id`, `ground`, `input`, `poll`, `wait_ms`,
  `needs_redraw`, `render`, `title`, `notice`); `Object`, the empty
  tag; `Window<'h, H>` (`new`, `handler`, `handler_mut`, `screen`), the
  `App` over a handler it borrows; and `run`.
- `window`: `DEFAULT_WIDTH` and `DEFAULT_HEIGHT`; `Flow`;
  `PointerPhase`, the driven seam's, re-exported; `Input<'a>`, the
  vocabulary a widget program reads; the `Handler` trait (`app_id`,
  `title`, `input`, `poll`, `wait_ms`, `needs_redraw`, `paint`,
  `notice`); `Object`, the empty tag; `Window<'h, H>` (`new`, `handler`,
  `handler_mut`, `surface`), the `App` over a handler it borrows; and
  `run`, under "Widget window" below.

## Driving

A td program is operated by a person at the keyboard and by an agent
acting for that person, and APPLICATIONS.md §S asks that agents be a
first-class consumer rather than a reader of an accessibility tree bolted
on afterwards. The toolkit therefore carries the driven UI's mechanism,
so that every consumer speaks one protocol on one transport and a
consumer adds only its own vocabulary. The shape is td-editor's, the
tree's precedent: a display-independent dispatcher, a headless replay of
it, and a control socket on the live window, all speaking one envelope.
td-editor's CONTROL.md remains the reference for its own verbs and their
admission rules; this section is normative for what moved here.

The split is fixed. The toolkit owns the wire (frame, envelope, codecs,
the two response lines and the transport's two error codes), the private
socket publication, the bounded worker and the replay runner. The
consumer owns its request type, parsed behind `Parse` on the worker's
thread; the meaning of every verb; the body of its `state`; and the
decision, per request, whether it reads or acts. The toolkit never
decides that last question: as td-editor separates `response(&Controller)`
from `execute(&mut Controller)`, a consumer answers a reading verb from a
borrow and dispatches an acting verb through the same admission its
keyboard goes through. Reading is the lower authority (§S); a consumer
that wants to grant it separately does so at its own boundary, not in the
toolkit.

### Framing

One frame is a four-byte unsigned big-endian length followed by that many
payload bytes. Length excludes the header and must be 1..=1,048,576 bytes
(`MAX_FRAME`), for requests and responses. There is no newline terminator.
`frame` checks the bound before allocating its output. `Decoder`
accumulates one frame, accepts arbitrary header/body fragmentation, and
exposes its payload only after all declared bytes arrive. A zero length
is `protocol`; a length over the ceiling is `limit`. Incomplete `finish`
is `protocol`.

Any decoder refusal permanently poisons it and drops partial payload
storage. More input cannot revive it. Bytes beyond the declared body, if
supplied to that decoder, are refused rather than treated as a second
request. Completion does not prove peer EOF or that no later bytes will
arrive: the worker dispatches at most once per connection and closes it
after the response. `finish` transfers the completed buffer; it performs
no read or EOF check. Empty input chunks make no progress. The decoder
allocates at most one MiB of payload storage, only after validating the
length; the full declared storage is allocated on header completion,
before body bytes arrive, so the worker budgets each admitted connection
against that one-MiB allocation, not against bytes received so far.

### Envelope and responses

Payloads are ASCII tab-separated records: `1 ID NAME ARGS...` with literal
Tab separators. Literal control bytes other than Tab, DEL and non-ASCII
bytes are refused. `decimal` fields contain one or more digits only:
leading zeros are accepted; signs, spaces, exponents and overflow are
refused. IDs fit `u64`; `size` additionally fits the host's `usize`;
`boolean` is `0` or `1`. Byte strings are lowercase `hex`, two digits per
byte, the empty string spelled `-`. `envelope` validates the version, the
ID and the presence of a name, and hands the consumer the name with the
remaining fields as a bounded iterator, never a vector proportional to
the Tab bytes in an untrusted payload; unknown names, missing or extra
fields and argument grammar are the consumer's `protocol` refusals.

A success is `1 ID ok BODY` (`ok`); an error is `1 ID error CODE HEX`
(`Refusal::response`), the diagnostic being the code's own ASCII bytes
in hex. A code is one to 32 bytes of lowercase ASCII letters, digits and
hyphens (`valid_code`), since it stands unquoted in the tab-separated
line; the toolkit renders whatever `ErrorCode` returns, so a consumer
pins its codes against that grammar in its own tests. The toolkit
renders these two lines; a consumer may answer with a further status
word in the same position for its own protocol, as td-editor answers a
queued job with `1 ID pending JOB`, which its own reference specifies.
A recoverable request ID is echoed even if the name is missing or
later arguments are invalid; failures before a valid ID is parsed use
zero, so frame-size, ASCII and version checks precede ID parsing. Zero is
also a valid caller ID and carries no authorization meaning. The
transport's own codes are `protocol` and `limit`; a consumer's `Error`
maps them into its own type (`From<control::Error>`) and gives its codes
through `ErrorCode`, so one `Refusal<E>` renders every refusal, the
worker's included.

### Private socket publication

`control_socket::Socket::bind` opens a Linux Unix-domain listener only when
explicitly called. It does not accept a CLI option, create a worker, decode
a request, read consumer state or send a response. `accept` and each
accepted stream are nonblocking; connection limits, deadlines,
cancellation and turn-loop integration are the worker's and the
consumer's. No raw syscall surface or dependency is used: the filesystem
operations use safe `std` and procfs, which must be mounted at `/proc`.
Like the transport beneath `wayland`, the crate's raw guard requires
Linux x86-64 at compile time; these open flags name that ABI, not all
Unix platforms or Linux architectures.

The socket pathname must be absolute, non-NUL and at most 107 bytes, with
a nonempty basename of at most 80 bytes. Trailing slash, `.`/`..`
basenames and parent `..` traversal are refused. Interior `.` and
repeated separators have ordinary `Path` normalization; there is no shell
expansion or canonicalization through symlinks. Non-UTF-8 names remain
literal OS bytes. The basename cap keeps the internal descriptor-relative
bind name within Linux's pathname socket limit independently of the
current descriptor number.

The final parent must already exist, belong to the calling UID and have
mode exactly 0700, including no special bits. The binder neither creates
nor chmods directories. Every ancestor, including root, must be owned by
root or the caller and must not be group/other-writable unless sticky.
This admits ordinary `/tmp` while protecting caller/root-owned path
components from rename by another UID. An otherwise private parent
inside a non-sticky shared writable ancestor is refused.

Ownership is interpreted in the consumer's current user namespace.
Unmapped ancestor owners are not trusted: the overflow UID (commonly
65534) is not an alias for root. A rootless container must provide a
path whose complete ancestry satisfies these checks; the optional
endpoint otherwise refuses. Before walking the path, the binder reads
`/proc/sys/kernel/overflowuid` with an 11-byte limit plus one byte for
overflow detection, requiring a decimal `u32` with at most one final LF;
missing, malformed or oversized data refuses publication. If the
configured overflow value equals the caller UID or zero, it refuses:
otherwise an unmapped owner could be numerically indistinguishable from a
trusted owner. This includes callers legitimately mapped to the overflow
number; the binder cannot distinguish the two cases from stat ownership.
No fixed overflow number is assumed and no sysctl is changed. Concurrent
changes to that global sysctl require the already-excluded privileged
host authority. Linux's [user namespace
contract](https://man7.org/linux/man-pages/man7/user_namespaces.7.html)
specifies this substitution for stat and process-status ownership. The
predicate is never relaxed on environment variables or test execution.
The crate declares `trusted-test-root = true` in its gate metadata: the
gate's derived test commands then set the variable the capped runner
consumes to run each test binary under a private caller-owned root with
its own `/tmp`, so the fixtures' ancestry is trusted inside the gate's
namespace, where the host's root otherwise appears as the unmapped
overflow UID and publication refuses (DEVELOPMENT.md, "Trusted filesystem
roots for permission tests"). That is a test environment, not a
production admission; the socket module reads no such variable and its
confinement test pins that.

The UID comes from a bounded `/proc/self/status` read (64 KiB plus one
byte for overflow detection): exactly one complete `Uid:` record with four
equal representable `u32` values, real, effective, saved and filesystem.
Missing/duplicate/malformed/mixed records refuse. UID zero is allowed
when all slots agree; this is not a sandbox for root. Environment
variables are never evidence of identity.

The binder walks directory components from an opened root using `O_PATH
| O_DIRECTORY | O_NOFOLLOW`, one component at a time through
`/proc/self/fd/N`. These kernel descriptor links are intentional; no
user-supplied symlink component is followed. It retains the final
directory descriptor and binds through its pinned descriptor path rather
than reopening the user's absolute path. Every existing final name is
refused without connecting to it: regular files, directories, symlinks,
live listeners and stale sockets all remain untouched. A concurrent
binder is still subject to the kernel's exclusive pathname creation.
Socket address diagnostics report the internal `/proc/self/fd/N/name`
bind address, not the requested pathname. A peer must connect using the
requested path it was given, not reuse `peer_addr` as a path in its own
descriptor table. Callers retain the requested path for diagnostics; the
guard does not store another copy.

Binding briefly creates a listening socket with umask-derived permissions
before mode 0600 is set. The already-private parent contains that
interval to the same caller/root trust boundary; changing process-global
umask would race unrelated file operations and is not used.

The binder opens the new filesystem socket inode with `O_PATH |
O_NOFOLLOW`, requires a socket owned by the caller, and pins that
descriptor too. It sets mode 0600 through its kernel descriptor path and
reads back mode/owner, then rewalks the visible parent, rechecking trust,
private mode and identity, and verifies the named socket still matches
the pinned inode. This catches parent/name changes during publication.
The filesystem socket inode is distinct from the listener's socket
descriptor inode; cleanup pins the former.

Explicit `close` makes one checked cleanup attempt and reports errors.
Drop attempts the same cleanup best-effort. Before unlinking the original
basename inside the pinned parent, cleanup requires socket type and
matching device/inode. A missing name is already clean only after
rechecking that the procfs parent descriptor path remains accessible;
this is an accessibility check, not detection of a renamed or removed
parent. A real procfs link still addresses that pinned inode after either
operation. This also handles a name removed between the identity check
and unlink. A replacement file/socket is left alone. Renaming the parent
does not redirect cleanup into a replacement directory. Keeping the
socket inode open prevents inode-number reuse from fooling the
comparison. If binding succeeds but inode ownership cannot be
established, the binder does not unlink an unverified name; a stale
endpoint may remain for caller inspection. Later setup failures attempt
normal checked cleanup.

Identity checks and pathname operations are separate kernel operations,
not an atomic compare-and-unlink primitive. These rules protect against
other UIDs under the admitted permissions; they are not isolation from
root or another process running as the same UID. Such a process can race
bind-to-pin or check-to-unlink, rename names, or change permissions, and
already has the endpoint's authority. The consumer must keep this path
private. Sharing it across a jail boundary remains a separate explicit
grant.

### Bounded worker

`control_worker::Worker::start` takes an already admitted `Socket`. One
thread, named `td-ui-control`, owns the listener and every accepted
connection; no socket I/O occurs in `try_request` or `Job::respond`. The
thread holds no consumer state and no lock on it: the one thing it knows of
the consumer is the request type `R: Parse`, whose `parse` it runs on each
complete payload. The consumer's turn loop owns dispatch and command-line
opt-in.

The worker admits at most eight connections (`CONNECTIONS`). When all
slots are occupied, further clients stay in the kernel listener backlog;
no descriptors, threads or request allocations are created for them. No
backlog connection deadline or backlog size is promised. The five-second
whole-request deadline starts when the worker accepts, never renews on
progress, and includes header, body, queue/response time and output. At
or after expiry the connection closes silently. Every loop checks each
live connection, allowing at most one 16-KiB read or write for it, then
parks for at most ten milliseconds with live connections, or 100
milliseconds when idle. Replies, abandoned jobs and shutdown unpark the
worker early. A ready reply attempts its first write in the same step; a
refusal after a read waits until the next step to preserve the I/O
bound. This deliberately paces output near 1.6 MiB/s per connection: a
maximal one-MiB reply takes 64 steps, about 640 milliseconds, and
td-editor's largest hex-encoded text page, 512 KiB, 32 steps, about 320,
excluding scheduling delays.
Idle polling trades up to 100 milliseconds of acceptance latency for
fewer wakeups; blocking accept would require a separate reliable shutdown
wakeup. Scheduling delay and kernel execution are not hard real-time
guarantees; the worker never deliberately blocks on socket I/O.

The endpoint is not an availability boundary against the same UID or
root. Such a process can continually occupy all eight slots, keeping
legitimate clients in the backlog. Deadlines bound each admitted
connection, not a peer's share of future admissions. A full consumer
queue closes without an error frame.

One complete request dispatches at most once, without requiring write
EOF. The shared decoder rejects zero/oversized frames and extra bytes
delivered in the same read. After completion no further request bytes
are read: later bytes never execute a second command. A premature EOF or
decoder refusal queues a framed error with ID zero; a parse refusal uses
its recovered ID and the consumer's code. Transport failures, deadline
expiry or a full consumer queue close the connection without a
guaranteed error frame. Successful output closes after its one complete
frame, without waiting for the peer to close.

Eight typed jobs fit in the consumer's queue; submission is nonblocking
and a full/disconnected queue closes that connection. Each job has a
one-element response channel and a liveness token combining the
acceptance deadline and connection lifetime. `try_request` never waits
and makes at most eight receives per call, returning the first live job.
After eight expired entries it returns no job; remaining arrivals or
worker disconnection are observed on a later poll. A disconnected worker
reports an error once that bounded expired prefix has been drained.
Dropping a job without a response disconnects its response channel
before waking the worker to close the client.

The consumer answers through `Job::respond_with`, which checks liveness
before passing the borrowed request to the consumer's handler; the
handler applies the consumer's own admission and returns one payload.
The underlying `Job::respond` checks liveness and frame limits before
copying/queuing it; success means queued, not delivered or rendered. It
does not validate the handler's response fields. A disconnected or
expired job returns false; an invalid payload size returns the shared
frame error. Closing a connection cancels its queued/held job; a deadline
never reserves a consumer snapshot. This transport API does not replace
the consumer's live admission of a request against its state.

Request payload allocation is at most one MiB per reading connection.
What a parsed `R` retains is the consumer's contract (td-editor's holds a
fixed-size descriptor plus at most 256 KiB of decoded text); the worker
drops the wire payload when moving to the wait and may briefly hold
both. Each response channel/connection owns at most one framed reply of
one MiB plus four bytes. A reply may briefly coexist with its original
request allocation during handoff; there are at most eight such
connections and eight queued jobs. One reusable 16-KiB scratch buffer
serves the thread. These bounds exclude consumer-owned response inputs
and retained jobs, allocator overhead and kernel socket buffers; there is
no unbounded worker byte queue. A job's debug output is its request's;
a consumer whose requests carry text redacts it there.

Explicit `close` and Drop set the stop flag, unpark and join the worker.
It invalidates all live jobs, closes clients, and uses the socket's
checked cleanup. Explicit close reports worker/cleanup failure; Drop is
best-effort. No detached thread survives a completed join. Interrupted or
aborted accepts and temporary descriptor/memory/buffer exhaustion retry
after the normal park; other listener errors stop the worker and socket
Drop attempts cleanup. A deadline representation overflow also stops it
rather than accepting without a deadline. Shutdown is not a wall-clock
promise about filesystem operations or host scheduling.

### Replay runner

`replay::run` reads consecutive frames from a reader until EOF and writes
one framed reply per frame, produced by the consumer's handler, to a
writer, flushing after each. EOF between frames ends the session
normally; a partial header, a zero or over-ceiling length, a short body
or a reply the frame encoder refuses is an error that ends it. The runner
owns no state and no descriptor: a consumer's `--replay` hands it its
stdin and stdout and a closure over its own headless session, so the
same parser and dispatcher answer both the socket and the replay.

### The semantic seam

`driven` is the half a consumer implements once so an agent or a test can
operate it without a protocol of its own; td-photo consumes it first. A consumer
gives a `Controller`: `bindings`, its closed action table; `action`, applying a
named action with the fields the request carried; `input`, delivering one
semantic `Input` (a key chord, a pointer phase at a pixel position, wheel rows
and columns, a resize, focus, a tick) through the same key and pointer paths its
window uses; `state`, the tab-separated body of its facts; `compose`, one
reading of what the window shows now, building a scene that borrows its model
per request if that is how it draws; and `request`, its own verbs beyond the
generic set, `protocol` by default. Its `Error` maps the transport's two in and
gives its codes out through `ErrorCode`, as for the transport. td-editor keeps
its own `Event`, whose tab and revision fences are its admission contract, and
does not adopt the seam; td-setup and td-portal adopt it when their pages need
driving.

The action table is `Binding` rows: the name `action` takes, the chord
the consumer's keyboard binds by default, the argument shape and a help
line. `check` holds a table to its grammar (names in the code grammar and
unique, chords unique and within the key bound, shapes and help printable
ASCII, help present), and a consumer pins its table with it; `bound`
answers which action a chord names; `help` prints the table aligned,
which a consumer's `--help actions` shows so an agent reads the table
instead of guessing.

`request` routes one payload over `control`'s envelope, at most eight
fields after the verb, and answers as `control` frames it (field spaces
denote literal Tabs):

- `1 ID state`: the consumer's body.
- `1 ID actions`: `N`, then per action its name and, each in hex with
  `-` for none, its chord, argument shape and help.
- `1 ID action NAME ARGS...`: NAME must be in the table, else
  `protocol`; the consumer's `action` judges the fields; the reply is the
  outcome word, `changed`, `ignored` or `quit`.
- `1 ID key HEX_CHORD`: the chord, 1 to 32 bytes of UTF-8 without control
  characters, through `input`; the outcome word.
- `1 ID pointer PHASE X Y`: `press`, `move` or `release` at pixel X, Y,
  each a `u32`; the outcome word.
- `1 ID wheel ROWS COLUMNS`: signed whole cells, magnitude at most
  16,777,216; the outcome word.
- `1 ID resize W H SCALE`: refused as the raster's `Scale` and `Surface`
  would refuse it, before the consumer sees it; the outcome word.
- `1 ID focus 0|1` and `1 ID tick MS`: the outcome word.
- `1 ID text`: `ROWS COLUMNS HEX_TEXT`: the cell grid's rows and columns
  and the read-back below, of at most ROWS lines.
- `1 ID frame`: `W H SCALE DIGEST`, the FNV-1a 64 of the frame's RGB rows
  in hex.
- `1 ID frame-page OFFSET LIMIT`: `W H OFFSET HEX_BYTES` of the RGB rows,
  LIMIT 1 to 256 KiB (`PAGE_BYTES`), an offset past the end `protocol`.
- A generic verb (`VERBS`) with fields it does not take: `protocol`,
  at the router. Anything else: the consumer's `request`.

The reading verbs borrow the controller; `action`, `key`, `pointer`,
`wheel`, `resize`, `focus` and `tick` go through the consumer's own
admission, which is where reading and acting are told apart (§S). The
digest is an equality witness for tests and agents, not a hash for
anything adversarial; a frame of any size crosses the wire in pages under
the frame ceiling, and `paint` repaints per request, so the pages of one
frame are consistent only while the consumer's state is.

A socket consumer runs `Worker<Payload>`: `Payload`'s `Parse` validates the
envelope and the field count on the worker's thread, so a malformed frame is
refused there as the transport promises, and the turn answers each job with
`request` over the payload's bytes, where the verb and its fields are judged
against the consumer. A replay hands `request` each frame's payload. `quit` is
reported, not acted on: the runner keeps answering and a window closes on its
own terms, so a consumer tracks its own quit.

`text` reads the composition's draw stream onto the surface's cell grid at its
scale: each glyph lands in the cell its origin names (an origin off the grid, a
glyph straddling the top or left edge included, names none), a later draw over
an earlier one, a fill clearing every cell it wholly covers, so an opaque panel
hides what it paints over, a glyph whose clip excludes it wholly absent; rows
are trimmed on the right and trailing blank rows dropped, so the text has at
most ROWS lines. That is the accessibility question answered at the seam the
toolkit already has: every widget emits its text as Unicode scalars, so nothing
is retyped for the agent. `paint` paints the composition into a fresh buffer
through the pinned face, parsed once per process, and returns the `Frame`, its
surface and tight RGB rows, whose `ppm` is the binary image; `raster::rgb` and
`raster::ppm` are the one XRGB-to-PPM writer, which td-setup's `preview_ppm` and
td-editor's `--preview` use (td-photo's PPM writer is for its RGB thumbnails,
not frames, and stays its own).

## Invariants

- Pure modules read no environment, clock, descriptor or filesystem.
  Adapters pass explicit ticks in milliseconds and explicit byte inputs.
  Outside the pure set are `notices` (three `include_str!` constants and
  nothing else), the transport pair `wayland` and `sys`, which own the
  stream, its deadlines and the pool files in the directory a consumer
  names, `client`, whose `run` reads the monotonic clock for the
  consumer's ticks and whose buffers are those pool files, and the three
  driving adapters: `control_socket`, which owns the listener it binds
  and reads procfs for the caller's identity; `control_worker`, which
  owns its thread and reads the monotonic clock for its deadlines; and
  `replay`, which reads and writes only the streams it is handed; and the
  two windows `screen_app` and `window`, whose `Window::new` reads the
  embedded face and whose loop is `client::run`. Even they read no environment
  variable, taking the display values and the socket path as explicit
  arguments.
- `control`, `driven` and `screen` are pure: the frame, envelope and
  codecs touch no descriptor, the seam reads only the composition it is
  handed and the embedded face, and the screen is a grid in memory that
  emits draws.
  The decoder allocates at most one frame, after validating its length,
  and `frame` checks the ceiling before allocating its output; the
  envelope hands fields on as a bounded iterator; the codecs and `ok`
  allocate in proportion to the caller's own input, which the caller
  budgets (hex-encoding a page allocates twice its bytes before `frame`
  judges the reply). The worker knows the
  consumer only as `R: Parse`; it holds no consumer state, and the one
  parse site is `R::parse`. Whether a parsed request reads or acts is the
  consumer's decision at dispatch, never the transport's.
- The transport: `endpoint` prefers `WAYLAND_SOCKET`, which `connect`
  duplicates close-on-exec rather than adopting, so the borrowed original
  stays open until its owner closes it and the consumer must give the
  transport exclusive use of the stream, whose timeouts are shared;
  otherwise an absolute `WAYLAND_DISPLAY` needs no `XDG_RUNTIME_DIR`, a
  relative one (default `wayland-0`) is joined to an absolute
  `XDG_RUNTIME_DIR`, and an invalid explicit value fails without trying
  another display. `backing_file` creates a pool file with `create_new` and
  mode 0600 in the directory the consumer names, under a checked
  process-local serial with 64 collision attempts, unlinks it at once, and
  hands back the owned `File` through which alone it is sized and written;
  an unlink failure reports the residual name. Every received right is owned
  at once in the eight-right FIFO, independent of message boundaries, and
  overflow, parse failure, protocol failure and disconnect close every
  retained right. `connect` gives a path connection attempt one bounded
  worker under `CONNECT_DEADLINE` and drops any late result; each outgoing
  message has one absolute `WRITE_DEADLINE`, including retries after
  temporary backpressure, capped by the remaining startup deadline while the
  consumer holds one, and reads use the same remaining startup budget; a 16
  KiB read feeds a 128 KiB pending-byte budget and an eight-right FIFO; the
  idle reader waits `IDLE_WAIT` through a socket timeout, or elapsed-time
  backoff for an inherited nonblocking socket whose shared status flags it
  never changes, and a consumer's `set_wait` shortens that wait.
- The client's table is dense: the fixed ids 1 through 9 are published in
  order before any dynamic id from 10, a slot is reused only after
  `delete_id` (for a consumer's object, only when its tag says it is
  retired), and exhaustion is a diagnostic. A consumer allocates and re-tags
  only its own objects: the client's slots and free ones refuse `set_tag`,
  so no consumer can free, fix or double-allocate an id. At most 128 live
  registry entries of at most 256 bytes each; an invalid event, an
  unexpected `delete_id`, a protocol error, or the removal of a required
  global the consumer does not forget ends the connection with a diagnostic.
  At most 256 events are dispatched before the turn ends and draws again.
  The first buffer must be submitted within 20 seconds of the registry
  request; after it a hidden surface may wait indefinitely for its frame
  callback. `present` refuses a zero axis, an axis past the raster's 8192
  ceiling or a frame over 32 MiB before any request. At most three backing
  files stay live; a submitted buffer is immutable until
  `wl_buffer.release`, a frame callback does not release it, a free buffer
  of the frame's pixel extent is preferred (the extent alone sizes the pool,
  so a consumer's other layout changes reuse it), a free one of another
  extent is destroyed and replaced, and while all three are busy nothing is
  painted. The pointer image is one immutable 1536-byte ARGB8888 pool built
  on the first `show_cursor` after ARGB is advertised, its role set before
  its first attach; later calls only re-send `set_cursor` with the new
  serial, relying on core wl_surface content staying attached across pointer
  leave and unmapping (enter makes the pointer-image association undefined,
  not the cursor surface's committed contents). `run` starts every turn from
  the idle wait, so what the consumer's `end_turn` sets decides each turn's
  wait; an event whose right has not arrived is parked in wire order,
  capping the wait by its remaining write deadline, and the consumer's
  `descriptor_wait` is told each turn it waits.
- The seat is bound on the initial roundtrip, after the toplevel: the lowest
  global offering `wl_seat` v5 or newer, at the lesser of its version and 7,
  and marked required; without one the client is still bound and the
  consumer decides. The keyboard and pointer are created only after their
  capability bits, the pointer first, and released when a bit clears; a seat
  name over `NAME_BYTES` and an unknown seat event end the connection. A
  released device's events are schema-checked and drained until `delete_id`,
  a keymap's right dropped unread, and its replacement is a fresh id.
  Removal of the bound seat's global releases the keyboard, the pointer and
  the seat, forgets the global and is reported as `SeatRemoved`, not as a
  fatal required removal. Keymaps are the client's one right consumer
  (`UNSAFE.md` §19): format 1 only, a regular file covering the advertised
  1..=1 MiB extent read positionally at zero, so the compositor's shared
  offset never moves, UTF-8 with one trailing NUL, compiled whole by
  `keyboard` before any press translates (the compiler's own `parse` takes
  the NUL as optional); a refused map leaves none, and every map cancels
  repeat and awaits a modifier snapshot. Keyboard enter and leave must name
  the surface and are the repeat policy's focus gain, with the held keys,
  and loss; the modifiers event is its snapshot, and the first after a map
  and focus is `Ready`, from which presses translate; the timing event is
  its rate and delay, at the consumer's clock. The pointer's events are
  decoded by `pointer`, which refuses unknown opcodes, truncated or trailing
  payloads, invalid axis and source numbers and invalid button states; its
  enter must name the surface, is remembered as `entered` until leave, and
  shows the pointer image at its serial, or on ARGB's arrival while inside;
  the image is built once, and later enters only re-send `set_cursor`.
- The clipboard: one seat-bound data device over core v3, bound after the
  seat at the v3 cap and not required, only when the initial registry offers
  both, its manager and device taking the two ids after the seat's; a
  manager advertised later is not bound. Offers are the server's ids at or
  above 0xff000000, at most `OFFER_LIMIT` retained, live and retired
  together, and a live id never reoffered, either an error; each offer's
  first `ANNOUNCEMENTS` MIMEs are inspected and later ones drained and
  ignored, of which the two text spellings, in any ASCII case and up to
  `MIME_BYTES`, are kept as announced, the explicit UTF-8 one preferred and
  no other encoding or parameter form considered. The selection names a live
  offer or nothing and retires every other; keyboard leave, keyboard loss
  and the device's release retire every offer and clear the selection; a
  drag's offer is retired without accepting, finishing or selecting it, a
  drag naming the selection's offer is an error, and a retired device's
  offers are retired at once. A retired offer is destroyed once and its id
  dropped behind one outstanding `wl_display.sync`, a batch of retirements
  sharing one, by id and generation, so an id reused before the callback
  survives it; retirements while one is outstanding coalesce behind the
  next. `offer_selection` creates a source advertising `UTF8` then `PLAIN`
  and sets the selection at the consumer's serial, destroying the previous
  source; a send on the live source for either MIME, in any ASCII case, pops
  its right and hands it on as `Send`, any other send (a retired source's,
  another MIME's) pops and drops exactly its right, and the compositor's
  cancel destroys the live source and is `Cancelled`. Removal of the
  manager's global releases the source and device and is `Released`; seat
  removal retires the offers with the keyboard, then releases the pointer,
  the source and the device, then the seat; the manager has no destructor
  and stays inert, and a retired device's events are schema-checked and
  drained until `delete_id`. Every event schema is checked whole before a
  right is waited for or consumed.
- The repeat policy (`repeat`): held keys up to the held-set budget;
  focus gain installs its held keys without typing or arming repeat, and
  presses wait for the modifier snapshot that follows; duplicate presses
  and unmatched releases are ignored; focus loss clears the held keys,
  the modifiers and the repeat; a modifier change and any new press
  cancel the old repeat, a release only its matching one; a map change
  cancels repeat and requires a new snapshot; a negative rate or delay is
  malformed and a zero rate disables repeat; rates above 1000 Hz clamp,
  intervals round upward to whole milliseconds, and a new positive rate
  and delay retime the current repeat from the clock they arrive with; at
  most one repetition fires per `repeat` call, missed repetitions are
  dropped, never burst after a stall, and timer arithmetic is checked,
  with exhaustion disarming the repeat.
- The raster writes only inside the validated surface and each draw's
  clip; row padding and bytes beyond the frame are untouched, and every
  refusal precedes every write. Medium-weight fringe colours derive from
  the explicit background a caller paints, never from buffer bytes.
- The pixel backend is a seam. Every widget and scene emits a semantic
  draw stream, `Fill` rectangles and `Glyph` scalars in a `GlyphStyle`,
  into a sink; none reads or writes buffer pixels. A `Glyph` names a
  Unicode scalar and a style, never a bitmap, so the glyph source is the
  font mount's alone. `Raster` executes that stream into a software XRGB
  buffer today; because the stream, not the buffer, is the toolkit's
  contract, a backend that keeps the cell model is a swap below this seam,
  executing the same draws at the same scalar positions: a GPU or
  glyph-atlas executor, or antialiasing that shades a glyph's coverage
  within its cell. Only proportional or variable-advance layout, which
  moves the scalar positions the widgets emit, is a remodel above the
  seam. td-editor/DESIGN.md owns the reference backend's operation set and
  lists the version-1 exclusions, antialiasing among them. Draw-stream
  oracles are the portable contract every backend keeps; the exact-pixel
  oracles and the `--preview` checksum pin the current software backend.
- Every budget carries over from td-editor unchanged: 1 MiB keymaps, the
  parser's token, depth, keycode, type, virtual-modifier, level,
  interpretation and modifier-map ceilings, and a 768-key held set.
- Production code has no `unwrap`, `expect`, panics or panicking indexing;
  invalid input returns a diagnostic or error naming the item.
- `unsafe` is confined to `sys`, the transport's raw module, under
  `UNSAFE.md` §19: two function-scoped allowances, one syscall instruction
  carrying `recvmsg`, `sendmsg` and `fcntl` pinned to `F_DUPFD_CLOEXEC`,
  one descriptor adoption site, and a crate root that denies it. Only
  `wayland` names the module. Reusing it does not transfer authorization
  to a new consumer, which gets its own roster entry.
- The shared font and wire sources are mounted here by exact repository
  path and nowhere else among td-ui's consumers. A future move of their
  canonical home updates staging, check mappings and every consumer
  atomically, as td-editor/DESIGN.md already requires.
- The text entry's masked mode is a plain rendering option, not a trust
  boundary. It draws a fixed mask glyph for each of the field's
  characters instead of the characters, so a consumer can collect a PIN
  or passphrase; the widget asserts nothing about whether the field is
  trusted, and td-ui does not gate its use. Anti-spoofing is the
  consumer's: a field collecting a secret must be presented only within
  the compositor's secure-attention and trusted-input path, which
  `td-install/ENCRYPTION.md` and Principle 7 require. td-ui provides the
  primitive; the consumer verifies it is used appropriately.

## Test contract

`tests/keyboard.rs` and `tests/xkb.rs` are td-editor's suites moved intact,
with the `tests/fixtures` directory: the libxkbcommon-generated `us.xkb`
map, the independent type and key oracles, and the retained
`XKB-COPYING`. `tests/fixtures/README.md` records their provenance and
reproduction. td-editor's in-file window tests read the same fixture by
relative path rather than carrying a copy.

`tests/raster.rs` holds the pixel oracles moved from td-editor's render
suite (fills and glyphs at every scale and weight against per-pixel
references), the validation order, a literal surface's ceilings, a
composition for another surface refused unpainted, and scrollbar
proportions along both axes. td-editor's render suite keeps the
scene-level oracles and the `--preview` checksum, which is byte-identical
across the move.

`tests/chrome.rs` holds the band oracles, draw-stream checks that read each
band's fills and glyphs: the menu bar's fill, its labels from cell
one and a header's cell extent; the panel's selection highlight, disabled
dim, check prefix and right-edge shortcut, and `step`'s wrap over the enabled
rows; the tab strip painting the active tab paper under a top and right
border with a dirty star, and an empty strip still filling its row with no
tab; the text block wrapping at its columns across a newline; and the status
row truncating a long line to an ellipsis in the last cell; and one
whole-surface pixel oracle rasterizing the status band to confirm its
border and fill land and the rows above stay untouched. The list adds
its own: the selection highlight, disabled dim, star mark and
right-aligned column, the whole rect painted chrome behind the rows, a
selection off the window drawing no highlight, `reveal`'s least-move
window, the scrollbar thumb tracking it and a disabled bar's border
thumb, `hit` mapping a point to a row, `new` refusing a rect the surface
or the gutter cannot hold, all at more than one scale, and a
whole-surface pixel oracle for its selection, its scrollbar and the
pixels around it left untouched. The text entry adds its own: the paper
ground, the one-pixel caret after the text, the mask glyph shown in
place of each character, a selection filled and its ink flipped focused
and unfocused, a dim placeholder only when empty, a scrolled window from
a non-zero first with the caret and selection shifted, `reveal`'s
least-move window keeping the caret at the inset without a needless
scroll, `hit`'s point-to-column clamped to the shown columns, `new`'s
refusals, a stale first that cannot panic, the geometry, caret and `hit`
at a second scale and an offset, and pixel oracles that show the
selection ground focused and unfocused with the ink over it, a clean
caret column, the mask glyph rasterized to exactly the bullet, and the
pixels around the field left untouched. td-editor keeps its scene-level
render, ui and menu oracles.

`src/sys.rs` and `src/wayland.rs` carry the kernel tests moved from
td-editor's adapter: close-on-exec duplication of an inherited stream,
owned and closed received rights, the ancillary walk past unknown records
and invalid entries, kernel truncation, byte-only EOF, the eight-right
FIFO budget with disconnect closing every owner, the idle wait on an
inherited nonblocking socket without touching its shared flags, write
backpressure under the startup deadline, environment precedence, and the
peer reader draining requests with their rights from a pool file that is
private, unlinked and exactly sized.

`tests/client.rs` drives the client with the smallest consumer, a probe that
fills one colour behind a marker pixel: the fixed ids and budgets by value;
the fixed ids in order and a pool crossing the socket private, unlinked and
pixel-exact; the frame callback not releasing a buffer and three busy
buffers bounding a resize storm; a release before done still waiting and a
matching buffer reused; invalid events, a protocol error, ids waiting for
`delete_id` and an oversize surface refused before any request; missing,
low-version, removed and excessive globals named; registry lookups binding
the lowest global and removal reporting whether it was required; a
consumer's objects handed back untouched and retired through their tag;
pings serviced while a frame waits and close stopping presentation; a later
free matching buffer preferred; presentation waiting for configure, XRGB and
the callback; a hidden surface without a callback deadline; the pointer
image's one pool, serial reuse and late arrival on ARGB, with decoded
motion, buttons, axes and frames handed on and another surface refused; the
seat bound from the lowest v5 global capped at v7 after the toplevel, its
devices created in order once and a name over budget refused; a keymap
crossing the socket and presses translating only after focus and the
snapshot, with arming, repeat, the timing retime and leave clearing
everything; a refused map disabling input, an unsupported format dropping
its right unread and every keyboard schema refusal; capability loss
releasing a device, retired devices drained until `delete_id` and a lost
keyboard's in-flight keymap dropped; seat removal releasing both devices and
the seat and forgetting the global; the data device bound after the seat at
v3 or not at all; offers budgeted, selected and retired behind one barrier
by generation; focus loss, drags and a retired device retiring offers and
the selection; a source offering both text MIMEs, its sends handed on or
dropped with their rights, a malformed send leaving the FIFO alone, and its
retirement on cancel or replacement; manager and seat removal releasing the
source and device in order; the complete loop on a thread accepting split
events and closing cleanly; and the loop parking an event until its right
arrives, in wire order, with the idle wait restored. Sixteen of these moved
from td-editor's window tests (the presentation nine, the pointer image, the
seat binding, the keymap reader's shared-offset and bad-source oracle, the
last a unit test beside `read_keymap`, and the clipboard lifecycle four:
offer retirement and reuse, coalesced barriers, drag offers with the
malformed-send FIFO oracle, and offer budgets with a retired device); the
editor keeps its transfer, request and control coverage and its reactions to
the client's device and clipboard outcomes.

`tests/confinement.rs` pins the source inventory, the exact three shared
source mounts, the absence of ambient I/O in pure modules, that `notices` is
three embedded texts and nothing else, the absence of `include!`, `cfg_attr`
and any dependency declaration, that the shared sources bind no input
interface, and the raw layer: the complete fingerprint of `sys.rs`, its
syscall and flag values, its two function-only allowances, the single
instruction and adoption sites, that the crate root denies `unsafe` and
declares the module private, that `wayland` is its only caller, through
exactly four wrapper calls, that no production module calls the client's
test support (`unconfigure`, `input_mut`), and that the client is the
toolkit's one consumer of a received right, through the pinned keymap reader
with its format, size and regular-file checks and the send that hands its
right on.

`control`'s in-file tests are td-editor's framing tests moved: every frame
split and single-byte delivery, truncation, zero/oversized/trailing frames,
the poisoned decoder, the envelope grammar with ID recovery and the lift
into a consumer's error type, the scalar and hex codecs, both response
lines, and arbitrary bytes as headers and as framed payloads.
`control_socket`'s kernel tests moved intact: they connect through the
requested pathname, exchange bounded bytes, check nonblocking and
close-on-exec flags and mode/owner, preserve every existing endpoint class,
reject symlinked/unowned/nonprivate/untrusted ancestors, exercise
replacement-name and renamed-parent cleanup, and prove through a
deterministic publication hook that a replaced visible parent refuses and
only the pinned candidate is cleaned. They create 0700 fixtures under
`/tmp`; the crate's `trusted-test-root` declaration gives them a private
caller-owned root inside the gate's namespace, where the unmapped host root
would otherwise refuse publication. These are transport-ownership tests, not
a consumer's endpoint or an adversarial same-UID race proof.
`control_worker`'s tests moved with a toy `Parse` type in place of the
editor's requests: deterministic connection tests inject time and cover
bytewise input, complete and truncated frames, refusal IDs, late extra
requests, full queues, abandoned jobs, invalid response sizes, and expiry
while reading, awaiting the consumer or blocked on output, with a separate
pre-output expiry test independent of socket-buffer tuning, plus large
draining replies, retry classification, idle pacing, liveness before
dispatch and explicit thread-error propagation; real Unix-socket tests cover
a framed round trip, admission backpressure with continued client progress,
shutdown cancellation and owned endpoint removal. `replay`'s tests drive the
runner bytewise through consecutive frames, EOF between frames, partial
headers, bad lengths, short bodies and a refused reply.

`tests/driven.rs` drives the seam end to end through a toy consumer, a counter
with a four-row action table, a two-line composition and one verb of its own:
the table's grammar (each refusal of `check` by message, `bound`, the aligned
`help`), every generic verb and its refusals in the envelope (the consumer's own
codes among them, and a code of its own marking whatever reached it, so the
router's refusals are told from the consumer's), the text read back with an
overpainted cell, a wholly clipped glyph, a fill over a row and over one cell, a
surface smaller than the text, one narrower than a cell and one taller than the
text, the frame digest stable across identical states and moved by a change, its
pages reassembling the RGB rows exactly and the largest page framing under the
ceiling, the whole seam behind the replay runner, and `Payload` behind a live
worker, a malformed frame refused on the worker's thread and the rest answered
on the turn.

`tests/screen.rs` holds the cell screen's oracles: the grid laid out over a
surface with its remainder as ground, writes clipped at the right edge, a
control scalar replaced, rows cleared from a column, a resize relaying out
and clearing, hits inside the grid only, a surface under a cell on either
axis giving no cells, and a row read back with its blank cells trimmed
and a space-like scalar a program wrote kept; the draw stream as the
ground fill first, one fill per run of cells sharing a background other
than the ground and one glyph per non-blank cell, clipped to the damage
by row and by column with the pixels of a culled paint equal to a whole
one, at scale one and two; a whole-surface pixel oracle through the
raster against the face's own bitmap; the driven seam's `text` reading
the grid back; and the chord translation, every prefix and named key,
the bare space and its named spelling, `plain_char` refusing every
modifier, and the refused forms including the function keys' non-digit
and zero-led spellings. `tests/screen_app.rs` drives the window against
a scripted peer: the title, app id and commit closing the binding and
the default grid delivered with it; configure laying the grid out, a
zero axis keeping the extent, an extent the raster refuses kept out,
reported and delivering nothing, and the extent the grid has keeping
the cells and delivering nothing; the frame presented once dirty with
the rendered cells and the ground in its pool, nothing re-rendered while
clean, a redraw over the released buffer, and a paint with every buffer
busy left dirty until one is released; the title re-sent with a frame
only when it changed; presses translated after the keymap, focus and
snapshot, repeat delivered at the turn's clock under the repeat's wait
and not after the window closed, a chord with a modifier, a bare
modifier typing nothing, focus following enter and leave and a handler
quitting on a press; clicks landing on the cell under the pointer in
24.8 fixed point only while inside, and wheel frames in cells; every
turn polling under the wait capped at the client's idle wait and floored
at one millisecond, a closed window not polling, and a lost keyboard
capability as focus loss once and only for a window that had focus; and
the whole loop over a socket to a close request.

`tests/window.rs` drives the widget window against the same scripted
peer: the binding delivering the default surface, configure laying the
surface out, keeping it through a zero axis, a refused extent and a
repeated extent, and the close request; the frame presented from the
handler's paint into a raster over the surface, clean until a redraw,
the buffer reused once released, every buffer busy asking for no paint
and leaving it dirty, and the title sent when it changed, ahead of the
frame when one follows and alone when none can; presses arriving as the
keymap's chords, plain and with modifiers, marked when the repeat clock
made them and not after the window closed, a bare modifier nothing,
focus following the keyboard and a handler quitting on a chord; the left
button's press at the pointer in surface pixels rounded down, motion
while held a drag signed past the edge, a second press while held
nothing, the release where the pointer is, motion without the button, a
stray release and the right button nothing, leaving or losing the
pointer while held a cancel with no release after it, a handler quitting
on that cancel hearing nothing of the resize that made it nor of
anything after, and Shift under a focused synchronized keyboard
extending the press; wheel frames in cells; the poll and wait each turn,
the keyboard capability's loss as focus loss once; and the whole loop
over a socket.

`tests/confinement.rs` holds `screen.rs` in the pure set and
`screen_app.rs` and `window.rs` among the adapters, adds `control.rs`
and `driven.rs` to the pure set and the three adapters to the inventory,
and carries td-editor's pins over the moved modules: the socket's open
flags, path and identity constants, procfs reads, mode and identity
checks, and the absence of `connect`, environment reads,
canonicalization, a fixed overflow number and the trusted-root variable;
the worker's thread name, its slot, buffer and deadline constants, its
bounded channels and nonblocking operations, `R::parse` as its sole
parse site, and the absence of blocking reads, writes and receives; and
the runner's ceiling check, its two exact reads and one framed reply.
The socket's `/proc/sys/kernel/overflowuid` literal is excluded from the
raw-module identifier count by name, as td-editor excluded it. The
consumer-level conformance, td-editor's request grammar, refusal parity
between socket and replay, and its window's two-jobs-per-turn polling,
stays in td-editor's suites and confinement tests.

The builder discovers the crate by existing. Its gate runs `cargo test` and
all-target Clippy; a change under `td-ui/` selects td-editor's tests through
the reader graph, because td-editor's manifest names the crate.

## Cell screen

A program written for a terminal draws in rows and columns of styled
characters. The cell screen keeps that shape so such a program moves to a
Wayland window without re-laying its views out over pixels: `Screen` is
a grid of `Cell`s, each a scalar and a `Style` (ink, background, weight),
laid out over a `Surface` at its scale as `width / (CELL_WIDTH * scale)`
columns by `height / (CELL_HEIGHT * scale)` rows, and no cells at all when
either axis is under a cell, so a program never sees rows it cannot lay a
column in; the surface's own frame ceiling bounds the grid. The remainder
of the surface beyond the grid is painted in the ground, the background
of the style the grid was last cleared to with `clear`; `clear_row`
paints its cells and leaves the ground, so a status row does not
recolour the surface around the grid.
`put` and `write` clip at the right edge and replace a control scalar with
U+FFFD, so every cell holds something the face can draw; `line` reads a
row back with its trailing blanks trimmed; `hit` maps a surface pixel to
the cell under it, none for the remainder.

As a `Composition` the screen emits, inside the damage it is asked for,
one fill of the ground over the whole surface, then per row one fill for
each run of adjacent cells whose background is not the ground and one
glyph for each cell that is not a blank, every draw clipped to the
damage. A frame is therefore one fill plus a fill per highlighted run plus
a glyph per visible character, never a fill per cell. The raster paints
only a glyph's lit pixels, so the screen guarantees a fill under every
glyph, the ground's or its run's; a stream that dropped the single-cell
run fill would leave stale pixels in a reused buffer. The driven seam's
`text` reads the grid back, which is how a screen program's tests and
its agent driving see it, up to that seam's own trimming: it drops
trailing whitespace of any kind and trailing blank rows, where `line`
drops trailing spaces alone.

The vocabulary a screen program reads is `Input`: a `Key` press with its
`control`, `alt` and `shift` modifiers, a `Click` on a cell, `Wheel`
travel in rows and columns, a `Resize` to the grid's rows and columns,
`Focus` and `Close`. `press` translates the keyboard's chords
(`keyboard.rs`: optional `C-`, `M-`, `S-` prefixes in that order, then one
printable ASCII scalar, the bare space an unmodified space bar is spelled
as, `Space` as it is spelled under a modifier, a named key or `F1`
through `F12` in digits with none leading) into a `Press`, and refuses
anything else, including a chord longer than the seam's `KEY_BYTES`; a
refused chord types nothing, as a terminal drops a sequence it cannot
name, and the window reports it through `notice`.

`screen_app` is the window over the client for such a program. Its
`Handler` names the toplevel, gives the ground style, reads each `Input`,
is polled every turn with the loop's clock, says how long the loop may
wait, says when the screen must be painted again, and paints it whole.
The window owns the client, the screen, the pinned face and the pointer's
position; the handler owns everything else, and stays the caller's
whichever way the loop ends, so a program reads its own state, its
restore failures and its exit code back after `run` returns. On `Bound`
the window sets the title and app id, commits, and hands the handler the
`Resize` of the grid it was laid out for, so a render is never the
handler's first word of an extent; on `Configure` it lays the grid out
for the extent (a zero axis keeps the current one; an extent the raster
refuses keeps the last grid, is reported through `notice` and delivers
nothing; the extent the grid already has, which compositors send for
activation and tiling changes, keeps the cells and delivers nothing),
hands the handler the new `Resize` and acknowledges; the close request
reaches the handler as `Close` and closes the window, as does a
`Flow::Quit` from any input or poll. The title is read again after every
render and sent with the frame when it changed, so a handler retitles
the window from its state. Presses are translated with `press` and
armed for repeat, and an idle turn of a window still open delivers the
repeat under the client's wait.
A left button press while the pointer is inside is a `Click` on the cell
under it; the wheel accumulates axis events per frame through
`pointer::Wheel` and each non-empty frame is one `Wheel`. Keyboard focus
is delivered as a change: `Focus(true)` on enter, `Focus(false)` on
leave, on losing the keyboard capability or the seat while focused, and
never for a keyboard the window did not have. The vocabulary is the two
terminal programs': one button, no release, drag, double click or
modifier on a click, and no clipboard; the client's selection events are
not delivered. Each is a later increment, added to `Input` when a
program needs it.

Each turn ends with the handler's `poll`, and the wait until the next is
the least of the client's own wait (at most `wayland::IDLE_WAIT`, shorter
under an armed repeat) and what the handler asks, floored at one
millisecond: a channel the handler reads from a fetch thread is never
left longer than the idle wait, a handler with a timer due sooner names
it, and one that asks for nothing is polled a thousand times a second.
The screen is presented when the handler says it needs a redraw or the
window's own state changed, once a frame can be presented; a frame
refused by the client (every buffer busy) leaves the window dirty, and
the handler paints again next turn, so `render` is a paint and not a
frame and must be repeatable.

## Widget window

A program that lays the toolkit's widgets out over its surface, td-news
and td-mail with their lists and td-editor's document pane, has no grid
to draw in: it paints compositions into a raster over the surface and
reads its input in surface pixels. `window` is the window over the
client for such a program, the cell screen's loop with the grid taken
out. Its `Handler` names the toplevel and its title, reads each `Input`,
is polled every turn with the loop's clock, says how long the loop may
wait, says when the surface must be painted again, and paints it whole
through `paint`, which receives a `Raster` over the frame's buffer and
the `Surface` it is laid out for; a paint is not a frame and must be
repeatable, as the screen window's render is. The window owns the
client, the pinned face, the pointer's position and whether the left
button is held; the handler owns everything else, and stays the
caller's whichever way the loop ends.

The vocabulary is `Input`: `Key` with the chord as the keymap spells it
(`a`, `C-x`, `S-Right`), so a handler translates it itself or hands it
to td-editor's controller unchanged, and `repeat` for a delivery the
repeat clock made; `Pointer` with the driven seam's `PointerPhase` (`Press`,
`Move`, `Release`), the position in the surface's physical pixels, signed so
travel past an edge is representable, and `extend`, Shift held at the
press as the keyboard's synchronized modifier state reports it while
the window has focus; `CancelPointer` when the pointer leaves or the
device goes while the button is held, so the handler ends a drag
without a release; `Wheel` in cell rows and columns as `pointer::Wheel`
accumulates a frame; `Resize` with the `Surface` the handler lays out
for; `Focus` and `Close`. Motion is delivered only while the button is
held, so a handler without drags sees no motion stream; a second press
while held, a release without a press and every other button are
nothing.

Binding, configure, the close request, focus, repeat, the poll and the
wait are the screen window's, and `Flow::Quit` is but for what may
follow it: on `Bound` the window sets the title and app id, commits, and
hands the handler the default `Resize`; on `Configure` it lays the
surface out for the extent (a zero axis keeps the current one; an extent
the raster refuses keeps the last surface, is reported through `notice`
and delivers nothing; the extent the surface has delivers nothing),
cancels a held button, hands the handler the new `Resize` and
acknowledges. A handler that answers `Flow::Quit` hears nothing more:
the window is closed, and the inputs that would have followed, a resize
after the cancel it quit on or the focus loss after a seat's removal,
are not delivered, where the screen window still delivers them. The
surface is presented when the handler says it needs a redraw or the
window's own state changed, once a frame can be presented, into a raster
over the pool file at the surface's own stride; the handler paints only
into a buffer, so a frame refused by the client (every buffer busy) asks
for no paint and leaves the window dirty until one is back. A paint that
fails ends the loop with its error, as a program that cannot paint its
surface has nothing to show; the refusals the loop absorbs are the
extent's and the frame's. The title is read before every attempted
present, which is when the window is dirty and a frame can be presented,
and sent then when it changed, ahead of the frame's commit when a frame
follows and alone when none can; a retitle made while painting rides the
next attempt. The client's selection events are not delivered: the
clipboard is a later increment, for the host that composes.

## Shared action button

`chrome::Button` paints a bordered paper action with selected and disabled
styling, using the shared text and palette: a `BORDER` bezel one scaled
pixel wide around a `PAPER` face, `SELECTED` with `PAPER` ink when
selected, `DISABLED` ink when disabled, the text a cell in and centred in
the height (four pixels down in a `ROW`-tall button). It accepts a
nonempty rectangle fully inside the surface; its hit test uses that exact
rectangle. Rendering clips both text and borders to damage and allocates
nothing. The consumer owns focus, enabled-state hit policy and matching
press/release activation, as with the other chrome geometry primitives.
Scale 1-4 pixel tests cover bounds, focus/disabled colors, the centring
and partial repaint equivalence.

`chrome::Buttons` is a strip of them on one `ROW`-tall band at a `y` the
consumer chooses: the buttons from cell one, each its label's cells and a
cell each side (so the first label starts at cell two), one cell between,
inset `BUTTON_MARGIN` (2) pixels above and below so two strips stacked
keep their bezels apart; the band fills `CHROME` behind them. A button the
surface cannot hold whole is neither painted nor a target, nor is one
whose geometry leaves the integer range; the layout is one pass over the
labels. `emit` takes each button's `(selected, enabled)` in label order, a
state it runs out of painting an enabled unselected button; `hit` answers
the button whose own pixels hold the point, the gap and the margin none.
A consumer that wants one selected at a time (a mode or a filter strip)
selects one; the strip itself imposes nothing. Its test pins the
geometry, the hit rule, the draw stream and the pixels at scales 1-4, a
descender's lowest row inside the bezel, partial repaint equivalence,
damage off the band painting nothing, a clipped last button, a band at
the surface's foot and past the integer range, and an empty strip.

## Shared menu controller

`menus::Model` is an immutable, caller-revisioned tree of `Node` values.
Each node carries a bounded `chrome::Row` and either a typed action ID
or a submenu. Parent indices precede children; only submenus have
children, branches cannot be empty, and bar roots are labelled submenus.
Context roots are the first panel's rows. `Model::storage_bytes` exposes
owned capacities for consumer budgets; borrowed labels remain their owner's
responsibility. Construction refuses invalid
trees, more than 256 entries, more than eight open panels,
empty/control-bearing labels, labels over 256 bytes or shortcuts over 64
bytes. Allocation is fallible; event handling and painting allocate no
collections.

`menus::Controller` owns the open path, enabled selection, scrolling,
placement, keyboard and pointer navigation, and dismissal. Consumers
feed explicit events and the current model revision and receive typed
outcomes; the toolkit never executes an action. Right/Activate opens a
submenu, Left/Escape closes one level, Up/Down wraps among enabled rows,
and Dismiss closes the path. Root Left/Right switches enabled bar
headers; a lone enabled header keeps its selection. Disabled header
presses and zero wheel deltas do nothing. Pointer movement into a child
retains its ancestors. An action closes the menu before emitting its ID;
release and key repeat cannot activate it again. Outside presses dismiss
and are consumed. Focus loss or resize closes all panels. A mismatched
or absent revision invalidates the menu without an action; replacing
data requires a new controller. Opening a closed menu is an explicit
consumer action, except for a press on an enabled bar header.

An open or switched panel that cannot fit returns `NoRoom`; the consumer
can show an enlargement notice. `Other` consumes unhandled input without
an action, subject to the same revision check.

`Fit::Adaptive` places children rightward, then leftward, within the
surface without overlapping any visible ancestor. When neither side
fits, it replaces the parent with a child and an actionable Back row.
Panels show at most 13 rows, scrolling within the available height, and
keyboard selection reveals its row. Scrolling clears a selection that
becomes hidden, so Activate cannot choose an unseen row. Width shrinks
to the surface; fewer than three text cells or no available content row
refuses with `NoRoom`. `chrome::Panel::within` validates the complete
rectangle and whole rows, sharing panel rendering and hit geometry.
`Fit::Complete` preserves document-menu admission: a root has the
existing 320-scaled-pixel width and every row above one status row; a
too-small surface refuses without clipped actions. A fully disabled
complete root retains its first-row highlight for editor compatibility,
but cannot activate. Submenus still adapt, including wheel scrolling.
`panel` and `row_rect` expose only visible levels. Bars remain part of
the consumer's chrome; the controller emits semantic panel draws.

The editor uses this controller for its complete menus. Its immutable
data captures application availability, checks, shortcuts and
tab/revision/key profile. Its adapter owns document cancellation policy
and executes typed items through existing commands; it has no private
menu navigation state. `tests/menus.rs` covers tree and text bounds,
disabled navigation, nested pointer paths, left/replacement placement,
Back, scroll/reveal, revision invalidation, focus/resize, repeated input
and complete-mode compatibility. Draw-stream and pixel oracles preserve
the editor's existing panel output at scales one through four; its scene
and native input regressions remain.

## Shared confirmation dialog

`confirmations::Model` captures owned immutable request text, a typed
confirmation action and a caller revision. Construction validates before
copying and allocates fallibly: at most 256 detail entries, 4096 bytes per
entry and one MiB of detail text, plus nonempty title/action labels of at
most 256 bytes each. Control characters are refused. A source change or
drop after capture cannot change the request presented for confirmation.
`storage_bytes` on the model and controller exposes actual retained text,
container and wrapped-row capacities for consumer accounting, excluding
allocator bookkeeping. A consumer reserves its bound before construction
and reconciles actual capacity before admitting the widget.
The consumer owns any authority or descriptors behind the action ID.

`confirmations::Controller` composes a title panel, a scrolling detail list
and fixed Cancel/Confirm rows inside a fully visible rectangle. Details
wrap at scalar boundaries without loss; precomputed offsets into the
captured strings avoid borrowed self-references and allocation while
handling ordinary input or painting. Layout reserves at most 65,536
wrapped rows. Insufficient width, height, label space or wrapping capacity
refuses with `NoRoom`, without omitting an action or part of the request.
A valid layout shows the complete title and action labels, at least one
detail row, and both actions. Title and actions stay visible while details
scroll, separated from the fixed controls by visible rules. Resize
retains the selected detail and scroll anchor by entry and byte offset,
then reflows fallibly; a refusal closes with `Unavailable` and
never leaves an old invisible confirmation target active.

Focus starts on Cancel. Tab/BackTab cycle only through the detail list,
Cancel and Confirm. Up/Down, PageUp/PageDown and Home/End navigate details;
Activate acts only on the focused action. Escape cancels. Primary pointer
press arms an action and release on that same action chooses it; moving
away or any intervening keyboard input, including repeats, cancels the arm.
Pointer actions preserve the keyboard focus choice; abandoning a pointer
gesture cannot move the default keyboard action to Confirm. Blank detail
rows do not select content. Outside
input is consumed without closing or reaching underlying controls. Other
unhandled input is consumed. Key repeat never activates. Focus loss
cancels; resize cancels a pending gesture and returns focus to Cancel.
A missing or changed revision closes stale without confirmation.

Confirmation, cancellation, stale data and an unavailable resized layout
each produce one `Closed` outcome. Later events are ignored and a closed
dialog emits no draws. The controller captures an optional opaque prior
focus ID, exposed by `prior_focus()`; the consumer supplies whether that
exact control still exists. Event handling returns an outcome directly;
resize failures are represented by `Closed` with `Unavailable`.
A close returns that ID only when valid, for the adapter to restore focus.
The adapter routes input through the modal controller while it is open
and executes only the typed outcome; td-ui grants no process authority.
The adapter must not position Confirm beneath the pointer that opened the
dialog: a second click in a double-click is otherwise a fresh gesture.

`tests/confirmations.rs` pins default cancellation, focus confinement,
press/release pairing, duplicate/repeated input, outside input, stale data,
focus loss, resize refusal and focus restoration. It covers capture
independence, lossless Unicode wrapping and scrolling, the full one-MiB
request bound, malformed input and unusable geometry. Draw-stream and
pixel checks at scales one through four keep the controls within the
dialog, preserve pixels outside it and respect partial damage.

## Shared directory finder

`finder::Listing` is one folder as the consumer read it: its path as
shown, its `Entry` values in the consumer's order, and whether the read
stopped short. An entry is a name, a right-aligned meta, a `Kind` of
folder or file, whether it can be descended into or chosen, and whether
it carries the mark, the list's star prefix: set on the entry with
`with_marked` or on the shown listing by entry index with `set_marked`,
refused `NoEntry` for one not there; what a mark means, a multiple
selection in td-portal, is the consumer's, and the marks go with the
listing they were on. The consumer reads the filesystem under its own
bounds and trust, as td-portal's chooser and td-editor's directory tabs
do, and the widget reads nothing: at most 4096
entries, 1024-byte names, 16-byte metas, a 4096-byte path and one MiB of
name and meta text between the entries, all control-free, refused before
capture. `storage_bytes` on the listing and the controller exposes the
retained capacities for consumer accounting.

`finder::Controller` lays a path row, a filter `TextEntry`, the entries as
a `List` and a status row inside one rectangle: four `ROW`s and twenty
cells at least, the list taking the whole rows between, else `NoRoom`. It
owns the filter query, the shown indices, the selection and the scroll
window; construction, like `set_listing`, selects the entry it is given
by name when listed, else the first. The filter is the launcher's rule,
`filter` mounted from the compositor: `insert` takes ASCII folded to lower
case under `MAX_QUERY_BYTES`, and every whitespace-separated term is found
in the name, `matches`' rule with the name folded at the comparison so no
folded copy of each name is held; a change resets the selection to the
first shown.
Up/Down, PageUp/PageDown and Home/End move and reveal the selection and
honour repeats; a move that goes nowhere is consumed. Activate descends
into an enabled folder (`Descend` with the entry's index) and, when files
are what is chosen, chooses an enabled file; Accept chooses the listed
folder itself (`Here`) when folders are chosen, so a selection resting on
a subfolder cannot be taken for the folder in view, and the selected
enabled file otherwise; Parent, and Backspace on an empty filter, ascend
(`Ascend`); Escape cancels. A repeated Activate, Accept, Parent, Escape or
empty-filter Backspace is consumed. A press on a shown row selects it and
the wheel over the list moves the window, keeping the selection in it;
other pointer input on the finder is consumed, and pointer input off its
rectangle is ignored, the consumer's to act on (its bar, its own
controls). Descend and Ascend leave the widget as it
is: the consumer lists the folder and installs it with `set_listing`,
which clears the filter and the note and selects the entry it names (the
folder an ascent came from), or says why it could not with `set_note`,
shown in the status row until the next listing. Resize relays out and
keeps the selection shown; a layout that cannot hold the finder closes it
`Unavailable`. A choice, a cancel and an unavailable layout each produce
one `Closed` outcome; later events are ignored and a closed finder emits
no draws. The consumer owns physical key bindings, the double click it
turns into Activate, the prompt that tells the user what Return, Backspace
and Accept do (the finder paints no affordance for them), and whatever the
chosen path is used for; the widget grants no process authority.

`emit` paints chrome under the rectangle, the path's tail when it is
longer than the row, the field with its `Filter` placeholder, the list
with a folder's meta `folder` when the consumer gave none (a meta is left
out of a row that cannot keep `LABEL_COLUMNS` of the label beside it, so
a meta at its bound never hides the name), `No match` or `Empty folder`
in an empty list, and the status row under a rule: the entry count
(`0 entries` for an empty one), the match count over the total while
filtering, `cut short` for a truncated read whether or not anything was
listed, or the note. It allocates nothing, which `tests/confinement.rs`
holds at the source: no formatting, collecting or boxing in the module.

`tests/finder.rs` pins the listing bounds (the text bound measured as
text, not capacity), the filter rule, navigation and reveal, an empty
listing taking every key and press, the outcomes of choosing in either
mode and of a disabled entry, listing replacement selecting by name, the
note's life, the press and wheel paths on and off the finder, a meta left
out of a row too narrow for it, a long query keeping its caret in a
narrow field, the geometry refusals and resize, and draw-stream and pixel
oracles at scales one through four keeping every draw inside the
rectangle, naming each band's text with counts of several digits and
leaving the surface around it untouched.

## Shared time-series charts

`charts::Chart` is a borrowed, validated view over caller-owned `Time`
and `Series` slices. `State` owns only an optional semantic selection and
an armed pointer gesture, so a consumer needs no self-reference or copied
history. Construction, ordinary input and painting allocate no storage
and perform no I/O. The caller owns collection, units, aggregation,
series choice, downsampling and revision policy.

A view admits up to 1024 strictly increasing u64 timestamps and 16
nonempty, uniquely identified series. Each series has one optional u64
value per timestamp. Zero samples are allowed. Labels are nonempty,
control-free and at most 128 bytes. The caller supplies a nonzero maximum,
its display label and units, plus a nonzero display divisor and zero
through six decimal places. Displayed values round half up in a fixed
stack buffer, so percentages and byte units retain integer observations
without unreadable raw counters. Values above the maximum and overflowing or
above-maximum stacked sums are refused. Invalid data never becomes a
partial plot. The view must fit completely inside a valid surface and
show its labels, legend and a plot at least two label rows high; otherwise
`NoRoom`
allows the consumer to show its fallback.

Lines interpolate only between adjacent present observations. A stacked
column requires every component; a missing value makes a gap in that
column rather than an invented zero. Cumulative boundaries interpolate
with exact integer arithmetic and consistent rounding, keeping layer
order and the maximum intact even at u64 limits. Painting and hit testing
share these clipped columns. Fixed input bounds and `MAX_AXIS` bound
emission to at most 155648 draw commands (`DRAW_LIMIT`); input validation
and the maximum-size draw oracle pin the work bound.

Explicit observation markers preserve isolated readings whose timestamps
fall between pixel columns. Markers paint after the interpolated plot;
later timestamps and then later series paint last where their markers
overlap. Hits use the reverse order and return the visible marker's exact
timestamp and opaque series ID. Elsewhere a hit returns the nearest
supplied timestamp (earlier on a tie), with a series only inside a stacked
band or within three scaled pixels of a line. A zero-height band has no
hit. The widget never interprets an ID as a PID or invents an identity
for a gap. The consumer may use a typed Other series to open a ranked list.

Axes, units, all legend labels, selected time and the selected series'
value are textual. A single observation has one centered timestamp label.
Missing selections or values show Unavailable. With no observations,
legends remain informational and emit no selection because there is no
timestamp to select. A selected time has a contrasting outlined plot
marker; the selected legend and graph focus use contrasting text and
backgrounds. A time absent from the supplied observations has no marker.
Legend clicks select that series at the current valid time, or the newest
time when none is retained.
Previous/Next/First/Last time keys and Previous/Next/Clear series keys
provide pointer equivalents. The adapter routes keys only while the
graph has focus and supplies focus separately for painting.

A primary press captures the semantic target and caller revision;
matching release selects once. A different revision, layout or plot mode
disarms even if a resize event was omitted; stale motion/release returns
Stale. A fresh press or key still processes its own intent. Moving away,
focus loss, resize, other input and keys (including repeats) cancel the
arm. Previous/Next time keys honor repeats; other repeats are consumed.
A consumer
may set a selection explicitly; unavailable IDs/times are not silently
retargeted until a new navigation intent. An explicit Resize event also
disarms when the geometry is unchanged. Navigation from an absent time
starts at the newest observation before applying the requested step.
Idle motion and cancellation events return Ignored when no arm exists.
`cancel_gesture()` retires input without a view, including before a
relayout that may refuse with NoRoom; it preserves the selected time/ID.

`tests/charts.rs` covers line/stack paint-hit agreement, gaps, isolated and
coincident readings, timestamp and counter extremes, keyboard and pointer
selection, refresh and interrupted gestures, input maxima/refusals and
bounded drawing. Scale 1-4 pixel oracles preserve pixels outside the
chart and compare partial repaint with full repaint.

## Shared split pane

`split::Controller` partitions a fully visible rectangle horizontally or
vertically into two children and an eight-pixel logical divider. Config
supplies nonzero logical child minima, each bounded by `MAX_AXIS`;
Surface scale converts them once into device pixels. `Share` stores an
exact nonzero-denominator fraction for the preferred first-child extent.
Layout clamps to both minima without changing that preference, so a
temporary small window cannot permanently move the divider. A stationary
click/release leaves that exact preference intact, including while
clamped. A drag released back at its initial position restores the
captured preference too.

A valid `Layout` contains three disjoint rectangles covering the entire
input rectangle. If the extent cannot hold both minima and the divider,
`layout()` returns None, emits no draws and supplies no hit targets. An
empty rectangle contained in the surface uses this same fallback. The
consumer owns the fallback and retains its content state. Invalid
surface or out-of-surface resize returns an error after retiring old
geometry and pointer capture. No child receives overlapping or invisible
hit regions. Focus gained during fallback is retained when useful
geometry returns.

A primary press on the divider takes local pointer capture, preserving
the offset within the handle. Motion and release adjust the split even
outside the window, clamping safely. A focused divider accepts decrease,
increase, first-minimum and last-maximum keys; the consumer maps
physical keys for the chosen axis. Keys, resize and focus loss end a
drag. A release without capture does nothing. A new press outside the
divider retires old capture and returns Ignored so a child can handle
that press. Repeated focus assignment is consumed without claiming a
visual change. The preferred share is exposed for the application; the
widget neither persists it nor owns child content.

Painting emits only the divider and its grip, with a distinct focused
background, through the clipped semantic draw stream. Geometry, input
and painting allocate no storage and use no I/O. Twelve tests cover both
axes and scales 1-4, exact partition/minima, extreme pointer
coordinates, keyboard movement, capture cancellation, temporary
clamp/fallback restoration, stationary/returning clicks, invalid inputs
and focused/partial repaint pixel oracles on both axes that leave child
pixels untouched.

## Completed double clicks

`pointer::DoubleClick<I>` pairs completed semantic clicks on the same opaque
identity within 500 milliseconds and four logical pixels per axis. The
caller supplies monotonic nanoseconds and logical coordinates; the helper
reads no clock and allocates nothing. Backward time, excessive delay,
distance or a different identity starts a new candidate. A completed pair
is consumed, so a third click cannot reuse it. The caller cancels on other
input or geometry/focus changes and remains responsible for matching each
individual press/release against its captured target. Tests cover identity,
time, coordinate extremes, explicit cancellation and consumed pairs.

## Shared tree table

`tree_table::Model` captures a validated visible preorder over opaque
`Copy + Ord` row IDs. The consumer chooses the full hierarchy, filtering,
expansion and sibling ordering; each visible nonroot names its nearest
preceding ancestor, whose children must be expanded. Root rows have no
parent and depth zero. Duplicate IDs, depth jumps, revived old ancestors
and children beneath collapsed or leaf rows are refused. Synthetic roots
have ordinary opaque IDs; the widget gives them no process authority.

The model owns only row metadata, a sorted ID/index lookup and column
headings. It reserves fallibly, sorts without allocation and exports
capacity-based `storage_bytes()` including titles and inline model state,
not allocator bookkeeping. Limits are 32769 visible rows, accommodating
32768 processes plus a synthetic root, depth 256, 16 columns, 128-byte
nonempty control-free headings and 16 MiB of model
storage. Columns provide logical minimum/preferred widths, at most 8192;
minima hold their complete headings, insets, sort mark and border. The first minimum is at
least 32 pixels; consumers can widen it to reveal deeper indentation.

Cell values remain in the consumer. `Cell::new` validates control-free
text through 4096 bytes, with an explicit empty-cell fallback. `emit`
requests borrowed values only for rows and columns intersecting both the
visible viewport and damage, then streams clipped draws without allocation
or I/O. Returned text must outlive the complete emit call; a callback cannot
return a borrow into its mutable formatting scratch. Consumers prepare
numeric strings in a bounded visible cache before painting.
Numeric columns align right; the first column reserves 16 logical pixels
per hierarchy level and a disclosure slot, clipped to that column.

`Controller` owns the model and stable selection. A model replacement
preserves selection and the first visible anchor by ID when present
(subject to viewport clamping), clamps the old position when its anchor disappears, and clears selection
when its ID disappears. Replacement preserves widths when headings and numeric roles are unchanged,
clamping to new minima; a changed column schema uses its new preferences.
`width` and `set_width` let the consumer save and restore widths. Mutation,
resize, focus changes, scroll and keys retire pending pointer capture.
`captured()` lets a consumer defer refresh while a pointer gesture is active.
The consumer must replace the model when row membership, ordering or
expansion changes; editing values alone does not reinterpret row IDs.

Shared geometry moves headings and cells together during horizontal
scrolling. A vertical scrollbar has a reserved gutter; a horizontal one
appears only when columns exceed the viewport. Captured scrollbar motion
retains its grab offset and exact starting position, clamps beyond the
window and ends on release or cancellation. Wheel-style scroll events
carry rows and 16-logical-pixel horizontal steps. Invalid geometry returns
an error after retiring old draws and hits. Extents too small for a header
and one row yield no layout, leaving the consumer its fallback.

Pointer presses capture row IDs and a role: selection, disclosure or
column heading. Matching release emits that semantic intent once. Moving
away, refresh, focus loss and resize prevent a later release from naming
a new row at the old position. Moving away retains canceled capture until
release, consuming that release without an intent even outside the table.
Disclosure intents include the requested
expanded state; sort intents name a column, without applying policy.
Pointer presses give rows keyboard focus; clicking a sort heading keeps
that row focus. Reapplying the same focus preserves capture, while changing
focus retires it. Selected rows and keyboard-focused headings have
contrasting feedback.
The consumer provides an optional ascending/descending sort indicator.

The adapter routes focus to rows or a validated heading. Up/Down,
PageUp/PageDown and First/Last select and reveal rows. Up/Down without a
selection start at the first visible row; unchanged navigation is consumed
without repeating a selection intent. Left collapses an
expanded branch, otherwise selects its parent; Right expands a collapsed
branch, otherwise selects its first visible child. Activate emits a row
activation or heading sort intent. Header Left/Right and First/Last move
and reveal the focused heading. ScrollLeft/ScrollRight move the horizontal
viewport. Navigation honors repeats; repeated Activate is consumed.
The consumer owns physical key bindings and focus traversal.

Tests cover bounded hierarchy and text validation, stable ID/anchor
replacement, stale and interrupted gestures, disclosure and navigation,
scrollbar capture, horizontal header/cell/hit alignment, visible-only
formatting, fallback and scale 1-4 clipping/partial-repaint pixel oracles.

## Task-manager widgets

[td-taskmgr](../td-taskmgr/DESIGN.md) is a planned consumer. Menus and
confirmations, nonclosable resource tabs, charts, the split pane and tree
table are implemented as shared widgets. Process collection, history and
signal execution stay in the consumer.

All these widgets preserve the pure-input and semantic draw-stream seams
above. They introduce no filesystem access, process authority, external
crate or direct pixel writes. State-machine and draw-stream tests cover
their interaction contracts, with software pixel oracles at multiple
scales, clipped/narrow surfaces, focus loss and stale consumer data. Extend
shared primitives atomically where necessary; no temporary application
copy of a widget is an acceptable completion of this work.

## Independently landable increments

The task-manager widget sequence is tracked in
[td-taskmgr's delivery plan](../td-taskmgr/DESIGN.md#validation-and-delivery):
menus, confirmations and resource tabs, followed by charts, a split pane
and a tree table, each with shared-widget oracles and existing consumer
regressions. Those increments extend the original sequence below.

1. Rule and crate: the lock guard admits sibling roster dependencies; the
   crate exists with the input layer, the shared codecs and the cell
   constants. Landed.
2. Raster: `Rect`, `Scale`, `Weight`, `GlyphStyle`, `Primitive`, `Draw`,
   `Surface`, `Composition`, `Raster`, the scrollbar geometry, the text-run
   painter, the palette and the font-licence strings. td-editor's
   `Geometry` and `Scene` compose them and `--preview` stays
   byte-identical. Landed.
3. Transport, in two landings. (a) `sys` (sendmsg, recvmsg, the pinned
   fcntl request) with `UNSAFE.md` §19, and `wayland` with the endpoint,
   connect, `Connection`, pool files, the pointer image and `peer::drain`;
   td-editor's window drives the connection from its own loop and its
   transport tests moved. Landed. (b) `client::Client` with the object
   table, registry, SHM buffers, frame callback, pointer image and the
   turn loop `run` behind the `App` trait, a consumer's own objects held
   in the table under its `Tag`; td-editor is the first `App`, its nine
   presentation tests moved, and the probe consumer in `tests/client.rs`
   drives the client. Landed.
4. Devices, in two landings. (a) The seat, keyboard and pointer inside the
   client: bound and created there, the keymap right consumed there, the
   keyboard's state owned there and its events delivered typed with their
   serials; td-editor acts on the outcomes and keeps its gestures. Landed.
   (b) The clipboard's data-device lifecycle: manager, device, offers with
   their budgets and retirement barriers, and sources, delivered typed;
   td-editor keeps its transfers. Landed.
5. Widgets. (a) The chrome bands td-editor already draws, as `chrome`:
   the menu bar and its panel, the wrapped text block, the tab strip
   and the status row, each with a draw-stream oracle and the status
   band rasterized whole to pixels; td-editor's `Geometry` and `Scene`
   delegate and `--preview` stays byte-identical. Landed. (b) The new
   widgets a scene did not draw, in two landings. (i) `List`, the
   panel's row painter over a scrolling, selectable window with a
   scrollbar and an optional right-aligned column, for the file chooser
   and choice lists, with a draw-stream and a pixel oracle. Landed.
   (ii) `TextEntry`, the single-line field with a caret, selection,
   placeholder, right-scrolling window and a masked mode a consumer
   applies under its own trust rules, for the second and third
   consumers, with a draw-stream and a pixel oracle. Landed.
6. `td-setup`: the installer front end's first page as the second
   consumer, in two landings. (a) The crate, and its `welcome` page built
   from the raster and the chrome bands: a chrome ground, a heading over a
   hairline rule, the wrapped disclosure prose in a `Block` and a `Status`
   footer, disclosing that storage is unencrypted and the account signs in
   automatically with no password or PIN field, per
   td-install/INSTALLER.md; the prose is word-wrapped in the crate, with
   draw-stream and pixel oracles and no live compositor. Landed. (b) The
   Wayland turn loop making it a live `App`. A minimal copy of td-editor's
   native compositor harness launches the client against the real headless
   compositor, reads the tile it is placed in and asserts the captured
   pixels equal the crate's own `preview` of that surface. Landed.
7. td-portal: the file chooser on td-ui, its private handshake and second
   rasterizer deleted, and its recipe converted to stage sibling trees.
   (a) The recipe becomes a generic `Recipe::rust` cargo build that stages
   td-portal beside the sibling trees its `#[path]` modules name — td-secret,
   td-busd, td-compositor, and (through td-secret's sha256) engine — the same
   shape td-net uses, so (c) can add the toolkit by naming one more tree. The
   static-shape and selftest proofs the hand-rolled recipe ran inline move to
   a td-portal-test companion, the split td-ui-test makes for the compositor.
   Landed. (b) The second private Wayland client — the boot channel probe —
   and its `TD-PORTAL-CHANNEL-READY` marker, boot-evidence service unit and
   boot assertion retired; the private registry it pinned moves to the
   surviving dialog. Landed. (c) The file chooser's render on td-ui's raster
   and chrome bands — a `Composition` with a chrome ground, the title over a
   hairline rule, the guest path and selection-status lines, the filter as a
   `TextEntry` and the entries as a `List`, on the shared light palette — its
   own second rasterizer and font mount deleted and its render oracles regolded
   from a byte fingerprint to named palette colours. Landed. (d) Its transport
   a td-ui `App`, the private dialog client deleted, under the native
   compositor harness.
8. Driving, in two landings. (a) The transport lifted from td-editor:
   `control` (frame, envelope, codecs, response lines, `Parse`),
   `control_socket` moved intact, `control_worker` generic over the
   consumer's request type, and `replay::run`; td-editor cut over
   atomically and wire-compatibly, its framing, socket and worker tests
   and their confinement pins moved, its own `Refusal` and worker types
   specialisations of the toolkit's. Landed. (b) The semantic seam under
   "Driving": `driven` with the input event, outcome, `Controller`
   trait, action table and generic verbs, `text` read back from a
   `Composition`'s draw stream and one XRGB-to-PPM writer, proven with a
   toy controller in the crate's tests. Landed; td-photo consumes it
   next.
9. Directory finder, in two landings. (a) `finder`, the shared directory
   finder over a consumer-supplied listing (see "Shared directory
   finder"), with its oracles; td-photo's roll chooser is its first
   consumer. Landed. (b) td-portal's file chooser over `finder`: its
   filesystem model stays (descriptors, no-follow, the bounds and the
   guest-path mapping) and hands the widget a `Listing` per folder, its
   own filter, selection, scroll window and `ChooserView` deleted, the
   multi-select mark added to the widget's `Entry` for the multiple-file
   mode, and its render oracles regolded over the widget's bands. Landed.
10. Button strip: `chrome::Buttons`, a row of bezelled `Button`s on one
    band (see "Shared action button"), the button's text centred in its
    height; td-photo's mode and filter strips are its first consumer.
    Landed.
11. The cell screen under "Cell screen": `screen`, the styled grid as a
    `Composition` with the key vocabulary and chord translation, and
    `screen_app`, the window that presents it and polls a `Handler`,
    proven with a recording handler against a scripted peer. Landed;
    td-news and td-mail drew in it (APPLICATIONS.md §W.8, "Reworked"),
    each landed in its own increment with its recipe, package and unit;
    neither does now, and it is deleted with td-mail's composing
    increment.
12. The widget window under "Widget window": `window`, the screen
    window's loop without the grid, whose handler paints into a raster
    over the surface and reads chords, button phases and wheel travel in
    surface pixels, proven with a recording handler against the scripted
    peer. td-news has moved onto it with the toolkit's `List` and
    td-editor's document pane (APPLICATIONS.md §W.8, "Reworked again"),
    and td-mail likewise, reading a message in the pane; td-mail's
    composing increment follows, and the cell screen and `screen_app`
    are deleted with it.
