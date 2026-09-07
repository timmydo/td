# td-editor

td-editor is a small, Wayland-native text editor with a Notepad-like window
and tabs. It is intended for ordinary text and prose, including use as the
foreground `$EDITOR` child of td-mail inside td-jail. It must also run on Linux
Wayland desktops outside td. This document is the component contract and
the starting point for successive agents; the root `AGENTS.md` and
`DEVELOPMENT.md` still govern changes and submission.

## Status and scope

The safe document core and `td-editor --replay` are implemented. They cover
UTF-8/file-format conversion, scalar edits and selection, bounded tabs and
undo/redo, save-snapshot state tracking, literal search/replace, paragraph
filling, Auto Fill, and logical Windows/Emacs key dispatch. The core opens no
files and reads no environment or clocks. The synchronous `files::Session`
adapter now implements bounded file baselines and atomic save I/O, separately
from the model. `--window` now connects it through one file worker to
file-backed tabs and keyboard Open/Save/Save As path prompts. Native pointer
selection, tab clicks and scrolling are connected. Native spelling loads an
explicit local dictionary through the file worker, scans on F7 in bounded
chunks, and publishes underlines and counts together. Format supplies
dictionary selection and next/previous marked-word navigation. GPU rendering
is not implemented yet. The optional native control socket now exposes
state/text and scan-pinned spelling queries, plus revision/selection-checked
edits. Native redraw/submitted/callback generations and bounded `wait-frame`
acknowledgement are connected. Remote Check Spelling returns a job ID with
bounded completion/error/cancellation history in native state. Remote New
creates an ordinary empty tab and returns its stable ID. Remote Open uses
the ordinary file worker and bounded job history. Remote writes and other
dialog answers remain unimplemented. Remote Close Tab, Quit and
live close-dialog Cancel/Discard use the ordinary close coordinator.
Replay emits explicit external-operation requests and does not pretend to
perform native file, clipboard or display work.
The allocation-free layout library supplies visual rows, glyph intervals,
caret affinity, pixel hit testing, vertical/page-motion calculation and
viewport scrolling. The safe UI controller connects those APIs to logical
keys, pointer selection, resize, scrolling and headless replay.
The safe reference renderer streams bitmap scene operations into a
caller-owned XRGB8888 buffer. `--preview` emits a fixed headless PPM fixture;
it is not an interactive window. Menus and status are drawn; tab clicks select
tabs and close marks emit typed requests. Native menus and file dialogs are
connected by the window adapter; remaining adapters are future work.
`--window-preview` now presents
editable scratch tabs through the real Wayland transport and SHM lifecycle.
It accepts keyboard input in both profiles, but cannot save or open user
documents. Dirty window close requires explicit discard; process termination
still loses scratch text. `--window` is a separate experimental file window;
it is not yet the usable `$EDITOR` milestone. See the implemented file-window
contract below for its narrower scheduling and close/conflict behavior.

File-window close now has per-document Save/Discard/Cancel decisions,
including Save As for untitled tabs and cancellation during a pending save.
Save conflicts now offer Reload / Save As / Cancel. Dirty Reload requires
a separate explicit discard answer and uses an atomic model/baseline handoff.

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

### Implemented input-controller contract

`ui::Controller` owns the document model, key profile/prefix, mark, drag and
per-tab view state. It exposes immutable model/view access and one typed
`Event` dispatcher. `replay::Session` owns a controller rather than a second
key dispatcher. Model commands, translated keys, pointer press/move/release,
scroll, resize, soft-wrap changes, focus and explicit clock ticks all use this
path. File, clipboard, spelling and prompt actions still return typed requests
with the target tab and revision; they are not completed I/O or visible dialogs.
Direct Close refuses dirty documents. No discard or save acknowledgement is
added to replay by this controller.

Each tab retains its viewport, soft-wrap flag, affinity, desired vertical
column, and metrics cache. Metrics are refreshed when its text revision, wrap
mode or full-cell width change; height-only resizes retain metrics and the
desired column. Edits/nonvertical selection changes reset
affinity to downstream and clear the desired column. Up/Down and Page Up/Down
retain that column across short rows and reveal the caret; Shift variants,
including Shift+Page Up/Down, extend selection. An Emacs mark extends the same
vertical path. Tab switches preserve the tab's origin; scrolling does not move
or reveal the caret. Later keyboard motion/editing reveals it. Resizing clamps
the existing origin without forcing the caret onscreen. These synchronous
scans are bounded by document size, not yet scheduled to an event-loop latency
budget. No per-scalar cache or unbounded event queue is introduced.

The initial headless geometry is 800x600 at scale 1. A decoded Resize must
already have nonzero dimensions; the future Wayland adapter owns zero-axis
configure retention. Surfaces with no complete document row or column retain
a virtual minimum 1x1 layout for bounded cached state but draw no document
cells and refuse vertical motion. The next usable geometry reflows/clamps
normally. `Controller::scene` supplies the renderer's exact geometry, origin,
wrap mode, affinity, focus and caret visibility.

Pointer coordinates are signed physical surface pixels. Presses in full
document cells use the layout's nearest scalar endpoint, preserving exact
physical-pixel midpoint ties at scales 1–4; Shift-press retains
the previous anchor. A drag remains anchored to its starting tab and byte,
clamps out-of-surface motion to the viewport edges, and ends on release.
There is no drag autoscroll yet. Blank rows below EOF select document end.
Typing, semantic edits, tab/profile changes, effective resize/scroll and focus loss
cancel dragging. Presses outside document cells or tab hit areas are ignored;
menu popups are owned by the native adapter, not this controller. Drawing
and hit testing share `Geometry::tab`,
`tab_close` and `status` rectangles for the hidden-tab slice, close area and
status-overlap precedence on tiny windows. A clamped scroll is ignored without
cancelling a drag.
The close area emits a request for the clicked tab, not necessarily the active
one. Pointer input can precede keyboard focus; only keys require focus.

Focus loss cancels prefixes, mark and drag without changing document selection;
focus gain restarts the caret without cancelling a just-delivered pointer press.
Replay starts focused; the Wayland adapter must supply actual focus events.
Stale commands and other rejected keys preserve prefix/view/model state. This
includes an invalid continuation of an Emacs prefix: Escape/C-g explicitly
cancels it. Direct `Keymap::translate` remains a lower-level decoder; the
controller stages translation until command admission succeeds.

The caller supplies monotonic milliseconds since controller creation, not
system uptime. It must dispatch a current Tick immediately before each timed
input event as well as on timer wakes. Events occur at the last supplied tick;
the controller does not infer time between them. Backward ticks are refused.
The caret is visible for 500 ms, hidden for 500 ms, and hidden when unfocused;
accepted keys and selection/edit actions restart its visible interval. Repeat
scheduling belongs to the seat adapter, not this blink clock.
Every successful command conservatively advances a checked controller generation;
ignored input and ticks that do not change caret visibility do not. Rejected
events do not advance it. This describes controller state only. The native
adapter separately tracks main-surface redraw/submitted/callback generations
under CONTROL.md; do not use the controller counter as a frame fence.

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
visible rows. Soft wrap is on by default and wraps at the last fitting
non-leading space or tab, or at a scalar boundary if there is none; it
inserts no bytes. Layout
produces the single position map used for drawing, selection and hit testing.

The layout library streams visual rows and their scalar-cell intervals from
borrowed validated text; it allocates neither a full-document row index nor a
per-scalar map. `Layout::new` validates at most 16 MiB of normalized text in
linear time. `Layout::for_document` borrows the already-validated model in
constant time; `Viewport::layout` additionally supplies its own wrap width.
The UI uses that viewport constructor so wrap and drawing widths agree.
Geometry is 1..=1,024 columns and 1..=512 rows (the 8,192-pixel axis ceiling
at scale one); the future pixel adapter handles chrome, scale and clipping.
Row scans, position lookup and vertical movement use constant auxiliary
space and linear work in document length. The UI adapter must schedule or
cache that work within its event-loop budget; this library is synchronous.
`metrics()` computes row count and longest-row width, including the final
caret cell, in one linear traversal. Cache metrics by document revision,
wrap mode and column width, not per scroll/hit event. A cloned row iterator
is a resumable checkpoint while its document borrow remains valid. Edits,
mode changes and resize invalidate layout caches; recalculate metrics and
clamp the viewport before reading its origin or drawing. Vertical motion
currently makes up to two linear row scans; this is not a UI latency claim.

All separators are retained. A separator selected as the soft-break point
stays on the preceding row; a nonfitting separator starts the next row.
The last fitting ASCII space/tab after a nonseparator on that visual row
is used only when the next scalar would overflow. Leading whitespace alone
is not a break opportunity: wrap it with subsequent text at a scalar boundary
instead of creating an avoidable whitespace-only row. Exact-width EOF creates
no extra row. A newline ends its row
without occupying a cell, and a final newline creates an empty final row.
Tab stops restart at each visual row. If a tab is wider than an empty row,
it occupies that row alone at its full tab width, clipped by the renderer;
layout always consumes at least one scalar. Other Unicode whitespace is
ordinary one-cell text, not a wrap opportunity.

A soft-wrap byte boundary has upstream (end of previous row) and downstream
(start of next row) affinity. Both represent the same selection byte offset.
Hit testing and vertical motion preserve the chosen visual side; ordinary
byte-only cursor placement uses downstream affinity. The UI adapter must
retain affinity and the desired column per tab, resetting them on nonvertical
motion or edits. Hit testing uses unscaled font pixels and picks the nearest
scalar endpoint, with midpoint ties before the scalar, including tabs.
Vertical/page movement clamps to the first/last row; the desired column is
retained through short rows. Scrolling clamps its first row so a full viewport
is shown when possible. Revealing a caret minimally adjusts the origin;
soft wrap always resets horizontal scrolling to zero.
Resize preserves the origin before clamping to the new layout dimensions;
invalid dimensions fail without changing the viewport. Horizontal scrolling
in unwrapped mode clamps against the longest row plus its final caret cell.
`caret_pixel` places the one-pixel caret at the row's top in unscaled
viewport pixels. In soft-wrap mode an end position at or beyond the visible
width (including an oversized tab) clamps to the last visible pixel column;
its semantic byte/column and affinity do not change. Unwrapped offscreen
carets are not drawn until revealed. The pointer adapter owns y-to-row
translation, integer scaling and chrome offsets; `Row::hit_test` owns x.

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
and no listable extended attributes (including extended ACLs). Preserve their owner,
group and permission bits; refuse replacement if the temporary inode cannot
match them. Refuse symlink destinations and offer Save As; reject symlinks
when opening too, so association and later saving use the same rule.
Extended-attribute inspection uses the audited file adapter's size-only
`flistxattr` query; safe `std` alone does not expose it. An unsupported query
refuses saving. Save As to a new path remains available for files whose
metadata is outside this profile, provided the new temporary inode passes
inspection too. Inherited extended ACLs or automatically supplied labels on
that inode are refused, not silently removed.

