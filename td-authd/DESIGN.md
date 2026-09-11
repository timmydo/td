# td-authd

This supplies the private channel, fixed terminal launcher and root side of
one-operation token release. The paired compositor invokes its secret session extension for physical
enrollment, release, and a queued credential write. The public write intake
admits only the configured human; it cannot select or acknowledge a prompt. The image
starts it paired with the dedicated compositor and uses its terminal launcher.
The eventual operation policy follows APPLICATIONS.md §L.1 and principle 7:
one named operation, typed and descriptor-pinned arguments, one protected
consent bound to that request, no remembered approval. Protector changes
add fresh hardware-backed authentication under
[`td-install/ENCRYPTION.md`](../td-install/ENCRYPTION.md). Separate
compositor/application identities and exclusive device ownership precede
enabling that path.

## Portal file preparation

The root-only `prepare-portal-files` startup operation exposes the fixed
human Downloads directory `/var/home/tester/Downloads` at
`/var/td-portal-files/1000/Downloads`. It changes neither on-disk ownership
nor file contents. A detached, nonrecursive idmapped mount maps filesystem
UID/GID 1000 to portal UID/GID 991 and requires read-only, nosuid, nodev and
noexec mount attributes before publication. This lets FileChooser retain
its bounded Downloads view when the portal stops sharing the human UID,
including files created with mode 0600. It grants no access to the rest of
the human home. The portal does not receive a mount or namespace descriptor. The empty
root-owned destination directories persist under `/var`, with the mount
recreated at boot. This shared view stays outside reserved private-runtime
trees so the jail's alias refusal does not also reserve human Downloads.

There are no caller-selected paths, IDs, flags or permissions. Startup uses
the same single-root-thread, no-controlling-terminal and standard-descriptor
admission as the terminal authority. Each path component is opened through
a retained parent descriptor without following links. Root ancestors and
the human home must exclude other writers; Downloads must be directly
owned by human UID/GID 1000. Root owns all destination parents and permits
traversal. Repeated preparation accepts only the same source inode exposed
at the fixed mountpoint with all four restrictive mount flags and portal
ownership. A replaced Downloads root, stacked mounts or changed parent
metadata is refused; this helper does not repair a live altered grant.
The portal waits for this unit to settle but does not require its success.
An unavailable grant refuses FileChooser requests; Settings and Secret
remain available.

The internal `portal-file-namespace` child also requires root startup and
has only the standard descriptors. Its parent clears the environment,
replaces stdin with a private socketpair endpoint, and replaces both log
descriptors with `/dev/null` before execing the fixed td-authd binary. The
child creates only a user namespace, reports readiness, then waits for its
parent to write the fixed one-entry UID/GID maps with setgroups denied.
The parent retains this unreaped direct child while opening its namespace
descriptor, so the numeric PID cannot identify a replacement process.
Each socket read/write has a five-second timeout. A child guard kills and
reaps the helper on failure; the namespace remains alive through the
retained descriptor after normal helper exit. This IPC carries no secret
or consent and does not use the attention channel.

`mount_sys.rs` confines four fixed calls and descriptor adoption separately
from the channel transport. The compositor does not include this module.
UNSAFE.md §16 records the exact syscall numbers, flags and layouts. The
kernel or filesystem refusing idmapped mounts fails preparation; there is
no chmod, ownership rewrite, writable view or ACL fallback.

The fixed root-only `release-portal-files` operation runs after services stop
and before `/var` is unmounted. It accepts no arguments and checks the bounded
mount table. An absent view succeeds, including failed preparation. A present
view invokes the existing td-init `/bin/umount` applet with exactly that path,
from `/`, with a cleared environment and null standard descriptors, then
requires the mount to be absent. It reports unmount failure; it does not use
lazy detach or fall back to another path. This uses no new raw syscall surface.

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
image activates these identities together with private state ownership and
credential/socket admission. The typed launcher consumes that reservation;
this identity parser grants no token consent.

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
protected runtime. The portal account `tdp1000` consumes UID/GID 991 and
its private `/run/td-portal/1000` runtime. Firstboot transfers
credential files and volatile release ownership to it. Stock
applications consume their assigned service accounts, private state and
runtime in one cutover. The paired compositor may enter its inert
credential and channel startup at its reserved UID before ledger
admission; the authority verifies the ledger before allowing device
access, worker creation, or human terminal launch. A failed firstboot
unit settles ordinary service ordering; the broker requires its success
explicitly. Ordering alone is not authorization. The boot oracle
requires the exact `TD-PRINCIPALS-ENROLLED` line on every successful
boot. Retired human accounts may disappear while their service and
application reservations remain.

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

### Application runtime preparation

For each installed and validated application account, firstboot prepares
`/sys/fs/cgroup/td-app-APP_UID` after private state conversion, before its
state-ready and enrollment markers. It requires one writable cgroup2 mount
at the unified hierarchy root, binds its major/minor device to the opened
hierarchy's metadata, and requires the cpu, memory and pids controllers
already enabled there by td-svc. It never changes the system root's policy.
An unavailable controller, foreign owner, redirected path, live descendant
or threaded cgroup fails firstboot before machine-id and host-key
provisioning as well as withholding enrollment and dependent services.
The supervisor's independent console-start guarantee remains in force.
All operations use safe filesystem
I/O through retained directory and control descriptors. Startup before any
human/application process and a root-controlled mount hierarchy are trusted.
This is not live cgroup repair or a credential-switch authority.

The app delegation is an empty domain cgroup. Firstboot enables and reads
back its three controllers, creates an empty domain `session` leaf without
subtree controllers, and requires that leaf and its subtree-control/type files to remain
root-owned. An existing app-owned leaf or subtree control refuses; it is
never reclaimed as a repair. The app owns the
leaf's cgroup.procs/cgroup.threads and the delegation's cgroup.procs,
cgroup.threads and cgroup.subtree_control. Directory ownership is published
last, after every control assignment reads back. The kernel hierarchy is
volatile; repeated pre-session preparation accepts those same assignments
but refuses populated subtrees. No limits are written here: td-jail applies
its immutable per-instance policy in siblings of session.

