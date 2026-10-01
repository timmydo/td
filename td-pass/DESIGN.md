# td-pass

td-pass is a two-pane encrypted notebook for legacy passwords, account
details and recovery notes. Its production target is one x86-64 Linux
executable usable on td and Guix System with a Wayland desktop. The
window below is implemented in standalone mode; "Implemented window"
states what it does and what remains.
[td-secret/PORTABLE.md](../td-secret/PORTABLE.md) owns storage, primary and
backup YubiKeys, authentication, migration and recovery.

## Notebook

The left pane contains a search field and a scrollable list of entry titles.
The right pane contains the selected title and a multiline text editor.
New, Rename, Delete, Save and Lock are visible operations; key management
and encrypted import/export are available without occupying the text area.
The status row distinguishes locked, unlocked, saving, saved, unsaved and
failed operations. Failure text contains no password, PIN or token output.

Contents are freeform text. No username/password schema, first-line password
convention, pass CLI, browser extension or Git synchronization is required.
Copy and paste operate on the selection, preserving bytes: a copy carries
the entry's stored line endings, pasted text takes the entry's one ending,
and a paste holding another control character but tab, or invalid UTF-8,
is refused whole rather than altered. Visual wrapping never inserts
newlines. Disable Auto Fill and spelling for vault documents. Standard
editor navigation, undo/redo, selection, find and keyboard profiles remain
available. Only titles participate in sidebar search initially.

Unlock explicitly authorizes a bounded notebook browsing session. Entry
selection and copy do not request repeated token touches. Saving and
protector changes follow td-secret's fresh-operation authorization contract.
Switching away from a dirty entry, closing the window or explicitly locking
asks Save / Discard / Cancel; failed Save keeps the document dirty. Save
captures an immutable entry revision before authentication and reports
success only after durable publication. A later edit stays dirty.

System lock, suspend, authority loss and session expiry cannot be delayed by
a dirty-document dialog. They hide the notebook and discard volatile unsaved
edits, with this behavior explained before the first editing session. On
unlock, report that interrupted edits were not saved without retaining their
contents. An encrypted draft mechanism is separate work and cannot bypass
the write policy. Host lock integration must be established before claiming
this behavior on a supported host.

Lock clears entry titles and bodies from the UI, undo/redo, search, pending
clipboard transfers and retained render buffers as their ownership permits.
Revoke the application's clipboard source on lock and expiry; do not clear
another application's newer selection. Once a recipient has copied bytes,
td-pass cannot retract them. No global clipboard manager or text-history
integration is added. Best-effort buffer clearing is not guaranteed erasure
of compiler or compositor copies.

## Reuse and authority

Use td-ui's Wayland client, font, raster, chrome, keyboard, pointer and
clipboard lifecycle. The editing, model and viewport behavior is td-ui's
editor core (`td-ui/DESIGN.md`, "Editor core"), moved there whole from
td-editor with its consumers updated atomically; the notebook's editor
pane is that core, never a copy. Its vault-document policy is the core's
too: filling refused, copy and cut of the selection alone, the editor's
find, line endings kept, and a lock that forgets every document and
its history. Complete the planned text-entry and list widgets in td-ui;
do not fork the editor or add another Wayland transport or renderer. No
plaintext file-save adapter, arbitrary Open/Save As path, spelling worker,
external editor, editor control socket, plugin or shell command reaches
vault contents.

The notebook speaks only td-secret's entry and lifecycle API. On td it
uses the admitted service and holds no vault key. Standalone mode runs
the same backend implementation privately, through the td-secret
library's `pass` module, with its host authentication adapter.
No failed td service request can select standalone mode. Production builds
do not contain a synthetic-token, file-key or test-authorization fallback.

Stored note text is ordinary application content. Authentication PIN entry
is a distinct adapter: td uses its trusted attention path, while the host
adapter states its host trust boundary. td-ui's ordinary editor is not a
trusted authentication widget. Protocol state, device paths, key material
and cryptographic details stay out of the notebook flow.

## Implemented window

`src/main.rs` dispatches td-secret's token worker before anything else,
then admits a mode from os-release: an `ID` or `ID_LIKE` naming td is
refused, since td mode's service is not reached by this build, and an
unreadable or repeated identity admits nothing; any other system runs
standalone. No refusal falls back to the other mode.

