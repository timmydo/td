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
profile is designed but not implemented in this core increment.

## Implemented core

The safe, dependency-free library implements UTF-8/BOM/LF/CRLF conversion,
scalar edits and selection, tabs, bounded undo/redo with saved-state tracking,
literal search/replace, paragraph filling and Auto Fill. Logical Windows and
Emacs keys share those commands. Navigation that needs viewport layout,
dialogs, clipboard, spelling and actual file I/O produces an explicit adapter
request. No Wayland window, GPU renderer, filesystem Open/Save, remote socket
or tmc integration is claimed yet. Do not set `$EDITOR` to this binary yet.

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
logical chords; and `replay.rs` drives the model with framed commands.
`tests/core.rs` covers byte round trips, stale/invalid commands, limits,
save completion after intervening edits, global history eviction, reflow
mapping, key-profile conflicts and generated edits against a scalar-vector
reference. It also launches the real replay executable without a display.

Editor-only code/configuration changes are routed by `td-builder ready` to
this crate's tests and Clippy, with the dependency-free lock checks retained.
Documentation-only changes keep the normal docs-only waiver. Neither runs
unrelated workspace tests or bootstrap/image gates. This is valid while no
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
actions, `prefix` for pending C-x, or `request NAME` for a translated action
whose adapter is absent. The latter is **not** a save, clipboard transfer,
spelling check or visible dialog. `close-tab` refuses dirty text; no wire
command can mark a buffer saved. Unknown/malformed commands are refused.

`state` returns `active=ID` (0 means no tab), `keys=PROFILE`, `prefix=0|1`, then one
`tab=ID,REVISION,DIRTY,BYTES,ANCHOR,CARET,AUTO_FILL,FILL_COLUMN,BOM,ENDING`
field per tab; ENDING is `lf` or `crlf`. Text pages never split scalars.
Use the returned revision in subsequent commands; undo and redo advance it.
Selection and formatting-mode changes leave the text revision unchanged.

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
