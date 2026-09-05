# td-editor

td-editor is a small, Wayland-native text editor with a Notepad-like window
and tabs. It is intended for ordinary text and prose, including use as the
foreground `$EDITOR` child of tmc inside td-jail. It must also run on Linux
Wayland desktops outside td. This document is the component contract and
the starting point for successive agents; the root `AGENTS.md` and
`DEVELOPMENT.md` still govern changes and submission.

## Status and scope

The safe document core and `td-editor --replay` are implemented. They cover
UTF-8/file-format conversion, scalar edits and selection, bounded tabs and
undo/redo, save-snapshot state tracking, literal search/replace, paragraph
filling, Auto Fill, and logical Windows/Emacs key dispatch. The core opens no
files and reads no environment or clocks. File baselines/metadata, actual
save I/O, layout/vertical motion, Wayland, GPU rendering, spelling and the
control socket are not implemented yet. Key bindings for those adapters
produce explicit requests; replay does not pretend to perform their work.

The rules below define version 1; milestones identify the
order of implementation, not choices left to each implementing agent.
The deliverable is a usable editor, reached through independently tested
core, UI, and integration increments. The separate GPU prerequisite is
described under Rendering and reuse.

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

Windows-like is the default. `--keys=windows|emacs` selects the profile at
startup and Edit > Key Bindings changes it for the whole window, cancelling
any pending prefix. The two profiles are complete alternatives over the same
commands, not two overlapping global maps. Windows-like bindings include
Ctrl+N/O/S,
Ctrl+Shift+S, Ctrl+W, Ctrl+Tab, Ctrl+Z/Y, Ctrl+X/C/V, Ctrl+A/F/H, F3, and
Shift+F3. Emacs bindings include C-x C-f, C-x C-s, C-x C-w, C-x k,
C-x C-c, C-/, C-space, C-w, M-w, C-y, C-a/e/b/f/p/n, M-b/f,
C-s/r, M-q, and M-x auto-fill-mode. Prefix state is explicit; C-g cancels
prefixes, selections, searches, and dialogs without making an edit. The
bindings above are required for version 1. F7 invokes Check Spelling in
both profiles; Emacs additionally exposes `M-x ispell-buffer`. The M-x prompt
accepts the named editor commands, with completion; it is not an interpreter
or an Emacs Lisp interface. Ctrl+Shift+Tab selects the previous tab in both
profiles. Common navigation keys and Shift-selection work in both profiles.

Find and Replace use literal, case-sensitive UTF-8 strings, without regular
expressions. Search reports reaching the end before an explicit next search
wraps; Replace All is one undo transaction and skips overlapping matches.
When matches exist, Replace All collapses the selection at document end;
Undo restores its original endpoints. Windows Escape cancels a pending action
without clearing the document selection; Emacs cancellation clears the mark.
Go To Line uses one-based logical lines, independent of soft wrapping. The
status row shows line, display column, line-ending mode, fill mode, key
profile, and spelling status. A missing search match changes no selection.

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

Version 1 stores text in a contiguous UTF-8 `String` and uses Unicode scalars
as its editing units. Left/Right and Backspace/Delete move or remove one
scalar; selections are half-open byte ranges whose endpoints must be scalar
boundaries. Commands with an invalid boundary fail before changing state.
Each non-tab, non-newline scalar occupies one 8x16 font cell. Tabs advance to
the next eight-column stop; newlines advance the logical line. A combining
mark gets its own cell, a missing or double-width glyph gets the visible
replacement glyph, and all text is laid out left to right. Grapheme editing,
bidi, shaping, wide cells, compose sequences and IME input are outside
version 1. Bytes remain intact even when their visual presentation is limited.

Up/Down move between visual rows, preserving a desired display column clamped
to the target row; a hit inside a tab chooses the nearest endpoint, with
ties before it.
Home/End address logical lines. Page movement uses the current viewport's
visible rows. Soft wrap is on by default and wraps at the last fitting space
or tab, or at a scalar boundary if there is none; it inserts no bytes. Layout
produces the single position map used for drawing, selection and hit testing.