Linux may hide attribute names from the caller's credentials, so an empty
list is not proof that privileged/hidden attributes are absent. The supported
profile is ordinary user text with no such hidden metadata; files depending
on it are outside the replacement guarantee. There is no elevated helper or
attempt to read privileged namespaces. This bounds the original blanket
"no extended attributes" requirement to what this interface can establish.
See the [Linux listing contract](https://man7.org/linux/man-pages/man2/listxattr.2.html).
The filesystem/LSM configuration must permit attribute-free temporary
inodes. A destination directory with inherited extended ACLs, or a host
that automatically labels every new file (for example with SELinux/Smack),
can therefore make all Save/Save As operations unavailable. A supported
destination is required; this is not a claim that every ordinary text file
on every Linux desktop fits the metadata profile.

Before replacing an existing destination, reread and compare its device/inode,
owner/group, mode, link count, length, mtime/ctime and complete bytes against
the last load/save baseline; atime is excluded because reads may change it. A mismatch
opens a conflict prompt with Reload / Save As / Cancel; Reload requires
explicit discard of dirty text. There is no force-overwrite command in
version 1. Save As refuses an existing destination. Publishing a previously
absent path uses a same-filesystem hard link from the complete temporary
inode and then removes the temporary name, so a concurrently created file
is never overwritten. Both paths sync the parent after publication.
Hard-link support is therefore required for missing-file Save and Save As;
filesystems without it are refused. File/directory synchronization and the
attribute query must also succeed. There is no weaker filesystem fallback.

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
| Spelling results | 10,000 stored ranges across the window, including a running scan; finish scanning and count additional unknown words, reporting when marks are capped. |
| Frames | 8,192 pixels per axis, 32 MiB per XRGB buffer, three live buffers; defer redraw/resize until a buffer can be retired. |
| Wayland input | 1 MiB keymap, 128 KiB buffered wire bytes, eight pending descriptors; byte/descriptor overflow closes the display connection, an over-limit map disables input. |
| Control | Eight admitted connections, eight queued query/edit jobs, 1 MiB request/response frame, 256 KiB raw text per response page or insertion, five-second whole-request deadline. |
| Control commands (version 1 target) | Sixteen queued typed command descriptors; refuse additional commands. The current subset admits only eight jobs under CONTROL.md's transport contract. |

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
missing pathname, which the file coordinator marks dirty at admission.

The first core increment exposes `save_snapshot`/`acknowledge_saved` as the
future file adapter's contract; it stores the saved content-state ID but no
file baseline. Snapshot tokens are bound to the originating editor instance;
another instance cannot acknowledge them even if its tab/state IDs match.
The adapter must serialize saves per tab and acknowledge only
after the captured bytes have been written. The replay wire cannot synthesize
save acknowledgements or discard dirty tabs. Replay EOF ends the in-memory
test session, with no persistence claim. `load` is a replay-only byte-fixture
operation, not filesystem Open. See README for the implemented wire subset.

### Implemented file-transaction adapter

`files::Session` is a synchronous, exclusive worker-owned adapter, with no
model mutation, UI dispatch, environment setting, process spawn or control
endpoint. Its public operations are Open, prepared Reload, Save, Save As,
baseline/path/missing queries, and Forget. File IDs are local to that session,
separate from tab IDs. At most 64 associations and 64 MiB of encoded baselines
are retained.
The 16 MiB file ceiling is checked before reading, snapshot validation and
publication; complete-file comparison streams through an 8 KiB buffer.
The baseline budget counts retained encoded bytes, not all process memory.
Save also owns the caller's at-most-16-MiB snapshot; validation temporarily
decodes up to another 16 MiB through the shared codec before writing. That
copy deliberately avoids a second codec or trusting caller-supplied bytes.
It is freed before publication. Session debug output exposes only counts,
never paths or document text, and a compile-time test pins `Session: Send`.
Open validates the file codec before association admission. Open, reload
preparation and save errors leave all associations and baselines unchanged.
Forget removes only an
association, never a filesystem object.

Paths retain arbitrary Unix filename bytes and must fit 4096 bytes both
before and after resolving the parent. Empty paths, NUL, and a final slash,
`.` or `..` are refused. Parent components are canonicalized once (ordinary
parent symlinks are allowed); the final component is never canonicalized or
followed. Open uses `O_NOFOLLOW | O_NONBLOCK`, checks regular-file identity
before/after open and metadata before/after reading. This refuses a FIFO
without waiting for a writer. A parent directory handle is retained for
sync, and its pathname's device/inode is checked again before publication.
It does not eliminate the already documented same-authority directory race.

Opening an already associated path or device/inode returns its existing ID
without refreshing the baseline or silently reloading text. A missing path
gets an empty baseline and explicit missing state, not a created file. The
future UI must make that tab dirty and pair each association with exactly
one model tab; untitled Save As can first reserve a missing association.
Save As refuses both existing names and names reserved by another open
association, and changes the association only on fully confirmed success.
Existing baselines retain their opened file handle, preventing inode-number
reuse while the association lives (at most 64 such handles plus 64 parents).

`Session::prepare_reload` rereads the associated stored resolved pathname
using the same regular-file, codec, stable-read and parent/name checks as
Open. It resolves that pathname's parent anew: a replaced directory is an
explicitly requested new location, not the old retained directory. If parent
resolution redirects the stored absolute path elsewhere, preparation refuses
with Conflict; use explicit Open for that different path. A final symlink or
special file is still refused. A candidate matching another live association's
path or retained inode is refused with Exists, never merged into that tab.
A missing destination produces an empty, explicitly missing candidate and
does not create anything. The window connects these file-adapter semantics
through the conflict dialog and revision-bound document admission below.

The returned `files::Reload` exclusively borrows its originating session.
No Open, Save, forget, second preparation or other session mutation can
intervene while it exists. It exposes validated bytes/path/missing state
and a fresh FileId via `file_id`, while retaining the original association
unchanged. Its Debug output contains only IDs, byte count and missing state,
never paths or text.
Dropping the candidate cancels it. Only after the model accepts the candidate
for its still-current, explicitly authorized revision may the coordinator
consume it with `commit`: that replaces the old association under the fresh
ID without further I/O or fallible admission. Old IDs stop resolving. Every
successful preparation consumes an ID, even if cancelled; failed preparation
does not. Exhaustion refuses preparation before filesystem access or any
association mutation.
Commit installs the read snapshot, not a claim that disk stopped changing;
the next ordinary Save still compares the complete adopted baseline.

Preparation admits `retained baseline bytes - old bytes + candidate bytes`
against 64 MiB and retains at most one additional 16-MiB candidate. It works
at the 64-association limit because it replaces one entry rather than adding
a live association. The old baseline and its descriptors remain pinned until
commit or cancellation. Preparation may transiently allocate the codec's
additional at-most-16-MiB decoded validation string and retain one extra
parent and regular-file handle. The worker-to-model handoff copies
one at-most-16-MiB encoded result and decodes it separately; it must not commit
the borrowed candidate when that admission is cancelled, stale or over budget.

The window integration keeps this borrow inside the worker, never returning
it from the one-job executor or sending it to the UI. The worker sends the
candidate's bytes/path/missing flag and fresh ID, then receives the next
ordinary job while retaining the borrow. That job's live association set
names the fresh ID only if model admission succeeded; otherwise it still
names the old ID (or neither if the tab closed). Commit or drop the candidate
before executing that next job. An idle worker already waits for jobs: no
new UI wait or acknowledgement channel is needed. Job-channel disconnection
drops the candidate; the session then exits. At most one candidate remains
retained while idle. A threaded channel oracle exercises both decisions.

Save takes ownership of one encoded, immutable model snapshot. It admits
the replacement baseline budget and validates the bytes before any write.
Unique same-directory names use create-new, mode 0600, a checked process-local
serial and at most 64 collision attempts. The temporary inode receives the
complete bytes, then supported owner/group/mode with readback, then attribute
inspection and file sync. Existing destinations get a second full baseline
comparison immediately before rename. New destinations publish by hard link,
so another creator cannot be overwritten even after the absence check.
Temporary-name removal and parent sync precede metadata/content/name
readback and installation of the new baseline. The published inode has one
link. No truncation fallback or force overwrite exists.

Every failure carries a kind, diagnostic, `publication_attempted`, `published`
and optional residual temporary path. The first flag is set immediately
before invoking rename/link; the second only after the kernel reports
success. A publication syscall error is not claimed to prove no change on a
remote filesystem. Failures after publication retain the old baseline and
must not acknowledge the model snapshot as saved. A subsequent ordinary
Save then conflicts with changed disk state; Reload or Save As is needed.
After a post-publication Save As failure, the old association remains and
the new path may exist; retrying Save As to that name is refused. Open the
published path as a separate association to inspect it, or choose another
Save As name. An error never silently adopts the new path or authorizes
deletion/overwrite of it.
Cleanup checks the temporary name's inode before unlinking, refuses to
remove a replacement object, and reports the exact cleanup path on failure.
An already absent name needs no cleanup and is not reported as a residual.
Other cleanup failures report an unconfirmed residual: existence and
ownership must be checked before manual removal, not inferred from the path.
Abrupt process death can leave a private temporary file; no startup sweep
deletes prefix-matching files.

The synchronous adapter itself does not mutate the model or run a window.
`session.rs` owns its one file worker and tab associations as specified below;
no synchronous document file I/O runs in the Wayland dispatch loop. Resource bounds are
byte/work bounds, not deadlines for a stalled filesystem. Tests use actual
temporary files and explicit stage-failure injection; a model integration
test edits while a snapshot is pending and acknowledges only the written
content state. The scratch-window warning remains unchanged.

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

This is scalar tokenization, not Unicode word segmentation. For example,
decomposed `nai` + U+0308 + `ve` produces two checked ASCII fragments, while
precomposed `naïve` is one skipped token. U+00AD soft hyphen and U+200D joiner
also delimit fragments. Those fragments can be marked unknown: version 1
does not normalize equivalent encodings or infer whole words across these
delimiters. Tests pin this consequence of the explicit scalar profile.

There is no writable personal dictionary in version 1. Users edit their
chosen word-list file with the editor and explicitly reload it through the
Dictionary command. Spelling results never change document bytes or history.

### Implemented spelling core

`spelling::Dictionary::parse` validates caller-supplied bytes against the
English word-list contract above. It performs no file lookup or I/O. A
bounded temporary ordered set deduplicates lowercase entries and becomes a
sorted vector for binary lookup. Invalid replacement data cannot mutate an
existing dictionary. Each successful parse has a fresh opaque identity,
including reloading identical bytes; this is the core dictionary-generation
token and prevents old results from silently becoming current again.

`spelling::Scan` borrows text from the specified editor/tab/revision on each
explicit `step`, consuming at most 4,096 scalars. The only token lookahead
is one scalar to decide whether an apostrophe is internal. A token crossing
a step boundary retains at most 64 normalized ASCII bytes; oversized or
unsupported tokens remain one skipped token without unbounded accumulation.
The scan holds no document copy and exposes no partial marks or counts.
`finish` publishes a report only after EOF and a final target/dictionary
check. Empty documents complete on the first step with zero counts.

Each report stores at most 10,000 ordered byte ranges. Scanning continues
past that limit so checked, unknown and skipped counts remain complete;
truncation is true only if an unknown range was omitted. Reports guard both
counts and ranges by editor identity, text revision and dictionary identity.
Edits (including Undo), tab removal and dictionary replacement invalidate
them; cursor motion, tab switching and save acknowledgment do not. A scan
that observes invalidation fails permanently even if supplied its old
dictionary later. Dropping a scan cancels it without changing any document.

`files::read_dictionary` reads the file at an explicitly supplied literal
path. The path is limited to 4,096 bytes. This regular-file reader does not
create a file association, reserve a missing path or retain a saved baseline.
Final-component symlinks and nonregular files are refused. The parent is
canonicalized and pinned; the existing 16 MiB stable-read ceiling bounds
input before parsing. After parsing, the name, parent identity and file stamp
are checked again before returning the dictionary. An observed replacement
or mutation refuses the result without writing anything. These checks are
race detection, not a filesystem snapshot or a guarantee against a writer
changing the file after the final check. No system dictionary search occurs.
This synchronous API belongs on the file worker, not the display loop.

### Implemented native spelling

`--window --dictionary PATH` loads one literal word-list path on the existing
file worker before connecting the display. The option may appear once;
its next argument is always the literal path, including a leading dash.
After `--`, all arguments are document paths. Invalid startup dictionaries
fail startup without writing any file. Format > Dictionary uses the same
keyboard path prompt and worker after startup, without blocking display
dispatch. Dictionary loads share the single pending-file-job admission guard;
they create no editable file association. The old dictionary, marks and scan
remain selected until a successful replacement is delivered. Failure leaves
them intact; success clears all results and cancels any scan, including when
the bytes are identical. Loading never starts a scan. No dictionary search,
download, subprocess or dependency is added. Scratch preview has no file I/O
and disables Dictionary; F7 there reports no dictionary.

F7 and Format > Check Spelling start a check in both profiles. Held-key repeat
does not start or restart work. Admission clears input prefixes, drag/repeat
state and pending Paste. There is one explicit `Scan::step` at the end of
each native event-loop turn, at most 4,096 scalars, with a 1 ms maximum idle
wait while work remains. The ticks before individual events only invalidate
stale state; even a full 256-event batch cannot multiply the scan allowance.
The scan may finish after a tab switch or focus loss; only text revisions,
tab removal, dictionary replacement, a new check or Escape/C-g cancel it.
Escape/C-g cancels an active scan even when also dismissing a menu, search,
path or close dialog; cancellation is window-wide, not only document input.
No partial counts or marks are exposed. Input-event and tick observers prune
stale reports, and rendering independently checks their revision identity.
Undo cannot resurrect old marks. Successful completion does not change text,
selection, dirty state or undo history.

The 10,000 stored-range budget is window-wide. Starting a check discards the
previous report for that tab and cancels the previous scan, retaining other
tabs' reports. Its mark allowance is the remaining budget at that instant;
freed space during the check does not expand that allowance. Even with no
remaining slots it finishes counting all words, reporting `marks capped`.
Completion releases unused vector reservation before retaining its report.
Checking again can use space released by edited or closed tabs. No automatic
eviction of another tab's valid results occurs.

After EOF, status shows unknown/checked/skipped counts and marks are drawn
as a one-scaled-pixel underline in muted brick `0x9c5548`, or paper-colored
over focused selection. Underlines follow the same clipped cell geometry as
text, including wrapping and horizontal scrolling; the glyph rasterizer is
unchanged. Narrow windows clip status rather than resize or wrap the chrome.
Format > Next/Previous Misspelling selects the next stored range after the
selection, or the previous range before it, and reveals it through the shared
controller. Navigation does not wrap and cannot reach unknown words omitted
by the budget; the status reports that truncation. A separate results-list
panel and control queries remain future increments. The named command
prompt below now connects `M-x ispell-buffer` to the same scan request.

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
same operations. No font discovery, outline rasterization, subpixel
antialiasing or fractional scaling is part of version 1. The fixed medium
bitmap weight below adds one explicitly shaded edge; it does not smooth or
resample the original glyph. A frame uses one scale throughout.

Draw only the visible viewport and damaged chrome; clip every operation to
the current surface. Coalesce redraws behind one outstanding frame callback.
A submitted buffer is immutable until `wl_buffer.release`;
`wl_callback.done` throttles frames and is not permission to reuse a buffer.
On resize, retain old busy buffers within the three-buffer budget and render
only the latest configured size when a slot becomes free. Initial client
size is 800x600 pixels. A zero configure dimension retains that axis's
current size; a dimension outside the resource ceilings closes the connection
with a diagnostic. Rendering failures must not mark any document saved.

### Implemented reference-renderer contract

`render::Scene` borrows the editor and display-only `Label` values. Labels
are not file associations: each names an existing tab, duplicates are refused,
and there are at most 64 labels of at most 4096 UTF-8 bytes each. Missing
labels display `Untitled`; dirty tabs prefix `*`. Labels use whole bitmap
cells, truncate without ellipsis and substitute U+FFFD for control scalars.
The caller owns the selected key profile and `View` (scroll origin, soft
wrap, caret affinity, focus and blink visibility). Rendering never mutates
documents or acknowledges a save. Scrolling is measured in visual rows and
columns; soft wrap ignores the horizontal origin. Origins are admitted up to
16 Mi rows and 128 Mi columns. The adapter must clamp them with `Viewport`
when the document or geometry changes; an admitted origin beyond the text
draws a blank document area.

`Geometry` admits nonzero axes through 8192 and at most 32 MiB of tight
four-byte pixels. `Raster` additionally validates the supplied byte stride:
it must be a multiple of four, at least width times four, with stride times
height at most 32 MiB and within the borrowed buffer. Validation happens
before writes. Pixels are B, G, R, 0xff bytes; row padding and any trailing
allocation bytes are untouched. The backend accepts only 8x16 fonts and
integer scales 1–4. The font is decoded once by the caller and borrowed;
the production face and parser are the compositor's existing source modules.
`--font-license` prints embedded provenance, COPYING and OFL notices from the
same assets directory. No host font search or new font input is introduced.
The source recipe must stage these five repository-relative inputs, keeping
their paths relative to `td-editor/src` exactly as in the checkout:

```text
td-compositor/src/font.rs
td-compositor/src/font_data.rs
td-compositor/assets/PROVENANCE
td-compositor/assets/unifont-COPYING
td-compositor/assets/unifont-OFL-1.1.txt
```

There is no editor recipe yet; adding one must replace the editor-only gate
exemption with target-artifact coverage, as specified below.

Chrome dimensions below are logical pixels multiplied by the frame scale.
The menu occupies the first 24 pixels, the tab strip the next 24, and the
status strip the bottom 24. Document content starts at (8, 48), has eight
pixels of right margin, and uses only full 8x16 cells. Tabs are 160 pixels
wide with 24 pixels reserved for the close mark. A contiguous slice of tabs
is shown, keeping the active tab visible; a surface narrower than one tab
clips that tab. Tiny surfaces may have no document cells; status paints last
and wins any chrome overlap. The default palette is warm #eee8dc paper,
#48453f charcoal ink, #e1dbcf chrome, #b5ada0 borders, #536b73 focused
selection with paper-colored ink, and #c8c4bb unfocused selection with
ordinary ink. It avoids white backgrounds and near-black text, including
in the menu, tabs and status bar. These are fixed defaults, not an OS theme
lookup or a user-configurable theme system. The caret is one logical pixel
wide; an upstream soft-wrap caret remains inside the row's right edge.

Every glyph draw carries a `GlyphStyle`: ink, the already-painted background
and `Regular` or `Medium` weight. Scene text uses Medium. Regular paints
exactly the pinned bitmap. Medium also paints an unset pixel whose immediate
left neighbour is set in the original glyph, with RGB channels
`floor((ink + 2 * background) / 3)`. Original set pixels keep their full ink;
the fringe is derived only from original bits, never extended recursively.
It stays inside the same 8x16 cell and scales by the same integer factor,
so spacing, wrapping, hit testing and selection boundaries do not change.
Space remains blank; missing scalars receive the same treatment on the
existing fallback glyph. All writes retain XRGB's 0xff high byte.

This is a synthetic medium presentation, not a new font face or altered
font asset. It adds modest weight without the solid one-pixel expansion
closing Unifont's small counters. Fringe colors use the explicit background,
not sampled buffer bytes: repeated/damaged redraws cannot accumulate weight.
Selected text uses the selection background; active tabs use paper and
inactive tabs, menus and status use chrome. Other transparent-glyph callers
must likewise supply the background they have painted. The compositor's
font decoder, font data and terminal appearance are unchanged.

Selection covers whole scalar cells, including the complete visible tab
span. A selected logical newline paints one trailing cell only where a full
visible cell remains, never a sliver in the unused partial-column space;
soft-wrap boundaries do not invent a newline cell. The renderer emits glyph
operations only for visible cells. It traverses layout to reach the first
visible row, then scans each visible row from its start to reach the horizontal
origin. A selected newline also needs the row's full width. This is not a
random-access row cache: scene construction and seeking may scan text.
`Scene::emit` streams solid fills and transparent glyphs without a retained
operation list. Each draw carries a signed-origin, bounded-size clip;
`Raster::draw` intersects it with the surface and primitive before writing.
`Raster::paint` refuses a geometry/scale mismatch before writing. A damage
rectangle replays the same scene clipped to that region; the caller owns
damage accumulation and persistent buffer validity. It must repaint newly
allocated buffers completely. Frame callbacks and release events are not
implemented by these APIs.

The headless `--preview` command uses these production APIs with a fixed
800x600, scale-1 two-tab fixture and writes binary P6 PPM to stdout. It does
not inspect files, environment, clocks or displays. Tests pin its complete
byte checksum, compare rasterization to independent pixel-membership and
font-row oracles for both weights, and prove repeated partitioned damage
matches a full repaint at every supported scale and in both focus states.

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

### Implemented experimental file window

`td-editor --window [--keys=windows|emacs] [--] [FILE...]` opens the file
window. The mode flag `--window` must come first. Following profile options
may precede or follow paths until `--`; dash-prefixed paths
need that delimiter. There are at most 64 literal Unix-byte paths of at most
4096 bytes each. No shell, tilde, variable or stdin expansion is performed.
Initial opens run sequentially on the file worker before connecting Wayland;
any failed initial open exits nonzero without creating a window or writing
files. Duplicate paths/inodes select the existing tab without refreshing its
baseline or replacing edits. With no paths, New creates one clean Untitled
tab. Missing paths create empty dirty tabs without creating a disk file.
Ordinary no-option/filename invocation remains refused: this explicit mode
is experimental, not the `$EDITOR`/td-mail integration milestone.

The file window reuses the preview's transport, input dispatcher and bitmap
renderer, with the warm palette and medium weight. It requires a v5+ seat
advertisement (unlike the presentation-only scratch exception); absent or
unsupported input capability/map still produces a retained-text warning.
Its title says experimental file window, not NO SAVE. Filename labels are
bounded escaped leaf names; arbitrary Unix bytes passed by CLI are preserved,
not reconstructed from lossy labels. The original two-tab scratch fixture
remains available only through `--window-preview` with no document access.

Windows Ctrl+O/S/Shift+S and Emacs C-x C-f/C-s/C-w request Open/Save/Save As.
Save on an untitled tab opens Save As. A modal path entry accepts individual
printable scalars, Space, Backspace, Ctrl+U to clear, Return to submit and
Escape/C-g to cancel. The current native input profile restricts what can be
typed; arbitrary byte paths can still be opened through argv. Entry is bounded
to 4096 UTF-8 bytes, refuses excess input, displays its final 160 scalars and
passes the full literal path to the worker. Other commands are consumed, not
sent to the document. Prompt input and confirmation do not auto-repeat.
Cancelling preserves document selection, undo and text. Opening a prompt
clears the previous notice so cancellation does not resurrect stale feedback.
Submission closes the
prompt and displays pending/success/failure; retry starts a fresh prompt.
Prompts and notices share the existing clipped top-six-document-row overlay.
When input is unavailable or not synchronized, the prompt instead prefixes
readiness instructions without erasing the entered path.
Path and confirmation dialogs remain keyboard-only. Native pointer selection,
tab clicks and scrolling follow the pointer contract below. Menus use the
native menu contract; clipboard uses the data-device contract below.

`session::Session` keeps FileId-to-TabId associations and an opaque model save
token per pending save. One `std::thread::Builder` worker exclusively owns
`files::Session`. There is exactly one submitted job and no waiting queue in
this increment; additional requests are refused visibly before allocating
another snapshot. Version 1's eight queued descriptors remain future work.
Two capacity-one channels carry jobs/completions; the UI never waits on them
or joins a file operation. It captures the encoded snapshot at admission and
polls completions from the existing at-most-100ms event-loop wake. Protocol
events, redraw, resize, tab switching and ordinary edits continue during I/O.
An Open completion that changes the active tab cancels held-key repeat so it
cannot continue typing into the new document. Save completion keeps repeat.
Saves verify the requested tab revision before snapshot creation. Only a
successful transaction dispatches `Event::Saved` with that snapshot's token;
later edits remain dirty. Failures preserve model text, saved state and the
old file association. Publication and cleanup warnings precede long path
diagnostics so bounded notices retain those consequences. A disk conflict
refuses the write and opens the conflict dialog described below. Save As
never replaces an existing or already-associated path.

Open admission and clean tab close update the coordinator's live association
set. Before each job, the worker forgets associations absent from that set,
including a previous Open result rejected by the model's tab/text budget.
Between jobs those unclaimed baselines may remain retained, still charged to
the file adapter's 64-file/64-MiB limits; they are released before any new
admission and when the worker exits. In addition to the retained baseline
budget, a new Open hands one at-most-16-MiB encoded result to the UI; decoding it can
transiently allocate another at-most-16-MiB string before model admission.
Duplicate opens transfer no text, only association metadata.
Save owns one at-most-16-MiB encoded snapshot, with the transaction adapter's
validation/readback scratch as specified in its section. No other queued
snapshot or file result can accumulate.

An ordinary Save's typed Conflict or Exists failure creates a conflict
question for that associated tab's current revision, including when newer
edits arrived during Save. Save As failures do not suggest reloading its old
association. Publication-attempt or residual-cleanup failures retain their
full warning notice instead of opening a question over it; explicitly retry
Save or use Save As after inspecting the warning. No diagnostic substring
determines whether a failure is a disk conflict. A failed close-initiated
Save first cancels the close plan; resolving its conflict never resumes
closing, and the conflict caption states that closing was cancelled.

In either key profile the conflict question offers Ctrl+R Reload, Ctrl+S
Save As to a new path, or Escape/C-g Cancel (revealing the original error).
Reload on dirty text opens a second question: Ctrl+D explicitly discards
that revision's unsaved text and reloads; Escape/C-g cancels the entire
conflict flow. A first-question Ctrl+D does nothing. Repeated keys cannot
request or confirm either action. Each question begins with the stable tab
ID and a shortened filename, uses at most six 32-scalar lines, and requires
the close dialog's minimum dimensions and synchronized input for non-Cancel
answers. Input loss or a smaller window retains the question and replaces
choices with restoration/resize instructions. Window-manager close can
replace an idle conflict question with the ordinary close flow.

`dialog::Conflict` pins editor identity, tab and revision. Only a live answer
can mint the opaque, single-use `Reload` permit; a dirty permit additionally
requires the second discard answer. The file coordinator checks the permit
before submitting one read job. While reading, the modal consumes ordinary
keys, but protocol handling and redraw continue. Cancel drops the pending
permit, not the read syscall: the eventual result is ignored and its prepared
baseline rejected. The user may edit again after Cancel. A new close during
the pending read is refused like other unrelated I/O. A stale completion,
invalid file, exhausted counter or failed budget admission leaves text,
history, selection, saved state and old file association unchanged.

Successful `Event::Reload` keeps the same TabId, active-tab choice and that
document's Auto Fill, fill column and soft-wrap preferences. It replaces
text and BOM/line-ending format, sets selection to byte zero, increments
revision and content-state IDs, clears that document's undo/redo history,
and reveals the origin in its viewport. Existing-file text becomes clean;
a missing destination is refused by the file-session coordinator before model
replacement, retaining the old document, history and baseline and directing
the user to Save As. The lower-level model can represent an authorized empty
missing-file replacement, but the native Reload flow never admits one.
Other documents and their history remain unchanged.
Controller generation is admitted before mutation; codec, text budgets and
both model counters are checked before replacement. No replay command can
construct a permit or bypass the live discard question.

Only accepted model replacement changes the UI association to the candidate's
fresh FileId and returned path label. The worker keeps its borrowed candidate
across completion delivery and receives the next ordinary job. It commits
only when that job's live association set contains the fresh ID and excludes
the old one; otherwise it drops the candidate. It resolves this decision
before executing the next job, so a rejected/stale/cancelled Reload still
uses the original conflict baseline on Save. While idle it may retain the
one extra candidate already budgeted above. Disconnection drops it and ends
the worker. There is no second acknowledgement queue or UI-thread wait.

Untitled Save As first probes destination metadata on the worker and refuses
an existing name without reading its contents. A name created between that
probe and reservation can still be read and decoded by the adapter's Open:
its bytes count toward the 64-MiB baseline budget, alongside the submitted
16-MiB snapshot and up to 16 MiB of transient decode scratch. That unclaimed
baseline is released before the next job. No-clobber publication is still
enforced by the transaction, not inferred from the early probe. Reservation
failures identify the destination, not the user's document, as the problem.

Starting tab/window close while an unrelated file job is pending is refused
with a notice; retry after completion. The file window otherwise creates a
revision-bound close dialog, asking about each dirty document, active tab
first, without switching tabs or changing selections/view state. Ctrl+S
saves that document, Ctrl+D approves discarding that document's edits, and
Escape/C-g cancels the whole close request. These modal keys are the same in
both profiles. Repeated keys never confirm a choice. The first caption line
identifies the document by stable tab ID, followed by its escaped leaf name
(or Untitled), shortened to 27 scalars plus an ellipsis when needed. Save and
Discard from physical input require synchronized input and at least 272x160
buffer pixels: all six caption/control lines then fit the notice region.
Smaller windows show a resize instruction and accept only Cancel. Input loss
retains the question
and shows restoration instructions instead of choices. A close-driven path
prompt explicitly says cancellation cancels the whole close request. Any
save refusal or failure explicitly reports that closing was cancelled and
tabs were retained, alongside the file diagnostic.

`dialog::Close` is a display-independent coordinator. Its model points bind
the originating editor, tab IDs and text revisions; another editor, changed
revision, removed tab or a new tab in a window-close request invalidates the
request before any discard. Direct model/replay dirty close still refuses.
Only this coordinator can construct the opaque `Discard` permit consumed
by `Event::Discard`, which rechecks its editor/revision binding before
removing a tab. The controller's generation admission still precedes mutation.
The window's single-threaded dispatcher supplies explicit choices; there is
no wire shortcut for minting a permit. Remote Close Tab, Quit and
Cancel/Discard now use this coordinator with a checked window-local dialog
ID plus current question tab/revision. A trusted control client may answer
while unfocused,
without a synchronized seat/keymap, or with a clipped prompt: its explicit
live-token answer is the authority, not synthetic physical input. Physical
confirmation retains its visibility/input requirements. This approved remote
policy also governs future Save/path answers; CONTROL.md defines the current
Cancel/Discard subset and its refusal, phase and shutdown semantics.

Window-close discard approvals are deferred: no tab is removed and no dirty
state is cleared while further decisions remain. Cancel drops the approvals
and retains every tab, its text, selection, view and history. When all dirty
tabs have been saved or explicitly approved for discard, the window exits.
A tab-close request removes only its own tab after its decision resolves,
exiting if it was the last tab. Completed saves stay saved if later choices
are cancelled or another save fails; all tabs remain open on such a failed
window-close attempt. There is no force-overwrite fallback.

Saving an untitled tab enters the existing Save As path prompt. Cancelling
that prompt cancels closing. An accepted save keeps the close dialog modal
until completion, while protocol events, resize and redraw continue. All
ordinary editing/close actions are consumed in this state. Escape/C-g may
cancel closing while I/O is pending, but does not cancel or undo the write;
editing resumes and the eventual exact-snapshot acknowledgement cannot
auto-close anything or mark newer edits clean. A failed save clears the close
request and exposes its diagnostic. After successful completion the dialog
revalidates every pinned revision and advances to the next unresolved tab.
Repeated window-manager close neither replaces the request nor erases an
entered Save As path. Input-unavailable and unfocused states give readiness
instructions instead of claiming confirmation works. There is no
recovery: abrupt termination or a fatal display error can lose unsaved edits.
A fatal display error during a file job additionally reports that the write
may have published. File syscall duration is not bounded; a stalled filesystem
can keep a job pending and ordinary close refused until the user terminates
the process. Dropping the session disconnects the worker, but cannot cancel a
filesystem call already in progress. No normal close exits with a pending job.

Pure dialog tests pin deferred discard, cancellation, ownership/revision
invalidation and saved-state retention. Fake-compositor file tests exercise
both profiles, Save As cancellation/completion, unrelated-tab retention,
held confirmation rejection, later-save conflicts and cancellation during
I/O followed by new edits. The scratch fixture keeps its explicit no-Save,
whole-window-discard behavior; it cannot exercise filesystem dialogs.

Deterministic channel-driven tests execute the production file jobs with
explicit completion timing, checking edit-during-save, stale requests,
missing/duplicate opens, conflict refusal, no-clobber Save As and rejected
admission cleanup. A real worker test covers startup handoff. Fake-compositor
tests route both profiles through modal paths, pending-close refusal and save
completion, checking actual file bytes and changed SHM pixels. These tests do
not claim the still-required interactive Weston US or the td-jail/td-mail
oracle.
The replay protocol remains filesystem-free and cannot forge save completion.

### Implemented scratch-window adapter

`--window-preview` is an explicit scratch milestone, not the usable-editor
milestone below. It opens one 800x600 scale-1 xdg toplevel and two initially
clean fixture tabs through `ui::Controller` and the reference renderer.
`--window-preview --keys=windows|emacs` selects the profile (Windows default).
The title and fixture say NO SAVE. Typing, selection, visual motion, undo,
tab switching, native pointer input, menus and core commands are connected.
Unavailable commands produce a visible, bounded notice,
retained until Escape/C-g or another explicit notice-producing action.
Notices wrap over the document's top six rows and clip on small surfaces;
they do not mutate document text. The binary refuses filenames and ordinary
`$EDITOR` invocation.

Closing a clean tab removes it, and closing the last clean tab exits. Dirty
tab close refuses with a notice; there is no tab-discard operation in this
increment. Window-manager close or Emacs Quit exits only if every tab is
clean. Otherwise it cancels repeat and displays a modal, window-wide discard
question. Only a fresh Ctrl+D press discards all and exits; Escape/C-g cancels
and other keys cannot edit. The question explicitly states that Save is
unavailable, rather than offering a nonfunctional Save button. When keyboard
input is unavailable the question instead says confirmation is unavailable,
text is retained, and terminating the process loses it. A pending focus or
modifier snapshot gets a readiness instruction, not an unconditional claim
that Ctrl+D works. No repeated window-manager close silently discards text.
Protocol pings
and resize continue during the question. Process termination, transport
failure and keyboard failure are not recovery mechanisms: scratch text is
memory-only and may be lost. Users must not keep important text here.

The adapter shares `td-compositor/src/wire.rs` without copying it. That sixth
shared input must be staged beside the five font/license inputs when the
future source recipe is added. This adapter owns display environment and
clock access; `files::Session` separately owns document file I/O. The core's
explicit-input contract is unchanged.

It binds compositor v4, SHM v1 and xdg shell v1, requiring those minimum
versions and capping higher advertisements. At startup it also binds the
lowest-global-ID seat offering v5 or newer, capped to v7, and requests its
keyboard and pointer only after their capability events. A missing/old seat
leaves a presentation-only window with a notice; this scratch-mode exception does not
weaken the version-1 required-seat contract below. Other globals are ignored,
subject to 128 live registry entries and 256 bytes per interface name.
Client IDs are dense in a 128-slot table and are reused only after delete_id;
object exhaustion produces a diagnostic. A 16 KiB read buffer feeds a 128 KiB
pending-byte budget and at most 256 messages are processed before checking
redraw/close again. Invalid events and removal of the bound compositor, SHM
or xdg-shell global disconnect
with a diagnostic. The first buffer must be submitted within 20 seconds of
the initial registry requests; connect separately has a five-second deadline.
After submission a hidden surface may wait indefinitely for a frame callback;
callback delivery is not a compositor-liveness requirement.
One bounded connect worker owns a path connection attempt and drops any late
result. Each outgoing message has a five-second absolute write deadline,
capped by the remaining startup deadline until the first commit. Temporary
backpressure retries within that deadline. Reads also use the remaining
startup budget. The idle reader uses a 100 ms socket timeout or elapsed-time
backoff for an inherited nonblocking socket, without changing shared flags.
An armed repeat shortens that wait to its next due time. Caret ticks use
monotonic milliseconds since the loop starts, immediately before each event
and on timer wakes; server timestamps are never compared to this clock.

The controller receives complete acknowledged configure sizes: zero axes
retain the previous configured axis even while a frame is outstanding.
Dimensions must fit `Geometry`'s 8192-axis/32 MiB limits. Configure batches
are coalesced before painting; a complete repaint is sent behind at most one
frame callback. Callback completion does not release a buffer. Three backing
files at most remain live, each at most 32 MiB. A free matching buffer is
preferred; otherwise a free wrong-size buffer is destroyed and replaced.
Busy old-size buffers
remain immutable until release. When all three are busy, only the latest
configured geometry is retained for the next free slot. One scratch raster
allocation is reused. The immutable pointer image has a separate 1536-byte
ARGB8888 pool, described below. This is CPU SHM presentation, not GPU rendering.

Pool files use `create_new`, mode 0600, in Rust's temporary directory (TMPDIR
or `/tmp`); a checked process-local serial and 64 collision attempts bound
name creation. They are unlinked immediately, then sized and written only
through the owned `File`. An unlink failure reports the exact residual name.
No mmap, host library or persistent font/file lookup is involved.

