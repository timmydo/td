# td-authd

This supplies the private channel and fixed terminal-launch prerequisite for
secure attention and subsequent one-operation elevation. It enables no secret
access, FIDO2 release, consent prompt, or public request listener. The image
starts it paired with the dedicated compositor and uses its terminal launcher.
The eventual operation policy follows APPLICATIONS.md §L.1 and principle 7:
one named operation, typed and descriptor-pinned arguments, one protected
consent bound to that request, no remembered approval. Protector changes
add fresh hardware-backed authentication under
[`td-install/ENCRYPTION.md`](../td-install/ENCRYPTION.md). Separate
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
descriptor. The channel diagnostic has no process-launch or thread path.
Terminal serving checks startup descriptors before the greeting and launches
children only after it; confinement tests pin that ordering. Every consumer
must enforce the same ordering. The inherited fd 0 remains open and must
never reach an untrusted child. Authentication pins a process, not its
executable: exec retains the pin, and same-uid ptrace would retain its
authority. Dedicated identities and a trusted exec chain are therefore
mandatory before any consumer enables consent.

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

The current consumer is td-firstboot's explicit `--enroll-principals`
mode. It requires the default persistent state directory and all four
root uid and gid fields. Before reporting machine identity or
provisioning applications, it loads the immutable table and reserves its
identities in `/var/lib/td/principals.tsv`. The image enables this mode
at sysinit. These are reservations only: it creates no accounts and
changes no running uid. Every reserved uid/gid is checked against the
complete current passwd, group, and shadow tables, including retired
assignments. A future activated account must use the canonical name
`tdc<owner>`, `tdb<owner>`, or `tdp<owner>` for compositor, broker, or
portal, and `tda<uid>` for an application. Its primary gid equals uid,
shell is `/bin/false`, and shadow field is exactly `!td-service`; it has
no supplementary membership and its primary group exists and admits no
other members. Orphan shadow records for reserved names are refused.
Aliases, shared primary gids, missing active human owners, duplicate
account records, and human-login shadow classes fail enrollment. The
image generator validates these same tables with the provisioner's
parser. A registry row alone does not activate a service. The image
consumes the compositor assignment through its paired root authority;
the broker consumes UID 992 through its service-only login path and
protected runtime. Portal and application accounts remain reserved. Each
atomic identity cutover consumes these assignments. The paired
compositor may enter its inert credential and channel startup at its
reserved UID before ledger admission; the authority verifies the ledger
before allowing device access, worker creation, or human terminal
launch. A failed firstboot unit settles ordinary service ordering; the
broker requires its success explicitly. Ordering alone is not
authorization. The boot oracle requires the exact
`TD-PRINCIPALS-ENROLLED` line on every successful boot. Retired human
accounts may disappear while their service and application reservations
remain.

The canonical table parser and reservation union live in
`engine/src/principals.rs`. td-firstboot includes that dependency-free source
and owns file validation, account checks, locking and persistent writes.
Registry maps remain private and have no unchecked constructor or mutable
accessor. Session and application row values are plain data, not authority
tokens: an admission consumer must obtain its assignments from a Registry
parsed from an authorized input. This shares source implementation, not Rust
type identity between separately compiled programs. The engine and host
provisioner compile the parser/enrollment fixtures; the target provisioner
recipe stages the same source and executes those fixtures.

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

## Fixed terminal launch prerequisite

`terminal-serve --user USER --uid UID --peer-uid UID` is a root-configured
consumer of the private channel. The image compositor runs at its reserved
identity and uses this channel for human terminal creation. The configured
application card already activates a supervised window; only terminal creation
needs this request. Direct compositor spawning remains a host-development
mode.

