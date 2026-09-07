# td-editor

A Wayland-native, dependency-free Rust text editor under construction, with a simple
tabbed interface, Windows-like and Emacs key profiles, paragraph filling,
and on-demand whole-document spell checking. It is intended to run both on
td and other Linux Wayland desktops and to support deterministic tests and
explicit local remote control.

Start with [DESIGN.md](DESIGN.md). It records the architecture, reuse map,
file-safety requirements, compatibility boundaries, td-mail integration findings,
acceptance tests, and independently landable increments. Version 1 uses
Unicode-scalar editing and single-cell Unifont rendering, preserves UTF-8
BOM and uniform LF/CRLF files, defaults to Windows-like bindings, and uses
an explicitly selected local English word list. Spelling runs only on
request; an edit invalidates marks without starting another scan. The spelling
library and native F7/Format controls are implemented.

## Implemented core

The safe, dependency-free library implements UTF-8/BOM/LF/CRLF conversion,
scalar edits and selection, tabs, bounded undo/redo with saved-state tracking,
literal search/replace, paragraph filling and Auto Fill. Logical Windows and
Emacs keys share those commands. The controller also handles visual navigation
and pointer selection. The experimental `--window` connects file Open/Save
and Save As through one worker, with literal keyboard path prompts and both
key profiles. Native mouse input selects/drags text, switches tabs, uses their
close marks and scrolls with wheels/touchpads. A small bitmap arrow supplies
the cursor. Click File/Edit/Format/Help or press F10 for menus; arrow keys
navigate, Return activates and Escape/Ctrl+G cancels. Edit switches key
profiles; Format exposes Soft Wrap, Auto Fill and Fill Paragraph.
Format > Fill Column sets the active tab's fill width (20–240 cells,
default 72) without reflowing existing text. Return applies the number,
Ctrl+U clears entry, and Escape/Ctrl+G cancels.
Edit also exposes Find/Find Next/Find Previous. Windows Ctrl+F opens Find,
F3 searches next and Shift+F3 previous; Emacs C-s/C-r open directional
search. Type a literal case-sensitive query, Return to search, Ctrl+U to
clear or Escape/Ctrl+G to cancel. Search reports the end before the next
explicit search wraps. Entry is submitted, not incremental while typing.
Windows Ctrl+H or Edit > Replace opens Find/With fields. Tab switches fields;
Return finds the next match, Alt+R replaces only the selected exact match,
and Alt+A replaces all matches in one undo step. Empty With deletes matches.
Escape/Ctrl+G closes the dialog without rolling back completed edits; Undo
is available after closing. Each field is limited to 4096 UTF-8 bytes and
does not support clipboard entry yet. The Edit menu now needs 360 scaled
pixels of height; Windows Ctrl+H works below that minimum, while Emacs
requires enlarging the window for menu access.
Edit > Go To Line or F6 accepts a one-based logical line in either profile;
soft wrapping does not affect line numbers. Return moves, Escape/Ctrl+G
cancels, and Ctrl+U clears. Invalid or nonexistent lines leave the prompt
open for correction. Replay also accepts `go-to-line TAB REVISION LINE`
(tab-separated arguments).
Native query/edit/file/dialog control is available explicitly. Decoded remote
key/pointer input, other keyboard-prompt answers, GPU rendering and td-mail
integration remain unimplemented. Do not set
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
The safe `control` library supplies one-frame decoding, controller state/text
queries and revision-checked edits through the shared controller. See
[CONTROL.md](CONTROL.md) for exact fields and bounds. The experimental
`--window --control-socket PATH` option connects state/text inspection and
coarse native modal/job/spelling flags. Remote `new` creates an ordinary
empty tab and returns its stable ID without touching existing documents or
opening a file. The control endpoint also supports Select Tab/Range, Insert,
Delete, Undo, Redo, Fill Paragraph, literal Find and Replace All.
Insert/Delete/Fill/Find check the expected directed selection as well as the
tab revision; native modals refuse remote edits. Auto Fill, fill-column and
key-profile setters and Go To Line also use revision-checked native control
dispatch without changing text or history. `spelling-results` exposes status,
whole-scan counts and bounded range pages pinned to both text revision and
scan ID. Checking starts with ordinary F7 or remote `check-spelling`;
pending scans expose no partial marks. Remote `open` queues the ordinary file
worker and records the selected/created tab ID and revision in shared bounded
job history, including duplicate-file and missing-file behavior. Paths use
literal OS bytes. Remote Close Tab, Quit and close-dialog
Cancel/Discard/Save/path are connected, as are conflict Cancel/Reload/Save As
answers.
These answers pin the live dialog ID, tab and revision and work independently
of physical focus/prompt visibility, without bypassing the close coordinator.
Remote `save` and `save-as` also return job IDs. They recheck the requested
revision before handing a snapshot to the worker; edits after handoff remain
unsaved and cannot change the bytes being written. Save requires an associated
file; Save As takes an explicit literal OS-byte path and cannot overwrite an
existing destination. Job errors may require inspecting disk and the native
warning before retrying. Close-dialog Save asks for a path for untitled tabs;
an explicit path answer queues the same Save As job. Cancel keeps tabs open
but does not roll back an accepted save. Dirty conflict Reload still needs
two explicit answers bound to the live dialog and revision. Reload job
history distinguishes replacement, failure and cancellation; Cancel drops
the replacement permit, not the read syscall. Conflict Save As takes an
explicit new destination and leaves the external file untouched. Ordinary
Open/Save As/Dictionary prompts also accept a literal Path or Cancel answer
bound to their live ID and revision, including while unfocused. Dictionary
jobs report installation or failure without changing document text/history.
Other keyboard-prompt answers (menu/Find/Replace/numeric/command) are not
remotely connected yet.
`check-spelling` returns a job ID; native state retains
up to 64 completion/error/cancellation outcomes, separate from scan-pinned
result pages. Native state also exposes separate redraw/submitted/callback
generations; `wait-frame`
waits for a matching main-surface callback without blocking editing.
The separate `control_socket` library publishes a private Linux Unix
listener only when explicitly requested. It checks directory
ownership/permissions, refuses symlinks and existing endpoints, and pins
parent/socket inodes for checked cleanup. It has no request worker or editor
access itself; CONTROL.md specifies the
absolute-path limits and trust boundary.
The `control_worker` library adds a bounded request thread with
eight connection slots, typed nonblocking UI queues, whole-request deadlines
and joined shutdown. The native adapter polls at most two jobs per outer
event-loop turn without performing socket I/O on the UI thread. Opting in
caps the receive wait at 10 ms even while idle; the default window does not
incur this polling cost.