The transport subset of `UNSAFE.md` §14 uses sendmsg, recvmsg and
F_DUPFD_CLOEXEC, with one syscall site and one owned-descriptor adoption site.
The file adapter adds a size-only flistxattr query through the same syscall
site; it neither adopts descriptors nor changes transport authorization.
`WAYLAND_SOCKET` takes precedence and is duplicated close-on-exec, not adopted
directly; its borrowed original is never closed by the adapter and stays open
until its owner or process exit closes it. The caller must give the adapter
exclusive use of the stream because socket timeouts are shared. Otherwise an
absolute WAYLAND_DISPLAY works without XDG_RUNTIME_DIR, and a relative display
(default `wayland-0`) is joined to an absolute XDG_RUNTIME_DIR. Invalid explicit
socket values fail without trying another display. Incoming descriptors are
immediately owned and queued in a bounded FIFO, independent of byte-message
boundaries. Only `wl_keyboard.keymap` consumes one. An event waiting for its
descriptor has a five-second deadline and retains wire order. Waiting cancels
repeat and uses the ordinary idle wait, capped to that deadline. Overflow,
malformed control data, protocol failure and disconnect close all retained
owners. Retired keyboard events are schema-validated and their keymap rights
dropped until delete_id, never applied to the replacement keyboard.