Files must be valid UTF-8, optionally starting with one UTF-8 BOM. Reject NUL,
C0 controls other than Tab/LF/CR, DEL, and bare CR. Strip the initial BOM into
a retained flag and normalize CRLF to LF in memory. Accept uniformly LF or
CRLF files; reject mixed line endings without opening an editable tab. New
files and files with no newline use LF. Save restores the original convention
and BOM and adds no final newline. Paste normalizes CRLF to LF and rejects
the same unsupported controls atomically; it cannot change the file's mode.
Repeated initial BOMs are refused on load.
An initial U+FEFF inserted through editing is refused, since it would become
a BOM on reopen; interior U+FEFF remains ordinary text.

Open paths are `OsString`/`PathBuf`; display escaping never changes a path.
`--` terminates options. Opening an already associated file selects its tab
using file device/inode identity. Only regular files are opened; devices,
directories, sockets and FIFOs are refused. A missing file opens an empty dirty tab
associated with that path; other initial open failures report nonzero status
before the window is created. An interactive open failure keeps existing tabs.

Saving creates a unique same-directory temporary file with exclusive
creation, writes the complete snapshot, syncs it, atomically renames it over
the intended destination, and syncs the parent directory. An unsuccessful
write must not truncate the original. New files start mode 0600. Existing
destinations must be regular files with one hard link, no setuid/setgid bits,
and no extended attributes (including extended ACLs). Preserve their owner,
group and permission bits; refuse replacement if the temporary inode cannot
match them. Refuse symlink destinations and offer Save As; reject symlinks
when opening too, so association and later saving use the same rule.
Extended-attribute inspection belongs in the future audited file adapter;
safe `std` alone does not expose it, and Save must not silently skip this
check. An unsupported attribute query refuses replacement. Save As to a new
path remains available for files whose metadata is outside this profile.

Before replacing an existing destination, reread and compare its device/inode,
owner/group, mode, link count, length, mtime/ctime and complete bytes against
the last load/save baseline; atime is excluded because reads may change it. A mismatch
opens a conflict prompt with Reload / Save As / Cancel; Reload requires
explicit discard of dirty text. There is no force-overwrite command in
version 1. Save As refuses an existing destination. Publishing a previously
absent path uses a same-filesystem hard link from the complete temporary
inode and then removes the temporary name, so a concurrently created file
is never overwritten. Both paths sync the parent after publication.

The existing-file check followed by rename is not compare-and-swap and
cannot exclude a writer racing between those operations. The file adapter
assumes the destination directory is not being maliciously replaced by
another process with the user's authority. Jail filesystem grants remain
the access-control boundary; editor path checks do not create a second jail.

The save adapter distinguishes failure before replacement from failure to
confirm durability after replacement. Both keep recoverable editor state;
the latter reports that the destination may already contain the new bytes.
Read-only directories and file-only jail grants can prevent atomic save even
when the file itself is writable. Do not silently fall back to truncation.

Version 1 resource ceilings are part of the API:

| Resource | Limit and overflow behavior |
| --- | --- |
| Documents | 64 tabs, 16 MiB encoded bytes per file, 64 MiB total live UTF-8 text; refuse the whole open/edit exceeding a limit. |
| Undo/redo | 64 MiB of edit payloads and 4,096 transactions per window; evict oldest complete transactions and any redo states depending on them, never a partial transaction. |
| File I/O | One worker, one in-flight job and eight queued job descriptors; reject additional jobs. Capture one immutable 16 MiB snapshot when a job starts, plus one 16 MiB encoded output at a time. |
| Saved baselines | At most 64 MiB encoded file bytes across tabs, charged separately from live text; refuse an open/save needing more. |
| Clipboard | 1 MiB per transfer; reject an oversized paste atomically. |
| Dictionary | 16 MiB input, 250,000 distinct entries, 64 ASCII letters/apostrophes per entry; reject an oversized or malformed load. |
| Spelling results | 10,000 stored ranges; finish scanning, count additional unknown words, and report that only the first 10,000 are marked. |
| Frames | 8,192 pixels per axis, 32 MiB per XRGB buffer, three live buffers; defer redraw/resize until a buffer can be retired. |
| Wayland input | 4 MiB keymap, 256 KiB buffered wire bytes, eight pending descriptors; exceeding a bound closes the display connection with an error. |
| Control | One active connection, 16 queued commands, 1 MiB request/response frame, 256 KiB raw text per response page, five-second whole-request deadline. |