For an explicitly controllable local window:

```sh
editor_control_dir=$(mktemp -d /tmp/td-editor-control.XXXXXX)
td-editor/target/release/td-editor --window --control-socket "$editor_control_dir/socket" -- notes.txt
```

The parent must already be caller-owned mode 0700, with caller/root-owned
trusted ancestors. Unknown owners in a rootless container remain refused;
there is no environment override. No endpoint is enabled without the option.
Its mode-0600 socket permits reading and editing open tabs, including
unsaved text.
Sharing it across a jail boundary is a separate grant. Socket existence is
not readiness; ask for state. Its controller `generation` is not frame proof:
use the separate `window-generation` with `wait-frame` and check the returned
rendered tab/revision. A callback is not physical scanout or buffer release.
Mutation replies confirm controller admission, not persistence or presentation.
A lost reply means an unknown outcome: inspect state/text before retrying.
Normal shutdown removes only the owned endpoint, not its parent directory.
After abnormal termination, inspect any stale endpoint before removing it.

`tests/core.rs` covers byte round trips, stale/invalid commands, limits,
save completion after intervening edits, global history eviction, reflow
mapping, key-profile conflicts and generated edits against a scalar-vector
reference. It also launches the real replay executable without a display.

`clipboard.rs` supplies tested, display-independent copy snapshots and
selection-bound Cut/Paste admission through the controller. Paste collects
at most 1 MiB of raw bytes; oversized, malformed or stale transfers cannot
partially edit a document. The experimental native window now connects these
operations to core Wayland data-device v3 when the compositor supplies it.

