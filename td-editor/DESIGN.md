# td-editor

td-editor is a small, Wayland-native text editor with a Notepad-like window
and tabs. It is intended for ordinary text and prose, including use as the
foreground `$EDITOR` child of tmc inside td-jail. It must also run on Linux
Wayland desktops outside td. This document is the component contract and
the starting point for successive agents; the root `AGENTS.md` and
`DEVELOPMENT.md` still govern changes and submission.

## Status and scope

This is the design foundation. No editor executable, Wayland compatibility,
GPU acceleration, spelling engine, or jail integration is implemented by
this document. Milestones below describe required behavior, not capabilities
that already ship. User decisions still pending are collected at the end.

The implementation uses Rust and `std`, with no external Cargo dependencies,
toolkit, libwayland, libxkbcommon, spell-check subprocess, language server,
network service, or plugin runtime. Font and dictionary files are data with
explicit provenance, licensing, and size limits. A new bundled dictionary is
a reviewed data input, not an undeclared host build dependency.

Production code must have no explicit panics or panicking indexing. Invalid
text, file failures, unsupported protocol input, exhausted resource budgets,
and stale commands return errors without silently losing document contents.
This is not a promise that Rust's allocator can recover from every process
out-of-memory condition. Resource admission and bounded work are required.

Wayland descriptor passing needs a small audited Linux boundary that stable
safe `std` does not supply. The document model and renderer remain safe Rust.
Before adding a syscall module, read and amend `UNSAFE.md`, define its exact
callers and descriptor ownership, and add source confinement tests. Reusing
another module does not transfer its unsafe authorization to a new crate.

## User interface

One process owns one window with a menu row, tab strip, document viewport,
and status row. Tabs show a filename (or Untitled), a dirty marker, and a
close affordance. The viewport owns scrolling and a visible caret and
selection. Menus expose File, Edit, Format, and Help; commands show the
active key profile's shortcuts. A path entry supports Open and Save As
without depending on a desktop file chooser or a portal service.

New, Open, Save, Save As, Close Tab, Quit, Undo, Redo, Cut, Copy, Paste,
Select All, Find, Find Next, Replace, Go To Line, Fill Paragraph, Auto Fill,
and Spelling are the intended basic command set. Keep optional features out
of the document area. Error messages stay visible until dismissed or
superseded by an explicit action. Failure to save must leave the document
dirty and accessible.

Closing a dirty tab or window asks Save / Discard / Cancel. Cancel preserves
the complete session. Explicit discard is the only ordinary close path that
may abandon edits. If saving one tab fails while quitting, quitting stops;
successfully saved tabs stay saved and remaining tabs stay open.

The two key profiles are complete alternatives over the same commands, not
two overlapping global maps. Windows-like bindings include Ctrl+N/O/S,
Ctrl+Shift+S, Ctrl+W, Ctrl+Tab, Ctrl+Z/Y, Ctrl+X/C/V, Ctrl+A/F/H, F3, and
Shift+F3. Emacs bindings include C-x C-f, C-x C-s, C-x C-w, C-x k,
C-x C-c, C-/, C-space, C-w, M-w, C-y, C-a/e/b/f/p/n, M-b/f,
C-s/r, M-q, and M-x auto-fill-mode. Prefix state is explicit; C-g cancels
prefixes, selections, searches, and dialogs without making an edit. The
precise initial subset must be listed in the README when it exists.

Text insertion follows keyboard layout translation before command dispatch.
Physical evdev positions must not stand in for letters on arbitrary host
layouts. Shortcut modifiers and text composition are distinct. Focus loss
cancels key repeat and pending key prefixes. Key repeat uses the compositor's
rate and delay with explicit time inputs for deterministic tests.

## Document model and file safety

An editor owns stable tab IDs. A document owns UTF-8 text, cursor, selection,
undo history, file association, saved revision, and per-document formatting
settings. Every mutation is a typed command with a single undo transaction.
Tabs retain independent cursors, selections, scroll positions, and histories.
Save acknowledgements carry the exact revision that was written; an edit
made during a save must not be marked saved by its eventual completion.

Begin with a bounded contiguous UTF-8 representation and measured limits.
Keep byte offsets private and validate UTF-8 boundaries on every external
position or range. Terminal cells are not document storage. Cursor motion,
deletion, rendering, and hit testing must agree about the supported text
units. If the first increment uses Unicode scalars, say so explicitly:
grapheme clusters, combining marks, wide glyphs, bidirectional layout,
shaping, and IME support are separate compatibility work, never silently
claimed by the ability to store UTF-8.