- **Vault thread.** `src/backend.rs` alone holds td-secret's `pass::Host`,
  the unlocked `Vault` and the enrolled keys' credentials, and serves the
  window's commands in order. A token operation blocks it, never the
  window. Each operation carries a number; its prompts reach the window
  with it, and an answer for another number is dropped, clearing any PIN
  it carried. Lock cancels the operation's `pass::Cancel` and declines
  the prompt it may wait on, then drops the vault.
- **Window state.** `src/app/` is the notebook as td-ui widgets: the
  action strip (New, Rename, Delete, Save, Find, Keys, Lock), the search
  field over the title list, the title field over the editor pane, and
  the status row. It reaches the vault only through commands and replies, so
  its tests run without a token. Titles and bodies travel in clearing
  owners (`src/plain.rs`), and the window's title names no entry.
- **Entries.** Every entry document is loaded through one function that
  refuses filling first; copy and cut take the selection alone. Save
  sends an edit, a creation or, for a title alone, a rename against the
  entry revision read; it reports success only on the vault's commit,
  and an edit made meanwhile stays dirty. Delete asks first.
- **Unsaved changes.** Choosing another entry, New, Lock or closing the
  window with unsaved changes asks Save, Discard or Cancel. While a save
  or a key operation is in flight another entry and New wait for it,
  and Lock or closing offers only Discard; when it ends the question is
  asked again against what is then unsaved, so edits made meanwhile are
  never given up unasked. Discard closes the entry's document. A failed save
  keeps the entry dirty. A save is recorded for the document it saved
  only while that document is open, no save starts while another entry
  is being read, and a read answered after the open entry was edited
  leaves that entry open.
- **Keys.** Keys (Ctrl+K) shows the notebook's enrolled keys in place
  of the panes, naming the one that authorizes saves; the open entry
  and its unsaved edits stay as they were, a paste asked for a pane is
  dropped and a drag ends, and putting the view away returns the focus
  it had, or the one an operation that ended under the view gave. Use for
  saves (Return) picks another enrolled key for later saves, without a
  token. Add backup (Insert) enrolls one more backup through the
  prompt, authorized by the key that authorizes saves. Replace (Delete)
  revokes the keys marked with Space or Shift and a press, or else the
  selected one, after a question naming them: every kept key is asked
  for, and one new key is enrolled, as the primary when a primary is
  revoked. The window refuses to revoke every key. It names keys by
  their place in the list the vault thread last gave it; the thread
  holds their credentials.
- **Copies.** In the keys view Export (Ctrl+E) opens td-ui's finder on
  folders, from `$HOME`; Ctrl+Return writes the notebook's encrypted
  copy, the authenticated ciphertext of the revision the session holds,
  into the listed folder as `td-pass-notebook-r<revision>.tdpass`: a
  new file, mode 0600, never written over an existing file, synced, and
  its folder synced where the folder allows it. A failed write empties
  the partial copy and removes its name only while that name is still
  the file made. On an account holding no notebook, Import (Ctrl+O)
  opens the finder on files, offering only those no larger than a copy
  can be. The vault thread checks that the chosen path is a file, opens
  it without waiting on a FIFO, reads it to that bound and lists the
  keys it opens with; the copy is imported with the one chosen, through
  the prompt, and td-secret authenticates it whole before placing it.
  Folders are listed on a thread of their own, to the finder's bounds,
  so a slow folder never holds the window; Ctrl+L, or a press outside
  the finder, closes it and acts. Only ciphertext and listings cross
  `src/files.rs`.
- **Prompt.** A presentation asks for the named key to be connected; a
  PIN request shows a masked field that refuses copy. The instruction
  names the key before the operation, so a narrow row cuts the
  operation's words rather than which key. Escape or Cancel
  declines, which the vault reports as cancelled.
- **Lock.** Lock forgets every document and its history, the find query,
  the titles, the fields and any pending paste, withdraws the window's
  clipboard offer, and has td-ui zero the frames it keeps: its pixels
  and every buffer the compositor has released at once, one still
  attached when released. A paste lands only where it was asked: one
  for a replaced entry or an ended prompt is dropped.
