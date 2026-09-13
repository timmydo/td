# td-ui

td-ui is the dependency-free Rust toolkit that td-owned graphical programs
share. Today it carries the pinned Unifont face and wire codec, the XKB
keyboard translation, repeat policy and pointer decoding, the clipped XRGB
raster with its palette and scrollbar geometry, the Wayland client
connection over its own raw descriptor transport, and the client over it
(object table, registry, one toplevel surface with its buffers and frame
callback, the seat with its keyboard and pointer, the pointer image and the
turn loop), all moved out of td-editor; the clipboard's device lifecycle and
the chrome widgets (menus, prompts, lists, tabs, status rows) that td-editor
draws follow by the increments below. td-editor is its first consumer; the
installer front end `td-setup` and td-portal's file chooser follow. This
document is the component contract and the starting point for successive
agents; the root `AGENTS.md` and `DEVELOPMENT.md` still govern changes and
submission.

## Status and scope

The crate exists and carries, unchanged in behaviour, what has moved out of
td-editor so far: the bounded XKB text-v1 keymap compiler (`keyboard`,
`xkb`), the explicit-clock held-key and repeat policy (`repeat`),
`wl_pointer` event decoding with axis-frame accumulation (`pointer`), and
the clipped, allocation-free XRGB painter over the pinned face (`raster`):
rectangle and glyph primitives, the integer scale, the warm palette,
scrollbar geometry, the text-run painter and the `Raster` that writes a
`Composition`'s draws into a caller-owned buffer, with the face's provenance
and licence texts embedded in `notices`; and the Wayland client connection
(`wayland`): display endpoint resolution from explicit environment values,
the bounded connect, request framing with at most one descriptor per send,
the receive path that owns every delivered right until an event's consumer
takes it, the startup and write deadlines, the unlinked private pool file
and the pointer image, over the private raw module `sys` that `UNSAFE.md`
§19 records; and the client over that connection (`client`): the object
table with its fixed and dynamic ids, the registry with its budgets, the
three globals every consumer binds, one toplevel surface with its SHM
buffers and frame callback, the seat with the keyboard and pointer its
capabilities give (the keymap right consumed and compiled, focus, held keys,
the modifier snapshot and repeat timing applied, pointer events decoded and
the pointer image shown on enter), and the turn loop that drives a
consumer's `App` under the startup deadline. It re-mounts the compositor's
`font`, `font_data` and `wire` sources exactly as td-editor did, so there is
still one Unifont face and one wire codec in the tree, and it owns the 8x16
cell constants every consumer lays text out on. td-editor depends on it by
path and uses those modules through the crate's public surface.

Not yet moved: the clipboard's device lifecycle (the data-device manager,
device, offers, sources and sync barriers), the widgets, and the second
and third consumers. The increments below schedule them. td-editor's
window is the first `App`; its clipboard objects live in the client's
table under the editor's own tag until the clipboard increment.

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
  pools, buffers, frames, the pointer image's, and the seat, keyboard and
  pointer with their retired states); the fixed ids `DISPLAY` through
  `TOPLEVEL` and the budgets `OBJECTS`, `GLOBALS`, `NAME_BYTES`, `BUFFERS`,
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
  drives: `cancel_repeat`, `arm`, `repeat` and `wait_ms`), the pointer
  image's `cursor`, the state accessors `bound`, `configured` and `closed`,
  `words`, `send` and `pop_descriptor` with the pending `descriptors` count,
  `needs_descriptor` for the keymap right the client consumes, `connection`
  for the schedule inputs, and `handle`, which takes the consumer's clock,
  consumes what is the client's in an event and returns `Handled`: `Done`,
  `Bound`, `Configure`, `CloseRequested`, `FrameDone`, `GlobalRemoved`,
  `Capabilities`, `SeatRemoved`, `Keyboard` with a `KeyboardEvent`
  (`Keymap`, `Focus`, `Ready`, `Key` with its serial, key and `Stroke`,
  `Refused`), `Pointer` with the decoded `pointer::Event`, or `Unhandled`
  for the consumer's own objects. `App` (`client`, `needs_descriptor` for
  the consumer's own rights, `descriptor_wait`, `tick`, `event`, `end_turn`,
  `draw`) is what `run` drives: the registry request and initial sync under
  the startup deadline, then, until the client is closed, at most
  `MESSAGES_PER_TURN` events per turn with one event parked while its right
  has not arrived (the client's keymaps and the consumer's rights alike,
  cancelling repeat), the consumer's end of turn and draw, and the
  transport's wait. `unconfigure` and `input_mut` are test support, public
  because a consumer's tests are another crate and hidden from the crate's
  documentation.

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
- No secret-entry widget. A text field that looks trusted but is not is
  what `td-install/ENCRYPTION.md` forbids; PIN and passphrase entry belong
  to the compositor's secure-attention path.

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
the seat and forgetting the global; the complete loop on a thread accepting
split events and closing cleanly; and the loop parking an event until its
right arrives, in wire order, with the idle wait restored. Twelve of these
moved from td-editor's window tests (the presentation nine, the pointer
image, the seat binding and the keymap reader's shared-offset and bad-source
oracle, the last a unit test beside `read_keymap`); the editor keeps its
clipboard and control coverage and its reactions to the client's device
outcomes.

`tests/confinement.rs` pins the source inventory, the exact three shared
source mounts, the absence of ambient I/O in pure modules, that `notices`
is three embedded texts and nothing else, the absence of `include!`,
`cfg_attr` and any dependency declaration, that the shared sources bind no
input interface, and the raw layer: the complete fingerprint of `sys.rs`,
its syscall and flag values, its two function-only allowances, the single
instruction and adoption sites, that the crate root denies `unsafe` and
declares the module private, that `wayland` is its only caller, through
exactly four wrapper calls, that no production module calls the
client's test support (`unconfigure`, `input_mut`), and that the client
is the toolkit's one consumer of a received right, through the pinned
keymap reader with its format, size and regular-file checks.

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
4. Devices, in two landings. (a) The seat, keyboard and pointer inside
   the client: bound and created there, the keymap right consumed there,
   the keyboard's state owned there and its events delivered typed with
   their serials; td-editor acts on the outcomes and keeps its gestures.
   Landed. (b) The clipboard's data-device lifecycle: manager, device,
   offers with their budgets and retirement barriers, and sources,
   delivered typed; td-editor keeps its transfers.
5. Widgets: text entry, wrapped text block, menu bar and panel, paged list,
   tab strip and status row, each with a pixel oracle.
6. `td-setup`: the installer front end's first page as the second consumer,
   under the native compositor harness shared from td-editor's tests.
7. td-portal: the file chooser on td-ui, its private handshake and second
   rasterizer deleted, and its recipe converted to stage sibling trees.