Firstboot then prepares root-owned mode-0755 `/run/user` and app-owned
mode-0700 `/run/user/APP_UID` using the existing pinned runtime provisioner.
An existing published runtime with wrong ownership or permissions refuses;
only interrupted root-owned creation is completed. No socket is created.
Only installed accounts consume reservations. The stock image installs
all four application accounts and couples their root launch, socket
admission, grants and private per-app fetch services.

Before dropping credentials, td-login selects `td-user-1000/session` for
the human and `td-app-APP_UID/session` for the reserved application UID
range. The kernel enforces placement and td-login reads membership back.
Its ordinary placement failure remains a diagnostic for console recovery;
the application launcher must require the expected membership after the
credential switch. UID selection creates no account, cgroup or privilege.

### Application state preparation

An activated application account's home must be exactly
`/var/lib/td/applications/APP_UID`. The shared account validator checks this
for retained as well as current application reservations. An absent account
remains a reservation; its row alone creates no application state or runtime.
The stock image activates these accounts together with the root launcher,
cgroups, socket admission and per-application fetch services.

When a deployed application account is present, firstboot prepares its home
after checking the durable registry and before reporting enrollment success.
It requires the configured human migration home for that application's
owner. The current provisioner invocation configures one human home; it
refuses a deployment with active applications for another owner. This is a
sysinit operation before human or application processes start, not a live
ownership conversion. Root configuration and that startup ordering are trusted.
It does not revoke descriptors or historical copies from an earlier session.
Unlike optional template provisioning, failed active-account preparation is
fatal to firstboot and withholds enrollment success and dependent services.
It must never authorize launch against an unconverted state tree. The image
currently has one human session; multiple active owners need a provisioner
configuration naming each migration source before that deployment can boot.
A refused source remains available for correction from a previous deployment;
an I/O-interrupted moved tree may require root offline repair. No fallback
silently creates a fresh app profile or discards existing data. Human UID and
GID may differ; the application account uses its assigned UID for both.
The stock /home link and private state share the persistent @var subvolume.
Cross-filesystem rename is unsupported and refuses before moving data.

The persistent applications parent is root-owned mode 0755. A stable,
root-owned, single-link, empty mode-0600 `.migration.lock` serializes home
conversion writers with an immediate busy refusal. It is acquired per home, not across template provisioning or credential
writes. It is never renamed or removed.
Interrupted creation may normalize missing owner mode bits on this validated
empty lock. Root must not replace a live lock or change the mount hierarchy.

Each new home stays root-owned mode 0700 until its entire conversion is
synced. Firstboot validates the existing human `.td/app/NAME` tree without writes,
then moves its directory into
that home, through retained source and destination directory descriptors.
It keeps the jail's `.td/app/NAME` layout. It never copies or merges a second
tree over existing state. A published app-owned mode-0700 home is complete;
later human recreations of the old path are ignored. All supported launches
must use the private home when the image activates the account.

Conversion preserves regular-file bytes and permission bits, changes UID/GID
to the reserved app identity, and syncs files and directories before
publishing home ownership. It preserves symlink text without following it;
only the link's owner changes. Special files, hardlinks, foreign owners,
cross-device trees and setuid/setgid bits are refused. Traversal is bounded
to one million entries and depth 64. A preflight refusal leaves the human pathname and ownership intact.
A later I/O failure during conversion leaves the moved tree behind
the root-owned home; it is not discarded. Failure before the move does not
claim removal of human access. Same-device bind aliases and previously open
descriptors are excluded by the trusted pre-session startup assumption,
not detected by the device-number check.

Restart accepts partially converted app-owned children under the root-owned
home, including converted scaffolds and an interrupted empty creation with
masked permissions. An unpublished empty destination does not override an
existing legacy tree: validation and rename are retried first. It never repairs a published app-owned home's contents.
The terminal configuration provisioner then writes only each application's
own template into its private home. Credential records retain the logical
human UID and the portal file owner; an application UID never becomes the
credential-store identity. No account, launch or FIDO consent is enabled by
this preparation support alone.

Recovery of this non-secret ledger is an offline maintenance operation, not
token recovery or an authorization bypass. Stop all enrollment writers
before repairing a scratch file; unlinking a live lock would split writer
serialization. A corrupt ledger requires a backup preserving every reserved
assignment, or reconstruction from complete deployment history and retained
state. Never reset it to the current table or prune retired rows. If the
union exceeds its bounds, raise the format's reviewed bounds in a deployment
update; overflow never silently drops reservations. Without a complete
ledger, keep the application UID launcher disabled. Console recovery does not
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
executable, environment, directory path, account, uid or argument vector. A
typed terminal kind selects either the account home or td's fixed task
worktree. All
authority-spawned credential-helper children replace stdin, stdout and stderr
with `/dev/null`, clear the environment, and start from `/`. The task variant's
eventual td-term child enters only the fixed worktree described below. Replacing
stdin also replaces the original private endpoint on fd 0: relying only on
Channel's CLOEXEC clone would leak authority. All subsequently created channel
descriptors are CLOEXEC. The daemon never passes an inherited root log
descriptor to a user program.

Before validation both ends exchange the framed `TDLA002` protocol greeting
with a final newline. After successful validation the authority sends `80`;
only then may the peer submit requests. The earlier transport greeting in
Channel pins the sender before the protocol greeting or any spawn.
Subsequent payloads are exact byte records:

| Request | Response |
| --- | --- |
| `01` | `81` plus a nonzero big-endian u64 process handle |
| `02` plus that u64 handle | `82 00` running, `82 01` successful exit, or `82 02` failed exit |
| `03` | `83` heartbeat |
| `04` | `81` plus a process handle for a terminal in the fixed task worktree |

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
terminal-exec UID GENERATION HANDLE [task]` in a new process group. The
optional literal is present only for request `04`; it is not a pathname.
td-login checks
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
/run/user/UID/td-auth-terminal-GENERATION-HANDLE.ready`. The task variant adds
`--working-directory /home/tester/src/td-vm/work`; the ordinary variant keeps
the verified account home. No shell command or
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

## Fixed application launch prerequisite

The root startup operation `application-start OWNER APP direct|terminal --
ARG...` consumes an installed application assignment. OWNER is currently
1000, the image's single graphical session; APP has the canonical registry
name grammar. The presentation and at most 128 literal arguments (32 KiB
including terminators) come from root-owned unit configuration. Direct and terminal presentation have no
request listener, caller-selected executable, credential change in td-authd,
or human elevation. Stock application units use this operation after
firstboot and, where needed, mapped-grant preparation succeed.

Startup requires the same root credentials, single thread, absent controlling
terminal and exclusive standard descriptors as the terminal authority.
The conservative shared audit also requires neither log to alias stdin;
a unit with all three descriptors pointing to /dev/null is refused. A
child runs only `/bin/td-firstboot check-launch-application OWNER APP` with
an empty environment, `/` working directory, null stdin/stderr and a private
stdout socketpair. The read-only check verifies immutable account files and
that the deployment is already durably enrolled. It selects only an active
current application account, never a retired reservation or caller UID.
Its exact bounded reply carries the canonical UID. One two-second deadline
covers reply reads and observed child completion; failure kills and reaps
the validator. Trusted root must not change the account database, deployment
or mount hierarchy during launch. The check creates no state and does not
substitute for successful firstboot dependency ordering.

After validation the authority execs the fixed td-login `exec-service-as`
helper for `tdaUID`, followed by its own `application-exec` wrapper. All
standard descriptors become null, the environment is cleared and cwd is `/`.
The supervisor PID is retained across execs. No inherited root endpoint or
log reaches application code. Root startup refusals are diagnosed; after the
handoff, failures appear as the supervised process's exit status. The fixed
firstboot check can be run directly for detailed admission diagnostics.

The unprivileged wrapper requires all four UID/GID columns to equal the
selected application UID, exactly its primary supplementary group, empty
inheritable/permitted/effective/ambient capabilities, and exact membership
in `td-app-UID/session`. A diagnostic-only td-login placement failure thus
cannot execute application code. This wrapper gains no privilege and is not
an alternative application-identity authority: an app UID invoking it directly
still has only that UID. Broker and jail activation must independently bind
that identity to its installed application.

The wrapper constructs only `/bin/APP` or `/bin/td-term run` with that fixed
application command. Terminal presentation selects the human session's public
Wayland socket and an app-private readiness socket. It supplies the canonical
private HOME and runtime, service USER/LOGNAME, `/bin/false` SHELL and `/bin`
PATH to its immediate child. td-term separately constructs the command's
environment from its service account: its existing PTY contract uses that
account's HOME as cwd, `/bin/sh` as SHELL, and its selected WAYLAND_DISPLAY.
This does not execute a shell for an explicit application command or change
its service-only account class. Direct presentation uses the immutable
`/bin/APP` td-jail link, whose product resolver fixes the display endpoint.
It passes literal application arguments without a shell and sets no
compositor control endpoint. The bounding capability set is not a held
privilege and is not required empty. No new syscall or unsafe surface is
introduced; confinement pins the complete application controller and startup.

## Claude shell launch

`application-start 1000 claude shell --` performs the same fixed deployment
admission, requiring Claude's installed UID 65539 and no startup arguments.
Root binds only `/run/td-claude-launch` below the root-owned non-writable
`/run`, then makes the socket mode 0600, owned by UID/GID 1000. It refuses
an active or unexpected existing inode; a refused connection to a stale
socket of the expected type and ownership permits replacing it. Root does
not accept or parse requests. The listener replaces stdin through the
existing service credential helper; stdout/stderr become null. After the
same complete credential and cgroup checks, application-exec serves under
UID 65539. This is the sole exception to the null-stdin startup handoff.

The human's `/bin/claude` invocation execs `td-authd application-client`
with literal OS-string arguments. The service-UID by-name path still enters
td-jail directly. A two-sided greeting orders installation of credential
receipt: the client's initial ready byte is only synchronization. The client
requires a root connection creator, then pins the actual UID/GID 65539
message sender. The server requires UID/GID 1000. Each side retains and
checks the live sender pidfd and its device/inode on every received fragment.
Only the server's one-byte terminal reply carries a descriptor; all other
messages refuse rights. No client-supplied descriptor reaches an application.

The length-prefixed request is at most 40 KiB, with at most 128 literal
arguments totaling 32 KiB including terminators, a 4096-byte absolute cwd,
a 64-byte ASCII terminal name and four u16 window dimensions. Each frame
has one two-second read/write deadline. The server maps cwd beneath
`/home/tester/src` or `/var/home/tester/src` to its existing private idmapped
`src` grant, rejecting parent traversal; other directories select `/`.
The jail independently resolves and validates that directory through its
ordinary grant policy. No grant or file ownership changes. No other ambient
environment, executable selection, UID selection or root command is accepted.

The application-UID server opens a fresh PTY and starts exactly
`/bin/td-jail --internal-application-session SERVER_PID /bin/claude ARG...`
with three clones of its slave, fixed private HOME/runtime and an empty
ambient environment. The jail becomes a session leader for terminal stdin,
then retains its existing fresh-terminal, installed-identity, registration,
cgroup, namespace and seccomp checks. The listener and all other descriptors
are closed on child exec or replaced with the slave. The server drops all
slave copies and transfers the master to the authenticated human client.
The client accepts only a PTY master character device and relays its terminal
with a bounded 64 KiB input queue, initial/window-change propagation and raw
input, restoring the outer mode when it returns. Interactive control bytes
reach the new terminal's line discipline. Redirected EOF sends Ctrl-D;
this is terminal behavior, not pipe half-close or separate stderr semantics.
External fatal signals can bypass raw-mode restoration.