Keymap format must be text-v1; the file must be regular and cover the declared
1..=1 MiB extent. Positioned reads copy exactly that extent without advancing
the compositor's shared file offset. The wire payload requires a trailing
NUL (the standalone compiler's optional-NUL API is unchanged), valid UTF-8
and successful whole-map compilation. There is no mmap, host include lookup,
or fallback physical US translation. Regular-file reads and compilation are
synchronous and bounded in bytes/work, not a hard filesystem-latency promise.
A refused initial or replacement map disables input and cancels prefixes and
repeat, retaining all text. A valid later map restores keyboard access after
the authoritative modifier snapshot. The notice reports waiting until that
snapshot arrives (a focus transition or pressing/releasing a modifier can
provide it); compilation alone is not announced as ready input.
Event-local translation refusals show a
notice and ignore that event without invalidating the map.

`seat::Input` retains at most 768 held key numbers. Enter installs held keys
without typing or arming repeat; presses wait for enter's modifier snapshot.
Duplicate presses and unmatched releases are ignored. Focus loss clears held
state, modifiers, prefixes and repeat. Modifier changes and any new press
cancel the old repeat; a release cancels only its matching repeat. Map changes
cancel repeat and require a new modifier snapshot. Only a repeatable stroke
accepted as a controller change arms repeat; requests, prefixes, ignored or
rejected strokes do not. Negative rate/delay is malformed; zero disables
repeat. Rates above 1000 Hz clamp, intervals round upward to milliseconds,
and a new positive rate/delay retimes the current repeat from the current
tick. At most one repetition is dispatched per loop turn; missed repetitions
are dropped, never burst after a stall. Buffered release/focus events run
before timer repeats. Timer arithmetic is checked; exhaustion disarms repeat.

Keyboard capability loss releases the keyboard, clears input, and retains
text. Reacquisition creates a fresh object, waiting for delete_id before ID
reuse. Bound-seat removal also releases the seat and leaves a notice, without
disconnecting or silently moving to another seat. Dynamic new-seat selection
and multi-seat editing are deferred. The clipboard descriptor consumer is
separately rostered under this crate's data-device contract below.

Automated socket tests inspect the actual received pool descriptor and pixels,
exercise fragmented events, ping/close, version/ID limits, both release/callback
orders and resize storms. The opt-in Weston test waits for a callback from the
real compositor after the real reference buffer commit. It proves presentation,
not live keyboard delivery, compositor screenshots, GPU rendering or td-jail
integration. Socket tests separately exercise real keymap transfers, both
profiles' edits/undo and changed raster pixels, map replacement and rejection,
held/focus/modifier/repeat state, and dirty-window discard/cancel.

### Implemented native pointer contract

The bound v5-v7 seat independently supplies a pointer. Capability loss sends
release, clears pointer state and ends the controller drag without changing
keyboard focus or selection. In-flight retired events are schema-checked but
not applied; IDs wait for delete_id. Seat removal releases both devices.
Pointer enter/leave must name the main surface. Unknown opcodes, truncated or
trailing payloads, invalid axis/source numbers and invalid button states are
errors. Right/middle/unknown buttons are otherwise ignored; only BTN_LEFT
press/move/release is connected. Duplicate presses and unmatched releases
do nothing. Enter never invents a held button; leave cancels dragging and
pending wheel motion without changing keyboard focus.

Mouse input works before keyboard focus. Shift extends selection only when
the compiled map validates the focused, synchronized modifier snapshot and
its declared Shift role is active. Otherwise a press starts a fresh anchor.
Chrome uses floor-rounded signed 24.8 coordinates. Text x coordinates round
up within their already-hit region, preserving strict midpoint ties; y rounds
down. Out-of-surface presses stay outside and drag endpoints clamp through
the controller. Native presentation is still scale 1. Selection, tab switching
and close-mark hit testing use the same controller as headless replay. A tab
close request goes through the revision-bound file close dialog, including
when the clicked tab was not active. Scratch dirty-tab close still refuses.
Accepted pointer actions cancel keyboard repeat. No double-click word select,
drag autoscroll, selection clipboard or context menu is implemented.

Path, close, conflict and pending-Reload modals consume pointer actions:
clicks cannot answer a question, activate obscured tabs or edit text. Starting
one of these flows clears native held-button/wheel state. Cancelling it does
not resume an old drag. The pointer remains visible over modal overlays.

Motion/buttons are dispatched in wire order; wheel axes accumulate until
wl_pointer.frame. One fixed-size two-axis accumulator accepts at most 256
axis-related events per frame, with no heap event queue. Discrete steps take
precedence over their paired continuous distance and scroll three rows or
columns per notch. A zero discrete value carries no notch and is ignored.
Without discrete steps, signed distance accumulates at
16 surface units per row or eight per column, retaining fractional remainder
between frames. Stop clears that axis's remainder after the frame; a changed
source clears both remainders. Each frame's output clamps to +/-16 Mi cells
before the viewport applies its document limits. Soft wrap disables horizontal
scrolling through the existing controller policy. There is no kinetic scroll.
The first axis event pins the active tab/revision: a changed tab/revision
before frame prevents applying that frame to a different document. Fractional
carry resets when the next frame targets a different tab or revision.

On enter, after ARGB8888 is advertised, one code-defined 16x24 charcoal/warm
arrow is installed with that enter's serial and hotspot (0,0). It uses a
separate surface and one unlinked 0600 backing file through the existing
SHM pool request. The local file closes after transfer; the server-owned
pool/buffer retains its bytes. Its 1536 bytes are immutable for the connection
lifetime, never rewritten while busy or reattached on enter. A release is
accepted exactly once for the sole attachment; subsequent enters reuse the
surface with the new enter serial. This relies on core wl_surface content
remaining attached across pointer leave/unmapping: enter makes the pointer
image association undefined, not the cursor surface's committed contents.
No cursor theme, font, host library,
frame callback, raw syscall or incoming descriptor consumer is added.

Controller pointer/scroll refusals show a notice instead of being classified
as malformed Wayland transport. Protocol/schema failures still disconnect.

Pure pointer tests cover exact schemas, signed/extreme coordinates, discrete
versus smooth diagonal frames, fractional carry and the event budget. Fake
compositor tests cover pre-focus scalar selection, dragging/leave, Shift
readiness, capability removal/reacquisition, retired events, modal safety,
tab close, frame target binding and cursor requests/descriptor bytes. The
cursor follows the core [Wayland pointer protocol](https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_pointer).
These are not yet a live hardware pointer or jail integration oracle.

### Implemented native menu contract

File, Edit, Format and Help open with a left-button press on their header.
F10 opens File in both key profiles; F10, Escape or C-g closes an open menu
without changing document selection. Left/Right switches groups and Up/Down
moves among enabled items, wrapping within that group. Return or Space
activates the selected item. Repeated keys never activate or navigate menus.
Other keys are consumed while open, not interpreted as text or Emacs prefixes.
Opening cancels pending key prefix, Emacs mark, controller drag and native
held/repeat/wheel state, preserving the document and selection. The new
controller CancelInput event performs this reset without a fake focus loss.

File exposes New, Open, Save, Save As, Close Tab and Quit. Edit exposes Undo,
Redo, Select All and the two key profiles, with clipboard commands enabled
according to the data-device contract below. Format exposes Soft Wrap,
Auto Fill, Fill Paragraph, Fill Column, Check Spelling, Dictionary and
Next/Previous Misspelling under the native spelling contract above. Help
shows an experimental-build About notice. A plus marks the active key profile
or enabled format toggle. Undo/Redo availability reflects the captured
history depth. Scratch mode disables Open/Save/Save As/Dictionary rather
than pretending to persist its text. Only existing bindings are shown:
Windows uses Ctrl+ labels, Emacs uses its C-/M- chord notation, and unbound
items have no shortcut.
Find/Find Next/Find Previous, Replace and Go To Line use the native contracts
below. Help > Command shares the named command prompt below.

Menu actions dispatch the existing controller events, file-path requests and
close coordinators. Keyboard, tab-close marks and menus share one native
close-tab handler. No menu creates a discard or Reload permit, writes files,
marks a snapshot saved or bypasses the pending-I/O guard. Save on Untitled
still enters Save As; Quit/Close still asks about dirty text. Path and
confirmation dialogs take precedence, so F10 and clicks cannot open a menu
through a modal question.

Menu state pins the active tab/revision and key profile at opening. Admission
rechecks them and the popup geometry before executing an item; a stale menu
dismisses visibly without issuing the command. File completion dismisses an
open menu before showing its result or switching tabs. Configure, keyboard
map replacement and keyboard-focus loss dismiss menus; pointer leave also
dismisses without altering keyboard focus. The first press outside a popup
only dismisses it; it cannot click through to text or tabs. Disabled entries
do nothing and stay open. Pointer motion highlights only enabled rows; wheel
events are consumed. Selecting another header switches menus, and selecting
the current header closes it.

Activation uses a separate press on an item after opening the header;
press-drag-release menu selection is not implemented. Release cannot
activate a row or resume a document drag behind the dismissed popup.

Each popup is 320 font pixels wide with 24-pixel rows, scaled by Geometry's
integer scale. The complete menu must fit above the status row: the minimum
size is 320 by (48 + 24 times item count), multiplied by scale. Horizontal
placement clamps to the right edge. A too-small surface refuses to open the
popup and displays an enlargement notice; invisible/clipped rows can never
be activated. A clipped-menu refusal does not reset pending prefix/mark/drag.
A new physical key press still cancels native repeat before menu admission,
as it does for every key. Escape/C-g with a menu open dismisses both the
menu and any underlying notice; F10 only toggles the menu. At most thirteen
rows exist, with static bounded labels, and painting and hit tests use the
same panel geometry. Header geometry derives
from the reference renderer's single menu-bar string. Colors remain warm
and muted, with dim disabled text and a highlighted selected row. The core
headless preview's closed menu bar has unchanged pixels.

Pure tests pin panel bounds and row hits at scales 1-4, disabled navigation,
shortcut widths and header mapping. Controller tests pin prefix/mark/drag
cancellation with selection retained. Fake-compositor tests drive physical
F10, mouse menus, disabled/outside/repeated input, profile/format changes,
Save and dirty-close flows, stale targets, resize and late file completion.
Menus do not imply a remote-control socket, GPU or jail milestone.

### Implemented clipboard admission prerequisite

The safe `clipboard` module captures clipboard intent independently of any
display, descriptor, clock or worker. It does not claim system clipboard
ownership; the native adapter below supplies protocol and descriptor policy.

`Snapshot::capture` requires the active tab and expected text revision.
It captures the directed selection, editor-instance identity and at most
1 MiB of selected UTF-8 bytes as an immutable shared string. Empty selection
returns None and must leave any existing clipboard ownership untouched.
Oversized selection is refused before copying bytes. Subsequent edits do
not change a retained snapshot. Debug output reports identity fields and
byte counts, never the selected text.

`Paste::begin` captures the same active tab/revision/selection binding.
Incoming chunks are collected up to 1 MiB of raw transfer bytes, counting
CRLF before normalization. Chunks may split a UTF-8 scalar or CRLF pair.
Overflow clears the buffered prefix and permanently poisons the transfer:
further chunks and attempted admission return `limit`. Dropping a transfer
cancels it without model effects. The adapter must dispatch Event::Paste
only after successful EOF, never after timeout, cancellation or I/O error;
this pure layer cannot infer EOF or a transport failure from supplied bytes.

Controller Cut/Paste admission consumes the snapshot/transfer. It checks
editor-instance identity, active tab, exact revision and the original
directed selection before any editing. A changed text revision returns
`stale-revision`; another editor, inactive target or different selection
returns `invalid-argument`; a closed target returns `missing-tab`.
Selection is compared by value: moving away
and back without editing satisfies that value check; a native adapter must
separately cancel on focus/target transitions. Text edit followed by Undo
still has a newer revision and cannot revive an old transfer.

Cut deletes only the captured nonempty range. The adapter retains the shared
snapshot for serving before dispatching Cut; a refusal leaves the document
intact and may still leave a useful copy. This token is not a compositor
ownership acknowledgement. Paste validates complete UTF-8 and uses ordinary
Insert admission, normalizing CRLF and rejecting unsupported controls or an
initial BOM in the resulting document. It cannot change file BOM/line-ending
mode and does not invoke Auto Fill. Empty paste is Ignored rather than
deleting the selection. Each text-changing paste or Cut is one ordinary
undo transaction, with the original selection restored on Undo. Pasting
identical text follows ordinary Insert's no-op rule: collapse the selection
without changing revision, dirty state or history. That selection-only change
is not undoable; Undo still refers to the previous text transaction. Existing
document/history budgets, revision/content counters and controller generation
admission apply before mutation. Rejected operations preserve view and input
state as well as text; accepted operations reset input and reveal the caret.
Like semantic Edit, these controller events do not require keyboard focus.
The native adapter must enforce focus and cancel pending transfers on loss.

The eventual window owns at most one copy snapshot and one incoming transfer
at a time, separately charged from document/history memory; the public
library API does not impose a process-wide allocator limit on callers.
Tests exercise byte-at-a-time Unicode/CRLF, cancellation, malformed/oversized
input, stale/foreign/inactive/selection-changed tokens, Undo, encoded-file
budgets and generation exhaustion through the production controller.
This pure layer introduces no replay command or raw consumer.

### Implemented clipboard transport prerequisite

The public `transfer` module owns bounded descriptor I/O independently of
the window loop. The native adapter below binds Wayland data-device objects,
received rights and selection ownership to these transport primitives.

`Incoming::begin` captures Paste intent and creates a private UnixStream
pair. It returns the producer endpoint as an owned File; the caller must
drop its local copy after handing it off so EOF can arrive. The receiver
uses std nonblocking mode, a reusable 16 KiB read buffer and Paste's 1 MiB
raw-byte budget. A step makes at most four read attempts, counting EINTR,
and checks the visible active tab/revision/selection before reading. This
early guard is advisory: an editor replacement with identical visible
values passes it, but final admission rejects its foreign instance. Any I/O,
budget, changed-target or clock failure permanently poisons the transfer.
Only successful EOF unlocks `finish`, which returns the Paste for final
controller admission, including editor-instance identity and UTF-8 checks.
No prefix from a failed or unfinished transfer can become an edit. Drop
cancels and closes without editing. Final admission remains mandatory even
after EOF; this helper is not authority to bypass the controller.

`Outgoing::begin` retains an immutable Arc<str> of at most 1 MiB and owns
exactly the supplied descriptor. The private raw Destination wrapper
requires a writable pipe/socket, adds O_NONBLOCK to its existing status
word and verifies readback before writing. Regular files, devices and
read-only pipes are refused, never reopened through procfs. Each step
makes at most four writes of at most 16 KiB each. Completion and explicit
Cancel restore the exact original status word with readback before std
closes the descriptor; errors are reported. Drop attempts restoration but
cannot report failure. Descriptor duplicates share status flags: callers
must supply an exclusively used write endpoint, with no other writer or
concurrent flag changes. Peer loss is an I/O failure under Rust's default
ignored SIGPIPE disposition; library embedders must retain that disposition
for ordinary std pipe writes. Post-payload restoration failure is diagnosed
as cleanup failure after sending, not a claim that no bytes arrived.
Already sent prefixes
cannot be retracted; this helper does not acknowledge remote paste success.

Both owners take caller-supplied monotonic milliseconds and have an absolute
five-second deadline from begin, never renewed by progress. Backward clock
values and clock exhaustion are refused. Completion and failure are terminal;
callers must keep stepping pending transfers, or drop them on cancellation.
These are per-transfer bounds, not a process-wide allocator or scheduling
guarantee. Metadata/flag queries are synchronous; no hard real-time guarantee
is claimed. The window adapter will separately enforce focus, MIME, serial,
object-lifetime and simultaneous-transfer admission rules.
It must also prove interoperability with real toolkit sources: this endpoint
is a socket, so a producer requiring a FIFO specifically is not supported.

The raw amendment is limited to fcntl F_GETFL=3 and F_SETFL=4 in surface 14;
there is no additional syscall, descriptor adoption site, worker or dependency.
Confinement pins the complete raw source, constants and sole Destination
consumer. Real pipe/socket tests cover byte-fragmented input, EOF-only
admission, 1 MiB round trips, per-step work, deadlines, cancellation, broken
pipes, non-endpoint refusal and restoration through shared descriptor aliases.

### Implemented experimental native clipboard

At initial registry synchronization, a seat plus optional core
wl_data_device_manager v3 enables one seat-bound data device. Higher versions
are capped at 3; missing/older globals leave clipboard commands disabled.
Late-added globals are not rebound. Removing the manager or seat releases
the device and retained source, cancels I/O and preserves documents. A
removed manager object has no destructor and remains inert until disconnect.

Windows Ctrl+C/Ctrl+X/Ctrl+V, Emacs M-w/C-w/C-y and Edit menu Copy/Cut/Paste
reach the same adapter. Copy/Cut require keyboard focus and the serial from
the current actual translated key press or left-button menu press. Synthetic
requests without that serial refuse ownership changes. Repeats never acquire
ownership or start transfers. Empty selection preserves the existing source;
oversized selection refuses before copying. A fresh source advertises only
`text/plain;charset=utf-8` and `text/plain`, both UTF-8. After sending
set_selection with that input serial, retain the immutable snapshot and then
admit Cut through the controller. Wayland provides no ownership acknowledgement;
feedback says the selection was offered. Undo restores a successful Cut.

The window owns at most one source snapshot, one outgoing writer and one
incoming transfer. Copy/Cut refuse while a writer is pending, bounding retained
copy bytes to 1 MiB even when ownership changes. Cancellation of a source
retires that object while an already-started writer may finish its retained
snapshot. Busy, unsupported-MIME and retired source sends consume and drop
exactly their descriptor. Device/source v3 schemas, including unused drag
events, are fully validated; retired client IDs drain until delete_id.

Server-created offers use a separate bounded map, never the client ID array.
There are at most 32 retained offers. Inspect only the first 64 MIME
announcements per offer and retain at most two supported strings, each at
most 256 bytes. Extra or longer valid MIME announcements are drained and
ignored rather than disconnecting the editor. Unsupported source sends,
including long MIME strings, drop their exact descriptor; transient decoded
strings remain bounded by the wire-frame budget. There is one outstanding
retirement barrier covering at
most 32 ID/generation pairs. Retirements after that barrier was sent coalesce
until its callback, then receive the next barrier. Prefer the explicit UTF-8
MIME; accept text/plain only as UTF-8. ASCII case variants of these two
spellings are accepted, preserving the exact offered spelling in receive.
Unknown encodings or other parameter forms are not guessed.
Selection replacement, null selection and focus loss cancel incoming Paste
and destroy obsolete offers. Destroyed server IDs retain schema tombstones
through a display-sync barrier; a generation tag prevents an old barrier
from deleting a new offer that reuses the same ID. Drag offers are destroyed
without accepting or finishing a drop and cannot replace the clipboard.
There is no PRIMARY selection, middle-click paste or drag-and-drop editing.
Selection may arrive immediately before keyboard enter, as the protocol
specifies: retain that offer while unfocused, but refuse Paste until focus.
Keyboard leave still invalidates and retires the previous selection.

Paste passes the fresh socket producer endpoint to offer.receive, drops its
local copy and collects through Incoming. It checks queued protocol events
before admitting EOF, then uses the same revision/selection-bound controller
Paste as headless tests. Focus loss, keymap replacement, target/selection
change, file/discard modal entry, offer replacement and Escape/Ctrl+G cancel.
Cancellation is checked after each native event, so selection-away-and-back
events cannot revive a pending paste. Empty EOF leaves the selection intact;
malformed, oversized and timed-out input never inserts a prefix. Nonempty
paste is one ordinary Insert transaction, with the documented identical-text
exception; no Auto Fill or file-format change is applied.

The event loop services pings/input while bounded transfers are pending and
caps its receive wait at 10 ms. Transfers retain their absolute five-second
clock and four-I/O-attempt step budgets. Missing keymap or source-send rights
use one five-second descriptor deadline after full-schema validation, not
assumptions about ancillary-message boundaries. Socketpair producers are an
experimental compatibility boundary: FIFO-specific writers are unsupported.
Tests use actual SCM_RIGHTS and pipe/socket endpoints, exact native input
serials in both key profiles, menu activation, fragmented UTF-8/CRLF, terminal
cancellation, immutable source data and repeated offer-ID retirement/reuse.
This does not claim live third-party toolkit clipboard interoperability yet.
The software window is still experimental, not the default $EDITOR path.

### Implemented native Find

Edit exposes Find, Find Next and Find Previous. Windows Ctrl+F opens a
forward query prompt, F3 searches next and Shift+F3 searches previous.
Emacs C-s/C-r open forward/backward query prompts; Return searches, and
C-s/C-r inside that prompt explicitly submit in the chosen direction.
This is submitted literal search, not incremental isearch while typing.
Find Next/Previous without a prior query open the corresponding prompt.

The query is case-sensitive UTF-8, at most 4096 bytes, without regex or
escape interpretation. The native single-line entry accepts printable
translated scalars, Space, scalar Backspace and Ctrl+U to clear. Repeated
keys do not type or submit, and empty Return leaves the prompt open. Seed
entry with a nonempty printable selection within the limit; otherwise use
the last submitted query. Queries persist across tabs for the window's
lifetime, not on disk. Prompt plus history retain at most 8 KiB of query
bytes. The notice includes at most the final 160 scalars, an ellipsis and
caret; the six-row overlay can clip this text on narrow windows, especially
while the input-readiness prefix is present.
Clipboard paste into query/path entry is not implemented in this increment.

Opening cancels prefix, mark, pointer drag, repeat and pending native Paste
without changing the document selection. Entry captures editor identity,
active tab, revision and directed selection. Submission after a changed
target refuses instead of retargeting. Focus/keymap loss pauses entry with
readiness text; restoring input preserves the query. Escape/Ctrl+G cancel
entry, clear pending wrap and leave document text/selection unchanged.
File/discard dialogs keep priority and pointer document actions are blocked
while entering a query. Window close cancels entry before ordinary close.

Each submitted search dispatches the existing controller Find command.
It starts after the current selection for forward search and before it for
backward search. A missing match changes neither selection, undo history,
dirty state nor viewport: report the end/start and record that search intent.
Only the next explicit search of the same query, direction, editor instance,
active tab, revision and selection may wrap. Matching anywhere after wrap
reports that it wrapped; no match reports absence from the whole document.
That attempt consumes wrap permission even when no match exists; another
search must report the boundary again before a further wrap.
Changing query/direction, editing, switching tabs, selection motion or
cancellation invalidates pending wrap. Per-event observation prevents
selection-away-and-back from reviving it; focus/keymap loss also clears it.
Successful matches reveal the selected range through the ordinary view
controller and never create an undo entry or change text revision.

The three Find entries are followed by Replace and Go To Line. Complete panel
fitting uses the menu's current item count. Tests cover native chords
in both profiles, query entry/cancel, both directions, explicit wrap,
missing/stale/foreign targets, scalar byte limits and overlay-only pixels.
The headless model/replay Find interface is unchanged.

### Implemented native Replace

Windows Ctrl+H and Edit > Replace open the same modal in both window modes;
the menu supplies Emacs-profile access. Find and With are separate single-line
UTF-8 fields, each capped at 4096 bytes. Find starts from a nonempty printable
selection within that limit, otherwise the last submitted search query. With
starts empty. Tab or Shift+Tab switches fields; printable translated scalars,
Space, scalar Backspace and Ctrl+U edit only the active field. There is no
clipboard entry, multiline/escape syntax, regex or case folding. Empty Find
is refused without editing. Empty With is a deliberate deletion replacement.
Typing never searches or edits document text. Repeated keys never act.

Return finds the next match through the existing search history/controller,
selecting and revealing it without editing. Reaching the end reports it;
another explicit Return may wrap under Find's same target/query rules. Query
edits invalidate pending wrap even if the old query is later restored. Alt+R
replaces only a selection exactly equal to Find; otherwise it asks the user
to select a match with Return. It neither searches nor advances implicitly.
Alt+A replaces all nonoverlapping matches in the active document, independent
of selection. Both replacements use the existing Insert/ReplaceAll controller
commands, not typing/Auto Fill. Each successful text change is one ordinary
undo transaction. A size/history limit refuses the whole edit and retains
both fields for correction; no prefix of the replacement is applied.

Replace All with no matches reports absence without dispatching or moving
the viewport. With matches it collapses selection at document end; single
replacement collapses at its replacement end. Identical Find/With reports
unchanged text and creates no undo entry or revision, while keeping these
normal selection-collapse semantics. Undo restores each edit's original
directed selection. Replace All counts for status/no-match admission, then
the model counts for admission and constructs the replacement: three bounded
synchronous scans. This increment does not claim an event-loop latency ceiling.

The modal remains open after searches and replacements. It captures editor
identity, active tab, text revision and directed selection, refreshing that
binding only after its own successful action. Other target changes refuse
submission instead of silently operating on the changed target. Opening
cancels input prefix/mark, drag, repeat, pending Paste and pending wrap while
preserving selection. Document pointer actions are blocked. File/discard
dialogs keep priority; focus/keymap loss pauses entry visibly without losing
fields. Escape/C-g closes and cancels pending wrap; completed replacements
stay edited and can be undone after closing. Window close dismisses entry
before normal dirty-document questions. Neither cancellation path rolls back
already confirmed edits.

The overlay shows result, Find/With tails (24 scalars each with ellipses),
active-field marker and action guidance. While paused, readiness replaces the
result line without discarding it, keeping close/clear guidance in six rows
at 320 pixels wide. Narrower windows retain the ordinary clipping rule. Field
switches retain the result; a changed field or a new action supersedes it.
Both fields plus retained search
history hold at most 12 KiB of text, with no document-sized modal snapshot.
Replace precedes Go To Line, making Edit thirteen rows: its complete popup
requires 320 by 360 pixels at scale 1 (previously 336 high), multiplied by
the integer scale. Windows Ctrl+H remains available below that height;
the Emacs profile requires enlarging the window for menu access.

### Implemented named command prompt

Emacs M-x and Help > Command open a bounded modal command-name prompt.
The Help entry works in either key profile; no Windows key binding is added.
The prompt starts empty and accepts at most 64 lowercase ASCII letters or
hyphens, ignoring other chords rather than inserting text into the document.
Backspace removes a character and Ctrl+U clears entry. Tab completes the
longest common prefix of the registered names; a unique match completes its
whole name. No match preserves input and shows an error. Empty or ambiguous
completion never chooses a command. Return requires an exact registered name;
an unknown or incomplete name remains editable with feedback. Repeated keys
never enter, complete, cancel or run commands. No command runs while typing.

The prompt shows the match count and at most the first three names in lexical
order as hints. Type a prefix to narrow them. Like the other bitmap modals,
it clips to six notice rows on narrow windows; resizing does not lose input.
Help now needs 320 by 96 scaled pixels for its two complete rows, up from
320 by 72 for About alone. M-x remains available without a fitting menu.

The closed registry is exactly:

| Name | Existing action |
| --- | --- |
| `auto-fill-mode` | Toggle the active tab's Auto Fill. |
| `fill-paragraph` | Fill its current paragraph. |
| `goto-line` | Open Go To Line. |
| `ispell-buffer` | Check the whole active document on demand. |
| `next-misspelling` | Select the next stored range, without wrapping. |
| `previous-misspelling` | Select the previous stored range, without wrapping. |
| `set-fill-column` | Open Fill Column. |

These names dispatch the same native item handler as menu activation. Toggle
commands use the setting at execution; this prompt does not display or imply
a captured setting. Numeric commands open their own captured-setting/target
prompt without editing text. There is no evaluation, argument syntax, shell,
subprocess, executable lookup, dynamic registration, plugin or interpreter.

Command entry pins editor identity, active tab, text revision and directed
selection. Return refuses a changed target rather than applying to another
document. Opening clears key prefix/mark, pointer drag/repeat state, pending
Paste and search wrapping; it preserves text and selection. Document pointer
input is blocked while modal. Escape/C-g cancels entry (and any active spelling
scan under its window-wide cancellation rule). Focus/keymap loss pauses entry
with visible guidance and preserves the name. Window close dismisses entry
before the ordinary close flow. File/discard dialogs keep priority.

### Implemented numeric prompts: Go To Line and Fill Column

Go To Line and Fill Column share the private `number` prompt implementation,
with separate range checks and commands. No old numeric-entry path remains.

Format > Fill Column opens the same numeric modal in either profile. It
shows the setting captured at opening and starts with empty input. At most 20
ASCII decimal digits are accepted, including leading zeros; Return accepts
only a representable value from 20 through 240. Empty/out-of-range/overflow
input remains visible with an error for correction, including `2400` rather
than silently truncating it to `240`. Backspace, Ctrl+U, Escape/C-g, repeat
suppression, focus pause and pointer blocking match Go To Line. There is no
new direct key binding. Fill Column follows Fill Paragraph, before the
spelling commands. The larger Edit menu follows its current item-count
minimum under the native menu contract.
Format itself now requires 320 by 240 pixels at scale 1, up from 320 by 216;
multiply both axes by the integer scale. At smaller heights its entire popup
is refused with the existing enlargement notice. F6 and F7 remain available
without their menus, but Fill Column and other menu-only actions require
enlarging the window.

The fill-column prompt additionally captures the original fill setting,
since setting changes do not advance text revision. A changed setting at
submission refuses along with a changed editor/tab/revision/selection.
Applying dispatches the existing `FillColumn` controller command: it does
not change text, selection, saved state or undo history, and affects no other
tab. It does not reflow immediately or enable Auto Fill. The next explicit
Fill Paragraph or Auto Fill typing uses the new setting. The setting is
per-document in-memory state, not persisted to files or a config directory.

Edit > Go To Line or native F6 opens a numeric prompt in either key profile.
F6 remains available when the complete Edit menu cannot fit; Ctrl+G retains
its existing Cancel meaning, including inside this prompt. F10 and menu
navigation also provide keyboard access. Entry
starts empty and accepts at most 20 ASCII decimal digits, scalar Backspace
and Ctrl+U to clear. Repeated keys do not type or submit. Return requires a
positive, representable number naming an existing logical line. Empty, zero,
overflow and out-of-range input retain the prompt and show an error so the
user can correct it. Leading zeros are accepted; signs, whitespace and other
characters are ignored. Escape/Ctrl+G cancel without moving selection.
Error text precedes the ordinary prompt/help so it remains visible at the
320-pixel menu minimum width even with a paused-input prefix. As with other
overlays, extremely narrow windows may clip the six-row notice.
The platform-independent 20-digit entry bound accommodates 64-bit numbers;
values overflowing `usize`, including on 32-bit hosts, refuse normally.

Logical lines are one-based and delimited only by normalized LF bytes,
independent of soft wrapping or viewport width. Every document has line 1;
a final LF creates a final empty line. A valid destination collapses selection
at the first byte of that line and reveals the caret through the ordinary
controller. Invalid destinations do not clamp or change selection, viewport,
generation, text, revision, saved state or undo history. The model's
`GoToLine` command scans at most the bounded document once without building
a line index or copying text. Replay exposes `go-to-line TAB REVISION LINE`
through the same controller command; the arguments are tab-separated on wire.

The prompt captures editor identity, active tab, revision and directed
selection. Submission refuses a changed target instead of moving another
document. Opening cancels prefix/mark, drag, repeat, pending search wrap and
native Paste while preserving selection. Document pointer actions are blocked
and file/discard dialogs retain priority. Focus/keymap loss pauses entry
visibly without losing digits. Window close cancels it before normal close
handling. The largest fixed menu has thirteen rows (a 320 by 360 pixel
minimum at scale 1) and is shown only when
its complete panel fits, using the existing scale bounds. Native tests cover
both profiles, real digit/Return events, invalid input, focus pause, stale
targets, close cancellation and overlay restoration. The shared command is
also tested through replay, including UTF-8 offsets and empty final lines.

### Version-1 compatibility target

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
pointer/menu access. Limit the text-v1 payload to 1 MiB including an optional
single trailing NUL, with no interior NUL. Limit the parser to 200,000 tokens,
nesting depth 32, 768 keycode names including aliases, 256 types, 24 virtual
modifiers, 16 levels per key/type, 1024 compatibility interpretations and
1536 modifier-map targets; overflows refuse the map. Sparse keycode values
do not allocate a dense table.

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

### Implemented keyboard compiler

`xkb::TypeCatalog` validates the bounded lexical envelope and compiles type
tables, not whole keyboard semantics. It requires one self-contained
`xkb_keymap` with unique keycodes, types, compatibility and symbols sections;
optional geometry is lexically bounded and otherwise ignored. There is no
file lookup or action execution. Comments and quoted names do not affect
delimiter depth; supported string escapes are backslash, quote, `n`, `r`,
and `t`. Other escapes, includes and malformed delimiters are diagnosed.

Type names are arbitrary. A type's `modifiers`, `map`, `preserve` and
`level_name` declarations compile to a lookup table. Missing matches select
level zero; preserve-only entries imply level zero. Selection reports
XKB-mode consumed masks (type mask minus the selected preserve mask), not
GTK-mode consumption. Virtual declarations and explicit real encodings are
collected across types, compatibility and symbols. `keyboard::Keymap` supplies
the remaining bindings derived from the map; neither API guesses Alt or
NumLock. Explicit encodings are ORed with implicit modifier-map bindings.
Entries requiring unbound virtuals are inactive, not zero-mask
matches. Numeric type masks are restricted to the eight predefined real
bits; virtuals must be named, with explicit encodings allowed across all
32 state bits. Distinct modifier expressions can resolve to the same mask
(for example Alt and Meta sharing Mod1); the first declared active entry
wins, including preserve-only entries, as in XKB. Duplicate definitions of
the same expression and entries outside the declared modifier mask are
refused. Keywords and named level prefixes are case-insensitive; quoted
type names and virtual modifier names retain their case. Unsupported assignment
fields on unused types are retained as named diagnostics and refuse only
when that type is resolved. This parses compiled tables, not source-level
type defaults, includes or merge operations.

`keyboard::Keymap::parse` compiles keycodes, aliases, symbols, real modifier
maps and compatibility interpretations from the same token stream. Keycodes
are XKB numbers, at least eight; lookup and translation take Wayland/evdev
numbers and add eight with checked arithmetic. Duplicate names/codes, alias
cycles or missing targets, duplicate symbol definitions, conflicting real
modifier assignments and out-of-range declared keycodes are refused. An
indirect modifier-map keysym selects the lowest retained level, then lowest
keycode, after type normalization.
Explicit key types have arbitrary names. Absent types use XKB's standard
one/two/four-level inference rules for ASCII case and keypad symbols; the
selected declaration supplies the semantics, not a hardcoded type table.
Symbols are truncated or padded with NoSymbol to that type's level count.

Interpretations match specific keysyms before wildcard symbols, then
Exactly, AllOf, NoneOf, AnyOf and AnyOfOrNone predicates, preserving source
order for equal priority. Identical headers are refused rather than applying
source merge operations. Ordered interpretation/key defaults, explicit key
repeat and explicit key virtual-modifier assignments are supported. At higher
levels, `useModMapMods=level1` predicates test an empty modmap; matching
actions still use the key's real map for their `modMapMods` operand. Those
higher-level matches do not add virtual bindings. Other implicit virtual
bindings OR the matched keys' real modifier maps. NoSymbol does not match
interpretations. Repeat comes from the first
level's winning interpretation, or defaults to true for a nonempty first
symbol and false for NoSymbol; an explicit key repeat value wins.

SetMods, LatchMods and LockMods operands identify the logical Shift, Caps,
Control, Alt/Meta and NumLock masks. Without a modifier action, the key's
real assignment supplies its role. Actions are never executed. Ambiguous
overlapping role masks, high-bit role encodings, explicit actions on used
keys and unsupported modifier-key actions are refused. Compatibility actions
for non-modifier keys, such as server VT switching, cannot become editor
commands. RedirectKey is refused anywhere. A used key means one with a real
modifier assignment or a supported text/command/modifier symbol after type
normalization. Every handled real-mask combination is validated on used keys.
Unsupported properties on unused keys/types do not activate them. Only one
symbol per level and one layout group are admitted; malformed declarations,
source merge operations and unsupported section-level syntax are refused.

`lookup` reports selected symbol identity, level, consumed mask and repeat
eligibility; unknown numeric vocabulary remains named. `translate` returns a
logical chord or an ignored-key result. The supplied depressed, latched and
locked masks are unioned without mutating state; nonzero groups and bits
outside the derived profile are diagnosed. These event-local refusals have
typed `InputError::UnsupportedState` and `UnsupportedSymbol` outcomes,
distinct from compilation diagnostics; adapters report and ignore that event
without disabling the validated map. Unknown/overflowing key numbers are
ignored before state admission. The key iterator also uses evdev numbers.
Unconsumed XKB real Lock (mask 2) uppercases ASCII letters after type
selection, regardless of which key/action sets it. A remapped Caps role is
admitted for type selection but does not invent a new capitalization rule.
Consumed Control/Alt do not become shortcuts;
preserved modifiers do. Alphabetic shortcuts normalize case and retain real
Shift intent (`C-S-s`), without treating Caps Lock as Shift. ISO_Left_Tab
produces `S-Tab`; keypad digits/operators become ASCII and keypad navigation
becomes its ordinary command. F13-F35, system/media keysyms and unsupported
function-key system-action levels are ignored. Other out-of-profile symbols
are diagnosed on use, never substituted with physical US text.

`TypeCatalog::parse` alone remains insufficient for keyboard activation.
The scratch-window adapter calls the whole compiler at its sole descriptor
consumer. Confinement tests pin that consumer, the raw boundary, and the
compiler/seat access roster. `seat::Input` owns repeat scheduling separately;
the compiler's repeat metadata alone is not a timer or held-key state.

`tests/fixtures/us.xkb` is a complete libxkbcommon-compiled evdev/pc105/US map
with upstream license/provenance, not a captured Weston keymap. All 26 type
tables are checked against independently generated libxkbcommon level and
consumed-mask results for every real-mask combination. Another independent
oracle checks 106 US keys' levels, keysyms, consumed masks and repeat flags
across all 32 supported real-mask states. The td map is also
read from its existing source for tests; no production compositor keyboard
module is imported. These fixtures do not replace the live Weston test.

## `$EDITOR`, td-mail, and td-jail

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

The caller inspected for this design is td-mail (then the standalone
`tmc` repository, now `td-mail/` in this tree). Its `src/tui/mod.rs`
selects `[ui].editor`, then `$EDITOR`, then `vi`; `spawn_editor` starts
`sh -c` with the editor command and displayed draft path concatenated into
one string. The TUI continues immediately. A background
thread waits for the shell child, ignores its exit status, and removes both
the draft and any attachment directory. td-mail neither rereads the saved file
nor submits mail. Consequently a normal Save followed by Quit loses the
temporary draft to caller cleanup; retaining it requires Save As to a
persistent granted path. This editor must not be described as a complete
td-mail mail-composition workflow until draft retention/submission is resolved.

`src/compose.rs` creates mode-0600 `.eml` files inside a mode-0700 directory,
preferring `$XDG_RUNTIME_DIR/td-mail/drafts`, then the XDG state directory. The
draft format includes mail headers, `--text follows this line--`, and
potential MML attachment tags pointing to temporary sidecar files. Preserve
these bytes as ordinary text. Saving the draft elsewhere does not preserve
the referenced attachment files when td-mail later removes them. Recognizing,
retaining, or submitting mail is a separate requested product capability.

td-mail's unquoted shell concatenation also means paths containing shell syntax
or spaces are not passed as literal argv today. td-editor can accept such
paths correctly but cannot repair a command already misparsed by its parent.
A td-mail integration change must resolve argument construction at the caller;
do not work around it by evaluating shell text inside the editor. The editor
must avoid consuming the TUI's inherited terminal input.

An integration increment must make the executable and exact runtime closure
available inside the jail in which td-mail runs, set its explicit `EDITOR`
environment, and provide the intended file/directory grants.
`APPLICATIONS.md` section X.4 currently says source-built td store closures
are absent from the jail, so
this requires an actual packaging/layout decision; a host `/bin/td-editor`
path is insufficient. Keep source-built editor artifacts distinct from
marked foreign application payloads.

The caller's real launch path is the acceptance test: launch td-mail, request a
draft, observe an editor frame, edit and save while its child remains live,
and verify exact saved bytes before caller cleanup. For retention, exercise
Save As to a persistent granted directory, close the window, and prove that
td-mail remains responsive, its temporary draft is cleaned up, and the retained
copy survives. Attachment retention and submission need their own agreed
oracle. Include filenames with spaces and leading dashes after correcting
the caller, unwritable paths, cancellation, missing display, and an attempted
path outside the grant. An isolated Wayland smoke test alone is not evidence
that td-mail's jail can launch the editor.

## Test and control architecture

The safe `control` library now supplies the one-frame decoder/encoder,
`state`/`text` queries and a bounded revision-checked editing subset. It
shares controller snapshots,
scalar-aligned text pages and byte codecs with replay. The exact implemented
field order, errors, limits and conformance fixtures are recorded in
[CONTROL.md](CONTROL.md). The separate `control_socket` library implements
explicit private Unix listener publication with descriptor-pinned paths,
owner/mode admission and identity-checked cleanup; its complete path/trust
contract and same-UID race boundary are in that reference. The
`control_worker` library adds eight-connection
nonblocking transport, typed bounded UI jobs and five-second acceptance-based
deadlines under CONTROL.md's exact scheduling contract. The experimental
`--window --control-socket PATH` adapter now connects read-only state/text
requests, including coarse native modal/job/spelling flags, plus Select
Tab/Range, Insert, Delete, Undo, Redo, Fill Paragraph, Auto Fill/fill-column
setters, Go To Line and whole-window key-profile selection. Literal Find
and whole-document Replace All use the same controller/replay commands with
bounded private text arguments. Find pins the starting selection and
explicitly selects direction/wrapping; neither operation changes native
prompt entry history. Its native `no-match` reply differs from a modal
`unavailable` refusal. Dispatch requires live job admission and target
revision; selection-relative operations pin the directed selection. Native
modals refuse edits without dismissal.
All edits use the ordinary controller, including history, view refresh and
native search/spelling/Paste/repeat invalidation. Its exact
implemented subset, startup/cleanup behavior and two-action-per-turn budget
are specified in CONTROL.md. Native spelling-result pages borrow validated
reports, pin text revision and a never-reused window scan ID, and expose no
partial marks. This also distinguishes a recheck or dictionary replacement
without text changes. Native frame snapshots and held `wait-frame` requests
now implement callback acknowledgement without blocking Wayland dispatch.
Remote Check Spelling invokes ordinary F7 admission and returns a checked
window-local job ID. Native state retains up to 64 ordered historical outcomes,
evicts only terminal rows, and exposes the associated scan ID for result pages.
CONTROL.md defines startup errors, cancellation, completion, eviction and
transport-lifetime semantics. Remote New uses the ordinary controller event,
returns the created tab ID, and preserves existing text and file associations.
It has no revision target or file authority; CONTROL.md pins its non-idempotent
admission and refusal rules. Remote Close Tab, Quit and close-dialog
Cancel/Discard are connected, with IDs shared by physical and remote close
and deferred window-close approvals. Remote Open uses the ordinary file
worker and shares the bounded job history with spelling. Completion captures
the exact selected/created tab and revision before later UI actions.
Duplicate Open retains edits and missing files remain unwritten.
CONTROL.md defines OS-byte
paths, admission guards, coarse file failure codes and historical outcomes.
Remote writes and other dialog answers remain unimplemented.
The complete endpoint below remains the version-1 target; controller
generations are not presentation evidence.

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
and has no TCP listener or compositor control dependency. The implemented
publication prerequisite additionally requires trusted ancestors and bounded
absolute paths, as specified in CONTROL.md. Refuse symlinked
socket parents and any existing endpoint, including stale sockets; the caller
removes stale endpoints explicitly. Cleanup checks the owned socket inode
under CONTROL.md's publication trust boundary. A control worker handles
framing and deadlines, sending bounded typed messages to the UI thread;
socket reads and writes
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
`insert`, `delete`, `undo`, `redo`, `find`, `go-to-line`, `replace`,
`fill-paragraph`,
`set-auto-fill`, `set-fill-column`, `set-key-profile`, `check-spelling`,
`spelling-results`, `save`, `save-as`, `close-tab`, `quit`, `dialog-answer`,
`key`, `pointer`, and `wait-frame`. Text mutations and close requests name a
stable tab ID and expected revision. Stale commands return `stale-revision`
without side effects. `state` reports the active tab, all tab IDs/revisions,
dirty flags, cursors/selections, modes, current dialog, spelling job/status,
and submitted/callback-completed frame generations. `text` takes tab ID,
revision, byte offset and byte limit; it returns a scalar-aligned page and
the next byte offset. Spelling result pages pin both text revision and the
native scan ID; zero discovers an ID only at range offset zero. CONTROL.md
defines their implemented status/count/range fields and 256-range ceiling.

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

Every main-surface redraw invalidation advances a checked native window
generation; conservative invalidations may advance it without new pixels.
Coalesced draws may skip generations. `wait-frame N` requires a nonzero,
already issued generation and waits for a committed buffer tagged with at
least N to receive its frame callback. It reports the actual generation and
document revision rendered. It times out after the whole-request deadline.
This acknowledges compositor processing, not physical scanout; image tests
must separately observe the presented pixels. Buffer reuse still waits for
release, independently of a frame-wait response. CONTROL.md fixes the snapshot
fields, held-job/turn budgets, timeout behavior and fail-stop overflow policy.

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
   The safe layout, bitmap reference renderer and input controller are landed
   prerequisites; they do not by themselves complete this window milestone.
3. On-demand whole-document spelling and complete local control: explicit
   scans, result marking/invalidation, paged semantic queries and frame
   synchronization. Exercise the production dispatcher through both inputs.
4. Source-built recipe and td-mail jail integration: staged shared sources and
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