Undo stores edit deltas and cursor/selection before and after the transaction.
The core uses one contiguous replacement span per transaction, trimming
unchanged scalar prefixes and suffixes. Replace All spanning distant matches
also retains the intervening bytes; its payload remains charged to the same
global budget. Evicting an old undo entry does not invalidate redo from the
current live state; evicting the next redo entry removes its dependent branch.
Each typed scalar, paste, replacement, fill, or delete command is one
transaction; typing coalescing is outside version 1. Content-state IDs are
retained by undo/redo so undoing to a saved state clears dirty status. A
new edit clears that tab's redo branch. Evicted history cannot be recovered,
but eviction never changes live text or the saved-state ID. A
separate monotonically increasing revision changes on every text transition,
including undo/redo, and rejects stale asynchronous/control results. IDs are
checked `u64` counters; exhaustion refuses the operation. Save completion
records the content-state ID of its snapshot, not the then-current state.

An untouched New/Untitled tab is clean; editing makes it dirty, and undoing
back to its initial state makes it clean again. This differs from opening a
missing pathname, which the future file adapter must mark dirty.

The first core increment exposes `save_snapshot`/`acknowledge_saved` as the
future file adapter's contract; it stores the saved content-state ID but no
file baseline. Snapshot tokens are bound to the originating editor instance;
another instance cannot acknowledge them even if its tab/state IDs match.
The adapter must serialize saves per tab and acknowledge only
after the captured bytes have been written. The replay wire cannot synthesize
save acknowledgements or discard dirty tabs. Replay EOF ends the in-memory
test session, with no persistence claim. `load` is a replay-only byte-fixture
operation, not filesystem Open. See README for the implemented wire subset.

## Paragraph filling

Auto Fill and Fill Paragraph insert real line breaks; soft wrapping only
changes display. Auto Fill is off by default, per document. The fill column
defaults to 72 and accepts integers from 20 through 240. Columns follow the
scalar-cell and eight-column tab rules above. An overlong word stays intact.

A paragraph is the maximal run of nonblank logical lines with exactly the
same leading space/tab byte prefix. A blank line contains only spaces/tabs.
The caret selects its current logical line; on a blank line Fill Paragraph
does nothing. Version 1 has no special mail quote, list, source comment or
Markdown syntax: their non-whitespace prefix characters are ordinary words.
Auto Fill remains off unless explicitly enabled, including for `.eml` files.

Fill removes the shared indentation for word splitting, treats runs of ASCII
space/tab/newline as separators, and joins words with one ASCII space. It
greedily places each whole word on the current line if its ending column is
at most the fill column; otherwise it starts a line with the original
indentation. The first word always fits by itself, even when it exceeds the
column. Trailing horizontal whitespace is removed; the paragraph's final
newline and surrounding blank lines are preserved exactly. A selection does
not change which paragraph is filled. Repeating Fill is byte-idempotent.

Reflow records the original-to-new offset of each preserved word scalar.
Cursor and both selection endpoints inside words follow those scalars;
endpoints in collapsed separators go before the next word, or after the last
word when there is no next word. Endpoints in indentation clamp to the same
indent column on the first output line. Endpoints outside the replacement
shift by its byte-length delta. Filling is one undo transaction restoring
the exact original bytes and selection; a no-op creates no history entry.

Auto Fill runs only after a typed ASCII space or Tab, never after paste,
remote text insertion, file loading, or an automatic replacement. If the
caret's current line exceeds the fill column, greedily wrap that line using
the same indentation and word-width rules. Unlike Fill Paragraph, retain
horizontal whitespace, replacing only the final space/tab before a wrapped
word with newline plus the original indentation. Extra separators remain as
trailing whitespace on the preceding line; trailing whitespace may exceed
the fill column. Retain the trailing typed separator so typing the next word
remains separated. An interior typed separator remains a separator or becomes
a line break, never a collapsed no-op. Do not pull text from
the next logical line. The inserted separator and any resulting wrap form
one transaction. A limit failure refuses that entire typing transaction.

## On-demand spelling

Check Spelling scans the entire active document only when explicitly invoked
from Format > Check Spelling, F7, `M-x ispell-buffer`, or the control API.
There is no spelling mode, idle timer, check-on-save, or checking while the
user types. Unknown words receive an underline and appear in a navigable
results list after the scan completes. Next/Previous Misspelling select the
corresponding range; correction is ordinary text editing. Automatic
suggestions and replacement dictionaries are outside version 1.