One unprivileged server permits at most 16 workers, including pending
handshakes. The accept loop sleeps in the kernel when idle and retries
aborted connections or transient resource exhaustion without ending active
sessions. Threads start only after the credential and cgroup checks.
Each worker performs one bounded admission and then owns its child and
polls completion and client liveness independently. A stalled handshake
cannot delay another invocation's exit status. Each spawning worker stays
alive through its child's kill/reap: Linux parent-death notification follows
that parent thread. Malformed or disconnected clients cannot terminate
other sessions. Client disconnection kills and
reaps its child. Server death triggers the jail's existing parent-death and
namespace cleanup path. Completed children return their exit code, or
128 plus signal, to the client after terminal output drains. No persistent
root request loop, authorization prompt or privilege acquisition is added.
The unit's `application-probe` exercises the authenticated greeting and a
bounded ping without starting Claude. The boot oracle additionally runs
human-UID `claude --version` while retaining the private-UID terminal-grant
refusal and Firefox identity checks.

## Application filesystem grants

Root startup prepares three writable idmapped views: Firefox and mail
share human Downloads, and Claude receives human src, each below the assigned
application's private home. `prepare-application-files APP` and the matching
shutdown operation accept only those three installed names. Active-account and
durable-ledger admission select the UID; no caller supplies a path or map.
The existing root namespace helper maps filesystem human UID/GID 1000 to
that application identity. Mount attributes require nosuid, nodev and noexec;
a cloned read-only source remains read-only and refuses writable admission.
The portal's separate read-only Downloads view is unchanged.

Every directory is opened through a retained parent without following links.
The human source must be directly owned by UID/GID 1000, and the private
application home must have exact app ownership and mode 0700. Preparation
runs before application processes start; it is not live repair of an
app-writable home or protection against a root mount-table editor. A present
view must match the source device/inode, selected mapped owner and all four
required options. Wrong or stacked mounts are refused. Shutdown resolves the
same enrolled assignment and invokes only the fixed view's umount helper;
failed ledger admission or a remaining mount is reported, not silently
ignored. The image must stop services before releasing these mounts and
release them before unmounting /var.

Application activation uses the same immutable application policy reader in
the broker, jail, portal, compositor and audio daemon. Broker registration
requires the assigned external UID; human UID registration is removed.
The portal validates broker-reported UID plus app name against its loaded
policy for both credential retrieval and FileChooser. Public Wayland admits
the human and assigned application UIDs; audio admits the human, its own
service and the assigned Firefox UID. Compositor control and
readiness remain human-only, and the private portal channel remains UID 991.

Mail and news fetch daemons use their own service accounts and mode-0700
runtime directories. Credential-bearing fetch requests therefore remain
inside the application's UID domain. The human fetch service is separate.
The fixed root Claude boot oracle uses its service account directly to
capture the no-terminal refusal and real PTY child-exit proof; production
application units use the typed root launcher with null application stdio.
Bus evidence connects as the human observer and independently checks the
subject application UID against the immutable policy. An unjailed service
UID retains launcher filtering and is not an unrestricted bus observer.

The grant preparer may create the two declared missing human directories,
initially root-owned mode 0700 and then transferred to the human UID/GID.
A pinned empty root-owned directory with only owner permissions is the
restartable creation intermediate; nonempty or nonprivate remnants refuse.
Existing sources must already have human UID/GID and mode 0700, checked
before publication; private parent modes must also be valid;
no recursive chown, fallback copy, or broad human-home grant is performed.
The writable mapped owner can change the shared source's permissions and
contents. Firefox and mail therefore share availability of Downloads, as
well as its data: either grantee or the human can invalidate its private
mode. The next preparation refuses and identifies the application and
source in its diagnostic. The human owner must restore mode 0700 on the
named source before retrying the service. Root does not silently undo an
owner's permission change or start an application without its required
grant. This shared directory is outside the private per-application state
boundary; arbitrary deletion or renaming by its owner can also deny it.

Jail admission permits only the declared projection's source identities,
including its root, through the otherwise reserved home boundary. The
maintenance backing-volume ancestor remains read-only and grants no
sibling state. Nested mounts pointing outside the declared human source
and protected mounts at or below it are refused.

## Immutable consent description prerequisite

`consent.rs` supplies a bounded, immutable public description for the trusted
renderer. The canonical wire value is `TDCONS01`, a nonzero 32-byte operation
nonce, a big-endian u32 human UID, and a one-byte operation. Enrollment (1)
adds platform profile 1 (TPM PCR 7), the recovery policy (1 second token,
0 explicitly unrecoverable, matching TDENROL1) and
step (1 create primary, 2 prove primary, 3 create recovery, 4 prove recovery).
An unrecoverable request refuses either recovery-token step. Unlock (2) adds
role 1 primary or 2 recovery. Credential write (4) adds role 1 primary
or 2 recovery, then big-endian u32 application and requester UIDs, then a one-byte length and ASCII bytes for
each application and credential name. The whole value is at most 256 bytes;
unknown tags, truncation and trailing bytes refuse.

The human UID is 1000 through 65533; the external application UID is 65536
through 2147483647. A write's requester must equal its human owner. Names are
at most 64 ASCII bytes: application names share the launcher predicate,
credential names admit alphanumerics, hyphen and underscore and retain case.
The value contains no credential bytes, file path, executable or arbitrary
instruction text. Rendering shows every human-relevant operation argument, token role or
recovery choice; the nonce is retained but not shown. Enrollment binds the
fixed TPM PCR 7 profile in its canonical encoding and refuses other tags.
This means exactly SHA-256 PCR selection `7` (mask bit 7 alone); the
store's other supported PCR selections are deliberately unencodable in
this prompt profile. The authority must refuse those selections, never
map a different mask onto this label. Spaces and dots are excluded from
credential names: indented continuation text cannot imitate the fixed
labels or token instructions. All consumers pin the codec source and its
tests assert the complete public argument display.

