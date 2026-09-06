# td-authd

This is the private-channel prerequisite for secure attention and subsequent
one-operation elevation. It enables no privileged operation, FIDO2 release,
consent prompt, or public request listener. The image does not yet start it.
The eventual operation policy follows APPLICATIONS.md §L.1 and principle 7:
one named operation, typed and descriptor-pinned arguments, one token
assertion bound to that request, no remembered approval. Separate
compositor/application identities and exclusive device ownership precede
enabling that path.

## Private channel

Root td-svc creates the socketpair through `pair-exec`; each peer receives
its endpoint on stdin. Both peers enable kernel credentials and sender
pidfds before their first write, then exchange an exact versioned greeting.
The receiving socket's `SO_PEERCRED` must identify a root creator. This
checks the launch boundary, not the opposite endpoint's eventual holder.

The trusted exec chain must complete this greeting before forking, starting
workers, launching subprocesses, or passing the endpoint elsewhere. The
first greeting pins its actual sender; the transport cannot identify the
intended direct child if trusted startup has already delegated the
descriptor. The current diagnostic has no process-launch or thread path,
pinned by its confinement tests. Every future consumer must enforce the same
ordering. The inherited fd 0 remains open and must never reach an untrusted
child. Authentication pins a process, not its executable: exec retains the
pin, and same-uid ptrace would retain its authority. Dedicated identities
and a trusted exec chain are therefore mandatory before any consumer enables
consent.

Sender authentication supplies no protection against an inherited endpoint
reading bytes. A consumer must keep its endpoint exclusive throughout its
lifetime, including after the greeting; no secret or replayable authority
may be sent until that ownership is enforced. Shutdown invalidates every
duplicate, including stdin, deliberately.

Each receive must instead carry exactly one `SCM_CREDENTIALS` and one
`SCM_PIDFD` supplied by Linux for the sending process. The configured peer
uid must match, and the constructor refuses overflow uid 65534 and u32::MAX.
A live pidfd is retained for the whole connection, and its device and inode
identity must match every subsequent receive's pidfd. A reusable PID is
never used to authenticate or signal the peer. After the greeting, a
descendant that inherits the stream is a different sender and is refused. A
dead peer is refused even if bytes remain buffered. Root and the kernel
remain trusted.

Frames contain a four-byte big-endian length and at most 4096 payload bytes.
The greeting and each complete frame have absolute five-second deadlines;
partial reads, writes, and interrupts spend that same deadline. Every
receive is bounded by the bytes remaining in the current frame and validates
its own ancillary identity. This relies on Linux AF_UNIX delivering
stateless sender credentials and pidfds on every fragment, including a
partial read of one socket buffer; it is a hard kernel dependency, not a
portable stream API. Any protocol, identity, transport or liveness failure
permanently closes the channel. There is no reconnect on an existing
endpoint. Completion observed after the deadline is refused even if the last
bytes arrived before it: success is bounded by observation, not kernel
arrival time. A send error may follow partial or complete delivery, as with
any stream; it is never permission to retry an operation.

All installed descriptors are owned before an ancillary-policy refusal. The
bounded walk collects recognizable rights and pidfds even after an
unsupported record; truncation and malformed data then drop every collected
owner. Broken framing stops at the last trusted boundary. Linux supplies
conforming control framing, installs a distinct descriptor number for each
delivered owner, and closes rights that do not fit. No received descriptor
is exposed to an operation or caller; only the admitted peer pidfd survives.

## Raw boundary

Linux x86-64 only. One function-scoped syscall instruction carries
recvmsg(47), setsockopt(54), getsockopt(55), and poll(7). The socket options
are fixed to SOL_SOCKET: SO_PASSCRED(16)=1, SO_PASSPIDFD(76)=1, and
SO_PEERCRED(17), with an exact twelve-byte credential result. Receive always
sets MSG_CMSG_CLOEXEC and uses one borrowed byte slice plus 128 aligned
control bytes. Poll asks POLLIN on one borrowed pidfd with timeout zero; any
returned event or error refuses liveness. One other function-scoped
allowance adopts freshly installed nonnegative descriptors into OwnedFd.
Linux SCM_PIDFD carries a nonnegative installed fd or a negative errno; the
latter is refused before adoption. MSG_CMSG_CLOEXEC additionally covers
refused rights; the behavioral pidfd test proves the pidfd itself is
close-on-exec, as Linux already guarantees. Safe std owns all other I/O,
duplication, timeouts, shutdown, metadata, and closes.

