# td-editor

A Wayland-native, dependency-free Rust text editor under construction, with a simple
tabbed interface, Windows-like and Emacs key profiles, paragraph filling,
and on-demand whole-document spell checking. It is intended to run both on
td and other Linux Wayland desktops and to support deterministic tests and
explicit local remote control.

Start with [DESIGN.md](DESIGN.md). It records the architecture, reuse map,
file-safety requirements, compatibility boundaries, tmc integration findings,
acceptance tests, and independently landable increments. Version 1 uses
Unicode-scalar editing and single-cell Unifont rendering, preserves UTF-8
BOM and uniform LF/CRLF files, defaults to Windows-like bindings, and uses
an explicitly selected local English word list. Spelling runs only on
request; an edit clears marks without starting another scan. That spelling
profile is designed but not implemented yet.

## Implemented core

The safe, dependency-free library implements UTF-8/BOM/LF/CRLF conversion,
scalar edits and selection, tabs, bounded undo/redo with saved-state tracking,
literal search/replace, paragraph filling and Auto Fill. Logical Windows and
Emacs keys share those commands. The controller also handles visual navigation
and pointer selection. Dialogs, clipboard, spelling and actual file I/O produce
an explicit adapter request. A read-only Wayland preview is available; no
interactive window input, GPU renderer, filesystem
Open/Save, remote socket or tmc integration is claimed yet. Do not set
`$EDITOR` to this binary yet.

Build and verify from the repository root:

```text
cargo build --release --manifest-path builder/Cargo.toml
cargo test --frozen --manifest-path td-editor/Cargo.toml
cargo clippy --frozen --manifest-path td-editor/Cargo.toml --all-targets -- -D warnings
cargo build --release --frozen --manifest-path td-editor/Cargo.toml
td-editor/target/release/td-editor --help
```

`src/model.rs` owns state and transaction admission; `text.rs` owns the
lossless file codec; `fill.rs` plans bounded reflow; `keys.rs` translates
logical chords; `ui.rs` owns input/view state; and `replay.rs` feeds that same
controller with framed commands.
`tests/core.rs` covers byte round trips, stale/invalid commands, limits,
save completion after intervening edits, global history eviction, reflow
mapping, key-profile conflicts and generated edits against a scalar-vector
reference. It also launches the real replay executable without a display.

`src/layout.rs` adds an allocation-free visual-row and scalar-cell map:
soft wrapping, tab widths, caret affinity, pixel hit testing, vertical/page
motion calculation, and independent viewport scrolling. `tests/layout.rs`
compares generated rows with an exhaustive scalar-vector reference, checks
every interior cell pixel, and round-trips every caret boundary through hit
testing. Use `Viewport::layout` to borrow validated model text with matching
wrap geometry. The controller caches metrics and retains per-tab scrolling,
caret affinity and desired vertical column. `tests/ui.rs` exercises keyboard,
drag, resize, focus and clock sequences, including identical pixels from
typed events and replay. No display is needed for those interaction tests.

`src/render.rs` supplies the safe software reference backend. A borrowed
`Scene` streams clipped rectangle/glyph operations; `Raster` writes them into
a caller-owned, stride-checked XRGB8888 buffer. The renderer uses the existing
compositor Unifont data and decoder directly, with no copied font or new
dependency. It draws tabs, bounded display labels, menu/status chrome,
selection, and a caret, at integer scales 1–4. Pixel-oracle tests cover clipping,
damage, padding, fallback glyphs, scrolling and extreme geometry. Menus remain
drawing only. Tab presses select tabs; close marks emit typed requests for the
clicked tab without discarding it.

Inspect a deterministic 800x600 rendering without a display:

```text
td-editor/target/release/td-editor --preview > /tmp/td-editor-preview.ppm
td-editor/target/release/td-editor --font-license
```

The first command writes a binary P6 PPM fixture, not an interactive window;
use a PPM-capable image viewer. The second prints the embedded font provenance
and complete notices. This backend performs CPU rasterization and does not
use a GPU. Building from source
currently needs the full td checkout for the shared modules and license data;
the resulting executable does not need an installed td system.

Try the actual window from a terminal in your Linux x86-64 Wayland session:

```text
td-editor/target/release/td-editor --window-preview
```

It shows a fixed read-only document and follows window-manager resizing.
Keyboard, pointer, menus and file editing are not connected yet. Close it
through your window manager or press Ctrl+C in the launching terminal.
This tests presentation, not a usable `$EDITOR`. No td compositor, GPU node,
libwayland, toolkit or installed font is needed. The normal WAYLAND_DISPLAY,
XDG_RUNTIME_DIR and inherited WAYLAND_SOCKET conventions are supported.
The latter is borrowed and duplicated; give this process exclusive use of it.
Temporary SHM files are private, immediately unlinked, and bounded to three
buffers; busy buffers are never overwritten before compositor release.

An optional test runs against a separately launched Weston (not a dependency
of the editor). Set the socket to your isolated test instance:

```text
TD_EDITOR_TEST_WAYLAND=/absolute/path/to/weston-socket cargo test --frozen --manifest-path td-editor/Cargo.toml weston_presents_the_reference_buffer -- --ignored
```

This waits for actual frame completion. The ordinary tests need no display
and check transferred pool pixels and lifecycle behavior with Unix sockets.

`src/xkb.rs` and `xkb_syntax.rs` provide the next keyboard foundation:
bounded text-v1 lexical parsing and table-driven type selection, including
virtual-mask bindings and preserved modifiers. Tests cover the td map and
all 26 types of a compiled ordinary US map, with libxkbcommon-derived results
for all 256 real-modifier combinations. The fixture's provenance and oracle
procedure are in [tests/fixtures/README.md](tests/fixtures/README.md).
This is not a complete keymap validator or key-event translator. Compatibility
interpretations, keycode/symbol compilation, repeat and window input remain
unimplemented; successful type parsing must not activate keyboard input.

Editor-only changes are routed by `td-builder ready` to this crate's tests
and Clippy alongside the workspace Rust suite, whose tests validate every
discovered crate's lock and manifest. Documentation-only changes keep the
normal docs-only waiver. Neither runs bootstrap/image gates. This is valid while no
recipe or workspace member consumes editor sources; builder regression tests
guard that boundary. Adding an editor recipe or another consumer must update
the routing and its guard in the same increment. A diff that also changes
the builder or another embedded component still selects its broader checks.

## Headless replay protocol

Run `td-editor --replay` with binary stdin/stdout. Consecutive requests use
the design's four-byte big-endian payload length followed by an ASCII
tab-separated record: `1 REQUEST_ID COMMAND ARG...`. Spaces here stand for
tabs; there is no newline in a payload. All numbers are unsigned decimal;
text/byte fields use lowercase hex, with `-` for empty. Each request has one
similarly framed response. EOF between frames ends the in-memory session;
truncated/oversized frames fail the process. Bad payloads return an error and
leave the stream available for the next frame. This explicit test mode has
blocking stdin/stdout; socket deadlines and UI scheduling belong to the
future control adapter.

| Command | Arguments after the command |
| --- | --- |
| `new`, `state` | none |
| `load` | hex-encoded file bytes; test fixture only, no path lookup |
| `select-tab` | tab ID |
| `set-key-profile` | `windows` or `emacs` |
| `text` | tab ID, revision, byte offset, page byte limit (4..=262144) |
| `select-range` | tab ID, revision, anchor byte offset, caret byte offset |
| `insert` | tab ID, revision, hex text; paste semantics, no Auto Fill |
| `delete`, `backspace`, `undo`, `redo`, `fill-paragraph`, `close-tab` | tab ID, revision |
| `set-auto-fill` | tab ID, revision, `0` or `1` |
| `set-fill-column` | tab ID, revision, column (20..=240) |
| `find` | tab ID, revision, hex needle, backward (0/1), wrap (0/1) |
| `replace` | tab ID, revision, hex needle, hex replacement; Replace All |
| `key` | active tab ID, revision, hex logical chord |
| `resize` | nonzero surface width, height, scale (1..=4) |
| `set-soft-wrap` | tab ID, revision, `0` or `1` |
| `scroll` | tab ID, revision, `rows` or `columns`, `forward` or `backward`, amount |
| `pointer` | active tab ID, revision, `press`/`move`/`release`, x, y, extend (0/1) |
| `focus` | `0` or `1`; keyboard focus, not pointer presence |
| `tick` | monotonic elapsed milliseconds for caret blinking |