These are structural checks, not caller admission or proof of
randomness. The private root unlock and enrollment workers in
`td-secret/DESIGN.md` consume this codec. The paired authority exposes its typed private
session extension; the paired compositor supplies physical enrollment and
unlock choices and their exact presentation receipts. The separate public
credential intake below accepts one queued write without opening a prompt.
The authority must pin the requester and credential input, admit
the application from deployment policy, own an immutable operation under
its fresh nonce, and bind its token challenge to the complete canonical
description. The compositor's presentation receipt is necessary but
insufficient: cancellation, peer loss, deadline or request replacement
must invalidate authority before committing any write or release. The
renderer and its private session client are specified in
`td-compositor/DESIGN.md`.

## Private token child supervision

`unlock.rs` owns one root-private `td-secret unlock-operation --uid
1000`, `td-secret enroll-operation --uid 1000`, or
`td-secret write-operation --uid 1000` child. The paired service calls this controller through the
private session extension below. The paired compositor enforces physical
selection, exact immutable step presentation and cancellation before commit.
This private controller exposes no public listener and performs no automatic
release. Its paired root caller enforces startup and session admission before
constructing the production controller; queued writes arrive through the
separate credential intake and require physical selection.
The child independently requires root; this controller adds no
credential switch. It generates a fresh kernel-random nonce for a typed
Unlock role, an Enroll request starting at CreatePrimary with one explicit
recovery policy and the fixed SHA-256 PCR 7 profile, or a Set request with
the admitted target, requester and selected token role. It
accepts no caller-selected nonce, executable, path, account or argument
vector.

The fixed child starts from `/`, with an empty environment, private
stdin socketpair, and null stdout/stderr. Its peer endpoint is
exclusively owned by the controller and is CLOEXEC. The child inherits
the authority process group; it does not create a detached lifetime. The
supervisor's endpoint is nonblocking. Each poll performs at most four
socket calls and reads no more than the current bounded frame. A partial
frame or pending write has a five-second deadline; waiting for token
work has the overall 120-second operation deadline. A received
presentation/commit invitation expires after three seconds, leaving
margin against the child's five-second window. Delayed delivery can
still exhaust the child's own deadline and fail the operation. Each
canonical description must exactly match the root-owned request. The
child-generated round stays private to this controller. The caller
receives only the immutable public description.

Presentation and commit acknowledgements are separate state transitions.
The caller must validate the actual completed compositor presentation
receipt before invoking `presented`, and serialize physical cancellation
and peer loss against `commit`. These are distinct methods: repeating a
presentation acknowledgement cannot approve the later commit round. A
matching public Request is structural evidence, not proof that pixels
were presented. Receipt validation remains a duty of the authenticated
paired caller; this controller cannot observe the compositor output. The
root controller validates the retained request and deadline again before
queuing either acknowledgement. Each controller owns exactly one
operation nonce and its current immutable step description. The caller must own at most one controller per admitted session
and must not construct a replacement while the old child remains owned.
Publication is complete only after the final success frame and observed
successful child exit. A final frame followed by a failed exit is a
failed operation and requires relocking.

Cancellation closes the endpoint, kills the exact unreaped child, and
polls for its exit before spawning the fixed root-only `td-secret
lock-session --uid 1000` cleanup. That helper clears volatile release
state without opening the persistent store or accessing hardware.
Cleanup runs with empty environment, `/` cwd and null standard
descriptors. Its observed deadline is two seconds; expiry kills the
helper. Polling waits for reaping even after a kill, so a blocked kernel
task cannot be abandoned and later publish into a subsequent operation.
Normal cancellation and cleanup never block the caller in `wait`; the
caller continues heartbeats while polling. A successfully relocked
failed or cancelled operation returns the typed `Event::Failed` result.
Failed helper spawn, exit or deadline produces a stable terminal `Err`,
which must end the authority generation. Callers must not classify
policy by parsing diagnostic strings. Repeated polling returns the same
terminal outcome; no failure becomes permission to retry an uncertain
token operation.

Dropping an unfinished controller is emergency teardown: it kills and
waits for its retained direct child, and does not claim runtime-key
cleanup. The live caller must cancel and poll through completed
relocking before dropping it. The paired supervisor must kill/reap the
prior process group and successfully clear its session runtime before
admitting a replacement generation. SIGKILL after publication still
requires that generation cleanup. These caller duties remain mandatory
for activation; the controller alone does not establish them.

Tests run actual exec children through the same sanitizer and socket
controller. They cover both round acknowledgements, failed exit after
the success frame, stale request/receipt refusal, partial-frame
deadlines, prompt polling during a stalled child, cancellation and
subsequent cleanup. They also cover missing, failed and stalled cleanup
helpers and stable fatal results. The target recipe stages and compiles
the same tests. No test substitutes a protocol acknowledgement for
physical token presence.

The ignored root fixture
`unlock::tests::root_supervisor_relocks_after_the_production_worker_refuses`
requires the marked disposable VM and production `/bin/td-secret`. It
starts the actual controller against a missing persistent store and
proves the child refusal is followed by successful real cleanup, no
presentation or release event, and removal of a seeded portal-owned
runtime key.

## Paired secret session extension

After root startup, sender-pidfd greeting and the existing immutable
account/ledger admission, the paired authority owns exactly one Session
for the configured human UID 1000. Its TDLA002 protocol has the exact
requests below; the existing terminal client does not send them yet.
No new socket or public listener is created. Only the pinned compositor
process may send these records. None contains a credential, token
assertion, private worker round, caller-selected executable or path.

