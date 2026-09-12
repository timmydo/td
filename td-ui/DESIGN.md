# td-ui

td-ui is the dependency-free Rust toolkit that td-owned graphical programs
share. Today it carries the pinned Unifont face and wire codec, the XKB
keyboard translation, repeat policy and pointer decoding, the clipped XRGB
raster with its palette and scrollbar geometry, and the Wayland client
connection over its own raw descriptor transport, all moved out of
td-editor; the client's object table and turn loop, the device lifecycle
and the chrome widgets (menus, prompts, lists, tabs, status rows) that
td-editor draws follow by the increments below. td-editor is its first
consumer; the installer front end `td-setup` and td-portal's file chooser
follow. This document is the component contract and the starting point for
successive agents; the root `AGENTS.md` and `DEVELOPMENT.md` still govern
changes and submission.

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
§19 records. It re-mounts the compositor's
`font`, `font_data` and `wire` sources exactly as td-editor did, so there is
still one Unifont face and one wire codec in the tree, and it owns the 8x16
cell constants every consumer lays text out on. td-editor depends on it by
path and uses those modules through the crate's public surface.

Not yet moved: the client's object table, registry, SHM buffers, frame
callback, cursor surface and turn loop, the seat, keyboard, pointer and
clipboard device lifecycle, the widgets, and the second and third
consumers. The increments below schedule them. td-editor's window drives
the connection from its own loop until the client lands.

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
  key press and release, arming, repeat and next-wake computation.
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
  glob-imports the module shadows `std::result::Result`. The behavioural
  contract (environment precedence, the
  deadlines, the byte and right budgets, pool-file creation, the shared
  status flags of an inherited socket) is the one td-editor/DESIGN.md
  records under "Implemented scratch-window adapter"; that text moves here
  with the documentation increment. `sys`, the raw module beneath it, is
  private to the crate.

## Invariants

- Pure modules read no environment, clock, descriptor or filesystem.
  Adapters pass explicit ticks in milliseconds and explicit byte inputs.
  Outside the pure set are `notices` (three `include_str!` constants and
  nothing else) and the transport pair `wayland` and `sys`, which own the
  stream, its deadlines and the pool files in the directory a consumer
  names; even they read no environment variable, taking the display
  values as explicit arguments.
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

`tests/confinement.rs` pins the source inventory, the exact three shared
source mounts, the absence of ambient I/O in pure modules, that `notices`
is three embedded texts and nothing else, the absence of `include!`,
`cfg_attr` and any dependency declaration, that the shared sources bind no
input interface, and the raw layer: the complete fingerprint of `sys.rs`,
its syscall and flag values, its two function-only allowances, the single
instruction and adoption sites, that the crate root denies `unsafe` and
declares the module private, and that `wayland` is its only caller,
through exactly four wrapper calls.

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
   transport tests moved. Landed. (b) `wayland::Client` with the object
   table, registry, shm buffers, frame callback, cursor surface and turn
   loop behind an `App` trait; the scripted peer fixture becomes shared
   test support.
4. Devices: seat, keyboard, pointer and clipboard device lifecycle inside
   the client, delivered as a typed event stream with serials.
5. Widgets: text entry, wrapped text block, menu bar and panel, paged list,
   tab strip and status row, each with a pixel oracle.
6. `td-setup`: the installer front end's first page as the second consumer,
   under the native compositor harness shared from td-editor's tests.
7. td-portal: the file chooser on td-ui, its private handshake and second
   rasterizer deleted, and its recipe converted to stage sibling trees.