Confinement tests enumerate production source files, pin both allowances and
their bodies, all four syscall numbers, socket options, ABI sizes, and
wrapper call sites. No caller-selected option, raw descriptor, pointer, or
signal is exposed. Any expansion amends this document and UNSAFE.md in the
same landing.

## Proof

Kernel tests exchange frames between separate processes over an inherited
socketpair, distinguish its creator from the message sender, reject a child
that inherits an authenticated endpoint, and reject buffered data after peer
death. Refusal tests cover credentials, unexpected rights, truncation,
bounds, and partial frames. Host diagnostics are not a target-kernel or boot
claim; the target recipe runs the same transport fixtures compiled with its
target toolchain. The production `channel-check` entry point is a diagnostic
and exports no operation. Its ping loop also expires after five idle
seconds; a diagnostic peer must drive it within that deadline. Its stdout
marker is the pass signal; all exits, including peer shutdown, fail the
paired lifecycle. It requires a root-created, unnamed socketpair on stdin;
there is no shipped bypass for a different creator. Tests exercise other
creator uids only through a private constructor.

The Linux 7.1.4 source pins the relevant kernel behavior in `net/core/scm.c`
(`scm_pidfd_recv`) and `fs/pidfs.c` (`pidfs_init_inode`): the received
descriptor identifies the sending task and each pid owns its inode identity.
Polling the retained descriptor rejects death before any numeric PID could
be reused. The supported runtime is td's configured Linux 7.1.4 or newer,
with pidfs, not every kernel that happens to accept SO_PASSPIDFD. Tests
compare pidfd inodes for distinct processes independently of credential
equality, so anon-inode pidfds on older hosts fail rather than silently
weakening the pin. Unsupported host kernels fail the tests rather than skip
them. The x86-64 `SO_PASSCRED` value is 16, as the pinned UAPI header
specifies.

## Principal registry prerequisite

The identity cutover uses one immutable `/etc/td-principals.tsv` table
shared by the trusted launcher, broker, provisioner, and secret authority.
UIDs are explicit assignments retained across deployments, never derived
from row order or application names. A session row binds a human account to
its dedicated compositor, broker, and portal identities; an application row
binds that human account and the deployment's installed application name to
a distinct external uid/gid. All application namespaces still present
uid/gid 1000 internally. Root launches the unprivileged jail at the assigned
external identity, so its existing single-entry self-mapping needs no
separately privileged id-map daemon.

The canonical format begins `td-principals-v1` followed by tab-separated
rows:

```
session<TAB>human-uid<TAB>compositor-uid<TAB>broker-uid<TAB>portal-uid
application<TAB>human-uid<TAB>application-name<TAB>application-uid
```

Session rows precede application rows. Each group is sorted by numeric human
uid, then application name for application rows. Human uids are 1000 through
65533, dedicated service uids are 1 through 999, and application uids are
65536 through 2147483647; gid equals uid for each dedicated identity. Zero
and the usual overflow ids are excluded. The producer must also reject any
assigned uid/gid already occupied by another system account. The parser
independently rejects duplicate service/application assignments, unknown
session owners, noncanonical decimal spelling, duplicate or unsorted rows,
and invalid names. It bounds the whole table to 64 KiB and 256 principal
rows, and admits only ASCII application names up to 64 bytes with a leading
lowercase letter and remaining lowercase letters, digits, dots, hyphens or
underscores.

The table contains identity, not authority to execute an arbitrary program.
A launch request names only an installed application. The authority selects
its immutable package and account from compiled deployment configuration; no
caller may select an executable, Unix identity, state directory, or cgroup.
Terminal arguments remain literal unprivileged application arguments. The
image cutover must migrate state ownership and all credential/socket checks
atomically; merely adding this parser enables no launch or consent path.