| Request | Response |
| --- | --- |
| `10` prepare this generation | `90` cleanup started |
| `11` poll session | `91` plus status and, for operation statuses, the full canonical description |
| `12 01` primary unlock, `12 02` recovery unlock | `92` plus the root-generated canonical description |
| `16 00` unrecoverable enrollment, `16 01` enrollment with a second token | `92` plus the root-generated CreatePrimary description |
| `13` plus canonical description | `93` presentation acknowledged |
| `14` plus canonical description | `94` commit queued |
| `15` plus the nonzero 32-byte operation nonce | `95 00` stopping, `95 01` already completed, or `95 02` already failed/relocked |

Session status is 0 unprepared, 1 preparing, 2 idle, 3 operation waiting,
4 presentation required, 5 commit required, 6 completed, or 7 failed
and successfully relocked. Idle does not assert locked state: a
completed unlock leaves its released key until generation cleanup.
Statuses 3 through 7 include exactly the retained public description.
Polling a terminal status retires that one operation; a lost response
ends the generation, never permits retry. Terminal heartbeats also poll
the child and enforce watchdogs but do not consume its result. The
compositor must continue ordinary heartbeats or requests within the
transport deadline, including throughout a token wait. During preparation
and an outstanding operation it must additionally poll secret status at
least every 250 ms; terminal heartbeats alone do not deliver invitations.
A heartbeat may start the supervisor's three-second acknowledgement
window while retaining its invitation for the next secret poll. Neither
polling nor heartbeat traffic renews that window. All helper deadlines
bound observed completion, not an unobservable earlier exit time; a peer
that delays observation beyond them fails the generation even if the
helper has already exited.

Prepare is single use. It runs only the fixed root `td-secret
lock-session --uid 1000` helper with an empty environment, root cwd,
and null standard descriptors. Polling requires successful observed
exit within two seconds before admitting any secret operation. Expiry kills and
retains the child until reaping; failed cleanup ends the generation.
This does not open a persistent store or access hardware. A new operation
requires completed preparation and no retained predecessor, including
an unpolled completion. Root generates its nonce and fixes its owner;
the peer selects only the unlock role or explicit enrollment recovery
policy. Unlock refuses a store that is not token protected; enrollment
refuses token replacement. Enrollment success leaves the store locked.

Acknowledgements must exactly match the immutable description and the
supervisor phase/deadline. Cancellation must match the active nonce and
is applied before any further child poll. Queueing commit is the root
execution decision: cancellation after the worker receives it cannot
undo an already completed release or enrollment publication. A presentation or commit
acknowledgement reports the queued transition,
not token success. Cancellation against a retained terminal result
reports that result explicitly and preserves it. A stopping response
requires subsequent polling for final cleanup; it is not proof that the
worker has already stopped. Only status 6 follows the
final worker response and successful observed exit. Repeated or stale
acknowledgements, unknown cancellation, malformed records and overlapping
operations end the paired generation, as terminal protocol errors do.

On any channel or dispatch failure after preparation was attempted,
the authority closes the private worker endpoint, kills and waits for
its retained direct child, then runs explicit runtime-key cleanup,
including after a completed unlock or the operation's own cleanup error.
It does not replay that internal error instead of attempting final
cleanup. Successful wait proves death even when the earlier kill failed;
an unreapable child refuses cleanup rather than permitting a late
publication. It accepts no more requests during teardown. Blocking wait
is confined to this already-failed peer path. Cleanup polls are bounded
but may retain a killed task indefinitely in kernel sleep; td-svc owns
the enclosing service containment and refuses a replacement while it is
populated. Root authority workers remain in that containment, unlike
terminals intentionally handed to the human session. SIGKILL cannot run
this cleanup, so the next compositor generation must complete Prepare
before device/input admission or any secret request. That activation
ordering is enforced before graphical input or client admission.

The compositor's private client binds one physical attention lifetime to one
operation and each required immutable presentation receipt. It serializes
cancellation against commit atomically before channel I/O, never renews its
overall deadline, and withholds graphical input admission until generation
preparation succeeds. Enrollment uses read-only inspection before beginning
and after failure; a lost completion reply cannot authorize a retry. An
already-enrolled store requires a separate fresh unlock. Typed credential
writes use the descriptor intake and their own fresh presented token
operation; an existing release cannot authorize a write.
The root verifies the authenticated peer and exact public description;
it cannot independently observe the peer's framebuffer. A matching
public byte string alone is not evidence of presentation or token touch.

Host exec tests cover preparation before admission, failed/missing/stalled
cleanup, one retained operation, bound presentation and commit records,
and nonconsuming heartbeat polls. The ignored root fixture
`session::tests::root_session_preparation_failure_and_generation_exit_relock`
requires the marked disposable VM and production `/bin/td-secret`. It
proves preparation removes a seeded runtime key, a missing persistent
store never reaches presentation, failed-operation cleanup completes,
and generation teardown removes a newly seeded key. This fixture calls
the actual session controller; the separate channel fixtures prove
sender authentication. It does not claim physical token presence or a
compositor presentation.


Enrollment progression is fixed by `Request::following_enrollment_step`
in the shared canonical codec. It retains nonce, owner, platform and
recovery policy, advancing CreatePrimary to ProvePrimary and, only when
SecondToken was selected, to CreateRecovery and ProveRecovery. A final
proof has no following presentation. The canonical wire format is
unchanged. All three consumers pin the updated source; literal sequence
tests distinguish the two policies.