`spelling.rs` supplies strict English word-list parsing and chunked,
revision-bound whole-document scans. `files::read_dictionary` reads only a
caller-selected regular file with bounded input and post-parse race checks;
it never probes system word lists, writes files or creates a saved baseline.
Use `--window --dictionary PATH [FILE...]` or Format > Dictionary to load a
word list. F7 or Format > Check Spelling checks the whole active document;
Escape/Ctrl+G cancels. Underlines and counts appear together on completion.
Edits clear that tab's results without rechecking, and dictionary replacement
clears all results. A failed replacement retains the old dictionary/results.
Format > Next/Previous Misspelling selects marked words without wrapping.
At most 10,000 ranges are stored across the window; omitted unknown words
still count and status reports capped marks. No word list is bundled or
downloaded, and no text leaves the machine. A separate results-list panel
remains unimplemented; scan-pinned remote result pages are available.

Emacs `M-x` or Help > Command opens exact named editor actions. Type a prefix
and Tab to complete; Return runs an exact name, Ctrl+U clears, and
Escape/Ctrl+G cancels. The fixed list is `auto-fill-mode`, `fill-paragraph`,
`goto-line`, `ispell-buffer`, `next-misspelling`, `previous-misspelling`, and
`set-fill-column`. These use the same handlers as the menus; they are not
shell commands, executable names or Lisp. Command entry works through the
Help menu in either key profile.

`transfer.rs` adds the tested descriptor transport prerequisite: bounded
nonblocking pipe/socket writes and private-socket reads, explicit clocks,
five-second deadlines and EOF-only Paste admission. Cancellation restores
outgoing descriptor flags. Windows Ctrl+C/X/V, Emacs M-w/C-w/C-y and the
Edit menu use these transfers. Copy/Cut require a focused physical input
event; Paste accepts UTF-8 text only. Escape cancels pending paste. There is
no PRIMARY selection or drag-and-drop editing, and FIFO-specific clipboard
producers are not supported by the current socketpair receiver. Live
third-party toolkit interoperability is not yet claimed.

`src/files.rs` now supplies the synchronous file-transaction adapter: bounded
regular-file Open and baselines, external-change detection, metadata-checked
atomic Save, and no-clobber Save As. It preserves BOM/line endings through
the model's encoded snapshots. Errors distinguish publication attempts from
confirmed publication and report temporary cleanup failures. Inline tests
exercise real files, failures at each save stage, concurrent changes and
exact saved-state acknowledgements. File-worker dispatch, tab associations and
window prompts are now connected by `session.rs`. It permits one file job at
a time; additional requests are visibly refused, not queued. Saves acknowledge
only the snapshot written, so typing during a save leaves newer edits dirty.
See DESIGN's file-safety section for metadata restrictions and race limits.

The file adapter also exposes a prepared Reload: dropping it keeps the old
baseline, while accepting it adopts validated replacement bytes under a new
association ID. The window accepts it only for the still-current document
revision and after explicit confirmation to discard dirty text.

An optional kernel attribute test needs a dedicated UTF-8 fixture with an
extended attribute (for example one created with `setfattr -n user.test -v x`).
No attribute tool is a build or runtime dependency:

```text
TD_EDITOR_TEST_XATTR_FILE=/absolute/path/to/dedicated-fixture cargo test --frozen --manifest-path td-editor/Cargo.toml attribute_fixture_is_refused_without_touching_it -- --ignored
```

Likewise, `TD_EDITOR_TEST_FIFO` can name a dedicated FIFO for the ignored
`fifo_fixture_is_refused_without_waiting_for_a_writer` test. It must return
without a writer both for an initial FIFO and a regular file replaced by
one between inspection and open. The ordinary suite also checks devices,
directories, symlinks and sockets. Neither fixture is needed by default.

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

