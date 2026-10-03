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

Unlock explicitly authorizes a bounded notebook session for browsing and
saving, until it locks. Entry selection, copy and Save do not request
repeated token touches. Protector changes and import follow td-secret's
fresh-operation authorization contract. Switching away from a dirty
entry, closing the window or explicitly locking asks Save / Discard /
Cancel; failed Save keeps the document dirty. Save captures an immutable
entry revision and reports success only after durable publication. A
later edit stays dirty.

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
then answers `--help`, then admits a mode from os-release: an `ID` or
`ID_LIKE` naming td is refused, since td mode's service is not reached
by this build, and an unreadable or repeated identity admits nothing;
any other system runs standalone. No refusal falls back to the other
mode.

- **Swap.** td-secret admits swap that keeps pages in memory, zram
  without a writeback device. Other active swap opens nothing: the vault
  thread keeps td-secret's `SwapRisk` and names its devices, and the
  window asks whether to open anyway, in a question as tall as the
  window's body so at 800x600 all of it shows unscrolled. It says first
  that the kernel may write the PIN being typed, the vault's key and
  entry text to that swap, where they can outlive td-pass; then each
  device, escaped and bounded so any swap table can be asked about; that
  Open anyway covers this run only; that swapoff does not erase what was
  written, that anyone who can read the storage, or unlock it under a
  lasting key such as full-disk encryption, can read it, that a key
  fresh at every boot loses it only at the next restart, and that
  hibernation writes all of memory the same way; and how to avoid it:
  swap off or zram. Cancel is the default and keeps the vault closed;
  Return asks again. The question that comes unasked opens away from the
  pointer. Open anyway passes the kept risk back, for those devices and
  this process only; nothing is stored, and the next run asks again. A
  device that appears later refuses the next token presentation.
- **Vault thread.** `src/backend.rs` alone holds td-secret's `pass::Host`,
  the unlocked `Vault` and the enrolled keys' credentials, and serves the
  window's commands in order. A token operation blocks it, never the
  window. Each operation carries a number; its prompts reach the window
  with it, and an answer for another number is dropped, clearing any PIN
  it carried. Lock cancels the operation's `pass::Cancel` and declines
  the prompt it may wait on, then drops the vault.
- **Window state.** `src/app/` is the notebook as td-ui widgets: the
  action strip (New, Rename, Delete, Save, Find, Keys, Lock, Quit), the
  search field over the title list, the title field over the editor pane,
  and the status row. Every strip, locked or unlocked, ends with Quit,
  which does what closing the window does, over an open prompt, question
  or finder too: it declines a waiting prompt and asks about unsaved
  changes. It reaches the vault only through commands and replies, so its
  tests run without a token. Titles and bodies travel in clearing
  owners (`src/plain.rs`), and the window's title names no entry.
- **Entries.** Every entry document is loaded through one function that
  refuses filling first; copy and cut take the selection alone. Save
  sends an edit, a creation or, for a title alone, a rename against the
  entry revision read; it reports success only on the vault's commit,
  and an edit made meanwhile stays dirty. Delete asks first.
- **Unsaved changes.** Choosing another entry, New, Lock, Quit or closing
  the window with unsaved changes asks Save, Discard or Cancel. While a
  save or a key operation is in flight another entry and New wait for it,
  and Lock, Quit or closing offers only Discard; when it ends the question is
  asked again against what is then unsaved, so edits made meanwhile are
  never given up unasked. Discard closes the entry's document. A failed save
  keeps the entry dirty. A save is recorded for the document it saved
  only while that document is open, no save starts while another entry
  is being read, and a read answered after the open entry was edited
  leaves that entry open.
- **Creation.** With no notebook yet, Create (Return) first asks which
  keys to create it with, Cancel the focus it opens on. Primary and
  backup enrolls both, either of which opens the notebook alone. One key
  only is the explicit decision td-secret/PORTABLE.md names: the
  question says that only an enrolled key and its PIN open the notebook,
  with no password or reset, and that with one key losing it or blocking
  its PIN loses the notebook for good; the question is tall enough that
  all of it shows at 800x600 without scrolling. A notebook with one key
  says so when it opens, and its keys view says Insert adds a backup;
  Replace on the only key refuses and says to add a backup first.
- **Keys.** Keys (Ctrl+K) shows the notebook's enrolled keys in place
  of the panes, naming the one that authorizes adding a key: the key it
  was unlocked with, or that key's replacement; the open entry and its
  unsaved edits stay as they were, a paste asked for a pane is dropped
  and a drag ends, and putting the view away returns the focus it had,
  or the one an operation that ended under the view gave. Add backup
  (Insert) enrolls one more backup through the prompt, authorized by
  that key. Replace (Delete) revokes the keys marked with Space or Shift
  and a press, or else the selected one, after a question naming them:
  every kept key is asked for, and one new key is enrolled, as the
  primary when a primary is revoked. The window refuses to revoke every
  key. It names keys by
  their place in the list the vault thread last gave it; the thread
  holds their credentials.