After acknowledging one step's presentation, the root supervisor accepts
only the next exact private worker `10` invitation. Only after the selected final proof
may it accept a private worker `12` commit invitation for that same final description.
Initial invitations still match CreatePrimary exactly. A skipped,
repeated, changed or early-commit invitation cancels the child and requires
reaping and successful relocking before failure can be reported. The
current public description changes only after this strict validation;
stale earlier receipts cannot acknowledge a later step. Neither an
accepted step nor its presentation renews the 120-second operation
deadline. Each new invitation receives only the existing three-second
acknowledgement window.

The paired Session exposes each current description through the existing
poll records and requires it in acknowledgement requests. Responses
contain only their acknowledgement tag. Cancellation retains the same nonce
throughout every step. Enrollment and unlock share the single concurrent
operation slot, generation preparation, terminal-result retention and
teardown. The compositor fully presents each exact next step and invalidates
its earlier receipt, without arbitrary prompt replacement or resetting
physical attention after cancellation. Queued writes use the same operation
slot and acknowledgement progression.

Host child fixtures exercise both complete enrollment sequences through
the supervisor and paired Session, with literal expected steps and a
fixed operation deadline. They reject changed nonce, owner or recovery
policy, skipped/repeated steps, early commit and a stale previous receipt.
The root lifecycle fixture additionally runs both enrollment policies
against the production missing-store refusal and verifies runtime cleanup.
These checks establish protocol sequencing and ownership, not physical
presentation or token presence.


The fixed 120-second operation ceiling covers every enrollment step,
including four touches and any second-token connection time. It is a
conservative refusal boundary shared with the private worker and HID
transport, not a measured usability budget. The UI must instruct the user
to have both tokens ready before starting and show the remaining overall
time. Physical timing validation remains an activation check; this API
neither certifies that budget for hardware nor renews it after a step.

The authenticated paired root API trusts the dedicated compositor to
enforce physical selection and complete presentation before acknowledging.
A trusted compositor that violates those duties could invoke the root API. The
root cannot observe its framebuffer and trusts that admitted peer to
supply receipts honestly. The compositor consumer enforces these duties. Presentation acknowledgements carry descriptions in the request;
responses are the bare 93/94 tags, while poll results carry descriptions.

## Paired read-only store inspection

The admitted paired peer may send exact request `17` only after Prepare
has completed and while neither an operation nor an unconsumed inspection
is retained. Root fixes owner 1000 and starts `/bin/td-secret inspect-store
--uid 1000` with an empty environment, `/` cwd, null stdout/stderr, and a
private stdin socketpair. The immediate response is `97` (started).
The peer continues polling at least every 250 ms. Poll `11` returns `91
08` while pending, `91 09 STATE` for a completed structural snapshot, or
`91 0a` for unavailable state. STATE is 0 file master, 1 TPM-only, 2 token
unrecoverable, or 3 token with recovery. Only a terminal poll consumes
the result; heartbeats advance the helper without retiring its state.
An unconsumed result excludes another inspection and every secret operation.

The nonblocking parent performs at most one read of three bytes per tick.
It requires exactly the helper's two-byte result, EOF, and a successful
exit observed within one two-second deadline. Partial, trailing or malformed
output, failed exit and deadline expiry become Unavailable only after the
child is reaped. Expiry or read failure closes the endpoint and kills the
owned child, retaining it through kernel sleep. Failure to kill or reap
ends the generation rather than losing ownership. Missing helper/spawn
failure also ends the generation. No uncertain result is retried by the
controller. A later explicit inspection can observe newer state.

Generation teardown drops the inspection, closes its endpoint, kills and
waits for its owned child before final runtime cleanup. This blocking wait
is confined to the already-failed peer path, with td-svc's containment and
replacement refusal still applying. The read-only helper cannot publish a
key or store; nevertheless a retained child is never abandoned to admit
another operation. It adds no listener, descriptor transfer to the peer,
raw syscall, authentication mechanism or compositor activation.

The result is structural metadata, not a claim that the last enrollment
succeeded, a token exists, or credentials are released. After an uncertain
enrollment outcome the UI must issue a new inspection, then offer fresh
unlock for a token store or a new explicit enrollment for an unenrolled
store. Unavailable state cannot select a fallback. An unlock/enrollment
independently revalidates the current store; no snapshot grants authority.

An inspection request before preparation, overlapping requests, or an
operation started before consuming the inspection result is a protocol
error that ends the authority generation. It is not a soft busy response.
The helper applies the same private root/stdin startup admission as the
token workers and writes directly to the socket, with no buffered stdout.

## Queued credential write

Prepare creates the sole configured session's named intake at
`/run/td-authd/1000/set`. The runtime and both parents must be root-owned
directories without unprivileged writers or special mode bits. Root owns
this paired generation's lifecycle; a replacement may remove its stale
socket, and teardown removes only the inode this generation created. The
socket belongs to human UID/GID 1000 with mode 0600. Incoming connections
require that owner in SO_PEERCRED and every fragment's SCM_CREDENTIALS;
the first live SCM_PIDFD is retained and each later sender must match its
credentials and pidfs device/inode identity. Root or service UIDs do not
acquire public submission authority. The named raw module is separate from
`sys.rs` and never weakens the paired channel's refusal of rights.

The versioned greeting, exact typed frame, single sealed descriptor and
bounded deadlines follow `td-secret/DESIGN.md`. There is no unsolicited
prompt and no public consent verb. Root sends the public `02` admission
receipt only after sender, target and descriptor validation; it is not
consent or completion. The request is unselectable until that receipt is
sent. Backpressure remains under the original admission deadline; a
successful send starts the queue lifetime. One accept and at most four
nonblocking I/O attempts run per terminal heartbeat. Extra connections are
closed while one pending client exists. Only the private peer's exact `18`
request selects a ready write. Busy, absent, expired or refused intake
returns `98 00`; a started write returns `92 DESCRIPTION`, like unlock.
Root resolves the installed application, captures its immutable bytes,
generates the nonce and spawns only `/bin/td-secret write-operation --uid
1000` with empty environment, `/` cwd, private stdin and null log fds.
The worker independently repeats installed-account and store admission.

