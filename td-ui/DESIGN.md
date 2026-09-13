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
compositor's `font`, `font_data` and `wire` sources exactly as td-editor
did, so there is still one Unifont face and one wire codec in the tree, and
it owns the 8x16 cell constants every consumer lays text out on. td-editor
depends on it by path and uses those modules through the crate's public
surface.

Newly built: `td-setup`, the second consumer, is a new crate whose
`welcome` page renders from the toolkit and whose Wayland turn loop
presents it as a live `App`, proven under the native compositor harness
(increment 6). Not yet moved: td-portal's file chooser, the third consumer,
still on its own rasterizer. The increments below schedule the rest.
td-editor's window is the first `App` and td-setup's the second; each owns
no Wayland objects of its own.

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
recipe cannot link a second crate. td-editor has no recipe yet, so nothing
changes for packaging until one exists.

## Public surface

The crate's `pub` items are the whole contract. Consumers use them through
`td_ui::` paths and nothing else; a consumer's confinement tests pin which
of its own files may name each module.

- `CELL_WIDTH`, `CELL_HEIGHT`: the 8x16 bitmap cell. `font::pinned` is held
  to them by a test.
- `font`: the compositor's PSF2 reader and pinned Unifont face, unchanged.
  Provenance and licences stay in `td-compositor/assets`.
- `wire`: the compositor's Wayland framing codec, unchanged.
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
- `raster`: `Rect`, `Scale` (1 through 4), `Weight`, `GlyphStyle`,
  `Primitive`, `Draw`, `Surface`, the `Composition` trait, `Scrollbar`,
  `text_run`, `Raster`, `Error`, the axis and frame-byte ceilings and the
  palette constants. A composition reports the surface it was laid out for
  and streams the draws inside a damage rectangle; `Raster::new` validates
  surface, font, stride and buffer before any write, and `Raster::paint`
  refuses a composition laid out for another surface. The behavioural
  contract (clipping, the medium fringe, scrollbar proportions and drag
  rounding) is the one td-editor/DESIGN.md records under "Implemented
  reference-renderer contract"; that text moves here with the
  documentation increment.
- `chrome`: `Bar` with its `Panel`, `Block`, `Strip`, `Status`, `List`
  and `TextEntry`, the `Row` a panel paints, the `Item` a list paints,
  the `Field` a text entry paints and `step`, the bands, the paged list
  and the text entry a td-owned window shares, over `raster` and
  independent of any scene. The bar, a panel's rows, the tab
  strip and the status row are each `ROW` (24) reference-renderer pixels
  tall, of 8x16 cells, scaled by the surface; the text block wraps in
  16-pixel cell rows. `Bar` fills the first row and lays its labels from
  cell one, three cells apart, and answers a header hit. Its `Panel`,
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
  registry (`find_global`, `global_name`, `bind`, `required`, `is_required`,
  `forget_global`), the toplevel (`set_title`, `set_app_id`, `commit`,
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

## Invariants

- Pure modules read no environment, clock, descriptor or filesystem.
  Adapters pass explicit ticks in milliseconds and explicit byte inputs.
  Outside the pure set are `notices` (three `include_str!` constants and
  nothing else), the transport pair `wayland` and `sys`, which own the
  stream, its deadlines and the pool files in the directory a consumer
  names, and `client`, whose `run` reads the monotonic clock for the
  consumer's ticks and whose buffers are those pool files; even they read
  no environment variable, taking the display values as explicit
  arguments.
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

The builder discovers the crate by existing. Its gate runs `cargo test` and
all-target Clippy; a change under `td-ui/` selects td-editor's tests through
the reader graph, because td-editor's manifest names the crate.

## Independently landable increments

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
   and chrome bands,
   its own rasterizer deleted. (d) Its transport a td-ui `App`, the private
   dialog client deleted, under the native compositor harness.