Startup requires all four root uid/gid columns, one thread, and only fd
0/1/2 inherited from the trusted supervisor. It proves each standard
descriptor is open before creating the descriptor-directory iterator, which
must then occupy fd 3. Otherwise a missing standard fd could disguise an
inherited fd 3 as the iterator. Standard log descriptors must not alias the
private endpoint, so a diagnostic cannot inject unframed bytes into it.
Startup also verifies the absence of a controlling terminal in
/proc/self/stat. These are consumer admission requirements, checked even
when launched through td-svc's pair-exec path. The daemon then
completes the Channel sender-pinning greeting before any child is created.
Its first child runs the immutable `/bin/td-firstboot check-launch-session
USER UID COMPOSITOR_UID`. This read-only root check runs once per
generation, requires the persistent ledger to exist, verifies that unioning
the current deployment would change nothing, and checks current account
databases against all retained reservations. The named human must have the
configured uid and primary gid, and that session's compositor must be the
configured peer uid. Neither startup ordering nor a missing ledger can
enroll or authorize a launch. The complete account check rejects human UID
aliases before the tuple check. Account files are immutable deployment
inputs: a trusted root changing them during a generation is outside this
contract. In particular, td-login resolves the named account again on each
launch; startup validation does not pin later account edits.

The validator has a two-second observed completion deadline, measured before
spawn and shorter than the channel frame deadline. Failure kills and reaps
the trusted validator and closes the channel. There is no caller-provided
executable, environment, directory, account, uid or argument vector. All
children replace stdin, stdout and stderr with `/dev/null`, clear the
environment, and start from `/`. This also replaces the original private
endpoint on fd 0: relying only on Channel's CLOEXEC clone would leak
authority. All subsequently created channel descriptors are CLOEXEC. The
daemon never passes an inherited root log descriptor to a user program.

Before validation both ends exchange the framed `TDLA001` protocol greeting
with a final newline. After successful validation the authority sends `80`;
only then may the peer submit requests. The earlier transport greeting in
Channel pins the sender before the protocol greeting or any spawn.
Subsequent payloads are exact byte records:

| Request | Response |
| --- | --- |
| `01` | `81` plus a nonzero big-endian u64 process handle |
| `02` plus that u64 handle | `82 00` running, `82 01` successful exit, or `82 02` failed exit |
| `03` | `83` heartbeat |

A full table returns `ff 01`; a spawn failure returns `ff 02`. Every other
request, trailing byte, unknown handle, wait error, timeout or transport
failure ends this channel generation, including ordinary peer EOF. td-svc
already counts either peer's exit, even zero, as pair failure and applies
its failure backoff. A malformed request therefore ends the graphical
session when this pair is enabled. There is no retry authorization after
a transport error. Successful terminal creation means the fixed credential
helper was spawned; polling distinguishes a later credential or exec
failure. A fresh 128-bit kernel-random generation nonce avoids PID reuse
collisions between terminals that outlive their authority. It is a
readiness-name identifier, not a secret or authorization token. These
dynamically launched terminals own their readiness sockets for human-session
diagnostics; the compositor observes Wayland surfaces and does not probe
those sockets. The nonce therefore stays private to the launcher and
terminal, and no reverse traversal grant into the human runtime is required.
Each generation holds at most sixteen records, including completions the
peer has not polled. A completion response retires its handle. Handles
increase without reuse; exhaustion fails before spawn. The peer must send a
request or heartbeat within each five-second receive deadline.

The authority runs `/bin/td-login exec-as USER -- /bin/td-authd
terminal-exec UID GENERATION HANDLE` in a new process group. td-login checks
the human account policy and drops and verifies credentials. Its exact
environment is `HOME`, `SHELL`, `USER`, `LOGNAME` from the account and
`PATH=/bin`, with no inherited `LANG`, `XDG_RUNTIME_DIR` or
`WAYLAND_DISPLAY`. The unprivileged terminal-exec entry requires all uid/gid
columns to equal the selected owner and `/proc/self/cgroup` to be exactly
`0::/td-user-1000/session`. Ordinary td-login makes placement failure
nonfatal for console recovery; this wrapper makes it fatal before terminal
code runs. The current launcher supports only uid 1000, the sole delegation
configured by td-login.