The parent first sends the canonical description, then a separate bounded
credential frame before accepting any child presentation invitation. Its
private wire clears credential output before reuse or destruction. Public
prompt frames remain bounded by the existing 289-byte limit. The child
uses the unchanged fresh presentation and commit rounds, and the paired
peer uses the existing exact-description acknowledgements. Peer loss or
expiry is checked again before accepting a write commit. Completion is
reported to the public client only after the child exits successfully;
missing success is never authority to retry. Credential bytes do not pass
through the compositor or its channel.

A cancelled write kills and reaps its worker without invoking lock-session,
so an unrelated prior release survives. Failed-generation teardown still
reaps every worker before invoking lock-session. The named listener adds
no process launch choice, path argument, remembered consent, shell, token
fallback or automatic write retry. Root, the deployment identity table and
the paired compositor remain trusted. The intake bounds resource use, but
a malicious human process can occupy the single queue slot until expiry;
this does not authorize its request or force a prompt.

## Focused credential authority VM checks

`cargo run --release --manifest-path recipes/Cargo.toml --bin
td-recipe-eval -- qemu-secret` builds the target kernel, production
`td-secret` and `td-init`, and the separate `td-secret-vm-test` artifact.
That artifact contains the source-built authority test executable and a
Rust fixture init; it is not an input to the distribution image. Host QEMU
is an execution oracle only, as for `qemu-boot-system`, and is required.
This command runs outside the host-free recipe-check sandbox.

Four fresh, diskless TCG guests exercise the existing root-only cases:
public `td-secret set` descriptor intake for both token roles, refusal of
nonhuman callers, preparation and generation-exit relocking, read-only
store inspection, and cleanup after the production worker refuses. Each
guest has private account files and tmpfs runtime state, no network, TPM,
token, or persistent disk, and a 180-second host deadline. The fixture
selects exactly one named ignored test, requires its successful exit and
one-test passing summary, then powers off. The host requires the fixture
marker and a clean guest exit without a kernel panic.

These checks prove the kernel-facing authority and client boundaries;
they do not claim token presence, a presented compositor prompt, TPM
unseal, or a credential-store write. The public intake fixture explicitly
stands in for the completion reply. Pinned-emulator tests in
`td-secret/DESIGN.md` cover the separate cryptographic store operations.


The optional `qemu-secret --tpm /absolute/path/to/swtpm` mode adds four
TPM device cases to those authority checks. It uses the existing pinned
host emulator and a private raw fixture disk, with cold reopen and changed
PCR/different-TPM refusals specified in `td-secret/DESIGN.md`. These
additional tests do not replace the authority's token-consent boundary.

## Consent for a locally built system

`deployment.rs` owns the sole stock installation intake at
`/run/td-authd/1000/install`. Prepare creates it after the credential intake
has checked the root-owned runtime parents. It has the same protected-parent,
mode-0600 human socket, per-fragment UID and live sender-pidfd policy, but
refuses every incoming SCM_RIGHTS descriptor. It uses the unchanged named
transport in UNSAFE.md section 16. No public message grants consent.

The client `td-authd request-update SOURCE DEPLOYMENT-ID` requires a root
peer and the exact eight-byte `TDUPD01` newline greeting. Its u16 big-endian
body length covers exactly 64 lowercase hexadecimal ID bytes followed by a
UTF-8 absolute path of at most 4096 bytes. NUL and parent traversal refuse.
Root pins the final directory without following a symlink, requires its
owner to be UID 1000, and checks a bounded requester-owned regular manifest
through that descriptor against the ID before sending admission byte 02.
Intermediate path lookup and regular-file reads are synchronous filesystem
operations; the byte/deadline bounds do not promise latency on a stalled
filesystem. This queue cannot execute the source or open an arbitrary root
command. One client, one accept and four nonblocking attempts per heartbeat
bound socket work. Admission expires in five seconds; a sent admission
receipt starts a sixty-second selection window. Extra traffic, foreign
senders, descriptors, disconnect or expiry retire the pending request.

Only private request 19 selects a queued installation; no ready request or
a busy operation slot answers 99 00. A selection holds the directory File,
generates a fresh 32-byte nonce and answers 92 DESCRIPTION. The description
is consent tag 5: owner, nonce, requester UID and full deployment ID. It
shares one operation slot with secret operations and read-only inspection.
The root installation controller accepts exact presentation (13) and then
one exact commit (14), within 120 seconds of selection. The compositor must
obtain a fresh physical Enter after presenting the whole description before
sending commit. Root rechecks the submitting process immediately beforehand.
Keyboard confirmation authorizes this installation only; it cannot release
secrets or authorize protector changes.

After commit, root spawns only `/bin/td-update apply-operation ID`, with an
empty environment, cwd `/`, the held directory File on stdin, null stdout
and authority stderr. The helper's independent admission and transaction are
specified in td-install/DESIGN.md. No key, command, device or mountpoint is
selected by a requester. The confirmation is consumed even if spawn fails.
Closing the screen or losing the public client after commit cannot revoke
an installation that may already have published. The controller retains its
single operation slot until the helper exits; it does not impose the consent
deadline on filesystem publication. The public client waits at most an hour
for its completion read. Only a successful helper exit permits public byte 01
and private completion; failure returns byte 00. Missing completion is an
uncertain result and never causes automatic retry. Restart is explicit.

Before commit, Escape, requester loss or expiry cancels without launching.
On failed-generation teardown, the controller kills and reaps its direct
helper before secret-session cleanup. This alone cannot prove that the
helper's boot-transaction descendant is gone. td-svc's existing authority
cgroup owns every descendant and forbids a replacement generation until the
entire previous cgroup is empty; helpers never detach from that containment.
The boot transaction owns recovery of interrupted publication. No privileged
shell, setuid entry, remembered consent or new credential switch is added.