- **Keyboard.** `F1` shows td-ui's key list over the window
  (td-ui/DESIGN.md, "Key list"): a section each for the prompt, a
  question, the finder, the swap screen, the window while nothing is
  open (opening, locking or refused), the locked view, a copy being
  imported, the keys view, the notebook's shortcuts and each focus,
  listed beside the input code in `src/app/input.rs`. The sections for
  what has the keyboard now come first; under the keys view, which takes
  every key, the notebook's and its focuses' come last.
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
  PIN request shows a masked field that refuses copy. The title names
  the operation and the instruction the key and what to do with it; a
  backup to enroll is asked for as a key not already enrolled, since a
  token holding one of the vault's credentials refuses. Both wrap at
  spaces to the prompt's width, a word longer than a row split, and the
  prompt grows by the rows they take; a window too short for them all
  keeps the field and buttons, the instruction's rows before the
  title's, and a text cut short ends in an ellipsis. The status line
  carries the instruction and the operation. Escape or Cancel declines,
  which the vault reports as cancelled.
- **Lock.** Lock forgets every document and its history, the find query,
  the titles, the fields and any pending paste, withdraws the window's
  clipboard offer, and has td-ui zero the frames it keeps: its pixels
  and every buffer the compositor has released at once, one still
  attached when released. A paste lands only where it was asked: one
  for a replaced entry or an ended prompt is dropped.
- - - **Host lock.** The window watches the host's screen lock and sleep
  through td-secret's `pass::HostEvents` (`td-secret/PORTABLE.md`), on a
  thread of its own so a slow system bus never holds it; unlock, create
  and import wait until the watch has started or its failure has been
  told. A screen lock, sleep, or the events being lost locks at once
  without the unsaved-changes question: the operation in flight is
  cancelled, every edit is given up, and the vault thread is told to
  lock even when nothing shows unlocked, so an unlock that finished as
  it was cancelled is dropped too; one already locking sends no second
  lock. Sleep waits, through logind's delay, until that thread has
  answered the lock; a delay is never held past thirty seconds, longer
  than logind's default maximum. The locked status says why it locked,
  and the next unlock says, once and without keeping either, that
  unsaved edits were given up (during a save, only edits made since it
  was sent) or that a save, delete, key change, creation or import under
  way may have been stopped. Before the first editing session the locked
  view says that a screen lock or sleep locks the notebook too; while
  the watch starts, that unlocking waits for it; on a host whose events
  cannot be watched, or are lost, to lock the notebook before leaving
  it, with the reason in the status, where a narrow row may cut it.
  Sleep that does not wait, at the start or at a later sleep, is warned
  of in the status. A warning waits behind a prompt's instruction.
- **Frames.** td-ui keeps each frame in a file in the directory it is
  given. The window gives it `$XDG_RUNTIME_DIR` or `/dev/shm`, whichever
  `/proc/self/mountinfo` shows on tmpfs or ramfs first, and refuses to
  start when neither is, so a frame never reaches a disk.

Tests drive the window state headless: unlock through both prompts,
search, open, edit and save with the entry's line endings, a failed and
a stale save, the unsaved-changes question, each strip's Quit (over the
swap question and a waiting prompt as well), selection-only copy and cut,
paste, creation, rename, delete, a declined prompt, lock during a save
with its late replies ignored, the rules for a save in flight, a read
answered after an edit, pastes bound to their place, the dialog's
placement, the keys view's adding key, add and replace by key and by
pointer, the refusal to revoke every key, marks that a held Space does
not flicker, lock during a key operation, a failure's report kept past the
view, the unsaved edits and focus kept under the view, the lists given
their keys when the window grows, the key list's order, the pane's
bezel on every side and its seams with the list and the search field
with and without an entry and finding, a pane too short for a row
keeping its lower bezel under the placeholder and an open entry, and
painting the notebook, its keys view, its prompt and dialogs and each
locked view, export into the
folder the finder accepts, import of a chosen copy with one of its keys,
a copy given up or unread, the finder painted, filtered and closed by
Ctrl+L or the strip's Lock, a listing for a closed finder dropped, a
host lock or sleep that asks nothing and is reported at the next unlock,
closes a question and gives up a copy being imported, sleep during a
save and an edit made after it was sent, a host lock during an unlock or
a key change, no second lock while locking, nothing locked while nothing
is held, unlocking waiting for the watch, a warning waiting behind a
prompt and keeping why it locked, and the warnings for a host not
watched, not delaying sleep, or lost. The vault thread's tests pin that
a prompt takes only its operation's answer, that cancel declines once
and that a key command without an open notebook is refused, as are a
copy that cannot be read and an export or import without a notebook; the
files' tests that a copy is written new and private, never over another,
and read to its bound, and how folders are listed; the frame
directory's, the mount table's rules. Confinement tests pin the source
inventory, that pure files reach no system, vault or compositor and only
the toolkit's drawing, widget and editor modules, that td-secret is
named only by the vault thread's file, for the vault and the host's
events, and the worker dispatch, the two reads the window makes itself,
that it lists folders only through `src/files.rs`, that pure files call
no path method that reaches the file system, that copies are written and
read only through `src/files.rs` in those ways, the vault-document
policy, and that the test vault below is built only by its feature.