The default appearance uses warm off-white paper and charcoal text, with
muted chrome and blue-grey selection. A synthetic medium bitmap weight adds
a faint right edge while preserving the original glyph pixels and 8x16
cell spacing. Both previews use it; no system fonts or theme services are
needed. There is no theme or weight settings UI yet.

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
td-editor/target/release/td-editor --window /absolute/path/to/test-draft.txt
td-editor/target/release/td-editor --window --keys=emacs /absolute/path/to/test-draft.txt
```

Use a disposable copy while this is experimental. With no path, it starts an
Untitled tab; a missing path starts a dirty new-file tab without creating the
file until Save. Windows: Ctrl+O opens, Ctrl+S saves, Ctrl+Shift+S saves as.
Emacs: C-x C-f opens, C-x C-s saves, C-x C-w saves as. Paths are literal:
Return submits, Escape/Ctrl+G cancels, Backspace deletes, Ctrl+U clears. Save
As requires a new pathname. Put the mode flag `--window` first, and use `--`
before dash-prefixed command-line paths.
Switch tabs with Ctrl+Tab. An existing file opened again selects its current
tab without reloading it. External disk changes refuse Save and offer Ctrl+R
Reload, Ctrl+S Save As to a new name, or Escape/Ctrl+G Cancel. These dialog
keys are the same in both profiles. Dirty Reload asks for a separate Ctrl+D
discard confirmation. Successful Reload clears that tab's undo history and
starts at the document's beginning. Reload refuses a deleted destination and
retains the document; use Save As to preserve it under a new name.
Cancelling a pending Reload retains both
the old text and its conflict baseline; the read may still finish.
Resize small windows to at least 272x160 to answer close/conflict questions.

Close asks about each dirty tab: Ctrl+S saves, Ctrl+D approves discarding that
tab's edits, and Escape/Ctrl+G cancels closing. Untitled tabs enter Save As.
Window close keeps all tabs until every choice is resolved; cancelling after
some discard choices retains those edits. A failed save stops closing and
keeps every tab open, including already-saved tabs. Completed saves are not
undone by cancellation or discard.

Starting close during unrelated pending I/O is refused. A save started by a
close dialog keeps the dialog modal; Escape/Ctrl+G cancels closing but the
save still finishes. You can then keep editing without an unexpected later
exit. Fatal errors or process termination can lose unsaved edits; a pending
write may have reached disk. There is no recovery. Conflict Reload uses
the explicit discard-before-replacement policy described above; ordinary
`$EDITOR` invocation remains future work.

The original no-file-access scratch fixture is still available:

```text
td-editor/target/release/td-editor --window-preview
td-editor/target/release/td-editor --window-preview --keys=emacs
```

It starts with two editable scratch tabs and follows window-manager resizing.
Type, navigate, select with Shift, undo, and switch tabs with Ctrl+Tab.
Windows-like bindings are the default; the second command selects Emacs.
Mouse selection, tab clicks, scrolling and menus work. Open/Save remain
disabled in the scratch preview, as is dictionary loading. F7 reports no
dictionary there. Clipboard
commands require an available data-device v3 and keyboard focus.
Unavailable commands show a notice; Escape/Ctrl+G dismisses it. Closing a
dirty tab refuses; undo to clean or close the window to discard all scratch
text. Dirty window close asks for Ctrl+D to discard everything, or
Escape/Ctrl+G to cancel. Killing the process still loses text: do not keep
anything important in this preview. It is not a usable `$EDITOR`.
No td compositor, GPU node,
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

`src/keyboard.rs` and the `xkb*` modules compile bounded, self-contained XKB
text-v1 maps into deterministic logical chords. Keycode aliases, symbols,
table-driven types, modifier maps and compatibility interpretations supply
level selection, virtual masks, consumed/preserved modifiers and repeat
eligibility. Tests cover the td map, all 26 types of a compiled ordinary US
map across 256 real-modifier combinations, and 106 US keys across the 32
supported states using independent libxkbcommon results. Fixture provenance
and oracle procedures are in [tests/fixtures/README.md](tests/fixtures/README.md).
The window now consumes keymap descriptors and calls this compiler before
accepting input. `seat.rs` supplies explicit-clock held-key and repeat policy.
Focus loss, modifier changes, map replacement and capability withdrawal cancel
repeat. Enter's already-held keys never synthesize presses. Protocol tests
send real descriptors and key events through both profiles, then check model
bytes and submitted pixels. This is not yet a live Weston input/pixel proof;
the optional Weston test above proves presentation only. The `$EDITOR`
warning still applies.

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

td-mail currently deletes its temporary draft and attachment files when its
editor child exits. Saving a draft in place does not retain it, and td-mail has
no mail submission path. The design describes this integration gap explicitly.