Logical chords use `C-`, `M-`, and `S-`; e.g. `C-x`, `C-S-s`, `M-q`,
`C-Space`, `Left`, `S-Left`, `Return`, `Tab`, `Space`, `Escape`, `F7`.
One printable scalar is a typed character. Send Emacs prefixes as separate
requests. A profile/tab switch or Escape/C-g cancels the prefix. Emacs mark
extends subsequent model motion; edits or cancellation retire it.

Responses begin `1 REQUEST_ID ok BODY...` or
`1 REQUEST_ID error CODE HEX_DIAGNOSTIC`. The current diagnostic is the
lowercase hex encoding of the error code. Creation returns the new tab ID;
semantic document commands return the current revision; `text` returns the
next byte offset and hex text. Keys return an empty body for completed core
actions, `prefix` for pending C-x, or `request NAME TAB_ID REVISION` for a
translated action whose adapter is absent. The latter is **not** a save,
clipboard transfer, spelling check or visible dialog. `close-tab` refuses
dirty text; no wire
command can mark a buffer saved. A New key returns the new tab ID, just like
the `new` command. Unknown/malformed commands are refused. Up/Down and
Page Up/Down (including Shift variants) now perform visual navigation rather
than returning adapter requests.

`state` returns `active=ID` (0 means no tab), `keys=PROFILE`, `prefix=0|1`, then one
`tab=ID,REVISION,DIRTY,BYTES,ANCHOR,CARET,AUTO_FILL,FILL_COLUMN,BOM,ENDING`
field per tab; ENDING is `lf` or `crlf`. Text pages never split scalars.
Use the returned revision in subsequent commands; undo and redo advance it.
Selection and formatting-mode changes leave the text revision unchanged.

`state` additionally reports `generation=N`, `window=WIDTH,HEIGHT,SCALE`,
`focus=0|1`, and one
`view=ID,FIRST_ROW,LEFT_COLUMN,COLUMNS,ROWS,SOFT_WRAP,AFFINITY,DESIRED_COLUMN`
per tab. Affinity is `upstream` or `downstream`; an unset desired column is
`-`. A zero-cell surface has a virtual minimum 1x1 cached layout but no text
input hit area. Generation is local state, not compositor frame completion.
Scroll amounts are unsigned and at most `isize::MAX`; direction carries the
sign. Replay pointer coordinates are unsigned surface pixels through
`i64::MAX`; the typed API also accepts signed out-of-surface drag coordinates.
Pointer drag clamps to viewport edges without autoscroll. Keys require focus;
pointer events do not. Focus loss cancels prefix/mark/drag, preserving selection.
An invalid key continuation preserves its prefix until cancelled explicitly.
Ticks are milliseconds since controller creation. Send a current tick before
each timed input event; input occurs at the last supplied tick, not an ambient
wall clock. Timer wakes must also send ticks to animate the caret.

For example, these payloads create a tab, insert `hello`, and read it back:

```text
1<TAB>1<TAB>new
1<TAB>2<TAB>insert<TAB>1<TAB>0<TAB>68656c6c6f
1<TAB>3<TAB>text<TAB>1<TAB>1<TAB>0<TAB>256
```

Replace `<TAB>` with actual tab bytes and prefix each payload with its byte
length. These are protocol examples, not lines to type directly into stdin.

Two constraints shape the first implementation: td's current bitmap renderer
is software-based, and td-term's exact-keymap check does not support arbitrary
host Wayland keyboards. Version 1 targets td and Weston's US English map.
`sockets=wayland` alone supplies neither GPU access nor an editor executable
inside td-jail. A render-node grant can be added for Firefox; the design
lists its driver, runtime-policy and DMA-BUF prerequisites. Direct GPU
rendering for the dependency-free editor also needs a source-built graphics
implementation; the software reference backend does not complete that goal.

tmc currently deletes its temporary draft and attachment files when its
editor child exits. Saving a draft in place does not retain it, and tmc has
no mail submission path. The design describes this integration gap explicitly.