The scan uses the current tab/revision and dictionary generation and runs in
chunks of at most 4,096 scalars per event-loop turn. Results publish together
at completion, never partially while scanning. A text edit cancels an active
scan, removes that tab's existing marks, and sets Spelling: not checked.
Cursor motion, scrolling, switching tabs and saving do not invalidate marks.
Undo is a text edit for this purpose. Changing the dictionary cancels scans
and clears marks in all tabs. Starting a second check cancels the old scan;
Escape/C-g cancels a scan and leaves the document unchanged. At most one scan
is active per window; checking holds no second full-document copy.

Version 1 uses an explicitly selected local English word list supplied by
`--dictionary PATH` or Format > Dictionary. No word list is bundled,
downloaded, or found by probing host directories. No dictionary means
Spelling: no dictionary, and Check Spelling reports that status without
marking words. A malformed replacement dictionary leaves the previous one
selected. No document text leaves the machine.

The dictionary is UTF-8 with optional initial BOM, LF or CRLF records, and
an optional final newline. Blank records are ignored. Each nonblank record
must contain ASCII letters and may contain ASCII apostrophes only between
letters; surrounding whitespace and other bytes are errors. Entries are
folded to ASCII lowercase, deduplicated and held in a sorted vector. An empty
dictionary is refused. Matching
uses binary search. There is no stemming, affix expansion, Unicode
normalization or Hunspell compatibility claim.

Document tokens are maximal runs of Unicode alphanumeric scalars, allowing
ASCII apostrophe or U+2019 between letters. Hyphens and underscores separate
tokens. Only tokens entirely composed of ASCII letters and those internal
apostrophes are checked; normalize U+2019 to ASCII apostrophe and lowercase
ASCII before lookup. Tokens containing digits, non-ASCII letters, or more
than 64 scalars are counted as skipped and never marked wrong. Other marks
and punctuation delimit tokens. This is an English ASCII spelling profile,
not a language detector. The result reports checked, unknown and skipped
counts, its revision, and whether the stored-mark ceiling was reached.

There is no writable personal dictionary in version 1. Users edit their
chosen word-list file with the editor and explicitly reload it through the
Dictionary command. Spelling results never change document bytes or history.

## Rendering and reuse

The current compositor and terminal use CPU bitmap rendering into XRGB8888
buffers. GNU Unifont is a font choice, not evidence of GPU acceleration.
`APPLICATIONS.md` section M owns future hardware rendering; `td-jail`
currently refuses `devices=dri`. A client with only `sockets=wayland` cannot
assume a render node or a GPU API. A host compositor may accelerate its own
composition of an editor's shared-memory buffer without accelerating the
editor's rasterization.

The mandatory reference backend rasterizes Unifont into persistent `wl_shm`
buffers. It is the deterministic test backend and the fallback on machines
without usable GPU access; it does not satisfy the GPU-acceleration objective
by itself. Layout emits clipped solid rectangles and bitmap-glyph draws with
integer coordinates, foreground/background colors and scale 1, 2, 3 or 4.
Font scale defaults to 1 and is user-selectable. Each backend consumes those
same operations. No font discovery, antialiasing or fractional scaling is
part of version 1. A frame uses one scale throughout.

Draw only the visible viewport and damaged chrome; clip every operation to
the current surface. Coalesce redraws behind one outstanding frame callback.
A submitted buffer is immutable until `wl_buffer.release`;
`wl_callback.done` throttles frames and is not permission to reuse a buffer.
On resize, retain old busy buffers within the three-buffer budget and render
only the latest configured size when a slot becomes free. Initial client
size is 800x600 pixels. A zero configure dimension retains that axis's
current size; a dimension outside the resource ceilings closes the connection
with a diagnostic. Rendering failures must not mark any document saved.

### GPU access and the Firefox prerequisite