- **Frames.** td-ui keeps each frame in a file in the directory it is
  given. The window gives it `$XDG_RUNTIME_DIR` or `/dev/shm`, whichever
  `/proc/self/mountinfo` shows on tmpfs or ramfs first, and refuses to
  start when neither is, so a frame never reaches a disk.

Tests drive the window state headless: unlock through both prompts,
search, open, edit and save with the entry's line endings, a failed and
a stale save, the unsaved-changes question, selection-only copy and cut,
paste, creation, rename, delete, a declined prompt, lock during a save
with its late replies ignored, the rules for a save in flight, a read
answered after an edit, pastes bound to their place, the dialog's
placement, the keys view's use, add and replace by key and by pointer,
the refusal to revoke every key, marks that a held Space does not
flicker, lock during a key operation, a failure's report kept past the
view, the unsaved edits and focus kept under the view, the lists given
their keys when the window grows, and painting the notebook, its keys
view, its prompt and dialogs and each locked view, export into the
folder the finder accepts, import of a chosen copy with one of its keys,
a copy given up or unread, the finder painted, filtered and closed by
Ctrl+L or the strip's Lock, and a listing for a closed finder dropped.
The vault thread's tests pin that a prompt takes only its operation's
answer, that cancel declines once and that a key command without an open
notebook is refused, as are a copy that cannot be read and an export or
import without a notebook; the files' tests that a copy is written new
and private, never over another, and read to its bound, and how folders
are listed; the frame directory's, the mount table's rules. Confinement
tests pin the source inventory, that pure files reach no system, vault
or compositor and only the toolkit's drawing, widget and editor modules,
that td-secret is named only by the vault thread and the worker
dispatch, the two reads the window makes itself, that it lists folders
only through `src/files.rs`, that pure files call no path method that
reaches the file system, that copies are written and read only through
`src/files.rs` in those ways, and the vault-document policy.

Not yet: the native compositor cases, host lock and suspend integration,
td mode, and the recipe and image integration of increment 5.

## Delivery and proof

### First foreign host: Guix System

Guix System is the first supported-host target. Use Sway as the initial
desktop baseline, matching the running desktop on the user's machine;
elogind is also present there. Weston fixtures can exercise client behavior
but do not prove the Sway session's lock or suspend integration. Record the
Guix system generation, kernel, compositor and session configuration with
the acceptance results instead of inferring them from the distro name.

Run the same td-built executable as the ordinary desktop user. USB access
comes from the host's declared device policy. This host already has FIDO
udev rules using `uaccess` and `plugdev`; the observed token node is mode
0660, owned by root and plugdev. The development agent runs under a separate
account and its denied HID open is not a test of the desktop user's access.
Acceptance must exercise both permitted access and a denied open, hotplug,
removal during authentication, and capability negotiation on both keys.
A permission error must remain distinct from an unsupported authenticator.
Device indices and numeric user/group IDs are not configuration identities.

Establish the Sway lock path and elogind suspend notifications explicitly,
including lock during editing or authentication, suspend/resume, and loss or
restart of the event source. Merely finding elogind or a Wayland socket is
not evidence that these events reach td-secret. Real-secret support still
requires the portable-vault contract's dump, swap, clipboard and plaintext
lifetime checks. No host configuration change or successful hardware unlock
is implied by naming this first target.

### Artifact and acceptance

The executable must have a portable runtime closure and a declared
architecture/kernel/Wayland baseline; compiling the source separately on two
systems is not same-executable evidence. Source-built distribution packaging
must meet td-profiler's flags and debug-companion contract. No ambient host
library or prebuilt binary enters a td output.

Use synthetic data for UI/model fixtures and a separately built test backend
for deterministic authorization failures. The shipped backend remains the
one used for real token, persistent-store and complete deployment tests.
Native compositor and independent foreign Wayland tests must observe entry
selection, edit/undo, copy/paste, save failure, stale revisions, dirty-close
decisions and lock during an in-flight operation. Inspect retained buffers
and clipboard lifecycle after lock; a backend key deletion alone is not a
successful lock test. The first production target is complete only with
td-secret's physical primary/backup and fresh-machine recovery evidence.