Loading must not replace invalid UTF-8 and later save the replacement over
the original. Detect and preserve UTF-8 BOM and the supported LF/CRLF
convention, including absence of a final newline. Mixed line endings require
an explicit preservation or refusal policy before implementation. Reject
binary or unsupported input with an actionable message. Open paths are OS
strings; non-UTF-8 filenames must not become different paths through display
conversion. `--` terminates options so filenames beginning with `-` work.

Saving normally creates a unique same-directory temporary file with exclusive
creation, writes the complete snapshot, syncs it, atomically renames it over
the intended destination, and syncs the parent directory. An unsuccessful
write must not truncate the original. Define symlink, hard-link, permission,
ownership, extended-attribute, and concurrent external-edit behavior before
shipping Save; metadata that cannot be preserved must not disappear without
an explicit decision. A check followed by rename is not compare-and-swap:
do not claim it excludes a writer racing between those operations.

The save adapter distinguishes failure before replacement from failure to
confirm durability after replacement. Both keep recoverable editor state;
the latter reports that the destination may already contain the new bytes.
Read-only directories and file-only jail grants can prevent atomic save even
when the file itself is writable. Do not silently fall back to truncation.

Admission limits cover bytes per document, total document bytes, tab count,
undo bytes, clipboard bytes, dictionary bytes, outstanding I/O, control
requests, and frame storage. Each refusal leaves state consistent. Avoid
whole-document copies on every keystroke; undo stores bounded edit deltas.
Expensive spelling and I/O work returns results tagged with tab and revision
so stale results cannot modify the wrong buffer.

## Paragraph filling and spelling

Soft wrapping is display-only. Auto Fill and Fill Paragraph insert real line
breaks and are independently selectable. A fill column is measured in the
same display columns used by layout, with a documented tab width. An
overlong word stays intact. Reflow is one undoable edit and does not consume
blank lines separating paragraphs. Cursor and selection mapping must be
defined and tested for the replaced range.

The first prose profile treats blank lines as paragraph boundaries and
retains consistent paragraph indentation. Auto Fill runs on a typed word
separator when the current line exceeds the fill column; it does not rewrite
an entire pasted file. Fill Paragraph explicitly reflows the paragraph at
the caret. Quoted mail, list prefixes, source-code comments, Markdown fences,
and language-aware paragraph rules require named extensions and tests.

Flyspell-like behavior means marking unknown words during editing, offering
bounded local suggestions, replacing a chosen word as one undoable command,
and supporting ignore-for-session and an explicit personal dictionary.
Absence of a dictionary disables checking with a visible status; it must not
mark every word wrong. No document text is sent off-machine.

A newline-delimited local UTF-8 word list is the proposed first dictionary
format. Case folding, apostrophes, hyphens, token boundaries, normalization,
and language coverage need an exact first profile. Dictionary membership is
not a claim of full Hunspell morphology or Emacs package compatibility.
Incremental checking prioritizes changed and visible text, with bounded work
per event-loop turn. Results carry the document revision and dictionary
generation. Suggestions have candidate and distance budgets.

## Rendering and reuse

The current compositor and terminal use CPU bitmap rendering into XRGB8888
buffers. GNU Unifont is a font choice, not evidence of GPU acceleration.
`APPLICATIONS.md` section M owns future hardware rendering; `td-jail`
currently refuses `devices=dri`. A client with only `sockets=wayland` cannot
assume a render node or a GPU API. A host compositor may accelerate its own
composition of an editor's shared-memory buffer without accelerating the
editor's rasterization.

The proposed first backend is persistent `wl_shm` buffers. Separate layout,
bitmap drawing, buffer submission, and completion so a future backend can
consume the same scene. Do not introduce a speculative general graphics
framework. A submitted buffer is immutable until `wl_buffer.release`;
`wl_callback.done` throttles frames and is not permission to reuse a buffer.
Resizes retain old busy buffers within an explicit budget. Damage, clipping,
and deterministic integer scaling belong in the renderer contract.

Relevant code in `td-compositor/src`:

| File | Reuse decision |
| --- | --- |
| `font.rs`, `font_data.rs` | Reuse the checked PSF2 decoder and pinned Unifont face; carry font provenance and license into standalone packaging. |
| `wire.rs` | Candidate shared Wayland framing codec, with existing malformed-input tests. |
| `conn.rs` | Reuse object allocation, framing and descriptor-lifetime lessons; remove terminal and exact-keymap coupling before sharing. |
| `term_client.rs` | Reference for configure/ack, release, resize, clipboard and focus lifecycle; do not fork the terminal loop into the editor. |
| `render.rs` | Reuse bounded glyph drawing and pixel-oracle approach, not terminal `Snapshot`/SGR data structures. |
| `socket.rs` | Reference for explicit socket lifecycle and refusal of live endpoints; editor control must enforce its own path ownership. |
| `keys.rs`, `keyboard.rs` | Reuse repeat/chord concepts and td test fixtures; terminal escape sequences and the fixed US keymap are not portable editor input. |
| `buffer.rs` | Compositor surface-storage and accounting design reference, not an editable text buffer. |
| `ui.rs` | Reference for pure rendering and input models, not a toolkit or the editor's state model. |

Prefer a small deliberate shared source boundary over copying large modules
or pulling the compositor binary into the editor. Any shared-file move is an
atomic migration that updates source staging, affected-check mappings, tests,
and every consumer in the same increment. The standalone package must build
from its documented source bundle without an installed td system.

## Wayland and host compatibility

Use core `wl_compositor`, `wl_shm`, `wl_seat`, and `xdg_wm_base`; clipboard
uses core `wl_data_device_manager` when available. Bind advertised versions
within implemented bounds and allocate object IDs densely. Missing optional
globals disable their feature. Missing required globals produce a named
error. Bound all wire messages and received descriptor queues; clean up
descriptors on parse errors and disconnects. Answer shell pings while I/O or
spelling is in progress. A configure with zero dimensions uses a safe default.

