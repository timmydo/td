# Control protocol reference

This is the implemented **library prerequisite**, not an available endpoint.
`control` has no listener, thread, filesystem access, clock or Wayland access.
The separate `control_socket` library can explicitly publish a private Unix
listener under the contract below; it is not connected to the executable.
`--control-socket` is not accepted yet. Native dialogs/jobs/spelling results,
remote mutations and frame acknowledgements still require the window adapter
specified in [DESIGN.md](DESIGN.md#test-and-control-architecture).

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
does not prove peer EOF or that no later bytes will arrive: the future worker
must dispatch at most once per connection and close it after the response.
`finish` transfers the completed buffer; it performs no read or EOF check.
Empty input chunks make no progress. The decoder allocates at most one MiB
of payload storage, only after validating the length.
The full declared storage is allocated on header completion, before body
bytes arrive. The worker must budget each admitted connection against that
one-MiB allocation, not just against bytes received so far.

The future worker owns deadlines, connection limits, request/response queues
and cancellation. The framing library claims none of those safeguards.
Private socket publication is implemented separately below. Replay retains
its consecutive-frame stdin/stdout runner; the one-frame decoder is for the
planned one-request-per-connection worker.

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
frame callback or scanout. A native endpoint must add its own state under the
full design contract before claiming complete remote control.

## Conformance

Tests pin exact request/error/text payloads, matching state/text responses
through replay, immutable controller state, maximum pages and 64-tab output,
stale-after-Undo rejection, every frame split, single-byte delivery, premature
EOF, zero/oversized/trailing frames, poisoned decoder behavior and arbitrary
byte input both as raw headers and as correctly framed payloads. Invalid
envelopes/read-only command refusals are compared with replay, separately
from the intentionally different mutation allowlists. Tests need no display,
socket, dictionary or external process.

## Private socket publication prerequisite

`control_socket::Socket::bind` opens a Linux Unix-domain listener only when
explicitly called. It does not accept a CLI option, create a worker, decode a
request, read editor state or send a response. `accept` and each accepted
stream are nonblocking; connection limits, deadlines, cancellation and UI
integration remain worker responsibilities. No new raw syscall surface or
dependency is used. The filesystem operations use safe `std` and kernel
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
