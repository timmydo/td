# td-pass

td-pass is a two-pane encrypted notebook for legacy passwords, account
details and recovery notes. Its production target is one x86-64 Linux
executable usable on td and a supported foreign Wayland desktop. This
contract starts the workstream; no td-pass executable is implemented yet.
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
Copy and paste operate on the selection, preserving bytes. Visual wrapping
never inserts newlines. Disable Auto Fill and spelling for vault documents.
Standard editor navigation, undo/redo, selection, find and keyboard profiles
remain available. Only titles participate in sidebar search initially.

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
clipboard lifecycle. Move reusable editing/model/viewport behavior from
td-editor into shared components with both consumers updated atomically.
Complete the planned text-entry and list widgets in td-ui; do not fork the
editor or add another Wayland transport or renderer. No plaintext file-save
adapter, arbitrary Open/Save As path, spelling worker, external editor,
editor control socket, plugin or shell command reaches vault contents.

The notebook speaks only td-secret's entry and lifecycle API. On td it uses
the admitted service and holds no vault key. Standalone mode runs the same
backend implementation privately, with its host authentication adapter.
No failed td service request can select standalone mode. Production builds
do not contain a synthetic-token, file-key or test-authorization fallback.

Stored note text is ordinary application content. Authentication PIN entry
is a distinct adapter: td uses its trusted attention path, while the host
adapter states its host trust boundary. td-ui's ordinary editor is not a
trusted authentication widget. Protocol state, device paths, key material
and cryptographic details stay out of the notebook flow.

## Delivery and proof

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
