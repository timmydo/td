# Control protocol reference

The experimental `--window --control-socket PATH` endpoint implements the
query and revision-checked editing subset below. It is off by default;
scratch preview and replay do not accept the option. Remote file operations,
dialog answers and Check Spelling admission remain unimplemented. The complete
version-1 target is specified in
[DESIGN.md](DESIGN.md#test-and-control-architecture).

`control` has no listener, thread, filesystem access, clock or Wayland access.
The separate `control_socket` library publishes a private Unix listener
under the contract below.
`control_worker` owns that listener on a bounded transport thread and hands
typed requests to the native window's UI thread.

## Framing

One frame is a four-byte unsigned big-endian length followed by that many
payload bytes. Length excludes the header and must be 1..=1,048,576 bytes,
for requests and responses. There is no newline terminator. `frame` checks
the bound before allocating its output. `Decoder` accumulates one frame,
accepts arbitrary header/body fragmentation, and exposes its payload only
after all declared bytes arrive. A zero length is `protocol`; a length over
the ceiling is `limit`. Incomplete `finish` is `protocol`.

Any decoder refusal permanently poisons it and drops partial payload storage.
More input cannot revive it. Bytes beyond the declared body, if supplied to
that decoder, are refused rather than treated as a second request. Completion
does not prove peer EOF or that no later bytes will arrive: the worker
must dispatch at most once per connection and close it after the response.
`finish` transfers the completed buffer; it performs no read or EOF check.
Empty input chunks make no progress. The decoder allocates at most one MiB
of payload storage, only after validating the length.
The full declared storage is allocated on header completion, before body
bytes arrive. The worker must budget each admitted connection against that
one-MiB allocation, not just against bytes received so far.

The worker below owns deadlines, connection limits, request/response queues
and cancellation. The framing library alone claims none of those safeguards.
Private socket publication is implemented separately below. Replay retains
its consecutive-frame stdin/stdout runner; the one-frame decoder serves the
one-request-per-connection worker.

## Requests

Payloads are ASCII tab-separated records. Literal control bytes other than
field-separating Tab, DEL and non-ASCII payload bytes are refused. Decimal
fields contain one or more digits only: leading zeros are accepted; signs,
spaces, exponents and overflow are refused. IDs/revisions fit `u64`; offsets
and limits additionally fit the host's `usize`. Missing/extra fields, unknown
versions and names are `protocol`. Parsing uses a bounded field iterator, not
a vector proportional to the number of Tab bytes in an untrusted payload.
Envelope validation and error-ID recovery are shared with replay, while the
two adapters retain distinct command allowlists.

In the examples below, field spaces denote literal Tab separators.

| Payload fields | Meaning |
| --- | --- |
| `1 ID state` | Snapshot the current controller. |
| `1 ID text TAB REVISION OFFSET LIMIT` | Read a scalar-aligned UTF-8 page. |
| `1 ID spelling-results TAB REVISION SCAN OFFSET LIMIT` | Read native spelling status and a scan-pinned range page. |
| `1 ID wait-frame GENERATION` | Wait for a main-surface callback at or beyond this native redraw generation. |

The editing subset is specified below. `new`, `load`, file I/O,
physical-input simulation and dialog answers remain refused; this parser is
not a route into replay's broader command set.
`Request::response` borrows `&Controller`, so it cannot dispatch an edit or
change selection, views, history or generation. It returns `unavailable`
for editing, spelling and frame requests; only `Request::execute` admits
edits, and the native adapter supplies its spelling/frame state.

An error response is `1 ID error CODE HEX_DIAGNOSTIC`. A recoverable request
ID is echoed even if the command name is missing or later arguments are
invalid. Failures before a valid ID is parsed use zero: frame-size, ASCII
and version checks precede ID parsing. Replay uses this same envelope helper.
Zero is also a valid caller ID; it carries no authorization meaning.
Currently the diagnostic is the stable code's own ASCII bytes encoded as
lowercase hex. Empty byte strings use `-`; nonempty hex has two lowercase
digits per byte. `hex`/`unhex` and the frame/page limits are shared with
replay, whose old public helper names remain re-exports, not duplicate codecs.
Replay also uses the bounded response-frame encoder.

## Revision-checked editing

All requests below name a stable tab ID and its expected text revision.
Arguments are tab-separated, and selection endpoints are directed UTF-8
byte offsets, not scalar indices or a sorted range.

| Payload fields | Meaning |
| --- | --- |
| `1 ID select-tab TAB REVISION` | Activate that tab after checking its revision. |
| `1 ID select-range TAB REVISION ANCHOR CARET` | Set the active tab's selection to these scalar-aligned byte offsets. |
| `1 ID insert TAB REVISION EXPECTED_ANCHOR EXPECTED_CARET HEX_TEXT` | Replace the expected selection with decoded text. |
| `1 ID delete TAB REVISION EXPECTED_ANCHOR EXPECTED_CARET` | Delete the expected selection, or the following scalar when empty. |
| `1 ID undo TAB REVISION` | Undo one transaction in the active tab. |
| `1 ID redo TAB REVISION` | Redo one transaction in the active tab. |
| `1 ID fill-paragraph TAB REVISION EXPECTED_ANCHOR EXPECTED_CARET` | Fill the paragraph at the expected caret through the ordinary controller. |

Success is `1 ID ok` followed by one trailing Tab (an empty body).
It means controller admission completed, not saved bytes, client receipt,
a frame callback or physical presentation. Query state afterward to obtain
the current revision/selection; another input may intervene before that query.
No history to undo/redo and no text-changing work follow the existing model's
no-op rules; an admitted command can advance controller generation without
advancing text revision or adding history.

The native adapter first refuses with `unavailable` while closed, quitting,
or any path, close, conflict, pending-Reload, menu, Find, numeric, command or
Replace modal is present. It neither dismisses nor answers the modal, and a
refusal does not replace the visible notice. Queries remain available.
Outside modals, a missing tab is `missing-tab`, a differing text revision is
`stale-revision`, and all operations other than `select-tab` require that tab
to be active (`invalid-argument` otherwise). Insert/Delete/Fill additionally
require the exact expected directed selection (`invalid-argument` on mismatch).
These checks run on the UI thread immediately before dispatch. A selection
that moved away and back without text edits passes the value check; Undo
cannot resurrect an old text revision. Select Range deliberately specifies
its destination, rather than capturing an earlier selection.

Insert accepts at most 262,144 raw decoded UTF-8 bytes before normalization.
Oversize is `limit`, malformed hex is `protocol`, and invalid UTF-8 or
unsupported text is `invalid-text`. CRLF normalizes to LF; existing BOM and
line-ending flags are unchanged. An empty `-` replacement deletes a nonempty
selection. Insert never invokes typing's Auto Fill. Each text-changing
Insert/Delete/Fill is one ordinary undo transaction, with the model's
document/history budgets and no-op rules. Invalid boundaries return
`invalid-position`, exhausted budgets return `limit`, and counter exhaustion
returns `exhausted`, without changing model, input or view state.
No intermediate Select is dispatched to implement an insertion.

Accepted operations use the existing `Controller::dispatch` path, reset its
prefix/mark/drag, cancel native repeat/wheel/drag and pending Paste, observe
search-wrap and spelling invalidation immediately, and request redraw.
Cancelling an in-flight Paste emits a visible cancellation notice; other
accepted edits do not replace an existing notice.
Selection-only changes retain spelling marks; edits and Undo clear stale
marks without starting a scan. Semantic commands do not require keyboard
focus or a physical-input serial; they cannot acquire clipboard ownership.
Ordinary file jobs may remain active while editing, as with keyboard input;
this subset neither initiates file I/O nor acknowledges a save.

One accepted connection dispatches at most once. `Job::respond_with` checks
its deadline/liveness immediately before invoking the UI handler, in addition
to queue admission and reply checks. A job already expired or cancelled there
does not execute. Expiry, transport failure or disconnect during/after
execution cannot roll back an accepted command; losing its reply means the
outcome is unknown to the client. Peer disappearance is not synchronously
detectable and the worker does not read further input after the first frame.
Request IDs correlate replies only: there is no cross-connection deduplication,
retry guarantee or cancellation RPC. Do not blindly retry a mutation after a
missing reply; inspect fresh state/text first. A deadline bounds transport
admission and output, not the duration of a synchronous admitted model command.

## Text response

Success is `1 ID ok NEXT HEX_TEXT`. `TAB` may name an inactive tab. Missing
tabs return `missing-tab`; a changed revision returns `stale-revision`, even
if Undo restored the same bytes. Revision is checked when answering, not just
when decoding, so queue time cannot silently retarget a page.

`OFFSET` must be a UTF-8 scalar boundary in `0..=text.len()`. Otherwise the
answer is `invalid-position`. `LIMIT` is 4..=262,144 bytes; other values
return `invalid-argument`. The page ends at the last scalar boundary not
beyond `OFFSET + LIMIT` or EOF. The four-byte minimum guarantees progress
unless already at EOF. At EOF return the same offset with `-`; no extra
terminator or BOM is inserted. Bytes are normalized document text, not the
encoded on-disk representation. Paging pins every request's revision; it is
not an unbounded snapshot retained across connections.

The largest text response uses at most 524,288 hex digits plus a bounded
header/next offset, within the frame ceiling. Serialization never copies
the whole document to produce a page.

## Spelling results

This native-only read query names any open tab (including inactive tabs),
its expected text revision and a scan ID. `SCAN=0` discovers the current
status/ID and requires `OFFSET=0`. Later pages must pin the returned nonzero
scan ID as well as the revision. `OFFSET` is a zero-based range index, not a
document byte offset. `LIMIT` is 1..=256 ranges. Queries never start a scan,
load a dictionary, advance scanning or publish partial marks. Use the ordinary
F7 action to check; remote `check-spelling` is not yet implemented.

Success is `1 ID ok TAB REVISION SCAN STATUS NEXT TOTAL CHECKED UNKNOWN
SKIPPED CAPPED` followed by one or more range fields. All separators,
including the line break shown here, are single Tabs on the wire.

- `STATUS` is `no-dictionary`, `not-checked`, `checking` or `complete`.
- `SCAN` is zero for the first two statuses. Each admitted native scan gets
  a window-lifetime, checked, strictly increasing nonzero `u64` ID, retained
  on completion. Cancel, edit, recheck, tab close or dictionary replacement
  invalidates the affected scan/report; IDs are never reused, even when a
  replacement dictionary has identical bytes. Counter exhaustion returns
  `exhausted` from the ordinary Check Spelling action, retaining prior valid
  work. IDs are not durable across editor processes/socket lifetimes.
- `NEXT` is `OFFSET` plus the number of returned ranges. `TOTAL` is the
  number of stored marks, not the number of unknown words. At `OFFSET=TOTAL`
  the page is empty. No status before completion exposes partial results:
  `NEXT` and `TOTAL` are zero and its four count/cap fields are `-`.
- Completed counts are unsigned decimals; `CAPPED` is `0|1`. Counts cover
  the whole scan, including words omitted from the shared 10,000-mark budget.
  A completed scan with no unknown words has numeric count fields with
  `UNKNOWN=0` and is distinct from an unchecked document.
- Each range is one `START,END` field, in ascending document order, with
  scalar-aligned, half-open UTF-8 byte offsets into the normalized text.
  An empty page has one `-` field instead. No word text is copied.

Validation order is missing tab/revision (`missing-tab`/`stale-revision`),
limit or zero-scan/nonzero-offset misuse (`invalid-argument`), nonzero scan
mismatch (`stale-revision`), then offset beyond stored marks
(`invalid-position`). This includes cancelled/replaced reports and edits
followed by Undo; old pages never become valid again. A fresh zero-ID query
can discover a newer scan, but cannot continue an older page sequence.
Pending scans accept only offset zero. Status and pages are borrowed on the
UI thread without changing selection, generation, notices, modals or history;
queries remain available during modals. Existing whole-request transport
bounds apply. Each page is below 16 KiB under the document/range ceilings,
and serialization neither copies the whole document nor clones its marks.

## Frame acknowledgement

Native `state` supplies `window-generation=N`, a checked redraw-invalidation
counter starting at one for the initial pending frame. Main-surface redraw
requests advance it, including native notices, menus, dialogs, spelling and
caret changes. Conservative invalidations and no-op edits may also advance
it; multiple invalidations can coalesce into one submitted frame, so values
may be skipped in the submitted/completed streams. This is separate from
the controller's `generation` and is not a version of every native job flag.
It is local to one window lifetime, not durable across endpoint restarts.

`wait-frame GENERATION` requires `1..=window-generation` at UI admission.
Zero or a future value is `invalid-argument`. The request has no side effects,
does not request a redraw or dismiss a modal, and does not require focus.
If a matching callback already completed, respond immediately. Otherwise
retain the job until a main-surface frame of at least that generation receives
its `wl_callback.done`, or the existing request deadline/cancellation wins.
There is no intermediate `pending` response and no new timeout clock.
Transport expiry closes the connection without a guaranteed error frame;
an occluded/unconfigured window need not produce a matching callback at all.

Success is `1 ID ok GENERATION,CONTROLLER,TAB,REVISION,WIDTH,HEIGHT,SCALE`.
The seven numbers form one comma-separated field after the ordinary Tab
header. They describe the immutable snapshot used to paint and submit the
acknowledged buffer, not the controller when the callback or reply arrives.
`TAB=0,REVISION=0` means the rendered snapshot had no active document.
Other fields retain their existing units: UTF-8 document revision, controller
generation and buffer pixel geometry/integer scale. A newer completed frame
may satisfy an older request, and edits can intervene before the reply.
Check the returned tab/revision when testing a particular edit.

Native state appends `frame-submitted` and `frame-completed`, each either
`-` (none) or the same seven-number snapshot. A sent commit records submitted
state; only the matching outstanding main-surface callback records completed
state. `wl_buffer.release` controls buffer reuse independently and never
satisfies a wait. No response claims physical scanout, visibility, screenshot
capture, compositor decorations or the separate pointer-cursor surface.

At most eight jobs may be held, sharing the worker's existing eight live
connection slots. Each outer turn inspects every held job once, drops dead
jobs, and shares a two-action budget between ready held replies and newly
admitted requests. Held replies have priority; unresolved holds consume no
reply budget, so other admitted requests and Wayland dispatch continue.
Eight holds can occupy all transport slots until completion/expiry; this
endpoint promises bounded resource use, not per-client fairness. Shutdown or
control disable drops all held jobs. If the defensive held-capacity check
refuses admission, the live request receives `limit`; it is not held beyond
the cap. Socket I/O remains on the worker.

Native generation exhaustion poisons the window and terminates the event
loop with `exhausted`, instead of wrapping, continuing redraws or reporting a
false acknowledgement. The last admitted input may already have changed the
model; this terminal display failure is not an undo/rollback or persistence
guarantee. Subsequent control admission and drawing refuse the poisoned
state, and ordinary endpoint cleanup cancels pending waits.

## Controller state response

Success begins `1 ID ok` followed by these tab-separated fields, in order:

1. `active=TAB_OR_0`, `keys=windows|emacs`, `prefix=0|1`.
2. One `tab=ID,REV,DIRTY,BYTES,ANCHOR,CARET,AUTO_FILL,FILL_COLUMN,BOM,ENDING`
   for each open tab in ascending ID order. Flags are `0|1`; `ENDING` is
   `lf|crlf`. Selection endpoints are directed UTF-8 byte offsets.
3. `generation=N`, `window=WIDTH,HEIGHT,SCALE`, `focus=0|1`.
4. One `view=ID,ROW,COLUMN,COLUMNS,ROWS,WRAP,AFFINITY,DESIRED_COLUMN` per tab
   in ascending ID order. `AFFINITY` is `upstream|downstream`; an absent
   desired column is `-`. View coordinates are the existing controller's
   zero-based visual row/column origin, not document byte offsets.

There are at most 64 tab/view pairs. No text bytes, file path, title, dictionary,
pending dialog/job or spelling range is serialized by this controller-only
snapshot. Its generation means local UI state, **not** a submitted buffer,
frame callback or scanout. The native extension below adds its own redraw
generations/snapshots and coarse flags. File/dialog job identities remain
later work before claiming complete remote control.

## Experimental native adapter

`--control-socket PATH` may appear once after `--window`, before the literal
`--` delimiter. Its next argument is one literal OS-byte pathname, not shell
text. It must satisfy the complete private-socket contract below. The caller
creates the private parent; no directory or endpoint is discovered, adopted
or repaired automatically. Giving access to the endpoint grants read/write
access to every tab's current in-memory text, including unsaved and inactive
tabs within the implemented operation subset above.

Startup binds before opening document/dictionary files or connecting to
Wayland, so an invalid endpoint fails startup without those operations.
The worker starts only after file preparation and window construction.
An earlier startup failure drops the socket and attempts checked cleanup;
no response is available during that interval. Pathname existence or a
successful connect is therefore not a readiness signal. A successful state
response proves the UI is answering, not that a frame has been presented.
Startup cleanup is best-effort; an abrupt death can leave a stale socket,
which the caller must inspect and remove explicitly before retrying.

The adapter shares at most two live actions between held frame replies and
new job admission at the end of each outer event-loop turn, after file/timer
processing and the single spelling step. It does not multiply that allowance
for each decoded Wayland event. Each worker poll may discard its existing
bounded prefix of expired jobs. While control is enabled,
the ordinary receive wait is capped at ten milliseconds, including idle time.
This opt-in latency tradeoff can wake an otherwise idle window 100 times per
second; the default window adds no control polling. All socket reads and
writes stay on the worker. Two maximum pages can allocate about one MiB of
hex response text per turn; this is a byte/work bound, not a real-time latency
guarantee. Edits may scan bounded whole documents and recompute layout;
there is no event-loop latency ceiling. State/text serialization only borrows
the controller immutably.
Queries neither answer nor dismiss a modal, move selection, start I/O, mark
a document saved nor invoke an edit.

Native `text` responses are exactly the shared response above. Native `state`
appends these tab-separated fields to successful controller snapshots, in
this order. Error responses are unchanged:

| Field | Value |
| --- | --- |
| `adapter=native-frame` | Explicit implemented adapter identity. |
| `native=...` | Configured, file session present, file job busy, quitting. |
| `modal=...` | Path entry, close question, conflict question, pending Reload, menu, Find, numeric entry, command entry, Replace. |
| `spelling=...` | Selected dictionary entry count or `-`, scan running. |
| `window-generation=N` | Current native redraw-invalidation generation. |
| `frame-submitted=...` | Last submitted snapshot, or `-`. |
| `frame-completed=...` | Last callback-completed snapshot, or `-`. |

Boolean flags are `0|1`; the dictionary field is an entry count or `-`.
These are coarse presence flags, not dialog/job IDs,
allowed answers, operation results or spelling ranges. No path or entry text
is disclosed by these added fields. Query `text` separately for document
bytes. Controller generation does not cover native-only modal/job changes,
and is not a submitted/callback-completed frame generation. Clients must not
use it as a native snapshot version or presentation fence. Use the separate
`window-generation` with `wait-frame` under the frame contract above.

On ordinary window exit, stop and join the worker and explicitly attempt
checked cleanup; a shutdown error contributes to nonzero exit status. Drop
also cancels the worker on other unwind/owner-drop paths. Incomplete or
nonreading peers cannot keep shutdown waiting for their request deadline.
If the transport thread fails while the editor is live, disable control and
show a retained diagnostic without discarding documents or stopping editing.
Any cleanup failure remains recorded for eventual nonzero exit status.
Invalid response frames report a notice; ordinary expired/disconnected reply
admission is silent and cannot overwrite a user-facing notice. A missing
mutation reply does not undo its already admitted edit.
No automatic rebinding or restart occurs. A reply that was queued but not
delivered is not a claim of client receipt.

Tests use actual private pathname sockets with the native window fixture,
exercise state/text, revision-checked edits and out-of-subset refusals,
preserve modal state, reject stale pages after edit/Undo, answer Wayland
pings with requests outstanding, and
check startup-failure cleanup, timed shutdown with partial/nonreading peers,
and retained cleanup errors without deleting replacement names. A separate
source confinement assertion pins the two-action per-turn budget. Held-frame
socket tests count ready replies across turns, exercise old callbacks after
newer edits, distinguish callback from release, preserve modal reads, service
pings, expire a wait without a callback, and cancel holds on shutdown.
Editing tests compare shared state/history with replay, native keyboard
edits in both profiles and rendered pixels; they cover expected-selection
races, expired jobs, pending-Paste/repeat cancellation, unchanged disk bytes,
and spelling invalidation without rescanning. These are fake-compositor and
local kernel tests, not a live independent-compositor or td-jail/td-mail oracle.
Spelling queries additionally cover native no-dictionary status, complete
range pages matching the window's marks and modal-preserving reads.

## Conformance

Shared control-library tests pin exact request/error/text payloads, matching
state/text responses through replay, immutable controller state, maximum
pages and 64-tab output,
stale-after-Undo rejection, every frame split, single-byte delivery, premature
EOF, zero/oversized/trailing frames, poisoned decoder behavior and arbitrary
byte input both as raw headers and as correctly framed payloads. Invalid
envelopes/read-only command refusals are compared with replay, separately
from the intentionally different mutation allowlists/selection guards.
Spelling-page tests cover cross-tab pending/report isolation, closed targets,
hidden partial results, range/mark limits, scan-ID reuse refusal after recheck
or dictionary replacement, and exhaustion that preserves prior valid work.
These library tests need no display, socket, dictionary file or external process.

## Private socket publication prerequisite

`control_socket::Socket::bind` opens a Linux Unix-domain listener only when
explicitly called. It does not accept a CLI option, create a worker, decode a
request, read editor state or send a response. `accept` and each accepted
stream are nonblocking; connection limits, deadlines, cancellation and UI
integration are separate worker/adapter responsibilities. No new raw syscall
surface or dependency is used. The filesystem operations use safe `std` and
procfs, which must be mounted at `/proc`. Like the existing editor transport,
the guard in `src/sys.rs` requires Linux x86-64 at compile time; these open
flags name that ABI, not all Unix platforms or Linux architectures.

The socket pathname must be absolute, non-NUL and at most 107 bytes, with a
nonempty basename of at most 80 bytes. Trailing slash, `.`/`..` basenames and
parent `..` traversal are refused. Interior `.` and repeated separators have
ordinary `Path` normalization; there is no shell expansion or canonicalization
through symlinks. Non-UTF-8 names remain literal OS bytes. The basename cap
keeps the internal descriptor-relative bind name within Linux's pathname
socket limit independently of the current descriptor number.

The final parent must already exist, belong to the calling UID and have mode
exactly 0700, including no special bits. The binder neither creates nor
chmods directories. Every ancestor, including root, must be owned by root or
the caller and must not be group/other-writable unless sticky. This admits
ordinary `/tmp` while protecting caller/root-owned path components from
rename by another UID. An otherwise private parent inside a non-sticky
shared writable ancestor is refused.

Ownership is interpreted in the editor's current user namespace. Unmapped
ancestor owners are not trusted: the overflow UID (commonly 65534) is not an
alias for root. A rootless container must provide a path whose complete
ancestry satisfies these checks; the optional endpoint otherwise refuses.
Before walking the path, read `/proc/sys/kernel/overflowuid` with an 11-byte
limit plus one byte for overflow detection. Require a decimal `u32`, with at
most one final LF. Missing, malformed or oversized data refuses publication.
If the configured overflow value equals the caller UID or zero, refuse:
otherwise an unmapped owner could be numerically indistinguishable from a
trusted owner. This includes callers legitimately mapped to the overflow
number; the adapter cannot distinguish the two cases from stat ownership.
No fixed overflow number is assumed and no sysctl is changed. Concurrent
changes to that global sysctl require the already-excluded privileged host
authority. Linux's [user namespace contract](https://man7.org/linux/man-pages/man7/user_namespaces.7.html)
specifies this substitution for stat and process-status ownership.
Do not relax the predicate based on environment variables or test execution.
The crate declares `trusted-test-root = true` for the cargo gate, which gives
its tests a private caller-owned filesystem root and `/tmp` through the
existing capped runner. See DEVELOPMENT.md's trusted-root fixture contract;
this changes the test environment, not production socket admission.

Read the UID from a bounded `/proc/self/status` read (64 KiB plus one byte
for overflow detection). Require exactly one complete `Uid:` record with
four equal representable `u32` values: real, effective, saved and filesystem
UID. Missing/duplicate/malformed/mixed records refuse. UID zero is allowed
when all slots agree; this is not a sandbox for root. Environment variables
are never evidence of identity.

Walk directory components from an opened root using `O_PATH | O_DIRECTORY |
O_NOFOLLOW`, one component at a time through `/proc/self/fd/N`. These kernel
descriptor links are intentional; no user-supplied symlink component is
followed. Retain the final directory descriptor. Bind through its pinned
descriptor path rather than reopening the user's absolute path. Refuse every
existing final name without connecting to it: regular files, directories,
symlinks, live listeners and stale sockets all remain untouched. A concurrent
binder is still subject to the kernel's exclusive pathname creation.
Socket address diagnostics report the internal `/proc/self/fd/N/name` bind
address, not the requested pathname. A peer must connect using the requested
path it was given, not reuse `peer_addr` as a path in its own descriptor table.
Callers retain the requested path for diagnostics; the guard does not store
another copy.

Binding briefly creates a listening socket with umask-derived permissions
before mode 0600 is set. The already-private parent contains that interval
to the same caller/root trust boundary; changing process-global umask would
race unrelated file operations and is not used.

Open the new filesystem socket inode with `O_PATH | O_NOFOLLOW`, require a
socket owned by the caller, and pin that descriptor too. Set mode 0600
through its kernel descriptor path and read back mode/owner. Rewalk the
visible parent after setup, rechecking trust, private mode and identity;
also verify the named socket still matches the pinned inode. This catches
parent/name changes during publication. The filesystem socket inode is
distinct from the listener's socket descriptor inode; cleanup pins the former.

Explicit `close` makes one checked cleanup attempt and reports errors. Drop
attempts the same cleanup best-effort. Before unlinking the original basename
inside the pinned parent, require socket type and matching device/inode.
A missing name is already clean only after rechecking that the procfs parent
descriptor path remains accessible; this is an accessibility check, not
detection of a renamed or removed parent. A real procfs link still addresses
that pinned inode after either operation. This also handles a name removed
between the identity check and unlink. A replacement file/socket is left alone.
Renaming the parent does not redirect cleanup into a replacement directory.
Keeping the socket inode open prevents inode-number reuse from fooling the
comparison. If binding succeeds but inode ownership cannot be established,
the binder does not unlink an unverified name; a stale endpoint may remain
for caller inspection. Later setup failures attempt normal checked cleanup.

Identity checks and pathname operations are separate kernel operations, not
an atomic compare-and-unlink primitive. These rules protect against other UIDs
under the admitted permissions; they are not isolation from root or another
process running as the same UID. Such a process can race bind-to-pin or
check-to-unlink, rename names, or change permissions, and already has the
endpoint's authority. The caller must keep this path private. Sharing it
across a jail boundary remains a separate explicit grant.

Kernel tests connect through the requested pathname, exchange bounded bytes,
check nonblocking/close-on-exec flags and mode/owner, preserve every existing
endpoint class, reject symlinked/unowned/nonprivate/untrusted ancestors, and
exercise replacement-name and renamed-parent cleanup. A deterministic
publication hook proves a replaced visible parent refuses and only the
pinned candidate is cleaned. These are transport-ownership tests, not a
native editor-control endpoint or an adversarial same-UID race proof.

## Bounded worker

`control_worker::Worker::start` takes an already admitted `Socket`. One named
thread owns the listener and every accepted connection; no socket I/O occurs
in `try_request` or `Job::respond`. The thread has no editor reference or
model lock. The native adapter above owns UI dispatch and command-line opt-in.

Admit at most eight connections. When all slots are occupied, leave further
clients in the kernel listener backlog; do not create descriptors, threads or
request allocations for them. The application does not promise a backlog
connection deadline or backlog size. The five-second whole-request deadline
starts when the worker accepts, never renews on progress, and includes header,
body, UI queue/response time and output. At or after expiry close silently.
Every loop checks each live connection, allowing at most one 16-KiB read or
write for it, then parks for at most ten milliseconds with live connections,
or 100 milliseconds when idle. UI replies, abandoned jobs and shutdown unpark
the worker early. A ready reply attempts its first write in the same step;
a refusal after a read waits until the next step to preserve the I/O bound.
This deliberately paces output near 1.6 MiB/s per connection and a maximum
hex-encoded text page near 330 milliseconds, excluding scheduling delays.
Idle polling trades up to 100 milliseconds of acceptance latency for fewer
wakeups; blocking accept would require a separate reliable shutdown wakeup.
Scheduling delay and kernel execution are not hard real-time guarantees; the
worker never deliberately blocks on socket I/O.

The endpoint is not an availability boundary against the same UID or root.
Such a process can continually occupy all eight slots, keeping legitimate
clients in the backlog. Deadlines bound each admitted connection, not a peer's
share of future admissions. Full UI admission closes without an error frame.

One complete request dispatches at most once, without requiring write EOF.
The shared decoder rejects zero/oversized frames and extra bytes delivered
in the same read. After completion no further request bytes are read: later
bytes never execute a second command. A premature EOF or decoder refusal
queues a framed error with ID zero; a parsed request refusal uses its recovered
ID. Transport failures, deadline expiry or exhausted UI admission close the
connection without a guaranteed error frame. Successful output closes after
its one complete frame, without waiting for the peer to close.

Only the query/edit subset above passes `Request::parse`. Eight typed jobs
fit in the UI queue; submission is nonblocking and a full/disconnected queue
closes that connection. Each job has a one-element response channel and a
liveness token combining the acceptance deadline and connection lifetime.
`try_request` never waits and makes at most eight receives per call, returning
the first live job. After eight expired entries it returns no job; remaining
arrivals or worker disconnection are observed on a later poll. A disconnected
worker reports an error once that bounded expired prefix has been drained.
Dropping a job without a response disconnects its response channel before
waking the worker to close the client.

The native caller consumes `Job::respond_with`, which checks liveness before
passing the borrowed request to its query/edit handler. The handler applies
the modal/target guards above and returns one payload. The underlying
`Job::respond` checks liveness and frame limits before copying/queuing it;
success means queued, not delivered or rendered. It does not validate the
caller's response fields. A disconnected or expired job returns false; invalid
payload size returns the shared frame error. Closing a connection cancels its
queued/held job; a deadline never reserves an old document snapshot. This
transport API does not replace live UI modal/ID/revision admission.

Request payload allocation is at most one MiB per reading connection. Parsing
retains a fixed-size descriptor plus at most 256 KiB of decoded Insert text
and drops the wire payload when moving to UI wait. Parsing may briefly hold
both. UI insertion clones that bounded text for the owned controller command;
the model separately normalizes/adopts it within its existing edit budgets.
Job debug output reports byte counts, never inserted text.
Each response channel/connection owns at most one framed reply
of one MiB plus four bytes. A UI reply may briefly coexist with its original
request allocation during handoff; there are at most eight such connections
and eight queued jobs. One reusable 16-KiB scratch buffer serves the thread.
These bounds exclude caller-owned response inputs/retained jobs, allocator
overhead and kernel socket buffers; there is no unbounded worker byte queue.

Explicit `close` and Drop set the stop flag, unpark and join the worker. It
invalidates all live jobs, closes clients, and uses the socket's checked
cleanup. Explicit close reports worker/cleanup failure; Drop is best-effort.
No detached thread survives a completed join. Interrupted or aborted accepts
and temporary descriptor/memory/buffer exhaustion retry after the normal park;
other listener errors stop the worker and socket Drop attempts cleanup. A
deadline representation overflow also stops it rather than accepting without
a deadline. Shutdown is not a wall-clock promise about filesystem operations
or host scheduling.

Deterministic connection tests inject time and cover bytewise input, complete
and truncated frames, refusal IDs, late extra requests, full queues, abandoned
jobs, invalid response sizes, and expiry while reading, awaiting the UI or
blocked on output when the kernel buffer fills. A separate pre-output expiry
test is independent of socket-buffer tuning. Tests also cover large draining
replies, text request admission, retry classification, idle pacing and explicit
thread-error propagation. Real Unix-socket worker tests cover a framed round trip,
admission backpressure with continued client progress, shutdown cancellation
and owned endpoint removal. These need no Wayland server and do not prove
native editor control or frame synchronization.