After that check it execs `/bin/td-term run --socket
/run/td-compositor/UID/wayland-0 --ready-socket
/run/user/UID/td-auth-terminal-GENERATION-HANDLE.ready`. No shell command or
consent operation is involved. Directly invoking terminal-exec cannot change
credentials or enter a different session. Its membership check verifies
placement after the trusted credential helper; it is not human
authorization. Opening one's ordinary terminal is session behavior and opens
an ordinary shell with the human account's existing authority; it grants
neither store access nor an elevated shell.

The compositor-owned runtime directory permits human traversal and socket
access, with kernel peer admission before protocol handling. The image uses
the private terminal channel. The PTY module constructs a separate shell
environment from its account and actual Wayland/control paths; it does not
inherit the six-variable helper environment. The CLI contract belongs to
`td-compositor/DESIGN.md` and `td-login/THREAT-MODEL.md`.

Started terminals belong to the human session cgroup and their own process
group. A paired-authority restart therefore does not terminate them through
either group. A placement failure creates no terminal. Their completion
handles expire with the authority generation. The daemon never removes
readiness paths in the human-owned runtime directory; the terminal owns its
publication and cleanup. Separate per-application UIDs, resource migration
and secure attention still precede secret release and elevation.

EOF, malformed requests and unknown handles all fail the paired lifecycle,
including a double poll of a retired handle. There is no clean-success exit
that can leave only one authority peer alive, and no soft recovery from a
caller protocol error. The compositor must serialize requests and discard
handles when that generation ends. Root diagnostics include validator exit
status and spawn errors; subprocess stderr remains null. An operator can run
the fixed read-only firstboot check directly to diagnose its refusal.

Host tests execute a real child through the same descriptor/environment
sanitizer and verify its inherited kernel descriptors, bounded records,
terminal argument selection, strict wire records, and completion retirement.
The target producer runs these tests with source-built Rust. No test or
terminal-launch success is evidence of FIDO2 authentication or consent.

`td-authd/tests/launch_vm.rs` is a standalone disposable-VM fixture. Compile
it with host rustc for an installed static target, then run its binary with
`--run-vm KERNEL AUTHD FIRSTBOOT LOGIN BUSYBOX NEW-LOG`, using absolute
paths. The supplied production binaries may be source-built target outputs;
the fixture itself is a host diagnostic, never an input to a target recipe
or part of an image. It requires td's pidfs-capable kernel and QEMU on the
host. It proves the real channel-to-validator-to-credential-helper chain,
verifies the terminal's uid/gid, empty capabilities, independent process
group, exact session cgroup, six-variable environment and absence of
inherited authority fds. A wrong sender, missing ledger and failed cgroup
placement all withhold terminal execution. A missing persistent state
directory is also refused without recreating it. The fixture's terminal
stand-in tests the launch boundary; it does not claim a rendered graphical
terminal or a FIDO2 flow.

Cargo type-checks this fixture as the host-only `terminal-launch-vm`
example; the recipe compiles production and inline tests directly with rustc
and does not build Cargo examples. An exact host compilation form is:

```text
rustc --edition 2021 --target x86_64-unknown-linux-musl -C linker=gcc \
  td-authd/tests/launch_vm.rs -o /tmp/td-terminal-vm-runner
```

The host must have that static target and linker installed. `cargo test`
executes the ordinary suite; its two ignored exec-only fixtures are invoked
by their parent tests with sanitized descriptors and environment. Running
all ignored fixtures directly is not a supported suite invocation.

The compositor client is specified by td-compositor/DESIGN.md. It
shares this transport, greets before workers exist, then keeps the endpoint
exclusive in one worker. The image activates that client at UID 993.
Linux's `include/net/scm.h::scm_send` supplies `task_tgid(current)`; a worker
thread retains the process pin while a forked descendant does not. The kernel
fixture exercises both cases. The shared module imports its sibling sys
module so each consumer's transport remains private to that consumer.

The terminal helper sets `TD_CONTROL_SOCKET` to the fixed compositor runtime
path before exec. The terminal passes its actual Wayland socket path to its
PTY child, preserving the connection endpoint across the compositor UID
cutover. No root authority operation accepts either path from a requester.