Native compositor cases run the binary under a real headless
td-compositor and type through its seat
(`tests/support/native_compositor.rs`). With the shipped backend, the
window maps, takes the keyboard and closes on Ctrl+Q, whatever the host
lets its vault do, and on td it refuses with td mode's reason. The rest
run over a test vault, `src/backend/fixture.rs`, which the `test-vault`
feature mounts in place of the vault thread's td-secret calls and host
watch, and which no recipe enables: two synthetic entries under one
primary key whose PIN is 1234, unlocking asking its presence and then
its PIN in td-secret's words and a save asking nothing, a journal of
what it was asked, and one-shot controls that report swap on storage at
the first open, refuse a save, save the entry elsewhere first so the
save is stale, or hold a save in flight until the window cancels it. Its
build gives mode admission a synthetic identity, so it runs on td too.
Over it they observe unlocking through both prompts, entry selection,
select all and copy offered as the window's selection, a paste held by
the compositor and released back to the window, an edit undone, the
save, asking nothing under the unlock, carrying exactly the text against
the revision read, a refused and a stale save, the closing question's
Save meeting the stale revision and Discard closing, and a lock while a
save is held asking only to discard, cancelling the save and saving
nothing, and swap on storage asked about before anything opens, Cancel
keeping the vault closed until Open anyway. After a lock the window
shows the locked view it started on, the compositor's one arm finds no
client selection, and no frame file the window keeps, read through
`/proc`, holds any frame it kept while at rest unlocked, sampled over a
second and a half. These observe the integrated result, and the frame
check is not the scrub's oracle: td-compositor releases each buffer
before the next frame, so the window repaints its one buffer and the
first locked frame overwrites it whether or not the scrub ran. The check
fails only if a second frame file survives the lock holding an unlocked
frame; td-ui's own tests remain the scrub's oracle.

Not yet: a native case that a missing frame scrub would fail, which
needs a compositor that holds the window's buffers across frames; the
host lock and sleep evidence on the supported host, td mode, the
foreign-host acceptance of the same executable, and increment 5's
independent recovery, migration and hardware evidence.

The search, title and find fields and the title list each carry td-ui's
one-scaled-pixel bezel (td-ui/DESIGN.md, `chrome`), so a field's bounds
show against the list under it. td-pass lays the same bezel round the
editor pane: the pane's rectangle is its outline less a scaled pixel
each side, so the pane's scene and its pointer target stop inside it.
A pane with no room inside its bezel shows neither the scene nor the
placeholder, since the editor refuses an empty frame and keeps its old
one, and the placeholder is cut to the pane's height. The left side
ends where the right begins, so the two meet at their own bezels with
no divider between.

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

### Implemented artifact

`recipes/src/recipes/td-pass.rs` builds td-pass as a target Cargo recipe
with the source-built stage2 toolchain, from the checkout's own trees:
td-pass, and the sibling trees its closure mounts by path (td-secret,
td-ui, td-compositor, td-authd, td-busd, td-firstboot and engine). Its
lock names only td-pass, td-secret and td-ui, so the closure is std. The
binary is static PIE with no interpreter, needed library or run path,
built with the target's frame pointers and line tables, and its output
carries the build-ID-matched debug companion. The system image copies
the whole output and links `/bin/td-pass` to it, so the image's
deployment index covers it and the same executable can be carried to a
foreign host. On td itself the window refuses until td mode is built;
the token worker's entry, dispatched before mode admission, still runs
there with only the caller's own device access, as on any host.
`td-pass-test` requires the binary, asserts its static shape and runs
`td-pass --help`, the one argument the window answers besides the token
worker's. A recipe test walks every literal `#[path]`, `include!`,
`include_str!` and `include_bytes!` the compiled crates name, and those
the named files name in turn, and pins the staged trees to exactly the
trees reached; a name built with `concat!` is not walked.

On a host, `./install-apps` builds td-pass with the host's cargo, as it
builds td-photo and td-editor, and installs it to run by name from
`~/.local/bin`; that copy is a development build, not the recipe's
static artifact, and it starts its token worker from its own executable
as the image's does.
