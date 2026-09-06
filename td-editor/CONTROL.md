# Control protocol reference

The experimental `--window --control-socket PATH` endpoint implements the
read-only subset below. It is off by default; scratch preview and replay do
not accept the option. Remote edits, dialog answers, spelling result pages
and frame acknowledgement remain unimplemented. This is not the complete
version-1 endpoint specified in [DESIGN.md](DESIGN.md#test-and-control-architecture).

`control` has no listener, thread, filesystem access, clock or Wayland access.
The separate `control_socket` library publishes a private Unix listener
under the contract below.
`control_worker` owns that listener on a bounded transport thread and hands
read-only requests to the native window's UI thread.

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

## Read-only requests

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

These are the only names accepted by `control::Request::parse`. In particular,
`new`, `load`, editing, file I/O, physical-input simulation and dialog answers
are refused; this parser is not a route into replay's broader command set.
`Request::response` borrows `&Controller`, so it cannot dispatch an edit or
change selection, views, history or generation.

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
frame callback or scanout. The native extension below adds coarse flags; a
complete native endpoint must add its own state under the
full design contract before claiming complete remote control.

## Experimental native read-only adapter

`--control-socket PATH` may appear once after `--window`, before the literal
`--` delimiter. Its next argument is one literal OS-byte pathname, not shell
text. It must satisfy the complete private-socket contract below. The caller
creates the private parent; no directory or endpoint is discovered, adopted
or repaired automatically. Giving access to the endpoint grants read access
to every tab's current in-memory text, including unsaved text and inactive
tabs. It does not grant remote writes in this increment.

Startup binds before opening document/dictionary files or connecting to
Wayland, so an invalid endpoint fails startup without those operations.
The worker starts only after file preparation and window construction.
An earlier startup failure drops the socket and attempts checked cleanup;
no response is available during that interval. Pathname existence or a
successful connect is therefore not a readiness signal. A successful state
response proves the UI is answering, not that a frame has been presented.
Startup cleanup is best-effort; an abrupt death can leave a stale socket,
which the caller must inspect and remove explicitly before retrying.

The adapter polls at most two live jobs at the end of each outer event-loop
turn, after file/timer processing and the single spelling step. It does not
multiply that allowance for each decoded Wayland event. Each worker poll may
discard its existing bounded prefix of expired jobs. While control is enabled,
the ordinary receive wait is capped at ten milliseconds, including idle time.
This opt-in latency tradeoff can wake an otherwise idle window 100 times per
second; the default window adds no control polling. All socket reads and
writes stay on the worker. Two maximum pages can allocate about one MiB of
hex response text per turn; this is a byte/work bound, not a real-time latency
guarantee. State/text serialization only borrows the controller immutably.
Queries neither answer nor dismiss a modal, move selection, start I/O, mark
a document saved nor invoke an edit.

Native `text` responses are exactly the shared response above. Native `state`
appends these tab-separated fields to successful controller snapshots, in
this order. Error responses are unchanged:

| Field | Comma-separated values |
| --- | --- |
| `adapter=native-read-only` | Explicit implemented adapter identity. |
| `native=...` | Configured, file session present, file job busy, quitting. |
| `modal=...` | Path entry, close question, conflict question, pending Reload, menu, Find, numeric entry, command entry, Replace. |
| `spelling=...` | Selected dictionary entry count or `-`, scan running. |

Boolean flags are `0|1`; the dictionary field is an entry count or `-`.
These are coarse presence flags, not dialog/job IDs,
allowed answers, operation results or spelling ranges. No path or entry text
is disclosed by these added fields. Query `text` separately for document
bytes. Controller generation does not cover native-only modal/job changes,
and is not a submitted/callback-completed frame generation. Clients must not
use it as a native snapshot version or presentation fence.

On ordinary window exit, stop and join the worker and explicitly attempt
checked cleanup; a shutdown error contributes to nonzero exit status. Drop
also cancels the worker on other unwind/owner-drop paths. Incomplete or
nonreading peers cannot keep shutdown waiting for their request deadline.
If the transport thread fails while the editor is live, disable control and
show a retained diagnostic without discarding documents or stopping editing.
Any cleanup failure remains recorded for eventual nonzero exit status.
Invalid response frames report a notice; ordinary expired/disconnected reply
admission is silent and cannot change text or overwrite a user-facing notice.
No automatic rebinding or restart occurs. A reply that was queued but not
delivered is not a claim of client receipt.

Tests use actual private pathname sockets with the native window fixture,
exercise state/text and mutation refusals, preserve modal state, reject stale
pages after edit/Undo, answer Wayland pings with requests outstanding, and
check startup-failure cleanup, timed shutdown with partial/nonreading peers,
and retained cleanup errors without deleting replacement names. A separate
source confinement assertion pins the two-job per-turn budget; the socket
progress fixture does not count exact per-turn admissions.
These are fake-compositor and local
kernel tests, not a live independent-compositor or td-jail/tmc oracle.

## Conformance

Shared control-library tests pin exact request/error/text payloads, matching state/text responses
through replay, immutable controller state, maximum pages and 64-tab output,
stale-after-Undo rejection, every frame split, single-byte delivery, premature
EOF, zero/oversized/trailing frames, poisoned decoder behavior and arbitrary
byte input both as raw headers and as correctly framed payloads. Invalid
envelopes/read-only command refusals are compared with replay, separately
from the intentionally different mutation allowlists. These library tests need no display,
socket, dictionary or external process.

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

## Bounded read-only worker prerequisite

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

Only `state` and revision-pinned `text` pass `Request::parse`. Eight typed jobs
fit in the UI queue; submission is nonblocking and a full/disconnected queue
closes that connection. Each job has a one-element response channel and a
liveness token combining the acceptance deadline and connection lifetime.
`try_request` never waits and makes at most eight receives per call, returning
the first live job. After eight expired entries it returns no job; remaining
arrivals or worker disconnection are observed on a later poll. A disconnected
worker reports an error once that bounded expired prefix has been drained.
Dropping a job without a response disconnects its response channel before
waking the worker to close the client.

The caller reads `Job::request`, serializes against its current controller
with the shared `Request::response`, then calls consuming `Job::respond`.
The latter checks liveness and frame limits before copying/queuing the payload;
success means queued, not delivered or rendered. It does not validate the
caller's response fields. A disconnected or expired job returns false; invalid
payload size returns the shared frame error. Closing a connection cancels its
queued/held job; a deadline never reserves an old document snapshot. This
read-only API is not authorization for future mutation dispatch: a native
mutation adapter must additionally enforce its own live IDs/revisions.

Request payload allocation is at most one MiB per reading connection. Parsing
retains only the fixed-size typed request and drops that payload when moving
to UI wait. Each response channel/connection owns at most one framed reply
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