GPU access is implementable in td-jail. The intended grant is `devices=dri`
with a selected `/dev/dri/renderD*` device, in addition to `sockets=wayland`.
Render nodes allow rendering without DRM master or modesetting authority;
the grant must not expose `/dev/dri/card*`, framebuffer or input devices.
The current refusal is policy/implementation state, not a Linux or Wayland
restriction. See the [kernel render-node contract](https://www.kernel.org/doc/html/latest/gpu/drm-uapi.html#render-nodes).

`APPLICATIONS.md` section M remains the normative owner of this system work.
The required increments for Firefox are:

1. Add an explicit render-node grant to permission validation, launch plans,
   device verification and mounts. Expose only the selected character device
   and its necessary read-only sysfs discovery paths. Verify host access
   permissions before launch; no grant exposes no GPU device. Existing
   seccomp policy admits rendering ioctls, but device access still adds the
   GPU driver to the application's reachable kernel surface.
2. Supply and verify the matching GPU userspace driver in Firefox's marked
   application runtime. Make the compiled Freedesktop 25.08 policy stop
   forcing `LIBGL_ALWAYS_SOFTWARE=1` for the GPU profile; retain the tested
   software profile. A QEMU GPU test also needs a 3D-capable virtual device
   and host rendering path; the current fbdev configuration is insufficient.
   [Mesa's VirGL design](https://docs.mesa3d.org/drivers/virgl.html) describes
   the guest-driver/host-renderer split.
3. Implement and test compositor DMA-BUF import, supported formats/modifiers,
   synchronization and buffer leases. It must handle normal overlapping
   windows as well as fullscreen. Section M requires a reliable CPU-mappable
   composition path before advertising DMA-BUF; direct scanout alone cannot
   satisfy that contract. DRM/KMS output, direct scanout, client GPU drawing
   and compositor GPU drawing are separate capabilities.
4. Prove the pinned Firefox runs with hardware WebRender on the granted GPU,
   presents correct frames during overlap and resize, releases buffers, and
   retains its sandbox. Check `about:support` for actual backend/adapter;
   a visible window or a GPU-process name is insufficient. Test denied-device
   and software-profile paths separately. Hardware video decoding is a
   separate capability and is not implied by this rendering test.

Current upstream Firefox's native Wayland compositor uses GBM/DMA-BUF and
checks the DMA-BUF and viewporter capabilities; the pinned td runtime must be
tested in its own right. See [Firefox's platform implementation](https://searchfox.org/firefox-main/source/gfx/thebes/gfxPlatformGtk.cpp).

This path can accelerate Firefox through its foreign runtime's driver stack.
It does not give dependency-free td-editor an OpenGL/Vulkan implementation:
a render node is a driver interface, not a portable bitmap-drawing API.
td-editor must not link or load the foreign runtime's Mesa, which would also
cross td's source-built/foreign-payload boundary. Its GPU producer requires
a separately specified source-built graphics implementation consistent with
the zero-dependency requirement. No general GPU driver or new library is
authorized by this document. Until that system design exists, only the
reference renderer is implementable here and GPU editor rendering remains
an explicit unmet objective, not a silently dropped requirement.

Relevant code in `td-compositor/src`:

| File | Reuse decision |
| --- | --- |
| `font.rs`, `font_data.rs` | Reuse the checked PSF2 decoder and pinned Unifont face; carry font provenance and license into standalone packaging. |
| `wire.rs` | Reuse the existing framing codec as shared source, including its malformed-input tests. |
| `conn.rs` | Reference for object allocation and descriptor lifetime; keep the editor connection adapter separate because this module imports terminal rendering and td's exact keymap. |
| `term_client.rs` | Reference for configure/ack, release, resize, clipboard and focus lifecycle; do not fork the terminal loop into the editor. |
| `render.rs` | Reuse bounded glyph drawing and pixel-oracle approach, not terminal `Snapshot`/SGR data structures. |
| `socket.rs` | Reference for explicit socket lifecycle and refusal of live endpoints; editor control must enforce its own path ownership. |
| `keys.rs`, `keyboard.rs` | Reuse repeat/chord concepts and td test fixtures; terminal escape sequences and the fixed US keymap are not portable editor input. |
| `buffer.rs` | Compositor surface-storage and accounting design reference, not an editable text buffer. |
| `ui.rs` | Reference for pure rendering and input models, not a toolkit or the editor's state model. |

Version 1 shares `font.rs`, `font_data.rs` and `wire.rs` through explicit
source-module paths, as td-portal already does. It neither copies those
modules nor depends on the compositor binary. The source bundle is the td git
checkout; `cargo build --manifest-path td-editor/Cargo.toml` will build the
standalone binary without an installed td system. The target recipe must
stage those exact shared sources and licenses, and shared-source changes
must select editor tests in affected-checks. A future move of a shared file
updates staging, check mappings and all consumers atomically.

## Wayland and host compatibility

Use core `wl_compositor`, `wl_shm`, `wl_seat`, and `xdg_wm_base`; clipboard
uses core `wl_data_device_manager` version 3 when available. Bind
`wl_compositor` at version 4, `wl_shm` at 1, `xdg_wm_base` at 1, and
`wl_seat` at the highest available version from 5 through 7. Lower required
versions are refused; higher advertised versions are capped. Allocate
object IDs densely. Missing optional globals disable their feature.
Missing required globals produce a named
error. Bound all wire messages and received descriptor queues; clean up
descriptors on parse errors and disconnects. Answer shell pings while I/O or
spelling is in progress. Configure dimensions follow the rendering rules above.

Resolve normal Wayland environment conventions, including an absolute or
relative `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, and inherited `WAYLAND_SOCKET`.
Descriptor adoption for the latter must be included in the audited boundary.
No compositor-private global, readiness socket, `/dev/input`, `/dev/fb0`, td
account database, fixed UID, or td-specific environment variable is required.

Version 1 supports the US English keyboard layout, including Shift, Caps
Lock, Control and Alt, under td-compositor and an independent compositor
configured with its ordinary US keymap. It accepts the supplied self-contained
XKB text-v1 map by meaning, not equality to `keyboard::XKB_KEYMAP` bytes.
The bounded parser reads keycodes, modifier assignments, key symbols and
table-driven XKB types: their modifier mask, level maps and preserve masks.
It resolves virtual modifiers, including NumLock, to real masks from the
supplied map; it does not whitelist type names or assume fixed modifier bit
positions. This covers the ordinary US map's alphabetic, keypad and function
key types, including Ctrl+Alt function-key levels. Unhandled keysyms at those
levels are ignored rather than interpreted as text or system commands.
The compositor's modifiers event is authoritative for depressed, latched
and locked state; the client never executes XKB actions such as VT switching.

Additional declarations for unused keys/types are parsed and ignored.
Unsupported symbol-selection semantics on a used key, redirect actions,
or additional layout groups refuse keyboard activation with the exact item
named. Includes are refused: no keymap file is loaded from the host. The
whole map is validated before accepting text input. A later unsupported map
cancels repeat and disables keyboard input while retaining documents and
pointer/menu access. Limit the parser to 200,000 tokens, nesting depth 32,
768 keycodes, 256 types and 16 levels per key/type; overflows refuse the map.

The initial translated set is ASCII printable text, Tab/Enter, navigation
and editing keys, F1-F12 and the profile's shortcut keys. Caps affects letters;
Num Lock selects digits versus navigation on the keypad. Non-US layouts,
AltGr levels, dead keys and compose/IME input are outside version 1 and are
diagnosed, never substituted with US physical-key translation. UTF-8 outside
ASCII remains editable through files, clipboard and semantic Insert commands.
The required independent-compositor fixture is Weston with its default US
map; a serialized fixture and live input/pixel test must both pass before
claiming host compatibility. Weston is a test environment, not a runtime or
target build dependency.

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

td-editor is a general text editor. Version 1 neither submits mail nor
interprets MML, starts a mail transport, or manages attachment lifetimes.
Those are outside this editor increment. Save As is the explicit way to
retain draft text before the caller removes its temporary file.

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
available inside the jail in which tmc runs, set its explicit `EDITOR`
environment, and provide the intended file/directory grants.
`APPLICATIONS.md` section X.4 currently says source-built td store closures
are absent from the jail, so
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

`--control-socket PATH` enables remote control. It is off by default, binds
a local Unix socket with mode 0600 under a caller-owned mode-0700 directory,
and has no TCP listener or compositor control dependency. Refuse symlinked
socket parents and any existing endpoint, including stale sockets; the caller
removes stale endpoints explicitly. Cleanup removes only the socket inode
this invocation created. A control worker handles framing and deadlines,
sending bounded typed messages to the UI thread; socket reads and writes
never hold the model lock or stop Wayland dispatch.

Each connection carries one request and one response, then closes. A frame
starts with a four-byte big-endian payload length, followed by exactly that
many bytes, within the one-MiB ceiling. The payload is an ASCII record with
tab-separated fields and no terminating newline. Its first fields are
protocol version `1`, caller-supplied decimal request ID, and command name.
Integers are unsigned decimal with checked conversion. Text and OS path
arguments are lowercase hex-encoded bytes; `-` denotes an empty byte string.
Text arguments must decode to valid UTF-8. Reject missing/extra fields,
unknown commands/versions, bad hex, overflow and truncated frames before
dispatch. A response echoes version/request ID, then `ok`, `error`, or
`pending`; errors carry a stable code and hex-encoded diagnostic.

Version 1 exposes `state`, `text`, `new`, `open`, `select-tab`, `select-range`,
`insert`, `delete`, `undo`, `redo`, `find`, `replace`, `fill-paragraph`,
`set-auto-fill`, `set-fill-column`, `set-key-profile`, `check-spelling`,
`spelling-results`, `save`, `save-as`, `close-tab`, `quit`, `dialog-answer`,
`key`, `pointer`, and `wait-frame`. Text mutations and close requests name a
stable tab ID and expected revision. Stale commands return `stale-revision`
without side effects. `state` reports the active tab, all tab IDs/revisions,
dirty flags, cursors/selections, modes, current dialog, spelling job/status,
and submitted/callback-completed frame generations. `text` takes tab ID,
revision, byte offset and byte limit; it returns a scalar-aligned page and
the next byte offset. Spelling result pages likewise pin the scan revision.

Save and spelling return a job ID with `pending` when work is queued;
`state` supplies completion/error. A queued save pins its expected revision;
if it differs when the worker is ready, the job fails stale instead of
saving unrequested later edits. One save per tab may be queued/in flight.
File prompts return a dialog ID and its allowed answers; `dialog-answer`
must name that live ID and revision, so a
late reply cannot discard a different tab. `key` and `pointer` use the same
decoded events as the physical adapters; replay tests also supply explicit
clock advances. A synthetic Save or close takes the same prompt/error path.
The implementation's protocol reference lists field order for every command
and response alongside conformance fixtures; it cannot invent additional
authority or a second mutation path.

Every accepted UI-visible change advances a window generation. `wait-frame N`
waits for a committed buffer tagged with generation at least N to receive
its frame callback and reports the actual generation and document revision
rendered. It times out after the whole-request deadline. This acknowledges
compositor processing, not physical scanout; screenshots and image tests
must separately observe the presented pixels. Buffer reuse still waits for
release, independently of a frame-wait response.

The control endpoint grants read/write access to all this editor's documents
within its existing authority. It cannot bypass dirty-close confirmation or
file conflict policy: discard requires the live close/reload dialog answer,
and force overwrite is absent. Do not expose arbitrary shell execution.
Sharing control across the jail boundary is a separate explicit grant, not
an implication of `sockets=wayland`.

Host protocol proof must include td-compositor and at least one independent
Wayland compositor with its real keymap. Distinguish headless model tests,
fake-server protocol tests, independent-compositor tests, and the full td
jail/image oracle in every readiness claim. A new standalone crate joins
td-builder's automatic cargo test/clippy gate and commits its one-package
`Cargo.lock`. Target recipe/image work also owes the profiler contract.

## Independently landable increments

1. Tested safe editor core: the specified scalar transactions, tabs,
   undo/redo, both key profiles, paragraph filling and headless command
   replay, with the exact limits above.
2. First usable Wayland window: shared codecs and audited descriptor/file
   adapter, reference bitmap rendering, US keyboard input, open/save, prompts
   and clipboard; deterministic protocol/pixel proof and the Weston US test.
3. On-demand whole-document spelling and complete local control: explicit
   scans, result marking/invalidation, paged semantic queries and frame
   synchronization. Exercise the production dispatcher through both inputs.
4. Source-built recipe and tmc jail integration: staged shared sources and
   data licenses, runtime closure, file grants, `$EDITOR`, debug companions,
   and a test of the actual caller's child lifetime and draft cleanup.
5. GPU editor rendering after the separately specified graphics producer and
   jail/compositor prerequisites. Validate both reference and GPU backends
   against the same scene operations and image oracles. A software-only
   milestone does not complete this objective.

More keyboard layouts, grapheme/IME editing, language-aware filling,
multilingual spelling and mail submission are outside version 1. They need
new concrete contracts when requested; implementing agents do not expand
the initial profile implicitly.