The live reader pins the root-owned `/etc` directory and opens only the four
fixed children `td-principals.tsv`, `passwd`, `group`, and `shadow`. Each
directory excludes unprivileged writers. It refuses leaf symlinks, bounds
reads, and requires single-link root-owned regular files: mode 0444 for the
principal table, 0600 for shadow, and no group/other write or special mode
bits for passwd and group. Nonblocking open prevents malformed FIFOs from
hanging admission. A broker or portal sharing the human or an app uid would
remain vulnerable to ptrace and message forgery, so their identities are
separate in the same table.

The image recipe runs `td-firstboot check-principals ROOT` before packing,
using the same parser and account checks. This read-only diagnostic pins the
selected staging tree and requires its files to share its owner's uid and
gid; the build sandbox uses its build identity, and the image packer
normalizes ownership to root. It never enrolls identities or writes state.
The live enrollment path always requires root ownership. The current image
declares one graphical session for uid 1000; adding another passwd account
does not implicitly create a second graphical session.

Account files have bounded, newline-terminated records: passwd has seven
colon-separated fields and group four, both with `x` in the password field;
shadow has nine. Comments, blank records, extra fields, duplicate names or
IDs, and noncanonical numeric IDs are refused. These are immutable generated
account databases, not the unrelated mutable `/etc` identity symlinks. An
invalid account edit therefore fails image construction and firstboot.

The current consumer is td-firstboot's explicit `--enroll-principals` mode.
It requires the default persistent state directory and all four root uid and
gid fields. Before reporting machine identity or provisioning applications,
it loads the immutable table and reserves its identities in
`/var/lib/td/principals.tsv`. The image enables this mode at sysinit. These
are reservations only: it creates no accounts and changes no running uid.
Every reserved uid/gid is checked against the complete current passwd,
group, and shadow tables, including retired assignments. A future activated
account must use the canonical name `tdc<owner>`, `tdb<owner>`, or
`tdp<owner>` for compositor, broker, or portal, and `tda<uid>` for an
application. Its primary gid equals uid, shell is `/bin/false`, and shadow
field is exactly `!td-service`; it has no supplementary membership and its
primary group exists and admits no other members. Orphan shadow records for
reserved names are refused. Aliases, shared primary gids, missing active
human owners, duplicate account records, and human-login shadow classes fail
enrollment. The image generator validates these same tables with the
provisioner's parser. These names reserve future service accounts; they do
not activate a service by appearing in the registry. The subsequent atomic
identity cutover must consume these same assignments and verify the ledger
before launching any process at a reserved uid. A failed firstboot unit
settles service ordering but does not block later units; ordering is not
authorization. The boot oracle requires the exact `TD-PRINCIPALS-ENROLLED`
line on every successful boot. Retired human accounts may disappear while
their service and application reservations remain.

The ledger retains the union of every successfully enrolled deployment,
including removed applications. Reassigning a tuple or reusing a retired uid
fails boot provisioning. A malformed existing ledger is never reset. A
separate, never-renamed lock serializes writers; a private staging file is
synced and renamed through a held directory descriptor, then its directory
is synced. Interrupted staging is removed only after its descriptor proves
it is a private, single-link, bounded regular file owned by root. Scratch
inspection uses O_PATH so an interrupted creation with masked owner bits can
be handled. The empty stable lock permits only owner read/write mode bits;
it is normalized through its pinned descriptor before blocking lock
acquisition. Staging with those same private mode bits may be removed.
Published ledgers still require exact mode 0600 and are never repaired.
Initial ledger and scratch opens refuse symlinks, and every directory is
root-owned without unprivileged writers. Once opened, directory identity
survives a rename. The persistent ledger is not an alternate source of
active applications: only the current immutable deployment declares those.
Root and deployment configuration remain trusted; these checks catch
accidental UID recycling, not a root attacker rewriting the ledger.

Recovery of this non-secret ledger is an offline maintenance operation, not
token recovery or an authorization bypass. Stop all enrollment writers
before repairing a scratch file; unlinking a live lock would split writer
serialization. A corrupt ledger requires a backup preserving every reserved
assignment, or reconstruction from complete deployment history and retained
state. Never reset it to the current table or prune retired rows. If the
union exceeds its bounds, raise the format's reviewed bounds in a deployment
update; overflow never silently drops reservations. Without a complete
ledger, keep the future UID launcher disabled. Console recovery does not
make an unverified deployment pass the boot oracle.