Resolve normal Wayland environment conventions, including an absolute or
relative `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, and inherited `WAYLAND_SOCKET`.
Descriptor adoption for the latter must be included in the audited boundary.
No compositor-private global, readiness socket, `/dev/input`, `/dev/fb0`, td
account database, fixed UID, or td-specific environment variable is required.

The terminal currently validates its received keymap against the exact
`keyboard::XKB_KEYMAP` bytes and refuses other compositors. That cannot be
the editor's portability claim. Before the portable UI milestone, implement
a bounded, explicitly documented subset of compositor-supplied XKB text maps
with real host fixtures, or obtain approval for a different dependency
policy. Unsupported map constructs must produce a clear diagnostic, never
silently translate a non-US map as US. Broad keyboard-layout and IME support
is substantial work under the zero-dependency constraint.

## `$EDITOR`, tmc, and td-jail

The command contract is `td-editor [options] -- [file ...]`. It opens the
requested paths in tabs and stays in the foreground until the invocation's
window closes. No implicit daemon, single-instance forwarding, shell
interpretation of filenames, terminal input requirement, or background fork.
This lets a caller set `EDITOR` to an executable path and wait normally.
Exit 0 means the user completed the session (including an explicit discard),
not that every file was saved. Invocation, open, and fatal runtime failures
have nonzero status and diagnostics on stderr.

The editor inherits the caller's filesystem namespace, working directory,
and Wayland connection environment. It never escapes a jail to find a host
editor or file. `sockets=wayland` grants the display connection only: it does
not install the executable, provide its runtime closure, grant file access,
share a control socket, or enable GPU devices.

The caller inspected for this design is `~/src/tmc`. Its `src/tui/mod.rs`
selects `[ui].editor`, then `$EDITOR`, then `vi`; `spawn_editor` starts
`sh -c` with the editor command and displayed draft path concatenated into
one string. The TUI continues immediately. A background
thread waits for the shell child, ignores its exit status, and removes both
the draft and any attachment directory. tmc neither rereads the saved file
nor submits mail. Consequently a normal Save followed by Quit loses the
temporary draft to caller cleanup; retaining it requires Save As to a
persistent granted path. This editor must not be described as a complete
tmc mail-composition workflow until draft retention/submission is resolved.

`src/compose.rs` creates mode-0600 `.eml` files inside a mode-0700 directory,
preferring `$XDG_RUNTIME_DIR/tmc/drafts`, then the XDG state directory. The
draft format includes mail headers, `--text follows this line--`, and
potential MML attachment tags pointing to temporary sidecar files. Preserve
these bytes as ordinary text. Saving the draft elsewhere does not preserve
the referenced attachment files when tmc later removes them. Recognizing,
retaining, or submitting mail is a separate requested product capability.

tmc's unquoted shell concatenation also means paths containing shell syntax
or spaces are not passed as literal argv today. td-editor can accept such
paths correctly but cannot repair a command already misparsed by its parent.
A tmc integration change must resolve argument construction at the caller;
do not work around it by evaluating shell text inside the editor. The editor
must avoid consuming the TUI's inherited terminal input.

An integration increment must make the executable and exact runtime closure
available inside the jail in which tmc runs, set its explicit `EDITOR` environment,
and provide the intended file/directory grants. `APPLICATIONS.md` section X.4
currently says source-built td store closures are absent from the jail, so
this requires an actual packaging/layout decision; a host `/bin/td-editor`
path is insufficient. Keep source-built editor artifacts distinct from
marked foreign application payloads.

The caller's real launch path is the acceptance test: launch tmc, request a
draft, observe an editor frame, edit and save while its child remains live,
and verify exact saved bytes before caller cleanup. For retention, exercise
Save As to a persistent granted directory, close the window, and prove that
tmc remains responsive, its temporary draft is cleaned up, and the retained
copy survives. Attachment retention and submission need their own agreed
oracle. Include filenames with spaces and leading dashes after correcting
the caller, unwritable paths, cancellation, missing display, and an attempted
path outside the grant. An isolated Wayland smoke test alone is not evidence
that tmc's jail can launch the editor.

## Test and control architecture

One command dispatcher drives interactive input, menus, replay tests, and
remote commands. A semantic snapshot exposes tab IDs, revisions, text,
cursor/selection, dirty state, modes, pending dialogs, spelling marks, and
viewport geometry. Layout supplies both drawing and hit testing. Testing
must not mutate private fields to bypass validation or file-safety decisions.

Headless replay injects commands, translated keys, pointer events, sizes,
clock advances, and I/O completions, and observes semantic state and pixels.
Keep state transitions pure; filesystem and Wayland adapters return typed
events. Tests cover UTF-8 boundaries, undo/redo and saved revisions, repeated
fill idempotence, spell result invalidation, tab isolation, and errors that
leave the document unchanged. Use real temporary-file tests for save failure
and exact round trips, wire fixtures for fragmented messages and descriptor
ownership, and deterministic pixel fixtures for selection, tabs, wrapping,
spelling underlines, dialogs, and extreme resize/clipping.

Remote control is an explicit opt-in local Unix socket with a versioned,
bounded protocol. It is off by default, under a verified private directory,
with mode 0600, no TCP listener and no compositor control dependency. Socket
creation must refuse unrelated files and live endpoints. A client has a
whole-request deadline and bounded response size; a slow client must not
stall the UI. Commands use stable tab IDs and expected revisions to reject
stale destructive edits. Text and filenames need an unambiguous length or
escape encoding, including embedded newlines. Responses distinguish command
completion from the frame that later presents it; a frame-wait operation is
needed for screenshot and integration oracles.

The control endpoint grants read/write access to all this editor's documents
within its existing authority. It cannot bypass dirty-close confirmation or
file conflict policy by accident; any explicit discard/overwrite command is
named and tested. Do not expose arbitrary shell execution. Sharing control
across the jail boundary is a separate explicit grant, not an implication of
`sockets=wayland`.

Host protocol proof must include td-compositor and at least one independent
Wayland compositor with its real keymap. Distinguish headless model tests,
fake-server protocol tests, independent-compositor tests, and the full td
jail/image oracle in every readiness claim. A new standalone crate joins
td-builder's automatic cargo test/clippy gate and commits its one-package
`Cargo.lock`. Target recipe/image work also owes the profiler contract.

## Independently landable increments

1. Design and a tested safe editor core: text transactions, tabs, undo/redo,
   key profiles, paragraph filling, dictionary profile, and headless command
   replay. Record exact limits and implemented commands in the README.
2. First usable Wayland window: shared transport decisions and audited
   descriptor surface, bitmap rendering, input, open/save, prompts and
   clipboard; deterministic protocol and pixel proof. Portable keyboard
   support is an explicit acceptance condition, not a later hidden fix.
3. Interactive spelling and complete local control: responsive incremental
   checks, suggestions, personal dictionary, semantic queries and frame
   synchronization. Exercise the production dispatcher through both inputs.
4. Source-built recipe and tmc jail integration: staged shared sources and
   data licenses, runtime closure, file grants, `$EDITOR`, debug companions,
   and a test of the actual caller's child lifetime and draft cleanup.
5. Further portability and graphics: more layouts, grapheme/IME work and
   hardware rendering only alongside the required graphics/jail contracts.

## Decisions awaiting the user

- First requested deliverable: design only, tested core, or a usable UI.
- Whether software bitmap rendering is acceptable for the first version.
- Default key profile; both Windows-like and Emacs remain requirements.
- Initial dictionary/language profile and local word-list acceptance.
- Whether this project includes mail draft retention/submission or stays a
  general editor; tmc's source is located, but its td jail package is not yet
  present in this checkout.
