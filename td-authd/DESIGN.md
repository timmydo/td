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
enabling that path. Its first operations are specified in "Elevation
operations", where `deploy-rollback` is live and the rest a target.

## Portal file preparation

The root-only `prepare-portal-files` startup operation exposes the fixed
human Downloads directory `/var/home/NAME/Downloads` at
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

Grant creation and jail admission share `primary_account.rs`: resolve UID
1000 from `/etc/passwd`, require GID 1000 and a canonical `/home/NAME` or
`/var/home/NAME`, and derive the physical home under `/var/home`. The
primary name starts with a lowercase ASCII letter and contains only
lowercase letters, digits, underscores and hyphens (at most 32 bytes).
All rows must have seven fields, unique names and UIDs, canonical decimal u32 IDs,
and absolute home and shell paths. The reader requires newline-terminated
UTF-8 without other ASCII controls, at most 64 KiB, 1,024 records and
4,096 bytes per record. Blank or malformed unrelated rows refuse too.
It refuses account-file symlinks, requires a root-owned regular mode-0644
file (also pinned by the image shape check), compares identity and metadata
after opening, and checks the read length against the opened size.
Configuration publication is trusted root work serialized before sessions;
this reader does not defend against a privileged concurrent file rewrite.
It reads no shadow data and grants no authentication authority. The stock
account remains `tester`; this lookup alone does not enable account rename.

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

## Portal handoff

The root-only `prepare-portal-handoff` startup operation gives the portal
one writable directory that Firefox, and only Firefox, reads: the files
OpenURI.OpenFile copies for Firefox to open (APPLICATIONS.md §E). The
directory is `Opened` in Firefox's private application home
`/var/lib/td/applications/UID`, with UID the active, ledger-admitted
Firefox assignment. Preparation creates it at mode 0700 owned by that UID
when absent, and otherwise requires exactly that ownership and mode. A
detached, nonrecursive idmapped clone maps filesystem Firefox UID/GID to
portal UID/GID 991 with nosuid, nodev and noexec, and is published at the
root-owned `/var/td-portal-files/1000/Opened`. The portal therefore sees its
own private directory and every file it creates is Firefox's on disk;
Firefox's read-only `~/Opened` grant resolves to the same directory at
`/home/td/Opened`. The human UID and the other applications have no
mapping and no grant, and the application home's mode 0700 keeps them out
of the source.

Every component is opened through a retained parent without following
links, as above. Repeated preparation accepts only the same directory at
the fixed view with rw, nosuid, nodev and noexec. It uses the existing
namespace helper with the one-entry map Firefox UID to 991, the existing
application mount attributes and the existing publication; no syscall,
flag value or caller-selected path is added. Firefox and the portal order
after this unit without requiring it: an unavailable view refuses OpenFile
alone, and the browser still starts. Only a missing or Firefox-unreadable
directory stops it, since the jail refuses a read-only grant without create
whose source it cannot use. An interrupted creation leaves a root-owned
directory, which the next preparation completes when it is still empty and
private, as application filesystem grants do; a non-empty or wider
root-owned remnant is refused and needs removing by hand. The
fixed root-only `release-portal-handoff` operation runs at shutdown beside
`release-portal-files`, with the same fixed-path unmount and checks.

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
permanently closes the channel, and a receive that fails, partway through
a frame or after all of it, first zeroes the bytes it read, since a request
may carry a PIN. There is no reconnect on an existing endpoint.
A frame whose completion this channel observes after its deadline is
refused even if the last bytes arrived before it: success is bounded by
observation, not kernel arrival time. This rule is the channel's own; the
login supervisor's last look at its operation deadline ("Login-key
operation supervision") concerns the private worker, not this channel. A send error may follow partial or complete delivery, as with
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

The Linux 7.2.13 source pins the relevant kernel behavior in `net/core/scm.c`
(`scm_pidfd_recv`) and `fs/pidfs.c` (`pidfs_init_inode`): the received
descriptor identifies the sending task and each pid owns its inode identity.
Polling the retained descriptor rejects death before any numeric PID could
be reused. The supported runtime is td's configured Linux 7.2.13 or newer,
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

`td-firstboot stage-primary-name ROOT NAME OUT` is a preparation-only
consumer of the same staged account validation. It validates name
collisions, complete membership and the resulting renamed tables before
writing a new private `OUT/etc`. It copies the reservation table unchanged
and preserves credential/lock fields and numeric IDs, changing only the
primary name, canonical home and matching group/member references. Its
files have exact passwd/group 0644, shadow 0600 and reservations 0444 modes;
its output root is 0700 and its `etc` directory 0755, independent of umask.
Both remain outside the live account view. Staging requires a
consistently owned parent without other writers and refuses an existing
output. The invoking uid/gid must match the source owner, and `/proc` must
be mounted for descriptor-relative access. Only a successful, synced
staging result may be consumed; failure
can leave a private partial output. This is neither enrollment nor live
account publication, and grants no session or authentication authority.
The caller must verify the source deployment and serialize any eventual
activation before account consumers start.

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
owner. The image invokes `td-firstboot provision --application-primary`,
which resolves the shared validated UID/GID-1000 account before any
provisioning write. Its canonical `/home/NAME` becomes the migration source;
missing or malformed account data fails without a fallback. The explicit
`--application-home DIR --application-owner UID:GID` pair remains available
for root configuration and fixtures, and cannot mix with the primary
selector. The current invocation configures one human home and refuses a
deployment with active applications for another owner. This is a
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

## Fixed program launch prerequisite

`terminal-serve --primary --peer-uid UID` is the image's root-configured
consumer of the private channel. It validates the peer UID before resolving
the UID/GID-1000 human through the shared primary-account reader. The
resolved name is retained for this generation's existing ledger check and
credential helper; no name or numeric human UID comes from the peer.
Missing or malformed account data refuses without a fallback name. The
named `--user USER --uid UID --peer-uid UID` interface remains available
for explicit root configuration and diagnostics; the two forms cannot mix.
Neither selector replaces root startup, sender pinning or ledger admission.
Primary resolution precedes the startup audit; its bounded account read
closes the file before the audit and retains no descriptor or child.
The image compositor runs at its reserved identity and uses this channel
for human terminals and the task manager. The configured application card
activates a supervised window; when there is none it asks root to start
the application's unit (APPLICATIONS.md §W.9). No card entry updates an
application: that is td-software's, through request `24`.
Direct compositor spawning remains a host-development mode.

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
executable, environment, directory path, account, uid or argument vector,
beyond `22`'s application name, checked in the registry grammar. A
typed program kind selects a fixed executable and, for terminals, either
the account home or td's fixed task worktree; the editor, photo tool,
review window, disk usage analyzer and coding agent start in the account
home, and the review window's one argument, the coding agent's three
variables and td-software's control-socket variable are fixed by the
authority. All
authority-spawned credential-helper children replace stdin, stdout and stderr
with `/dev/null`, clear the environment, and start from `/`. The task variant's
eventual td-term child enters only the fixed worktree described below. Replacing
stdin also replaces the original private endpoint on fd 0: relying only on
Channel's CLOEXEC clone would leak authority. All subsequently created channel
descriptors are CLOEXEC. The daemon never passes an inherited root log
descriptor to a user program.

Before validation both ends exchange the framed `TDLA003` protocol greeting
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
| `05` | retired (formerly the Codex agent); refused like any unknown request and never reassigned |
| `06` | retired (formerly Claude Code in the fixed task worktree); refused like any unknown request and never reassigned |
| `07` | `81` plus a process handle for the human task manager |
| `08` | `81` plus a process handle for the human text editor |
| `09` | `81` plus a process handle for the human photo tool |
| `0a` | `81` plus a process handle for the human code review window |
| `0b` | `81` plus a process handle for the human disk usage analyzer |
| `0c` | `81` plus a process handle for the human coding agent |
| `0d` plus an application name | `84` once root has started `/bin/td-svc start NAME` |
| `0e` | retired (formerly Firefox's forced update check); refused like any unknown request and never reassigned |
| `0f` plus an application name | `85` and the `NAME` unit's state as root last read it: `00` idle, `01` starting, `02` running, `03` failed, `04` unknown |
| `22`, optionally with an application name | `81` plus a process handle for the human application manager, td-software, opened at that application |
| `23` | `88` and which of the registry's Flathub declarations are installed ("Application requests") |
| `24` plus a verb byte and an application name | an application cache request ("Application requests"): `86` once an operation started, or for status `87` and the last record |

`0d` and `0f` name an application: the rest of the frame, one to 64
bytes in the registry grammar (anything else is malformed and ends the
generation). Root answers `ff 04` for a name its registry,
`/etc/td-applications.tsv` read as for `23` ("Application requests"),
does not hold, and `ff 02` while it cannot read the registry. The image
names each application's unit after it, which a recipe test holds:
every shipped application has a unit of its name that starts it, and no
other unit is one a peer can name. `0d` creates no handle. Root
runs the fixed td-svc client itself, with the same cleared environment,
`/` and null standard descriptors, holds at most one per unit in flight,
which another start of that unit joins (`ff 01`), and 16 in all (`ff 05`
for a start beyond them, which is refused), answers `ff 02` if it cannot
start one, reaps each at a later request, logs a client that failed, and
kills one still running after 12 seconds, as revocation bounds its own.
On a live boot, while `/run/td-media` exists, it answers `ff 03`: the
live table skips the application, and td-svc's `start` would run a
skipped unit. There is no rate limit beyond one in flight: `0d` restarts
a unit td-svc holds in backoff each time the last start finished. The
compositor already owns the session's input, so it gains no more than
repeating a click. The peer names an application root's immutable
registry declares, never a unit outside it: no unit grants the human
anything a launch does not, since the application runs as its own
account, and a launch fetches nothing (td-software/DESIGN.md §4.1).

`0f` lets the compositor say what became of a `0d`: it reads
`/bin/td-svc status NAME`, the fixed client with the same cleared
environment, `/` and null stdin and stderr, its standard output a socket
read without blocking, at most 256 bytes within five seconds. Root
follows one application at a time, the last a `0d` or `0f` named, and
what follows is of that application. At most one such client is in
flight, a `0f` starts one at most once a second, a `0d` included, and
only while no `0d` client for the application runs, and every later
launch request or heartbeat reaps a finished one, so a peer asking
faster gets the last read again and no more root work. The answer is
`01` while a `0d` client for it runs; `03` once one failed, timed out or
could not start, until a read sees the unit starting or running;
otherwise the last read since the last `0d`, or `01` until the first
read after one: `02` for `ready` with a live pid, `00` for `ready`
without one, `skipped` or `stopped`, `01` for `down`, `starting` or
`stopping`, and `03` for `failed` or `held`, each only from exactly
td-svc's status line for `NAME` (td-svc/DESIGN.md §8), ` leaf=unproven`
only after `stopping`. `04` is any other line, or, for the application
followed, no `0d` and nothing read yet. A busy, failed or late client
leaves the last read standing. A client that began before a `0d`, or
before a `0f` for another application, is reaped and discarded, so it
cannot answer for what came after it. The states are td-svc's, not the
application's: `02` says td-svc holds a live leader, which a launch
holds before any window maps, `03` includes a crash, or td-jail's
refusal of an application not installed, that td-svc's
`restart=on-failure` will retry after its backoff, and a
`0d` client killed at its bound reads `03` until a read sees the unit it
may still have started. A `0f` for another application than the one
followed starts over on it as a `0d` would, with nothing read and no
start asked but one of that application still in flight, so the
compositor asks about the launch it shows; a failed start counts as `03`
only for the application followed. On a live boot `0f` answers `ff 03`
as `0d` does. It reveals only the supervisor's state word for a unit the
compositor may already start, never the application's output, account or
files; it is generic launch feedback, not a download channel.

For a launch, a full table returns `ff 01`; a spawn failure returns `ff
02`. Every other request, trailing byte beyond a request's operand,
unknown handle, wait error on a launched terminal (a supervised client's
is logged), timeout or transport failure ends this channel generation,
including ordinary peer EOF. td-svc already counts either peer's exit,
even zero, as pair failure and applies its failure backoff. A malformed
request therefore ends the graphical session when this pair is enabled.
There is no retry authorization after a transport error. Successful
terminal creation means the fixed credential helper was spawned; polling
distinguishes a later credential or exec failure. A fresh 128-bit
kernel-random generation nonce avoids PID reuse collisions between
terminals that outlive their authority. It is a readiness-name
identifier, not a secret or authorization token. These dynamically
launched terminals own their readiness sockets for human-session
diagnostics; the compositor observes Wayland surfaces and does not probe
those sockets. The nonce therefore stays private to the launcher and
terminal, and no reverse traversal grant into the human runtime is
required. Each generation holds at most sixteen records, including
completions the peer has not polled. A completion response retires its
handle. Handles increase without reuse; exhaustion fails before spawn.
The peer must send a request or heartbeat within each five-second
receive deadline.

The authority runs `/bin/td-login exec-as USER -- /bin/td-authd
terminal-exec UID GENERATION HANDLE
[task|taskmgr|editor|photo|review|dua|agent|software [APP]]` in a new
process group. The optional literal selects requests `04`, `07` through
`0c` and `22`; it is not a pathname. `APP` follows `software` alone,
for a `22` that names one.
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

For request `07`, after that check it execs `/bin/td-taskmgr` with no
arguments, adding only `WAYLAND_DISPLAY=/run/td-compositor/UID/wayland-0`
to the verified human environment. It keeps the outer PID namespace and
ordinary human credentials, including kernel signal permission checks.
No authority descriptor, extra capability or elevation enters the program.
The program is a source-built system tool, not a jailed application.

Requests `08` and `09` are the same shape for the editor and the photo
tool the image ships: after the same checks they exec `/bin/td-editor` or
`/bin/td-photo` with no arguments and the same single added
`WAYLAND_DISPLAY`. Unlike the task manager, each starts in the verified
account home, derived through the shared primary-account reader as the
task terminal's worktree is, so its file dialogs begin there rather than
at `/`. No request carries a path, file or argument.

Request `0a` is the same shape for td-review, the one desktop program
with a fixed argument: it execs `/bin/td-review --choose-repo` from the
verified account home. Every repository td-review could open lies under
the human's `src`, and git honours a repository's own configuration
(hooks, fsmonitor, ssh command). This document earlier gave td-review no
request until a repository no application can write, or a git invocation
that ignores repository-configured commands, existed. Claude Code's
`src` view was then the application that could write one; it has left
td, and no shipped application's grant reaches `src` now. The condition
was withdrawn for another, which still holds: the human chooses the
repository each time, and nothing else does. The
request names none, since a fixed one would be opened on every press.
`--choose-repo` opens a chooser over the repositories td-review's window
opened before, kept in `~/.config/td-review/repositories`, outside every
application view, and a folder browser from `~/src` in which Return and
a double press only enter folders; a repository opens only on
Ctrl+Return in it or from the saved list. A folder's `git` mark is
state an application can set, so it never turns a navigation key into a
git run. The folder chosen must be a work tree's top through its own
`.git`, so a repository planted above it is not taken for it. Git then
runs in that repository as under `td-review -C`, and so with the same
exposure to configuration an application planted in it as a terminal
start has: the card adds a way to start, not a new way for planted
configuration to run. It differs from a terminal start in its
environment, td-login's alone: no ssh agent, `LANG` or
`XDG_CONFIG_HOME`, and standard streams on `/dev/null`, so a push that
needs an agent fails from the card where a terminal's may not.

Request `0b` is the same shape as `08` and `09` for td-dua, the disk
usage analyzer: it execs `/bin/td-dua` with no arguments from the
verified account home, which is therefore the directory it scans. It
reads the home and deletes as the human, on the human's keys in its
window, `D` without a question (td-dua/DESIGN.md, "Keys"). The home
holds trees an application view can write, such as `~/Downloads`, and
td-dua's deletion checks are path-based: a directory an application
swaps for a link between a check and its listing is listed through the
link, so the entries deleted can lie outside the tree the human chose
(td-dua/DESIGN.md, "Deletion"). A terminal start of td-dua has the same
exposure; the card adds a way to start, not a new way to delete. What
td-dua finds there is data it lists and measures, never runs.

Request `0c` is the same shape for td-agent, the coding agent's window
(td-agent/DESIGN.md), and the one desktop program with more than
`WAYLAND_DISPLAY` added: it execs `/bin/td-agent` with no arguments from
the verified account home, with `XDG_RUNTIME_DIR=/run/user/UID`, where
the fetch service's and the egress relay's sockets are, and
`TD_AGENT_JAIL=/bin/td-jail` and `TD_AGENT_TXT=/bin/td-txt`, the image's
programs its tools run through, each fixed by the authority; the request
carries none of them. td-agent runs as the human, outside application
confinement, as td-review does: it reads its API key from its own file
in the human's configuration, keeps its conversations in the human's
state directory, sends model requests through the fetch service, and
runs every tool a model asks for inside `td-jail --workspace`, which on
td serves this account alone (APPLICATIONS.md §C), under its approval
policy (td-agent/DESIGN.md §11); a workspace reaches the network only
through the relay, per connection, as its policy, the person, or in
`auto` the classifier allows. What a model writes reaches the human's
files through the directories a workspace grants, which the person
decides, and through td-agent's git worker, which runs outside any jail
as the human. That worker pushes to a local repository the person admitted
as a remote, running that repository's receive hooks as the human
(td-agent/DESIGN.md §7, "Local repositories"): an application whose
grant reached such a repository could plant or swap a hook that runs as
the human at the next push there, the class the `0a` paragraph names for
git's repository configuration. No shipped application's grant reaches
`~/src` now. Its environment is the card's alone, as `0a`'s
is: no ssh agent, so a remote that needs one fails from the card, and
no passphrase prompt, since td-pinentry runs only on a host. A terminal
start of td-agent with those variables has the same reach; the card
adds a way to start.

Request `22` is the same shape for td-software, the application manager
(td-software/DESIGN.md): it execs `/bin/td-software` with no arguments,
or with the one pair `--select NAME` below, from `/` as the task manager
does, since it has no file dialog, with
`WAYLAND_DISPLAY` and `TD_CONTROL_SOCKET=/run/td-compositor/UID/td-control`,
the compositor's control socket, fixed by the authority. Its requests
reach root only as the compositor's `24` ("Application requests"),
which any program of the human's on that socket can make as well: the
card adds a way to start, not a new reach. A `22` may carry an
application name, the rest of the frame, one to 64 bytes in the
registry grammar (anything else is malformed and ends the generation):
the launcher's Install row opens td-software at that application
(td-software/DESIGN.md §5). Root passes it after the `software`
literal; terminal-exec checks the grammar again, takes a name after
`software` alone, and execs `/bin/td-software --select NAME`. The name
selects td-software's own entry and nothing else: it names no program,
path or file, and td-software leaves its selection where it was for a
name its registry lacks, so root does not look it up.

An authority older than one of requests `07` through `0f` or `22`
through `24`, or than `22`'s name form or the named `0d` and `0f`,
closes the entire launch channel on it, as an authority newer than the
compositor closes it on a bare `0d` or `0f`; its paired compositor then
restarts. The image ships both peers atomically. Requests `07` through
`0b` were additive within TDLA002, without negotiation or mixed-version
compatibility. The login requests `1b` and `1c` came with the bump to
TDLA003, because `1c` carries a PIN; `0c` through `0f`, `22` through
`24` and `22`'s name were added within it later, and `0d` and `0f` took
their names within it. Both peers again ship together, without
negotiation.

For terminal requests it execs `/bin/td-term run --socket
/run/td-compositor/UID/wayland-0 --ready-socket
/run/user/UID/td-auth-terminal-GENERATION-HANDLE.ready`. The task variant adds
`--working-directory /home/NAME/src/td-vm/work`, deriving NAME through the
shared primary-account reader after checking the human credentials and
session cgroup. The ordinary variant keeps the verified account home.
No request carries a directory or account name. No shell command or
consent operation is involved. Directly invoking terminal-exec cannot change
credentials or enter a different session. Its membership check verifies
placement after the trusted credential helper; it is not human
authorization. Opening one's ordinary terminal is session behavior and opens
an ordinary shell with the human account's existing authority; it grants
neither store access nor an elevated shell.

A poll's `82 01` and `82 02` are the launched program's own exit status,
zero or not, uninterpreted; the compositor logs `launched program failed`
on its standard error for the second. What a status means is the
program's: td-term answers zero when its shell logs out, whatever the last
command's status, or when a close is asked for, and fails for a signal, a
shell that exits non-zero before anyone typed, a `--command` child's
failure or its own fault (td-term/DESIGN.md §4).

Launch is not authentication, settings synchronization, or build readiness.

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
The target producer runs these tests with td's Rust toolchain. No test or
terminal-launch success is evidence of FIDO2 authentication or consent.

`td-authd/tests/launch_vm.rs` is a standalone disposable-VM fixture. Compile
it with host rustc for an installed static target, then run its binary with
`--run-vm KERNEL AUTHD FIRSTBOOT LOGIN TD-INIT NEW-LOG`, using absolute
paths. The supplied production binaries may be source-built target outputs;
the fixture itself is a host diagnostic, never an input to a target recipe
or part of an image. It requires td's pidfs-capable kernel and QEMU on the
host. Its primary account is `alice`, exercising `--primary` and a
nondefault name through the real channel-to-validator-to-credential-helper
chain. Missing and duplicate UID-1000 records refuse before channel
admission. It verifies the
terminal's uid/gid, empty capabilities, independent process
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

The root startup operation `application-start OWNER APP direct --
ARG...` consumes an installed application assignment. OWNER is currently
1000, the image's single graphical session; APP has the canonical registry
name grammar. The presentation and at most 128 literal arguments (32 KiB
including terminators) come from root-owned unit configuration. Direct
presentation has no request listener, caller-selected executable,
credential change in td-authd, or human elevation. Stock application
units use this operation after firstboot and, where needed, mapped-grant
preparation succeed.

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

The wrapper constructs only `/bin/td-jail` with APP as its argv[0], which is
how td-jail selects the application (APPLICATIONS.md §A.0), so the launch
needs no `/bin/APP` link; td-jail refuses a by-name launch under any UID
other than the one the deployment assigned APP. It supplies the canonical
private HOME and runtime, service USER/LOGNAME, `/bin/false` SHELL and
`/bin` PATH, passes literal application arguments without a shell and sets
no compositor control endpoint; td-jail's product resolver fixes the
display endpoint. There is no terminal presentation: td-term's `--command`
cannot set its program's argv[0], so it could not name an unlinked
application. `direct` is the only presentation word.
The bounding capability set is not a held privilege and is not required
empty. No new syscall or unsafe surface is
introduced; confinement pins the complete application controller and startup.

## Application cache operation

The root operation `application-cache OWNER APP VERB` changes or reads
one Flathub application's cache as that application's account
(td-software/DESIGN.md §3, APPLICATIONS.md §W.9). OWNER is 1000; APP has
the registry name grammar; VERB is exactly `install`, `update`,
`remove`, `delete-data` or `status`. No caller supplies an executable,
UID, path, ref or flag. td-software's requests reach it through the
compositor's private channel ("Application requests" below).

Startup requires root credentials, one thread, no controlling terminal
and only the three standard descriptors. Unlike the launch authority's,
any of them may be the same file, since this operation's stdin is no
channel. Root then runs the fixed `check-launch-application OWNER APP`
admission, which selects the active application account from the
immutable deployment and the durable ledger, as `application-start`
does. An application the image ships has no cache: `status` answers
`format=1`, `name=`, `kind=image`, written by root without td-app, and
every other verb refuses.

For a Flathub declaration root runs `/bin/td-login exec-service-as
tdaUID -- /bin/td-app VERB APP`, with td-app's verb `update APP --force`
for both `install` and `update` (they differ only in whether a `current`
exists, which td-software shows) and the verb's own name otherwise. The
environment is cleared, the working directory is `/` and stdin is null;
td-login supplies the account's home, its other identity variables and
its session cgroup, as for an application's launch. For `status`
td-app's stdout is a private socket and its stderr null; root reads at
most 4 KiB, requires EOF and a successful exit, parses the record with
`flathub_app::Status`, requires it to name APP with `kind=flathub`, and
prints its canonical rendering, which is exactly what it parsed. A
record's name has the registry grammar, so root's own `kind=image`
record exists for every registry name. For the other verbs stdout is
null and stderr the socket; root keeps only its last 512 bytes, and on a
failed exit reports the last line as printable ASCII in its own
diagnostic. A failed `status` reports only td-app's exit. Either failure
exits 1; success of a verb other than `status` prints nothing.

Each run is bounded: an hour for `install` and `update`, a download of
about a gigabyte on a slow link and the validator over it, ten minutes
for `remove` and
`delete-data`, and 60 seconds for `status`, whose deploy-size walk runs
on every poll. At the bound root kills and reaps td-app; td-app's cache
lock and generation leases make a killed run safe to repeat. The bound
holds only while the operation lives: root carries no lifetime into
td-app, so a caller that kills the operation early leaves td-app to
finish its run under its own lock, and the caller must not, except as
a paired generation's end does ("Application requests"). td-app's
lock blocks rather than refuses, so a second request for the same
application waits behind a running check: a `remove` or `delete-data`
issued during an hour's install can reach its ten minutes first and
fail without changing anything. Request `24` therefore runs one
operation per application at a time and starts one only on a recent
`status` that shows no writer. The operation reaps td-app with a
blocking wait, as `application-start`'s admission child does; a serving
authority must run it as its own process rather than in its loop.

None of these is elevation (td-software/DESIGN.md §3): each runs the
application's own account on its own cache or state, which a first
launch and the forced check did before td-software's cutover
(td-software/DESIGN.md §6.4). The confinement test pins the
one program, its argument shape, the order of parse, startup audit,
admission and spawn, and no environment beyond the cleared one.

## Application requests

Request `24` is how td-software's requests, which it makes on the
compositor's control socket (td-compositor/DESIGN.md §15, "Application
requests"), reach the operation above. Its operands are one verb byte,
`00` status, `01` install, `02` update, `03` remove or `04` delete-data,
and the application name, the rest of the frame: one to 64 bytes in the
registry grammar. Anything else, an empty name included, is a malformed
request and ends the channel generation, as every other one does. The
peer names nothing else: root runs `/bin/td-authd application-cache
OWNER APP VERB`, with OWNER the configured human, through the same
cleared spawn as every authority child (null stdin, `/`, no
environment), and that operation's own admission is what checks the
name against the immutable deployment and selects the account. A name
the deployment does not hold fails there, and the failure is reported
like any other.

Root runs it as a process of its own and never waits on it in the
serving loop, which is the operation's obligation above: the answer
comes at once, and every later launch request or heartbeat reaps what
has ended. For each name root holds at most one operation (install,
update, remove or delete-data) at a time and at most one status read,
so it sends one request per application at a time as the operation
requires. It starts an operation only on its own last read of the
name's status, ended in the last 30 seconds: a second operation while
one runs, or one whose read shows a writer root did not start since
(an oracle's install, td-jail's offline revalidation at a launch or a
previous generation's operation, whose lock the new one would wait on),
is refused with `ff
01`; with no such read it is refused with `ff 05`. Either refusal
starts a read, so the next request finds one; td-software asks a status
far more often than that. A recent read that failed admits the
operation: td-app cannot read a damaged cache's status, `remove` is
what clears one, and the operation's own bound covers a wait on a lock
the read could not show. It never kills an operation early: an
operation is stopped only once it has outlived its own bound by a
further minute, which the operation, killing td-app at its bound, never
does, and an unreadable output ends only the reading. One that exits
with its output held open by a process it left is taken as ended two
seconds later, its output then possibly cut: a status is then no
record, and another operation is reported as failed with the tail it
had. A generation keeps at most 16 names, forgetting the least recently
named one with nothing running and no request for two seconds to make
room, and refuses with `ff 04` when none is: so a peer cycling names
starts no more reads than 16 kept names allow. `ff 02` says root could
not start the operation, and on a live boot, while `/run/td-media`
exists, every `24` answers `ff 03`, as `0d` does.

An operation answers `86` once started. A status answers

```text
87 OP LAST RESULT LENGTH REASON STATE [RECORD]
```

`OP` is the operation running now and `LAST` the last one that ended,
each its verb byte (`00` is none, a status never being one); `RESULT` is
`00` when it succeeded or none ended and `01` when it failed, when
`LENGTH` (1 to 200) printable ASCII bytes of `REASON` follow: the
operation's last diagnostic line without its `td-authd: ` prefix, as
`reason_text` makes it printable. `STATE` is `00` while no read has
finished, `01` and the record when the last read began no earlier
than the last operation ended, `02` and the record when it began before
(a new read is then under way), and `03` when the last read failed. The
record is exactly what the operation printed, which root parses again
with `flathub_app::Status`, requiring it to name the requested
application and to fit the frame beside the header; one that does not
reads as `03`. A status read starts when a status request finds none running and
none begun in the last two seconds, or an operation has ended since the
last began: each read walks the generations' deploys, so a peer polling
faster is answered from the last read and costs no more walks.

Request `23`, with no operand, is the installed set the compositor's
launcher follows (td-software/DESIGN.md §5). Root answers at once from
the same entries as `24`'s status; it writes no file and reads only the
registry:

```text
88 COUNT (STATE LENGTH NAME)*
```

one row for each of the registry's Flathub declarations, in its order,
`NAME` its `LENGTH` (1 to 64) bytes. The registry is
`/etc/td-applications.tsv`, the image's immutable table, read once per
generation in the builder's bounds (1 MiB, 256 lines, no name twice);
a registry root cannot read answers `ff 02` and is read again by the
next request. Root keeps only the names in the registry grammar, the
grammar every request names an application in: the builder's identity
grammar is wider (APPLICATIONS.md §A), and a name outside it is one no
request can name. A declaration is one `flathub_app::FLATHUB_SOURCES`
names, compiled into root; a test holds them to 8, half the 16 names
root follows, so the set never fills what root follows for td-software
and the answer fits a frame. A registry with more answers `ff 02`
rather than leave one out, since leaving one out would say the image
ships it. A registry name the answer leaves out is one the
image ships, always installed. `STATE`'s bit `01` says the last read of
that name's status found a `current` generation, which is what
installed means; bit `02` says that may change soon: no read has
finished, an operation runs on it, or the last read, whether it was
read or failed, began before the last operation ended. A failed read
begun after it is settled and not installed. Each
row follows its name as a status request would, starting a read when
one is due and never more often, so asking costs no more reads than
`24`'s status; with no room among the names root follows, a row is
`02` and starts nothing. On a live boot `23` answers `ff 03`. Each row
due starts its own read, so a `23` asked every interval runs up to one
`td-app status` per declaration each interval, as many as td-software
polling each would.

The serving loop's work per request is bounded: it reads the registry
(at most 1 MiB) once per generation, or again on each request while it
cannot, at most 16 KiB from each running child, keeps a status record
only up to the record's 4 KiB bound and an operation's diagnostics only
as their last 512 bytes, and blocks only to reap a child it has just
killed. Its rate is not: a peer can keep 16 names' operations and reads
starting, each a root `application-cache` process, as fast as the
control socket serves it, which costs the human's machine no more than
any program the human runs. A paired generation's end, by either peer's
exit, ends what root ran for it with the pair's cgroup, as a kill would:
td-app, in its account's session cgroup, finishes its run under its
lock, and td-app's lock and leases make the run safe to repeat. The next
generation knows nothing of it but what td-app's status shows. None of
this is elevation, for the operation's reason above; a human program
reaching the control socket can run these operations, which
td-software/DESIGN.md §3 accepts.

## Application filesystem grants

Root startup prepares two writable idmapped views: Firefox and mail
share human Downloads, resolved through the same primary-account reader as
portal preparation and jail admission, each below the assigned
application's private home. `prepare-application-files APP` and the matching
shutdown operation accept only those two installed names. Active-account and
durable-ledger admission select the UID; no caller supplies a path or map.
The existing root namespace helper maps filesystem human UID/GID 1000 to
that application identity. Mount attributes require nosuid, nodev and noexec;
a cloned read-only source remains read-only and refuses writable admission.
The portal's separate read-only Downloads view is unchanged; its writable
handoff view maps the other way, Firefox's identity to the portal's
("Portal handoff").

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
the human, the live installer wizard's UID 990 and assigned application
UIDs; audio admits the human, its own
service and the assigned Firefox UID. Compositor control and
readiness remain human-only, and the private portal channel remains UID 991.

Mail and news fetch daemons use their own service accounts and mode-0700
runtime directories. Credential-bearing fetch requests therefore remain
inside the application's UID domain. The human fetch service is separate.
Production application units use the typed root launcher with null
application stdio.
Bus evidence connects as the human observer and independently checks the
subject application UID against the immutable policy. An unjailed service
UID retains launcher filtering and is not an unrestricted bus observer.

The grant preparer may create the declared missing human directory,
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

`consent.rs` supplies a bounded, immutable public description for the
trusted renderer. The canonical wire value is `TDCONS02`, a nonzero 32-byte
operation nonce, a big-endian u32 human UID, and a one-byte operation.
Enrollment (1) adds platform profile 1 (TPM PCR 7), the recovery policy (1
second token, 0 explicitly unrecoverable, matching TDENROL1) and step (1
create primary, 2 prove primary, 3 create recovery, 4 prove recovery). An
unrecoverable request refuses either recovery-token step. Unlock (2) adds
role 1 primary or 2 recovery. Credential write (4) adds role 1 primary or 2
recovery, then big-endian u32 application and requester UIDs, then a
one-byte length and ASCII bytes for each application and credential name.
The reserved whole-disk description (6; preserve form 38), no longer
issued by td-authd, adds a big-endian u32 requester and u64 capacity
in bytes, then one-byte-length fields for the disk's kernel name (1 to 31
ASCII alphanumerics, hyphen or underscore), its model and serial labels, the
hostname (1 to 63 bytes) and the username (1 to 32), then the first eight
bytes of the deployment ID and one storage byte, 0 unencrypted or 1
device-bound, any other refused. A label's length byte is 0xFF when the
disk reported none, and otherwise its length (at most 32 for the model, 24 for
the serial) with bit 7 set when it was cut. Its bytes are the canonical
escape of what the disk reported: printable ASCII other than space and
backslash as itself, backslash as `\\`, space as `\s`, every other byte as
`\xNN` in lowercase hex, cut before the first escape that would pass the
width; a cut label has no room left for another escape. Hostname and
username are printable ASCII without space or backslash, and the requester
must equal the owner. The widest value is 253 bytes. The whole value is at
most 256 bytes; unknown tags, truncation and trailing bytes refuse. Tag 3,
a retired write form, stays unassigned.

Login-key operations ("Login keys and session lock" below) take tags 7 to
10: session unlock (7), first enrollment (8), key addition (9) and key
removal (10). Each adds a big-endian u32 account, which must equal the
owner, then the key count before and after the operation, one byte each.
Unlock has 1 to 8 keys and the same count after. Enrollment has 0 before
and 1 or 2 after, then one byte naming the new key the step concerns, 1
through the after count. Addition has 1 to 7 before and one more after.
Removal has 1 to 8 before, then a one-byte count of 1 to 8 and that many
slots, and after is before less that count. A slot is its 1-based
position in the record's canonical slot order (one byte, at most the
before count), which the key-management screen's digits name, then its
fingerprint; positions strictly increase. A fingerprint is the first four
bytes of the credential ID's SHA-256, so two slots may share one, and the
position tells them apart: the authorize step's client-data hash binds the
record digest, which fixes the order. Then one step byte and the step's
fields:

| Step | Byte | Fields |
| --- | --- | --- |
| identify | 1 | none |
| authorize | 2 | fingerprint, retries |
| create | 3 | retries |
| prove | 4 | fingerprint, retries |
| repeat | 5 | fingerprint, retries |
| probe | 6 | fingerprint |
| unlock | 7 | fingerprint, retries |
| connect | 8 | none |

Steps 1 to 7 are the client-data phase bytes of `td-login/TOKEN-LOGIN.md`,
which reserves 8 for connect. Connect, in which the person connects the
next new key alone, sends no assertion and has no client-data phase: it is
the presentation root derives before a create step, whose retries only
the connected key can answer. Identify asks nothing of the person; probe
asks no PIN, but its no-PIN assertion asks for a touch, which its rows
say (`KEEP IT CONNECTED AND TOUCH IT`). Retries, on
exactly the PIN steps, are one byte: the key's own getPINRetries answer,
shown as the key's claim; the codec takes any value and admission refuses
zero, since the worker reports a key with none left as PIN blocked instead
of presenting a PIN step. A step's fingerprint names the identified
baseline slot (authorize, unlock) or the credential just created (prove,
repeat, probe). An identified fingerprint is the key's own answer to the
silent identify probe: it shows which enrolled credential the key
claims, not proof that the connected key holds it, which only the
following signed assertion gives. The only orders are
identify, unlock for unlock; identify, authorize for removal; identify,
authorize, connect, create, prove, repeat, probe for addition; and connect,
create, prove, repeat, probe for each new key of an enrollment in turn. A
step outside its operation's list refuses. The widest login value, a
removal of eight keys at its authorize step, is 98 bytes.

Step admission has two parts. The descriptions alone decide a step's
shape, which a reader without the record or the device can check, as
the compositor's private client does (`td-compositor/DESIGN.md`,
"Login-key operations"); root's record and the
worker's report decide its fingerprints. `Request::login_start` is the
first presentation's shape: a login operation's first step (identify, or
connect for an enrollment's first key, never a later key's).
`Request::login_successor` is a next step's: it refuses unless the next
description keeps the nonce, owner, operation, account, counts and
removal slots; its step is the one legal successor, an enrollment's probe
moving to the next key's connect; a repeat or probe names the credential
the step before it named; and a PIN step's retries are not zero. It
answers `Last` when that step is the operation's final one (unlock's
unlock, removal's authorize, the last new key's probe), after which every
step refuses, and `Next` otherwise. A step either admits may still be
one root refuses. Both see one step and its predecessor, so a rule that
spans further is the reader's own: in an addition the new key's prove,
repeat and probe never name the key that authorized it, which root
derives from the baseline and a reader without it must remember from
the authorize step.

`Request::begin_login` builds root's first presentation of a login
operation: `login_start`, then the baseline, as below.
`Request::admit_login_step` is root's pure check on a worker's next step,
amendment 4 below: `login_successor`, then the device data. The baseline
is the presented record's slot fingerprints in canonical order; the
created fingerprint is the credential the worker reported creating in the
current key's ceremony. It refuses unless the baseline holds the before
count (none for enrollment) and each removal slot's fingerprint at its
position; an authorize or unlock fingerprint is in the baseline; a prove,
repeat or probe fingerprint is the reported credential, which no baseline
slot names; and no credential is reported before prove. Its answer is the
successor's; the operation's commit is legal only after `Last`. Root's
supervision (`login.rs`, "Login-key operation supervision") calls
`begin_login` and `admit_login_step`, and the worker
(`td-secret/DESIGN.md`) applies the same admission to each step before
presenting it. `LoginStep::asks_pin` names the PIN steps,
`Request::is_login` and `Request::login_step` read a description, and
`Request::login_ceiling` is the operation's deadline, amendment 6:
`LOGIN_CEREMONY` (120 seconds) per key ceremony, `LOGIN_TWO_CEREMONIES`
(240) for two, plus `LOGIN_READING` (120) for an operation that
discloses; `LOGIN_TWO_KEY_ENROLLMENT` (360) is an ordinary two-key
enrollment's, and `LOGIN_LONGEST` (480), a two-key enrollment by disk
proof (amendment 10), the largest. Root bounds its
operation by these constants until its first step, then by that step's
`login_ceiling`, which the worker takes too, and reads a step and
whether it asks for a PIN through these accessors.

A disk proof's descriptions (amendment 10, tags 13 to 15) carry
`Operation`'s `proof` in every step, `Request::login_proof` reads it, and
three entries admit them. `begin_login` refuses a proof, so root begins
one with `Request::begin_proof`: `begin_login`'s rules, then the proof's
first step one the storage worker's baseline offers (`DiskProofs`: a
`recovery-key` marker for the recovery-key step, a fido2 token for disk
identify). `Request::admit_proof_step` admits the storage worker's next
step as `admit_login_step` does, but against that baseline: a disk
authorize step's key names one of the header's fido2 tokens, never a
login slot, and `admit_login_step` refuses every proof step.
`Request::begin_proven` is the login worker's start rule: the proof's
last step of the first key, its retries not zero, against the worker's
own record, answering `Last` for a removal, whose commit follows it, and
`Next` otherwise. A proof step's key ordinal is always 1, since the
proof is made once, before an enrollment's first key.

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
credential names, and spaces and backslashes are escaped or excluded in a
disk installation's labels and names: indented continuation text cannot
imitate the fixed labels or token instructions.

A disk installation shows its disk name, size in decimal GB and exact
bytes, model and serial (`NOT REPORTED`, `REPORTED EMPTY` or `(TRUNCATED)`
where they apply; each marker holds a space, which a label always escapes),
that all data on the disk will be lost, the hostname, user and deployment
ID prefix, its storage with automatic login (`UNENCRYPTED STORAGE,
AUTOMATIC LOGIN` or `ENCRYPTED TO THIS COMPUTER, AUTOMATIC LOGIN`, the
latter device-bound storage; login is the v1 plan's fixed policy), and
`ENTER: ERASE AND INSTALL   ESC: CANCEL`. The
widest summary, with its time line, is shown whole on a 1024x768 output; a
smaller output that cannot hold every row refuses, as for any request. The
summary is display only and does not bind the installation plan: the
operation that presents it must bind the plan itself. The plan admits
longer disk names (64 bytes) and backslashes in names, which this summary
refuses, so the producer must narrow those or refuse the plan before
consent; a kernel disk name is at most 31 bytes. "Whole-disk installation
intake" below presents it. All consumers pin the codec source and its
tests assert the complete public argument display.

A login prompt shows the operation with its key count as `(BEFORE ->
AFTER KEYS)`, an enrollment's `NEW KEY K OF N`, the account UID, a
removal's slots as `POSITION:FINGERPRINT`, two to a row under `REMOVE:`
with continuation rows aligned beneath, and `LOGIN WILL NOT NEED A KEY`
when a removal leaves none. Every login row is at most 34 columns
(`PROMPT_COLUMNS`), the renderer's width at its narrowest accepted output
of 320 pixels, so wrapping never splits a fingerprint. Then the step:
which key to connect or remove, or the step's fingerprint in lowercase
hex, and on a PIN step `N PIN ATTEMPTS LEFT ON THIS KEY` and `ENTER ITS
PIN, THEN TOUCH THE KEY`. Unlock alone omits its count, which it does not
change and which its bytes still bind: the lock surface shows the prompt
to whoever is at the machine. A disclosure step's rows end with its
disclosure, whose rows include `LOGIN WILL NOT NEED A KEY` for a removal
leaving none, then the approval rows, which replace the generic Escape
row ("Elevation operations").

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

## Physical console power

The admitted compositor's private session request `21 00` asks for restart;
`21 01` asks for power off. Other lengths or values refuse. The compositor
requires a fresh physical choice on secure attention, then a visible named
confirmation and fresh Enter timestamped after its frame completed; Escape cancels before the request. No public
socket, application request, account password or token enrollment is involved.
This authorizes only physical console power, never a credential operation.

Root requires a prepared, idle session and refuses while revocation is
restarting. It runs only `/bin/td-svc reboot` or `/bin/td-svc poweroff`, with
empty environment, `/` cwd and no inherited channel. The existing bounded
inspection helper caps stdout at 128 bytes and the exchange at two seconds,
then reaps within 50 ms. Only successful exit and the exact supervisor
acceptance for the chosen verb count as success. Reply `a1 00` means accepted,
`a1 01` busy/unprepared, and `a1 02` failed or uncertain. The client claims the same atomic execution decision as other commits before
sending; cancellation that wins first sends nothing. There is no automatic
retry: a lost reply may follow acceptance. The ordinary supervisor performs
ordered service teardown, sync and unmount before the power applet.

### Suspend to idle

Physical secure attention also offers Sleep. Request `25 00` reports that
no login lock was required; `25 01` carries the paired compositor's receipt
that the enrolled/unavailable session's lock frame reached the display.
Only these exact frames start a suspend. Root requires the same prepared,
idle slot as console power and refuses during revocation restart. For `00`,
it reads the login predicate afresh and refuses any enforced state; the
live medium is unenrolled as for `1a`. The compositor is the trusted source
of presentation receipts; no public socket admits this request.

Root starts the fixed `/bin/td-authd suspend-system` helper through the
existing bounded inspection launcher, with empty environment, `/` cwd,
null stdin/stderr and private stdout. The helper requires the existing root,
single-thread, no-controlling-terminal and standard-descriptor admission.
It holds an exclusive root-only `/run/td-suspend.lock`, requires `mem` and
`s2idle` in the kernel's power interfaces, selects `s2idle`, reads the bounded
`wakeup_count`, writes that same canonical number back and writes `mem` to
`/sys/power/state`. Each sysfs write is one write, with short writes refused;
a wakeup-count failure refuses entry without retry. No caller supplies a
path, mode or command. Hibernation remains disabled.

Reply `a5 00` means pending, `a5 01` busy/unprepared and `a5 02` refused or
failed. Exact request `26` polls, returning `a5 00` until the helper is reaped,
then `a5 03` only for successful exit with exactly `suspend returned\n`, or
`a5 02` otherwise. Polling with no pending helper fails. Completion releases
the slot; until then no other slot-taking request is admitted, and revocation
waits. The helper has 60 seconds of monotonic time (Linux excludes time
asleep); timeout kills it and the slot stays held until it is reaped. Peer
teardown kills and reaps it before generation cleanup, as for inspection.
The client polls through ordinary bounded channel exchanges and never
resubmits an uncertain request. Successful return says the kernel's entry
write returned, not that hardware reached its deepest sleep state.

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
for the configured human UID 1000. Its TDLA003 protocol has the exact
requests below; the existing terminal client does not send them yet.
No new socket or public listener is created. Only the pinned compositor
process may send these records. None but `1c` contains a credential,
and none contains a token assertion, private worker round,
caller-selected executable or path. The login requests `1b` and `1c`
("Login-key operation supervision" below) share this Session, and `1c`
carries one PIN.

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
and successfully relocked; `0b` to `0e` are a login operation's. Idle does not assert locked state: a
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

Five fresh, diskless guests exercise the existing root-only cases:
public `td-secret set` descriptor intake for both token roles, refusal of
nonhuman callers, preparation and generation-exit relocking, read-only
store inspection, cleanup after the production worker refuses, and the
login worker's supervision (`supervise-login`, "Login-key operation
supervision" below). Each
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

The same command then runs eleven fresh, diskless, TPM-free guests of
td-secret's login-key worker over UHID virtual keys, every non-power-cut
worker case TOKEN-LOGIN.md's increment 2 lists, with the same bound,
selection and passing summary (`td-secret/DESIGN.md`, "Login-key worker
guests"), and one more booted twelve times on a disposable disk, ten of
them killed by the host inside or just after a record write
(`td-secret/DESIGN.md`, "Login power-cut guests"). They play root
themselves. Another TPM-free login guest, `login-desktop`, runs this
authority's `terminal-serve` paired with the production compositor
over a record such a worker enrolled. It exercises the authority's `1a`
answers, its supervision of the worker through an unlock, and its
starting no worker when the chord on a damaged directory's lock surface
sends nothing (`td-secret/DESIGN.md`, "Login desktop guest").

The optional `qemu-secret --tpm /absolute/path/to/swtpm` mode adds four
TPM device cases and the eleven HID guests `td-secret/DESIGN.md`
describes to those checks, fifteen more guests. It uses the existing
pinned host emulator and private raw fixture disks, with cold reopen and
changed PCR/different-TPM refusals specified there. These additional
tests do not replace the authority's token-consent boundary.

## Consent for a locally built system

`deployment.rs` owns the stock update intake at
`/run/td-authd/1000/install`. On an installed system Prepare creates it
after the credential intake has checked the root-owned runtime parents; a
live boot binds "Whole-disk installation intake" instead. It has the same
protected-parent, mode-0600 human socket, per-fragment UID and live
sender-pidfd policy, but refuses every incoming SCM_RIGHTS descriptor. It
uses the unchanged named transport in UNSAFE.md section 16. No public
message grants consent.

The client `td-authd request-update SOURCE DEPLOYMENT-ID` requires a root
peer and the exact eight-byte `TDUPD01` newline greeting. Its u16 big-endian
body length covers exactly 64 lowercase hexadecimal ID bytes followed by a
UTF-8 absolute path of at most 4096 bytes. NUL and parent traversal refuse.
Root pins the final directory without following a symlink, requires its
owner to be UID 1000, and checks a bounded requester-owned regular manifest
through that descriptor against the ID. Since L5 the queue has the
hostname intake's durable backoff ("Elevation operations", Backoff):
before the source is read a running backoff refuses; after the checks
the principal table must grant the requester, the owner,
`deploy-publish`, as the hostname intake's must grant `set-hostname`;
then root writes the queue's count before sending admission byte 02.
A refused whole frame is answered as the hostname intake's is: 03 while
the backoff refuses, 04 for any other reason (a source or manifest that
does not check, a table that does not grant the requester or cannot be
read, or a backoff root cannot read or write); a malformed
frame is closed without one. The client prints which, or that the update
is ready and how to review it.
Intermediate path lookup and regular-file reads are synchronous filesystem
operations; the byte/deadline bounds do not promise latency on a stalled
filesystem. This queue cannot execute the source or open an arbitrary root
command. One client, one accept and four nonblocking attempts per heartbeat
bound socket work. Admission expires in five seconds; a sent admission
receipt starts a sixty-second selection window. Extra traffic, foreign
senders, descriptors, disconnect or expiry retire the pending request.

Only private request 19 selects a queued installation, the operation
`deploy-publish` (APPLICATIONS.md §L.1). A busy operation slot answers
99 00 first. Root then reads the principal table again, as admission
did, and answers 99 02 unless its `deploy-publish` row grants the
session owner, the only requester the intake admits ("Elevation
operations"), leaving the request queued; then no ready request answers
99 00. On an enrolled or unavailable machine, a queued deployment that
cannot read the login record answers 99 01 and is retired with
completion byte 00 ("Login keys and session lock", amendment 8). A
selection holds the directory File, draws a fresh 32-byte nonce from
`/dev/urandom` and answers 92 DESCRIPTION. The description is consent
tag 5: owner, nonce, requester UID and full deployment ID. It shares one operation slot with secret operations
and read-only inspection. The root installation controller accepts exact
presentation (13) and then one exact commit (14), within 120 seconds of
selection, each carrying the description. The compositor sends commit
only after Enter approves the fully presented prompt
(td-compositor/DESIGN.md, "Elevation consent").
Root rechecks the submitting process immediately beforehand. Keyboard
confirmation authorizes this installation only; it cannot release
secrets or authorize protector changes.

At commit root consumes the confirmation, then clears the queue's
backoff; a backoff it cannot clear fails the installation before
anything starts. It then spawns only `/bin/td-update apply-operation
ID`, with an empty environment, cwd `/`, the held directory File on
stdin, null stdout and authority stderr; a backoff it cannot clear is
said there. The helper's independent admission and transaction are
specified in td-install/DESIGN.md. No key, command, device or mountpoint is
selected by a requester. The confirmation is consumed even if spawn fails.
Closing the screen or losing the public client after commit cannot revoke
an installation that may already have published. The controller retains its
single operation slot until the helper exits; it does not impose the consent
deadline on filesystem publication. The public client waits at most an hour
for its completion read. Only a successful helper exit permits public byte 01
and private completion; failure returns byte 00. Missing completion is an
uncertain result and never causes automatic retry. Restart is explicit.

Before commit, Escape, a press that ends the prompt, requester loss or
expiry cancels
without launching, and the request has counted.
On failed-generation teardown, the controller kills and reaps its direct
helper before secret-session cleanup. This alone cannot prove that the
helper's boot-transaction descendant is gone. td-svc's existing authority
cgroup owns every descendant and forbids a replacement generation until the
entire previous cgroup is empty; helpers never detach from that containment.
The boot transaction owns recovery of interrupted publication. No privileged
shell, setuid entry, remembered consent or new credential switch is added.

Host tests drive the queue's backoff (a request admitted only once
counted, 03 within the backoff with nothing written, 04 for a manifest
that does not check or a table that does not grant the requester,
uncounted, or for a backoff root cannot read or write, until a write
succeeds), each description's fresh nonce, request 19's 99 02
for a table without the owner's `deploy-publish` row or none at all,
before the queue is touched, and only a commit clearing the count,
before the helper starts or instead of it. Confinement pins hold the
queue's backoff row and table, the grant and then the write ending
admission, the reply byte, the clear before the spawn, and the table
before the queue in request 19. `qemu-update` (td-update/DESIGN.md)
reads a booted system's prompt, shows a cancelled request refusing the
next inside its window, and approves with Enter.

## Whole-disk installation intake

On a live boot td-authd installs disks and never updates itself. Prepare
reads `/proc/cmdline` (a longer one than 4096 bytes fails Prepare): exactly
one `td.live=` token, `td.live=1`, marks a live boot; none marks an
installed system; any other spelling or a repeated token is ambiguous and
fails Prepare. A live boot binds `disk_install.rs`'s setup intake at
`/run/td-authd/1000/setup` in place of the update intake; an installed
system binds only the update intake. Both use one binder: protected
root-owned parents, a stale socket of root or of the socket's owner
replaced, mode 0600, owned by that owner, nonblocking, and on failure or
teardown the inode this generation created removed and no other. The update
socket's owner is the session owner, UID 1000; the setup socket's is the
live wizard's service identity, `tdi1000` (UID/GID 990).

The setup intake accepts one connection per heartbeat. While no service runs
or awaits reaping and no installation has completed this generation, a peer
of UID 990, the wizard's (the fixed peer-UID query), gets a service; any
other connection is closed unanswered. A peer of the session owner cannot
connect at all, the socket being mode 0600 and the wizard's. The service is
`/bin/td-install serve /bin/td-boot
/run/td-media / /bin/td-firstboot`, with an
empty environment, cwd `/`, the accepted socket as stdin, one end of a new
socketpair as stdout and authority stderr: td-install/INSTALLER.md
"Installation service core" says what it admits, including the supervisor's
orderly reboot or power-off while its installation is complete. td-authd
keeps the other end, nonblocking, and speaks td-install/INSTALLER.md
"Installation consent channel" on it, compiling the service's own codec,
with at most four reads and four writes per heartbeat. It sends its
greeting, requires the service's, and holds at most one open review from its
review report to the service's ended or finished report, admitting started
only after it sent consent, finished only after started, and ended only
before started. Reports read before the channel ends count. A report out of
that order or a malformed frame breaks the channel, as does a failed write
(owed bytes mean the service has not started), and td-authd kills the
service. The service closing the channel, or having exited, once everything
it sent before exiting is read, retires it unsignalled, so a started
installation is not interrupted. Either way its open review ends then (a
started one fails, its outcome unknown), and nonblocking polls reap the
retired child; a hung one keeps the intake busy until teardown. A
finished-complete report marks the generation complete and its intake starts
no further service. A later generation's intake starts afresh: each
installation still needs its own review and wizard confirmation.

The trusted live wizard owns the final human confirmation, for both erase
and preserve modes. Its exact Execute request causes the fixed root service
to send Review over its private channel; td-authd acknowledges that nonce
immediately. This acknowledgement admits execution and supervises lifecycle;
it is not a second human consent or evidence of secure physical input. Root
retains source authentication, plan equality, disk exclusion and its exclusive
disk claim. The trust boundary includes the live wizard and its ordinary input.
Installed-session clients cannot use this intake. On a live boot private
request 19 always answers 99 00; no disk installation occupies the attention
operation slot. A later generation requires a new review and confirmation.

On failed-generation teardown td-authd kills the service and any retired one
and waits for each, before secret-session cleanup, and removes its socket;
unlike an update helper's, a wait error goes unreported. A service stopped
while writing leaves the disk incomplete; td-install's failure semantics and
td-svc's authority cgroup containment apply as for an update. td-install
opens the channel only for a validated execution from the live wizard.

The host-only launch VM fixture also accepts `--run-taskmgr-vm`, followed
by the ordinary kernel/authd/firstboot/login/td-init inputs, compositor,
task manager, optional deployment EROFS, and a new absolute log path.
With EROFS it mounts the deployment read-only and resolves every program
under test through its shipped /bin link and /td/store output. It drives
request 07 through the real root private channel, verifies all four human
UID/GID columns, zero capabilities, outer PID view, human session cgroup
and null standard descriptors. Real compositor keyboard input and
correlated captures check live repaint, default Cancel, and confirmed
suspend/resume of a child owned by the driver. Host fixture source is not
staged into the target authority recipe.

## Elevation operations

Implemented and live: `deploy-rollback` (L3), from the principal table
and request `1d` through its commit and td-boot's checked pair;
`set-hostname` (L4), from its intake and backoff through request `1e`'s
commit and the canonical write; and `deploy-publish` (L5), tag 5, the
table's row for request 19 and the update queue's backoff ("Consent for
a locally built system"). Since TOKEN-LOGIN.md increment 5's A4 the
login tags' disclosure steps await the same approval, live since its A5
admits a write.
APPLICATIONS.md §L.1, "The v1 operations", says what
`deploy-rollback`, `set-hostname` and `deploy-publish` are and why,
and its "Elevation increments" which commit lands each part;
`td-compositor/DESIGN.md`, "Elevation consent", says which presses
confirm. The private channel routes requests `10` to `1f` to the secret
session and refuses `20` and above; ENCRYPTION.md increment 8e's storage
commit routes `20` there too ("Login keys and session lock", amendment
10).

**Consent tags.** Tags 11 (`deploy-rollback`) and 12 (`set-hostname`)
follow the login tags. No consent description carries an approval key:
the prompt approves with Enter (`td-compositor/DESIGN.md`, "Elevation
consent").

- Tag 11 carries, right after its tag byte, the current and the previous
  deployment ID, each 64 lowercase hexadecimal bytes as tag 5 carries
  one, and the two differ: 173 bytes in all.
- Tag 12 carries a big-endian u32 requester, which must equal the owner,
  then the old and the new name, each a one-byte length and 1 to 63
  bytes that `Hostname::parse` (`td-firstboot/src/hostname.rs`) admits,
  and the two differ: at most 177 bytes.

Both stay under the 256-byte bound. Each renders its operation, both IDs
or the requester and both names, and that a restart completes it, then
ends its description with the approval rows, `ENTER: APPROVE` and `ESC:
CANCEL`, in place of the generic Escape row. Tag 5 carries its
requester and ID after its tag byte, 113 bytes in all, and its rows end
with the same approval rows. Tag 6, the live disk installation, keeps
its own `ENTER:` row.

`consent::Request::awaits_approval` names the requests and steps whose
prompt takes that approval, the one predicate root, the compositor, the
workers and td-install ask: tags 5, 11, 12, 16, 18, tag 17's review,
tag 21's identify step and, since TOKEN-LOGIN.md increment 5's A4, a
login description that carries a disclosure. Those are the first step
of a first enrollment (8), its first `connect`, of either count, and the
first step of a removal (10), its `identify`, when the removal leaves at
most one key. Each such step is presented before any token I/O, and its
disclosure is the whole operation's, so one approval confirms the
operation and no later step awaits one; consent's
`Login::discloses` marks root's own first step exactly when that
step discloses, and the worker receives that description and must repeat
it exactly. The step's rows end with the disclosure,
`TOKEN-LOGIN.md`'s ("Enrollment, addition and removal") in rows that fit
the narrowest prompt, then the approval rows in place of the generic
Escape row. The compositor sends that step's presentation receipt,
which carries the exact description, only once Enter approves it.

**Requests.** `1d` asks for a rollback and `1e` selects the queued
hostname request; neither takes an operand. Each answers `92` and the
root-generated description, holding the one operation slot that secret
operations, inspection and request 19 share, or `9d` (for `1d`) or `9e`
(for `1e`) and one refusal byte: `00` when there is nothing to approve
or the slot is busy, `01` when the principal table refuses or cannot be
read, and for `1d` `02` when the selectors cannot be read or name one
deployment. Root reads `/etc/td-elevation.tsv` here, before any
description exists, and for `1d` the table before the selectors, so a
refused owner reads no volume; request 19 on an installed system
consults its `deploy-publish` row the same way and answers a miss with
`99 02`, beside `99 00` (nothing ready or busy) and `99 01` (cannot read
the login record). Presentation (`13`), then one commit (`14`), each with the
exact description, follow within 120 seconds of selection and are
answered `93` and `94`, as for request 19. The compositor sends commit
only after Enter approves the fully presented prompt. Root
cannot observe those presses: it trusts the paired compositor for them,
as it trusts that compositor's Enter for a live boot's disk review. Root
rechecks the
submitting process immediately before acting and consumes the nonce
first, so a consumed nonce is never reused, even when the operation then
fails. Escape, expiry or, for a hostname, requester loss before commit
cancels without acting.

`1f`, which takes no operand and holds no slot, answers `9f`, the
hostname intake's queue byte and its backoff count: `00` when nothing
waits and `01` when an admitted request does, each followed by the count
of consecutive requests that ended unapproved, 255 when it is larger,
which leaves out the one admitted and not yet ended; or `02 00` while
root cannot read the backoff or its last write failed, so the intake
refuses. Root reads an unreadable backoff again at each `1f`. A session
without the intake, a live boot's, answers `00 00`. This is how root
tells the compositor that a request waits: the compositor asks `1f` on
each turn of its authority worker and the attention menu's `H` row shows
the answer it holds when the menu opens. The answer grants nothing; `1e`
selects the request and checks the table again.

**Principal table.** `elevation.rs` reads it at each `1d` and `1e`, at
each request 19 on an installed system, and at each hostname and update
admission. `/etc` is opened as a directory without following a link and
must be root's and writable by neither group nor other; the table is
opened through that descriptor's `/proc/self/fd` path with `O_NOFOLLOW |
O_NONBLOCK` and admitted only as a regular root-owned file of mode
exactly 0444, with one link and at most 4096 bytes, before a byte is
read. Its text is the line `td-elevation-v1`, then at most 64 rows in
strictly increasing UID order, each a canonical decimal UID from 1000 to
65533 and one or more operations, each after a tab, from
`deploy-rollback`, `set-hostname` and `deploy-publish` in that order
without a repeat; every line ends in a newline, and nothing else is
admitted. The system recipe ships the v1 row, `1000`, then all three
operations (`build_elevation_table`), as regular immutable `/etc`
content its shape check holds at 0444. Any departure refuses as the
table's `01`; the table grants and never denies.

**`deploy-rollback`.** At `1d`, after the table, `rollback.rs` opens
`/run/td-volume/td` and reads `boot/current` and `boot/previous` through
that held descriptor with `login_tier.rs`'s selector reader, each exactly
`../deployments/` and 64 lowercase hexadecimal digits; a missing,
malformed or equal pair is `02`. It then draws the 32-byte nonce and the
two key bytes from `/dev/urandom` (a failed draw ends the generation)
and describes the pair under tag 11. Its commit first consumes the
confirmation, then reads both selectors again through the same
descriptor: a pair moved since selection fails the request without
starting anything.

**`deploy-rollback` after commit.** Root spawns only `/bin/td-boot
on-volume rollback /run/td-update CURRENT PREVIOUS`, the two IDs the
person approved. Under its transaction lock and before any mutation, a
selector temporary's reaping included, td-boot requires `current` to
name CURRENT and `previous` to name PREVIOUS, and refuses otherwise
(td-install/DESIGN.md, "Full-system volume consumers"). Both are needed:
when `current` is still pending, an update replaces `current` and keeps
`previous` (td-boot's `install_deployment` and `activate_install`), so
after approving `current` B over `previous` A, publishing C leaves
`previous` A, and a check of `previous` alone would let the rollback
silently undo C. Root's own second read narrows the window; td-boot's
check under the lock closes it. The rollback makes `previous` current
and leaves `previous` naming it. The spawn reuses request 19's rules:
an empty environment, cwd `/`, null stdin and stdout and authority
stderr; the confirmation consumed even if spawn fails; the slot held
until the helper exits, with no automatic retry; and on
failed-generation teardown the direct helper killed and reaped before
secret-session cleanup, inside td-svc's authority cgroup. Only a
successful exit permits the success screen, which asks for a restart;
td-authd never reboots.

Host tests drive the table's grammar and file checks, the table's
refusal before the selectors, both before a description, each `9d`
answer, exact presentation before one commit, a pair
moved after selection starting no helper, the helper's argv and its
reaping; confinement pins hold the fixed argv, the empty environment,
the second read and the selector reader's one use. `qemu-deploy-rollback`
(an integration-tier oracle) boots the system image on a volume whose
`current` and `previous` differ, reads the prompt off the screen, shows
that a key other than Enter pressed first commits nothing and leaves
both selectors, and that the control socket refuses key requests
outright (input automation is off in production; the compositor's
source pins hold that no injected press reaches the prompt when it is
on), approves with Enter, and boots `previous` next.

**`set-hostname` intake.** On an installed system Prepare binds
`/run/td-authd/1000/hostname` beside the update intake, with the same
binder, owner UID 1000, mode 0600, per-fragment UID and live
sender-pidfd policy, and every SCM_RIGHTS descriptor refused; a live
boot binds none. The client `td-authd request-hostname NAME` requires a
root peer and the exact eight-byte `TDHST01` newline greeting, then
sends one length byte and 1 to 63 name bytes. Root parses the name with
`Hostname::parse`, refuses the current name and a requester the
principal table does not grant, writes the backoff's increment and only
then sends admission byte 02. As for the update intake, one client, one
accept and four nonblocking attempts per heartbeat bound socket work;
admission expires in five seconds, and a sent receipt starts a
sixty-second selection window. Admission also refuses while the backoff
refuses, cannot be read or cannot be written, and while
`/var/lib/td/hostname` is missing or unreadable. A refused whole frame
is answered with one byte before the connection closes: 03 while the
backoff refuses, 04 for any other reason; a malformed frame is closed
without one. The client prints which, or that the change waits and how
to review it, then waits for one completion byte: 01 when the name is
saved, 00 otherwise. `set_hostname.rs` holds the intake;
its socket work is `deployment.rs`'s `bind_intake` and the shared
`secret_sys.rs` transport, which refuses every descriptor.

At `1e`, after the table grants the owner, root takes the waiting
request and answers `9e 01` unless the table grants its requester, the
sender's UID. It then reads the old name from `/var/lib/td/hostname`
with `td-firstboot hostname`'s bounds, draws the nonce and key, and
describes tag 12; a failure of any of these ends the request as
`9e 00`, and the client reads 00. The request counted at admission, so
one that fails here has counted though no prompt was shown.

**Backoff.** The hostname intake, since L4, and the update queue, since
L5, each queue one request at a time and keep a backoff of their own.
Every admitted request counts until an approval: the intake refuses new
requests until 30 seconds after the latest an admitted request can end,
doubling with each consecutive unapproved request to at most 960
seconds; an approval resets it. Each intake's count and wall-clock
refusal deadline live in one root-owned mode-0600 file,
`/var/lib/td/authd/backoff`, in a root-owned mode-0700 directory on
`@var`, so neither a new authority generation nor a reboot clears them.
Root writes the increment, synced, at admission, before the admission
byte, with a deadline 180 seconds on, the request's selection and
consent windows, plus the delay, and clears it only on approval. The
bound is approximate: sending the admission byte, within the five-second
admission, can start the selection window up to five seconds after the
write. So however a request ends, declined, expired, ended by a key,
requester loss, a failure before its prompt, or a teardown or crash of
the authority before or after selection, it has counted; a crash between
the write and the admission byte counts a request the client never heard
admitted. Any process of UID 1000 can spend an intake's shared count:
each request it gets admitted delays the person's own renames or updates
by up to 180 plus 960 seconds, and one that asks again as each deadline
passes can keep them waiting indefinitely; that is the accepted trade
APPLICATIONS.md §L.1's prompt-spam row names. The file is the line
`td-authd-backoff-v1`, then a row for each intake whose count is not
zero, `hostname` before `update`: its name, the count and the deadline
in seconds since the epoch, each after a tab in canonical decimal; every
line ends in a newline. Each intake writes its own row and keeps the
other's as read, so a file root cannot read is never replaced. It is
written through `td-firstboot/src/saved.rs`'s synced temporary and
rename, and admitted only as one link of root's, mode 0600, at most 256
bytes, opened without following a link or waiting on a FIFO; a missing
directory is created by std, mode 0700, beneath a parent of root's that
no other may write. A missing directory or file reads as zero; a
malformed file, or one root cannot read, refuses both intakes with a
named diagnostic on root's standard error, and the hostname intake's
`1f` answers `02`. A failed write does the same and stays until a later
write succeeds, whatever is read in between: admission retries the
write, so the first one that succeeds admits again. The standing failure
is the generation's: a new authority generation starts without it, and
its first admission refuses again by failing the same write. The
deadline is wall-clock. One set forward shortens the refusal; one set
back cannot lengthen it past 180 plus 960 seconds from when admission
reads it, since admission cuts a later deadline to that bound and writes
the cut one back.

Recovery from an unusable backoff is outside td's operations, since
nothing td offers the person writes `/var/lib/td/authd`: a file root
cannot read blocks every update and every rename until it is repaired
or removed, which a missing file then reads as zero, from physical
access as `td-login/TOKEN-LOGIN.md`'s "Recovery" describes it.
`deploy-rollback` does not help, since it selects a deployment and
leaves `@var`, the file included, as it is. A full or read-only `@var`
fails the write, and so blocks updates and renames, until space is freed
or the volume is writable again; the next admission then writes.

**`set-hostname` after commit.** Root consumes the confirmation, then
clears the backoff, then re-reads `/var/lib/td/hostname` with
`td-firstboot hostname`'s bounds, refuses unless it holds the old name,
and writes the new name with the provisioner's synced temporary-file and
rename protocol, root-owned, mode 0644 and newline-terminated, from
`td-firstboot/src/saved.rs`, which td-authd compiles by `#[path]` rather
than copying and which runs no `td-firstboot` command. The public client
then reads byte 01, and 00 on failure, as for an update. Root changes
neither the kernel name nor `/etc/hosts`; APPLICATIONS.md §L.1 says what
reads which name until the next boot.

Host tests drive the intake's UID, greeting, name and sender rules, one
request at a time, the refusal bytes, the backoff's grammar, file
checks, doubling, persistence and clearing, the count written at
admission and kept across a teardown before selection, a failed write
refusing until one succeeds, a deadline cut to its bound, a malformed
backoff refusing the intake and answering `1f`'s `02`, the table before
any description and the
requester before the old name is read, a cancelled or uncommitted
request writing nothing, one commit writing once, and a saved name
changed since selection left alone; confinement pins hold the socket,
greeting, write path and order, and that the intake spawns nothing and
names no kernel hostname interface. `qemu-deploy-rollback` then, as UID
1000, sees a malformed name refused, requests a name and sees a second
request turned away while it waits, reads tag 12's old and new names
off the screen, presses a key other than Enter first and sees the
request end unwritten with the menu's count at 1 and a new request refused with
`03`, reboots and sees the count survive. By the guest's clock it
requires `03` for every request made before the 210 seconds from the
first admission end, less a margin, and admission only after; when the
reboot itself outlasts that window it fails as a slow boot, naming both
times, rather than as a refusal missing. It types the admitted
request's key, sees
`/etc/hostname` read the new name while
the kernel keeps the old one and the count cleared, and reboots to
`TD-HOSTNAME-READY` with the new name.

## Login keys and session lock

Implemented and live since TOKEN-LOGIN.md increment 5's A5: the
consent operations and step admission (amendments 3 and 4), and the
worker supervision, requests and deadlines of amendments 2, 5 and 6
("Login-key operation supervision" below). The paired compositor sends
`1b` for a first enrollment, an addition and a removal from its `K`
screen, each of which a production build starts, and an unlock's `1b`
only from its lock surface while the state is enrolled
(`td-compositor/DESIGN.md`, "The lock surface"). Its PIN field sends
`1c` only after this supervisor's `91 0c` for a presented PIN step
(`td-compositor/DESIGN.md`, "The PIN field"). Login state (1) is
implemented and live: request `1a`, its cache and the `9a` answer, which
the compositor asks at connect, uses for `D`'s key list and, since
TOKEN-LOGIN.md increment 4's C7, locks on at every generation's start
where it is enrolled or unavailable, drawing its names on the lock
surface, and since C9 reads to decide whether `Super+l` and the
attention menu's `L` lock (enrolled or unavailable) or do nothing
(unenrolled, where `L` shows `NO LOGIN KEYS ENROLLED`). No td-authd
change was needed for either: the connect-time `1a` already preceded the
compositor's first paint, and the later answers already reached it.
Update consent (8) is implemented (C4) and live: on an enrolled or
unavailable machine request 19 refuses a queued deployment that cannot
read the record. Every deployment built from TOKEN-LOGIN.md increment
4's C10b onward carries the tier marker, so on such a machine it admits
a marked deployment that reads the record's version (any marked one
while the record cannot be read) and refuses one built before C10b,
which carries none; nothing enrolled before increment 5. Revocation (7)
is implemented and live (TOKEN-LOGIN.md increment 5's A3,
`revocation.rs`): every generation's first `1a` and every login
operation's end check the record. On a stock boot firstboot's record
names the unenrolled state, so the check changes nothing until the owner
enrolls a key or removes the last one, which cuts over.
[`td-login/TOKEN-LOGIN.md`](../td-login/TOKEN-LOGIN.md) owns the
login-key tier. "Session lock" there is the compositor's display and
input lock; it is unrelated to this document's secret-session statuses
(idle, a released key, relocking), and unlocking the session neither
releases nor clears an application-store key. td-authd's part amends the
contracts above as follows; increment numbers are TOKEN-LOGIN.md's.

1. **Login state (4).** Once Prepare has completed, and before the
   compositor admits any client or input, the compositor sends request
   `1a` and the authority answers with the human's login state:
   unenrolled, enrolled, or unavailable with its typed cause, as
   TOKEN-LOGIN.md defines them ("Login-key operation supervision" below
   gives the bytes). The answer also carries the revocation status of
   amendment 7: `00` settled, `01` pending, `02` failed and restarting
   and `03` held. Only td-authd
   decides that a session unlocks: on a login unlock's observed success.

   An enrolled answer also carries the record's slot count and each
   slot's four-byte fingerprint in canonical slot order, as
   `inspect-login` reports them: the key list whose positions the
   key-management screen's removal digits name, and the shape of the
   worker's baseline (`18 01`, `td-secret/DESIGN.md`). Every answer, in
   every state, carries the primary username that `terminal-serve`
   resolved and validated at start, and the hostname, both of which the
   lock surface shows. Root re-reads `/proc/sys/kernel/hostname` for every
   answer, cached or not, through a 65-byte bound, drops one trailing
   newline and sends the name only when it passes td-firstboot's
   `Hostname::parse` rules; an unreadable, longer or invalid one is sent
   empty. Increment 4's C3 implements all of this (`login_status.rs`);
   before it the compositor had no key list ("Login-key operations" in
   `td-compositor/DESIGN.md`).

   On a live boot (Prepare's `td.live=1`, "Whole-disk installation
   intake") root answers every `1a` unenrolled without the predicate or
   the helper below, so the live medium never locks; firstboot's
   directory step still runs there.

   Root first applies the same predicate firstboot and td-login use:
   `/var/lib/td/login` opens as a valid root:root mode-0700 directory and
   the record name is absent. That alone, with no helper, is
   unenrolled, unless the protected marker is there too or the boot's
   `/run/td-admit` exists, which is unavailable with cause `0d`
   (amendment 9); an invalid directory is unavailable (damaged
   directory). Only when the
   record name exists does root run a fixed read-only `td-secret
   inspect-login --uid 1000` child to tell enrolled from a damaged record.
   It shares store inspection's fixed helper launch (empty environment,
   `/` cwd, null stdout and stderr, private stdin socketpair), its exact
   bounded result (here at most 36 bytes, the parent reading at most
   one past them) and its two-second observed deadline, but not its
   protocol (`td-secret/DESIGN.md`, "Read-only enrollment-state
   inspection", gives its result): it is not request `17`, it runs
   synchronously inside the `1a` that needs it, it never occupies the
   operation slot, and a missing helper, failed spawn, malformed result,
   failed exit, failure to reap or expired deadline answers unavailable
   (state could not be read) rather than ending the generation. Root
   drops the result's version byte, which `9a` does not carry. A name
   that exists is therefore always the enforced state, whatever the
   helper does, and a helper failure on an unenrolled machine cannot
   arise. The username is the one `terminal-serve` resolved, and a
   session whose name fails `primary_account`'s rule refuses to start,
   before it admits the compositor, so every answer's name is one the
   compositor admits.

   Root caches the last state, with an enrolled state's key list, and
   serializes `1a`, so requests never overlap; the cache holds nothing
   else. A generation's first `1a` fills it, and a later one returns it,
   refreshing it first only after a login operation (once its terminal
   status has been polled) or while it reads could-not-be-read. While it
   reads could-not-be-read, root pauses the helper after its last run
   ended: two seconds, doubled for each consecutive run its deadline
   ended beyond the first (2, 4, 8, then 16 s at most), and back to two
   once a run ends by the helper's own exit or the state reads anything
   else. A `1a` inside the pause answers the cached could-not-be-read at
   once. The run itself never waits past its deadline: root kills a
   helper at the deadline and answers at once, keeps the killed child,
   reaps it with nonblocking polls at every later tick and every `1a`
   that reads the record, and runs no other helper until it is reaped,
   so a child the kill has not yet stopped costs nothing more. One `1a` therefore delays the requests
   behind it on the serialized channel by at most one helper deadline,
   two seconds, and a persistently stuck helper by that at most once in
   sixteen seconds. Ending the generation never waits for this child:
   td-authd exits after teardown, and the read-only child, if still
   unreaped, is reparented and reaped outside it. Store inspection
   shares this launch and its nonblocking polls but not this end: its
   result stays pending until its child is reaped, and teardown still
   kills and waits for it before cleanup, as "Paired read-only store
   inspection" states. The predicate is not
   paused, so a name that disappears resolves at the next `1a`. Request
   19's fresh read (amendment 8, C4) also updates the cache. The compositor sends `1a` again
   after every completed, failed or uncertain login operation, and every
   250 ms while a revocation is pending or the state could not be read,
   so a first enrollment, the removal of the last key or a transient
   helper failure resolves without a reboot.
2. **Worker (2).** One fixed root `td-secret login-operation --uid 1000`
   child per login operation, with the unlock worker's launch, framing,
   presentation and commit rounds, cancellation, reaping and generation
   teardown. It shares the single operation slot. Its baseline (`18`),
   PIN (`16`) and typed failure (`15`) frames, and the commit round an
   unlock also runs, are `td-secret/DESIGN.md`'s, "Login-key worker"; the
   worker's unlock, first enrollment, addition and removal are
   implemented in `login_operation.rs`. The slot is what
   serializes the worker's writes to the login store: each of the
   worker's token sessions holds `/run/td-fido/operation.lock` only for
   that session. Across td-authd processes the worker depends on td-svc:
   it stays in the `wayland` unit's leaf, and td-svc kills that leaf
   when td-authd exits and starts no new generation until it is empty.
   Root's acknowledgement of a write's commit round (`13`) is where the
   write may begin: from then on any outcome but `14` with a successful
   exit, whether a typed failure, a lost channel, a failed exit or root's
   own deadline, is UNCERTAIN, not failed, and root re-reads the login
   state before showing a result. An unlock writes nothing, so its
   non-success after `13` is a failure. The re-read is amendment 1's: root
   reports the uncertain end (`91 0e`), and the compositor's `1a` after
   every login operation refreshes root's cached state.
3. **Consent operations (2).** Implemented: `consent.rs` has tags
   7 to 10, login unlock, first enrollment, key addition and key removal.
   Each carries its step, the key count before and after and the
   fingerprints involved, and every PIN step of every one of them carries
   the key's reported PIN retries. "Immutable consent description
   prerequisite" above gives the bytes, which literal tests pin.
4. **Step admission with device data (2).** `following_enrollment_step`
   derives the whole successor and requires the worker's invitation to equal
   it. A login step instead names data only the token supplies. Root still
   derives nonce, owner, operation, counts, removal slots and the next
   step. The baseline is the slot fingerprints of the worker's `18` frame,
   which it sends before any presentation. Root accepts the worker's
   fingerprint only when it names a slot of the baseline record (the
   authorize or unlock step after identify) or the credential the worker
   reports creating (prove, repeat, probe), which no baseline slot names,
   and a retry count only as one nonzero byte displayed as a device
   claim. Any other difference cancels the
   child. A commit frame is accepted only after the operation's final
   step. `begin_login` and `admit_login_step` are those checks, and
   `login.rs` applies them; `login_start` and `login_successor` are the
   parts that need neither the record nor the worker's report.
5. **Requests and version (2).** The paired protocol becomes `TDLA003`,
   because TDLA002 states that no record contains a credential. Request
   `1b` selects a login operation (unlock, one-key or two-key first
   enrollment, addition, or removal with its slots), and `1c` carries the
   exact current description followed by one PIN. Root forwards that PIN to
   the worker as one bounded frame in a clearing owner and keeps no copy.
   Both peers ship atomically, without negotiation, as earlier additions
   did; the compositor's private client sends `1b` from increment 3,
   and its PIN field `1c`. Since activation (TOKEN-LOGIN.md increment
   5's A5) every `1b` starts its worker, and an unlock ends as NO
   RECORD while no record exists. Host
   child fixtures and the ignored root session fixture drive both requests
   through the real paired Session, as they do for secret operations.
   Request `1a` landed with login state in increment 4's C3, still
   `TDLA003` and without negotiation. The worker's baseline, not root, tells
   it that no record exists: an unlock against an unenrolled baseline ends
   as NO RECORD before any description.
6. **Deadlines (2).** The fixed 120-second ceiling becomes 120 seconds per
   key ceremony, fixed when the operation starts and never renewed: unlock,
   removal and one-key enrollment 120 seconds, addition and two-key
   enrollment 240. An operation that shows a disclosure (TOKEN-LOGIN.md
   increment 5's A4), every first enrollment and a removal leaving at
   most one key, adds a 120-second reading allowance, so reading it and
   approving it take nothing from the ceremonies: a one-key enrollment
   and such a removal 240 seconds, a two-key enrollment 360. The
   allowance is shared slack rather than reserved: an approval at once
   leaves the ceremonies the whole ceiling, and the slot may be held
   that long, while each token session keeps its own bound.
   The ceiling is consent's `login_ceiling`, so root, the worker and the
   compositor's attempt lifetime agree. Root does not know a removal's
   remaining count until the baseline, so until its first step it bounds
   a removal by the allowance too and then narrows to that step's
   ceiling; the worker is bounded by the longest, 360, until the
   description names its operation. Each token transaction keeps the
   transport's own two-minute bound. Physical timing is activation
   evidence. Root's deadline counts from just before it spawns the
   worker, so it ends a moment before the worker's, which counts from
   the worker's startup. The worker's commit margin makes it likely, not
   certain, that a write and its success frame land within it; a success
   root has not seen by its deadline is uncertain (below).
7. **Revocation (5).** Root performs TOKEN-LOGIN.md's "Cutover" whenever
   the reduced login state is not the one the volatile record
   `/run/td-login-cutover` names for this boot. That record names the
   state the console and SSH were last brought to.

   **The reduced state** is `unenrolled` or `enforced`, decided by the
   shared predicate's directory-and-name check alone, the one firstboot,
   td-login and `render-ssh-policy` apply (TOKEN-LOGIN.md, "SSH"). It is
   not the `1a` answer, whose helper may also report the record
   unreadable. So root and the verb always reduce alike: a valid
   directory without the record name is unenrolled, and anything else is
   enforced. A later state that enforces without a record extends that
   shared check, never root's reduction alone:
   `td-install/ENCRYPTION.md` increment 8's protected marker and the
   boot's `/run/td-admit` do (amendment 9), so a valid directory without
   the record name is enforced while either exists.

   **The record.** It is exactly three lines, each ending in a newline:
   - `td-login-cutover-v1`;
   - the boot ID, the 36 bytes of `/proc/sys/kernel/random/boot_id`
     without its newline;
   - `unenrolled` or `enforced`.

   It is root:root mode 0600, published by renaming a temporary
   `/run/td-login-cutover.tmp`, which the writer opens with
   `O_CREAT|O_EXCL|O_NOFOLLOW` after removing a stale one. Every writer
   holds the exclusive lock on the persistent `/run/td-login-cutover.lock`
   from before that removal to after the rename, a lock file it opens
   only as a single-link root:root mode-0600 regular file, so writers
   never interleave. Both writers run one source for the bytes and this
   publication, `td-firstboot/src/cutover.rs`, which td-authd compiles
   by its reviewed path.
   - Firstboot writes it in stage 1, under `/sysroot`, right after
     publishing its boot-time render, naming the form it rendered. A
     failure to write it fails that step, which stops boot as a failed
     render does.
   - Root removes it, under the same lock, as every cutover's first
     step, and writes it only after a cutover is observed complete,
     naming the form `render-ssh-policy` reported publishing. So a
     cutover interrupted at any point, or whose record cannot be
     written, leaves no record: a stale one could otherwise name the
     very state the login state returned to while the console and SSH
     were brought to another, and the next check would skip the
     revocation. A record that cannot be removed fails the revocation.
   - Root reads it with `O_NOFOLLOW|O_NONBLOCK` and requires a regular
     file, one link, root:root, mode 0600, at most 128 bytes and that
     exact grammar. Anything else, an absent record included, reads as
     naming no state, as does a record naming another boot ID.
   - A record that names no state for this boot needs a cutover to the
     current reduced state, whichever it is. On a stock boot firstboot
     has written it, so the check changes nothing.
   - An unavailable state never renders the ordinary policy: it reduces
     to enforced.

   **When it runs.**
   - Root begins the generation's check inside the first `1a` after
     Prepare: it computes the state, begins the check, then answers, so
     that answer already reads `01` if a cutover began. That first check
     is the authority-start check, since root learns only from Prepare
     whether the boot is live.
   - After every login operation, including one whose worker failed or
     whose outcome is uncertain, the check begins when the operation
     ends, before its terminal status is delivered. The compositor's `1a`
     after that status therefore already sees it.
   - A live boot (`td.live=1`) runs none, since root answers `1a`
     unenrolled there by fiat.

   **Beside the slot.** A cutover does not take the one operation slot,
   so no request meets it as busy, and a login operation, an inspection
   or an elevation may run beside it. A check that falls due while a
   cutover runs is taken in the same `tick` that settles or holds it, so
   no `00` or `03` is reported between the two, and a state that changed
   meanwhile converges to the newer state; a failure that owes a reboot
   drops it, since the reboot enforces whatever state it would find. The
   cutover is a nonblocking state machine that `tick` advances; `1a`
   reports it pending (`01`) until it settles. Generation teardown
   abandons a cutover in flight without writing the record, killing its
   render or td-svc client, so the next generation's check repeats it.

   **The cutover:**
   - It removes the record (**The record**), before anything else.
   - It renders the SSH policy through the fixed root subcommand
     `td-firstboot render-ssh-policy` (TOKEN-LOGIN.md increment 5's
     A2). That verb applies firstboot's account validation, publishes
     the policy by rename, and prints the one word of the form it
     published, which is the form root records. The account tables are
     read-only binds from `prepare-primary-profile` to shutdown, so
     within a boot it validates the set stage 1 validated. The rename is
     what serializes publication against `sshd`'s readers, and an
     exclusive lock serializes overlapping renders
     (`td-login/THREAT-MODEL.md` §1). Root
     launches it as it launches its helpers, with an empty environment
     and `/` as its working directory, and reaps it without blocking.
     Its output must be exactly `unenrolled` or `enforced` and a newline,
     read through a 16-byte bound, and its exit successful; anything
     else is a failed render. At ten seconds, and at generation teardown,
     root kills and reaps it, so no stale render can land after a newer
     cutover's.
   - It returns the greeter's line to root:root with the pinned terminal
     mode, so no descriptor opened after the state change and before the
     restart outlives it (`td-login/TOKEN-LOGIN.md`, "Cutover").
     - The line is `/dev/ttyS0`, the greeter unit's `tty=`, which a
       recipe test pins.
     - Root takes the line only once `symlink_metadata` shows a
       character device: `lchown` to 0:0 first, then mode 0600, then it
       reads both back.
     - An absent node is skipped, since no session can hold it.
     - A node of any other type, or a failed change or read-back, is a
       failed revocation.
   - It asks td-svc to restart `sshd` and `greeter`, issuing both
     requests before polling either so their TERM waits overlap. It
     names no other unit.
   - Each control exchange, `restart NAME` or `status NAME`, is a
     `/bin/td-svc` client child launched as the render is: empty
     environment, `/` as its working directory, its reply read through a
     256-byte bound, reaped without blocking, and killed at its bound.
     td-svc serves one connection at a time, and its client and server
     each wait 10 seconds for an answer (`td-svc/src/control.rs`,
     `REPLY_TIMEOUT`), so a busy supervisor says so only after 10
     seconds, which a shorter bound would never see. The bound is 12
     seconds: past those 10, by which the client's own timeout or the
     server's busy reply is reached and the client exits, with two
     seconds for its start and its reply.
   - It polls `status` for each unit every 250 milliseconds until the
     unit has left `stopping` and runs a new leader. Leaving `stopping`
     means, for `greeter`, that its `tty=` containment is empty; for
     `sshd`, that its whole service leaf is (`td-svc/DESIGN.md`,
     `stop=leaf`). A new leader is a pid different from the leader
     recorded before the request, whose `/proc` start time is later than
     the request. Both units are `restart=always`, so after the request
     `failed` with a retry pending and `held` are td-svc's restart
     backoff, and `down` a start still to come: each is polled until a
     new leader runs or the deadline passes. Only `stopped`, an
     operator's standing stop, fails the revocation at once.
   - Each unit must finish within 30 seconds of its request: longer
     than one exchange and its retry (24 seconds), and than the default
     10-second `stop-timeout` plus KILL observation even under
     emulation. The deadline is checked before any reply is accepted,
     so a reply readable only after it fails the unit.
   - Only then does it write the record and settle (`00`).

   One transient td-svc failure, a busy reply or a client that exits
   having printed nothing (its own timeout, or no supervisor to
   connect to), is retried once. A failed revocation, never only a
   warning, is any of:
   - a refusal after that retry;
   - a failed render;
   - a failed hand-back;
   - a record that cannot be removed;
   - a unit `stopped` after its request;
   - a client still running at its 12-second bound;
   - a teardown still pending at its deadline.

   On a failed revocation:
   - The `1a` revocation status reads failed (`02`).
   - The compositor shows `A CONSOLE OR SSH SESSION COULD NOT BE CLOSED:
     RESTARTING` on its lock surface and attention screen.
   - Root asks td-svc for an orderly `reboot` through `/bin/td-svc
     reboot`, as td-install requests one: a client child bounded and
     retried once after a transient failure exactly as a control
     exchange is.
     The reboot's boot ordering then enforces the boundary: firstboot
     renders the enforced form before `sshd` and the greeter start.
   - Root makes that request only once no operation holds the slot, so a
     reboot never interrupts an update, an elevation or a login commit.
     The slot operation's own deadline bounds the wait. While `02`
     reads, the compositor sends no request that takes the slot, and
     one that arrives is a protocol violation that ends the generation.
   - If that request fails too, the authority ends its generation, and
     the next generation's check repeats the cutover.
   - If td-authd ends the generation while the reboot is owed and td-svc
     has not accepted it, teardown requests it once the slot, its
     operation and its inspection, is reaped, or waits for a request
     already made, no longer than the request's bound and its one
     retry's; a refusal is logged, and the next generation's check
     repeats the cutover. A slot that could not be reaped requests no
     reboot: the guard stands, and the next failure in that boot holds.
     This covers only the generations td-authd itself ends: if the
     compositor dies first, pair-run SIGKILLs td-authd, no teardown
     runs, the guard stands, and a second failure in that boot holds
     likewise.

   **Reboot guard.** Before requesting an automatic reboot, root writes the
   current boot ID to the durable root-owned mode-0600
   `/var/lib/td/login/cutover-reboot`; readers of the record ignore it, and
   the first successful check of any later boot removes it. A failed
   revocation while that file exists requests no further reboot: the
   machine stays in its boot-time state, where firstboot's render and
   td-login's refusal already enforce the boundary, and the `1a` status
   reads held (`03`), which the compositor shows on its lock surface and
   attention screen as `SESSION REVOCATION FAILED AFTER A RESTART`. If the
   guard itself cannot be written, for example because the directory is
   damaged, root likewise holds without rebooting and shows the same
   notice; it never reboots unguarded. Root walks the guard's directory
   without following a link, holds it, and creates the guard through it
   only when the shared predicate's directory check passes on the held
   descriptor (root:root, mode 0700, its rule for the login directory);
   a directory that fails it is unwritable. At
   most one automatic reboot therefore separates two successful checks.

   **As implemented (A3).** `revocation.rs` holds the state machine;
   `session.rs` begins checks and ticks it from every request's `tick`,
   `1a`'s included, and refuses a request that takes the slot (`17`,
   `18`, `19`, `12`, `16`, `1b`, `1d`, `1e`) while `02` reads. The
   render, each control exchange and the reboot request are the bounded
   helper launch of amendment 1's helper, reading standard output
   instead; a control client's output is read whatever its exit, since
   td-svc's client prints a refusal and then exits unsuccessfully.
   Where the text above leaves a choice:
   - Each unit's leader is read with `status` before either restart is
     requested. A new leader's start time, `/proc/PID/stat` field 22, is
     compared with `/proc/uptime` read just before its request, both in
     USER_HZ (100) ticks on the boot clock, so "later" is at tick
     granularity: a start in the request's own tick counts.
   - A transient failure is td-svc's two busy replies (its loop did not
     answer in time, or is not accepting requests), or a client that
     exits unsuccessfully having printed nothing: its own reply timeout,
     or a connection it could not make, which its standard error alone
     tells apart. Each exchange, and the reboot request, retries once.
     Any other `error:` reply, a reply past 256 bytes, a client root
     killed at its bound, or a reply that is not exactly a status line,
     a `NAME: ...` acknowledgement or one of `reboot`'s two acceptances,
     is a refusal: a client still running past the bound is not a busy
     supervisor.
   - A record that cannot be written after a completed cutover is logged
     and the status still settles: the sessions are already closed, and
     the next check repeats the cutover.
   - Teardown kills a render or control client in flight and reaps it
     within one second, then leaves it to the launch's nonblocking drop,
     for the drop's reason: a child the kernel cannot kill must not wedge
     td-authd. An owed reboot's request is awaited to its bound and its
     retry's instead, since killing it could lose the reboot.
   - The guard is created directly with `O_CREAT|O_EXCL|O_NOFOLLOW`, so
     any existing entry under its name, of any type, holds. A successful
     check removes a guard whose contents are not exactly this boot's ID
     and a newline, and keeps every guard while the boot ID cannot be
     read.
8. **Update consent (4).** On an enrolled or unavailable machine, request
   19 refuses, before any presentation, a queued deployment that does not
   carry the login-key tier marker or, when the record on disk is
   readable, whose marker does not list that record's version. The marker
   is a member of the deployment's `initramfs.cpio`, not a manifest line;
   TOKEN-LOGIN.md, "Deployments", owns its bytes and how a reader checks
   them, here through the held directory File, against the ID the update
   was admitted with, opening `initramfs.cpio` as that section requires
   of a requester-owned file. The state, and an enrolled record's
   version, the helper's version byte, are read afresh for the
   selection: amendment 1's predicate, then its helper, whose answer
   updates amendment 1's cache. A refusal sends no description, retires
   the queued request as one whose installation cannot start (its public
   client's completion byte is `00`) and answers `99 01`; the compositor
   shows `UPDATE CANNOT READ LOGIN KEYS` on the attention screen, where
   `99 00` shows its no-installation notice. An unenrolled machine, a
   live boot's whole-disk installation and every `99 00` case are
   unchanged. TOKEN-LOGIN.md cites this as the update-consent rule. No
   key is required; consent is otherwise unchanged.

   Implemented in increment 4's C4. td-authd compiles td-secret's
   `login_tier.rs` through one reviewed `#[path]` in `main.rs`, which
   request `1d`'s selector reader shares, and `Ready::reads`
   reads the marker afresh at selection, through the held directory, as
   the requester's files, against the admitted ID; the selection is
   already taken, so a refusal is `Intake::finish(false)`. The status's
   `admits` is amendment 1's `refresh`, which also returns an enrolled
   record's version, and reads no marker on an unenrolled machine. The
   read is synchronous inside the request, on the serialized channel
   whose compositor end waits five seconds for an answer, so it gives up
   after two seconds (`MARKER_GIVE_UP`), reading no version: with
   amendment 1's two-second helper before it, a requester's archive too
   large or slow to hash in time is refused with `99 01` inside that
   receive rather than ending the paired generation (TOKEN-LOGIN.md,
   "Deployments"). The worker's own reads keep ten seconds each.

   The system recipe writes the marker into every deployment from
   TOKEN-LOGIN.md increment 4's C10b, which landed after C10, since the
   marker claims that a deployment honours the record at every entry
   point (console, SSH, locked start and session lock), which no build
   before C10 does. Between C4 and C10b, request 19 on an enrolled or
   unavailable machine refused every td-built queued deployment with
   `99 01`.
   From C10b it admits a queued deployment whose marker lists the
   enrolled record's version, or any marked one while the record cannot
   be read, and still refuses one built before C10b, which carries no
   marker; no machine enrolled before increment 5, and an unenrolled
   machine reads no marker. The full-system refusal, in
   `qemu-login-system` (C11), depends on C10b's marked deployments.
9. **Session admission.** `td-install/ENCRYPTION.md` increment 8's
   "Verified account handoff" owns the admission record and the rules
   td-authd applies to it. At an installed boot's Prepare, before the
   compositor's first `1a`, root reads, unlinks and judges
   `/run/td-admit/v1` after creating `/run/td-admit/consumed`
   exclusively, and creates `/var/lib/td/login/protected` whenever
   `/run/td-admit` exists, which only a protected release leaves
   (`handoff.rs`, compiling td-boot's `admission.rs` for the record's
   bytes). A live boot consumes nothing. With that marker or
   `/run/td-admit` present an unenrolled login state is answered as
   unavailable with the new cause `0d`, so it locks as every unavailable
   answer does; the lock surface says no login key is enrolled and that
   a boot with the disk PIN or a primary disk key unlocks. The shared
   predicate reports either without the record name as its own protected
   state, which firstboot, td-login and the revocation's reduction
   enforce, and request 19 treats as it treats unavailable. The `9a`
   answer gains one final byte after the revocation byte: `01` in the
   first answer of a generation the record admitted, `00` in every other
   answer. The
   compositor then paints the session rather than the lock surface for
   that generation's first frame (`td-compositor/DESIGN.md`, "The lock
   surface"), and ends the generation on a `01` in any later answer. The
   admission authorizes no login-key operation; with the marker or
   `/run/td-admit` present root and the worker apply TOKEN-LOGIN.md's
   protected-volume rule
   ("Enrollment, addition and removal"): the last key's removal is
   refused (`85`), and so is a first enrollment without a disk proof
   (`84`), the operation only that proof can authorize; until
   ENCRYPTION.md's item 9 no production build decodes the proof
   (amendment 10), so there every first enrollment is.
   The disk proof's keyslot test runs through amendment 10's
   `storage-operation`, never in the login worker. Both peers ship the
   byte atomically, still `TDLA003`, as request `1a` did.

   Choices the handoff left open, each the fail-closed reading: the
   record directory must itself be a root:root mode-0700 directory, or
   nothing in it is touched; the record must share its device with that
   directory and with the topmost mount at `/run` in
   `/proc/self/mountinfo`, which must be tmpfs; a name at `v1` of any
   kind is unlinked before the consumed marker is made; whatever
   `/run/td-admit` holds, a refused record, a record of method 0, an
   untrustworthy directory or nothing, the boot is protected, so the
   persistent marker is made, and one that cannot be made is retried at
   every later Prepare while the predicate enforces through
   `/run/td-admit` alone; `td-boot admit` leaves the directory, empty,
   after a failure where it can; the age is `/proc/uptime`'s first
   field, `CLOCK_BOOTTIME`, to the hundredth; a record admits only when
   the protected marker was made or found; and the admission byte goes
   with the generation's first `1a` answer whatever that answer's
   state.
10. **Storage operations.** Fixed by ENCRYPTION.md increment 8e's
    documentation commit, before their code. The selector update is
    implemented (`storage.rs`, `20 01`); like the login worker's writes
    it starts no worker outside test builds (`OPERATIONS`) until
    ENCRYPTION.md's item 9, so a production build answers every `20`
    `a0 00`. The disk proofs are implemented too (`proof.rs`, `1b 0b`)
    and decoded only where `OPERATIONS` is set, so a production build
    reads `1b 0b` as a malformed selection, a protocol violation, until
    item 9 lifts both together. The upgrade and its cancellation are
    fixed by ENCRYPTION.md increment 8f's documentation commit, before
    their code, and implemented (`20 02`, `20 03`) behind the same
    `OPERATIONS`. Protector management (`20 04` to `20 09`) is fixed by
    increment 8g's documentation commit, before its code. Its consent
    tags 19 to 24 and the choose step are implemented. So are the
    addition, the removal and the recovery-key removal (`20 04` to
    `20 06`), behind the same `OPERATIONS`, and the compositor selects
    them. So are the three requests (`20 07` to `20 09`), behind the
    same `OPERATIONS`, and the compositor selects them. Secure Boot
    enrollment (`20 0a`) is not yet fixed.
    One fixed root `/bin/td-install storage-operation --uid 1000` child
    per operation, the storage worker (`td-install/DESIGN.md`, "Storage
    operation worker"), launched and supervised as the login worker is
    ("Login-key operation supervision" below: empty environment, `/`
    cwd, null stdout and stderr, a private stdin socketpair, the
    authority's process group, the nonblocking framed endpoint), with
    the same presentation and commit rounds, cancellation, deadlines and
    teardown, and holding the same single operation slot, carries
    ENCRYPTION.md increment 8's selector update, upgrade and its
    cancellation, protector management (token addition and removal,
    recovery-key removal, the PIN-change, TPM-clear and re-enrollment
    requests), optional Secure Boot enrollment, and the keyslot test of
    TOKEN-LOGIN.md's login-key disk proof. Physical selection alone
    starts one, from the attention menu's storage screen
    (`td-compositor/DESIGN.md`, "Storage operations") or, for a disk
    proof, from the key-management screen. Each presented step's
    description names the operation, its tokens' fingerprints and, for a
    PIN or recovery-key step, which secret it asks for; secrets reach
    the worker through `1c` and its `16` frame.

    **Selection.** Request `20` and one operation byte selects a storage
    operation: `01` selector update, `02` upgrade, followed by its shape
    and key count ("The upgrade" below), `03` upgrade cancellation, and
    protector management ("Protector management" below): `04` FIDO2
    token addition, followed by its proof byte and its role byte, `05`
    FIDO2 token removal, `07` PIN-change request, `08` TPM-clear request
    and `09` TPM re-enrollment request, each followed by its proof byte,
    and `06` recovery-key removal, which takes no operand. A proof byte
    is `01` for the volume's recovery key or `02` for a disk security
    key, and a role byte `01` fido2-primary or `02` fido2-recovery. `0a`
    Secure Boot enrollment is assigned here, and its operands and steps
    are fixed by its own increment's documentation commit, before its
    code, under this framing; until then it decodes and is refused as
    this build's. Every build decodes the operand bytes of `20 02` and
    of `20 04` to `20 09` first: an operand outside its range, a missing
    operand or a trailing byte after any selection is a protocol
    violation that ends the generation, as a malformed `1b` is, before
    the rule below answers a well-formed one. `20` needs completed
    preparation and an empty slot, as `1b` does, or ends the generation;
    it answers `a0 01` and the operation's 32-byte nonce, started, or
    `a0 00`, refused in this build. `a0 00` comes first: an operation
    this build refuses, and every well-formed `20` in a production
    build, is answered `a0 00` before preparation or the slot is
    checked, so such a `20` never ends the generation. The private
    channel routes `20` to the paired session beside `10` to `1f`, and
    routes `21`, the console power choice ("Physical console power"),
    there too, and no request from `22` up: `22` to `24` are the program
    authority's ("Application requests"). Polls `11`, cancellation `15`,
    presentation `13`, commit `14` and the secret request `1c` are a
    login operation's, with the same answers (`91 0b` while root waits
    for the worker's baseline, `91 03` to `91 06`, `91 0c`, `91 0d` and
    `91 0e`; `9c`; `95`).

    **Disk proofs.** Request `1b 0b P` followed by a first enrollment
    (`08 01`, `08 02`), an addition (`09`) or a removal (`0a`, the count
    and slots), each as `1b` spells it, selects that login operation
    authorized by a disk proof instead of an enrolled key: `P` is `01`
    for the volume's recovery key and `02` for a disk security key, any
    other refused. An unlock has no such form. It is a login operation
    in every other respect: the `9b` answers, the slot, the polls and
    the ends. Root reads the protected marker at the start, as for `84`
    and `85`, a read failure counting as present, and without it ends
    the operation with `86`, a disk proof on a machine that is not
    protected, before any worker starts. Otherwise root starts the login
    worker, which sends its baseline (`18`), then the storage worker,
    which sends its own; root builds the first description (consent
    tags 13 to 15, the proof's first step, with the disclosure's key
    where the operation discloses) from both and sends it to the
    storage worker alone. The storage worker presents the proof's
    steps, takes the recovery key or the disk key's PIN, verifies the
    proof and tests the keyslot it opens, then sends `14` and exits; it
    writes nothing and has no commit round. Only after observing that
    success and that exit does root send the login worker its first
    frame, the description of the proof's last step, which the worker
    admits without presenting it (`td-secret/DESIGN.md`, "Login-key
    worker"); the operation then continues in the login worker, whose
    next step is a new key's connect or, for a removal, the commit round
    over that proof step. The login worker's first-frame wait is
    therefore bounded by its operation deadline rather than a frame
    time. A failure, cancellation or deadline at any point stops and
    reaps both children before the end is reported: the login worker is
    killed, and the storage worker's channel is closed first, which its
    keyslot test's child reads as cancellation, then the worker is given
    up to one second (`GRACE`) to remove its header copy and exit
    before it is killed; teardown and drop do the same. Root reads the
    volume from the storage worker's baseline: no encrypted td volume,
    or one the worker refused, ends the operation `20`, an upgrading
    one, or one whose baseline does not offer the chosen proof (no
    `recovery-key` marker, or no fido2 token), `21`. `proof.rs` holds
    the two workers in order: the login worker until its baseline, the
    storage worker through the proof, then the login worker resumed. Root admits each
    step the storage worker presents against both baselines: a disk
    key's fingerprint must name a fido2 token of the volume's baseline
    and no key of the login baseline, and its retries be one nonzero
    byte, as an authorize step's are; the
    recovery-key step needs a `recovery-key` marker in that baseline. A
    `1c` for the recovery-key step must hold exactly 48 ASCII digits,
    which root forwards unparsed; anything else is a protocol violation
    that ends the generation, since the compositor's field admits
    nothing else. The ceilings (amendment 6) count the proof as one key
    ceremony: a removal by disk proof 120 seconds, an addition 240, a
    first enrollment 240 for one key and 360 for two, each plus the
    reading allowance where it discloses, so `LOGIN_LONGEST` becomes
    480. Only a build with `storage::OPERATIONS`, a test build until
    ENCRYPTION.md's item 9, decodes a disk-proof selection; elsewhere
    one is a protocol violation, and 8d's interim refusals stand. In a
    test build a first enrollment on a protected machine without a proof
    still ends with `84`.

    **Consent tags.** Tags 13, 14 and 15 are a first enrollment, an
    addition and a removal authorized by a disk proof: each is tag 8's,
    9's or 10's value with one proof byte after the account, `1` the
    recovery key or `2` a disk security key, any other refused. Three
    steps join the table above: recovery key (9, no fields), disk
    identify (10, no fields) and disk authorize (11, fingerprint and
    retries), the fingerprint being the first four bytes of SHA-256 over
    the credential ID a fido2 token of the header names. The proof steps
    replace identify and authorize: recovery key alone, or disk identify
    then disk authorize, as `P` chose, followed for an enrollment by
    each new key's connect, create, prove, repeat and probe, for an
    addition by the new key's, and for a removal by nothing, its last
    proof step being `Last`. An enrollment's first step is its proof's
    first, which then carries the disclosure and awaits approval, and
    no connect does; a removal leaving one key discloses on its proof's
    first step. Tags 8 to 10 never carry a proof step, and 13 to 15
    never identify or authorize. Recovery key and disk authorize ask for
    a secret (`asks_pin`), the first the recovery key, the second the
    disk key's PIN. The rows name the operation as tags 8 to 10 do, then
    `AUTHORIZED BY DISK RECOVERY KEY` or `AUTHORIZED BY A DISK SECURITY
    KEY`, each within `PROMPT_COLUMNS`; the recovery-key step shows
    `TYPE THE DISK RECOVERY KEY`, disk identify `CONNECT ONE DISK
    SECURITY KEY ONLY`, and disk authorize `DISK SECURITY KEY` with its
    fingerprint, then a PIN
    step's two rows. The widest such value, a removal of eight keys at
    its disk authorize step, is 99 bytes.

    Tag 16 is the selector update: right after its tag byte one form
    byte, `1` direct on a device-bound volume, `2` direct on a
    FIDO2-primary volume or `3` staged on a TPM-primary volume, any
    other refused, then the 64 lowercase hexadecimal bytes of the running
    deployment's manifest ID, which is `current`'s: 110 bytes in all. It
    has one step and no step byte: its description is presented once,
    Enter approving that presentation, and it is `Last`. Its rows are `UPDATE
    THE BOOT SELECTOR ON DISK`, `FROM THE RUNNING SYSTEM` and the ID's
    rows as tag 5 shows them, then the form's two: `THE NEXT BOOT ASKS
    FOR THE DISK PIN` and `AND COMPLETES THE UPDATE` (3), `THE NEXT BOOT
    NEEDS THE RECOVERY KEY` and `AND A CONFIRMED RESEAL` (1), or `EVERY
    BOOT ASKS FOR A SECURITY KEY` and `AS IT DOES NOW` (2); then the
    approval rows.

    Tag 17 is the upgrade. Right after its tag byte come the big-endian
    u32 account, which must equal the owner; the shape byte, `1` TPM
    primary or `2` FIDO2 primary; the new key count, 0 to 2 for the TPM
    shape and 1 to 2 for the FIDO2 shape; and the 64 lowercase
    hexadecimal bytes of the running deployment's manifest ID, as tag 16
    carries it. Then the step: one key byte, `0` for the review and 1
    through the count for that new key's ceremony, the step byte and
    the step's fields. A new step joins the table above, review (12, no
    fields), which awaits approval; the key steps
    are connect, create, prove, repeat and probe with their login
    fields. The only order is the review, then for each new key in turn
    its connect, create, prove, repeat and probe; the last step is the
    review when the count is 0 and the last key's probe otherwise, and a
    repeat or probe names the credential its prove named. The widest
    value, a prove or repeat step, is 122 bytes, and the review 117. The
    rows are `PROTECT THE DISK: TPM AND PIN` or `PROTECT THE DISK:
    SECURITY KEYS`, then `NEW RECOVERY KEYS: N` (TPM) or `NEW PRIMARY
    KEYS: N` (FIDO2), then the step's. The review's are the ID's rows
    as tag 5 shows them and the disclosure ENCRYPTION.md's phase 1
    requires before consent, `THE NEXT BOOT REWRITES THE DISK`, `ONE
    READ AND ONE OR TWO WRITES`, `OF THE WHOLE VOLUME`, `IT NEEDS MAINS
    POWER`, `THE MACHINE IS UNUSABLE UNTIL DONE`, `IT ASKS FOR THE
    RECOVERY KEY` and `WHICH STAYS THE RECOVERY PROTECTOR`, then `AND
    FOR A NEW PIN` (TPM, no new keys), `AND FOR THESE KEYS AND A NEW
    PIN` (TPM) or `AND FOR THESE KEYS` (FIDO2), then for the FIDO2 shape
    its trade-offs, `AN ALTERED BOOT CAN TAKE THE DISK`, `FROM ONE USE
    OF A KEY AND PIN`, `WHILE A FIRMWARE TPM GUESS COUNT` and `CAN ROLL
    BACK WITH ITS FLASH`, and last the approval rows. A key
    step's are a first enrollment's, `NEW KEY K OF N` and the step's own
    rows, but for the create step's `CREATE A DISK CREDENTIAL`, without
    the ID, which its bytes still bind and the review showed. Every row
    but the ID's, which wraps as tag 5's does, is within
    `PROMPT_COLUMNS`, and none uses an apostrophe or a parenthesis.
    Create, prove and repeat ask for the new key's PIN (`asks_pin`).

    Tag 18 is the upgrade's cancellation: nothing after its tag byte, 45
    bytes in all, one step and no step byte, `Last`, as tag 16's. Its rows are `CANCEL PROTECTING THE DISK`, `THE
    NEXT BOOT MAY NEED` and `THE RECOVERY KEY AND A RESEAL`, since the
    selector phase 1 wrote may no longer match the device-bound seal,
    then the approval rows.

    **Tags 19 to 24** are protector management's, one per operation
    byte:

    | Tag | Operation | Its own fields |
    | --- | --- | --- |
    | 19 | `04` addition | role byte: `1` fido2-primary, `2` fido2-recovery |
    | 20 | `05` removal | the volume's tokens and the chosen position (below) |
    | 21 | `06` recovery-key removal | none, and no proof byte |
    | 22 | `07` PIN-change request | pending byte (below) |
    | 23 | `08` TPM-clear request | pending byte |
    | 24 | `09` re-enrollment request | pending byte |

    Each value starts, right after the tag byte, with the big-endian u32
    account, which must equal the owner. Then comes the proof byte, `1`
    the recovery key or `2` a disk security key, as in tags 13 to 15;
    tag 21 has none. Then come the tag's own fields, the step byte and
    the step's fields. A later step repeats every field before its step
    byte.

    Tag 20's own fields, in order:

    1. shape: `1` TPM primary, `2` FIDO2 primary;
    2. marker: `1` when a `recovery-key` marker names a keyslot, else
       `0`;
    3. tpm-pin: `1` when a tpm-pin token naming a keyslot is on the
       header, else `0`;
    4. the count of FIDO2 protectors, the fido2 tokens naming a
       keyslot, 1 to 4;
    5. that many protectors in token-number order, each its role byte
       and fingerprint;
    6. the chosen position: `0` at the choose step, 1 through the count
       at every later step.

    In the TPM shape every listed token is fido2-recovery. In the FIDO2
    shape the tpm-pin byte is `0`. Other values refuse.

    The pending byte of tags 22 to 24 is `0` when the header holds no
    `td-request` token, or `1` to `3` for the pending request's
    operation in tag order: PIN change, TPM clear, re-enrollment. The
    new request replaces it.

    **The choose step.** A new step joins the step table above, beside
    tags 13 to 15's: choose (13, no fields). It asks for an entry
    (`asks_pin`), one digit naming the token to remove. Its successor
    sets the chosen position, which no later step changes. The chosen
    position must name a removable token (`Removal::removable`):

    - in the TPM shape, any token while the tpm-pin byte is `1`, and
      otherwise only one whose removal leaves another FIDO2 token, so
      the recovery key is never left alone;
    - in the FIDO2 shape, one whose removal leaves a fido2-primary token
      and either two FIDO2 tokens or the marker.

    Tag 20's disk authorize step must name a token other than the chosen
    one.

    **Step orders.** The proof steps are tags 13 to 15's: the recovery
    key alone, or disk identify then disk authorize, as the proof byte
    chose. Tag 21 always uses the second.

    | Tag | Steps, in order | `Last` |
    | --- | --- | --- |
    | 19 | the proof steps, then the new key's connect, create, prove, repeat and probe | probe |
    | 20 | choose, then the proof steps | the last proof step |
    | 21 | disk identify, which carries the disclosure and awaits approval, as a login disclosure step does; then disk authorize | disk authorize |
    | 22 to 24 | the proof steps | the last proof step |

    **Widest values.**

    | Tag | Widest step | Bytes |
    | --- | --- | --- |
    | 19 | prove, repeat or disk authorize | 57 |
    | 20 | disk authorize, with four tokens | 81 |
    | 20 | choose, with four tokens | 76 |
    | 21 | disk authorize | 55 |
    | 21 | disk identify | 50 |
    | 22 to 24 | disk authorize | 57 |

    **Rows.** A description shows, in order: the operation's rows below;
    for tags 22 to 24, the pending row; the proof's row, `AUTHORIZED BY
    DISK RECOVERY KEY` or `AUTHORIZED BY A DISK SECURITY KEY`, tag 21
    always the second; then the step's rows, as tags 13 to 15 show a
    proof step and tag 17 a new key's step, with `CREATE A DISK
    CREDENTIAL` at create; tag 19's connect step after a disk key's
    proof first shows `REMOVE THE AUTHORIZING KEY`, as tag 14's does.
    Last, tag 21's identify step shows the approval rows. Every
    row is within `PROMPT_COLUMNS` and uses no apostrophe, comma or
    parenthesis. The one exception is a PIN step's fixed row `ENTER ITS
    PIN, THEN TOUCH THE KEY`.

    | Tag | The operation's rows |
    | --- | --- |
    | 19 | `ADD A DISK SECURITY KEY`; `AS A PRIMARY KEY` or `AS A RECOVERY KEY` |
    | 20 | `REMOVE A DISK SECURITY KEY`; at the choose step, the list below; at every later step, the chosen token as `REMOVES 2: RECOVERY KEY 1a2b3c4d` (its position, role and fingerprint, since two tokens may share a fingerprint) |
    | 21 | `REMOVE THE DISK RECOVERY KEY`; at its identify step, the disclosure `AFTER THIS ONLY THE SECURITY KEYS`, `ON THE DISK CAN OPEN IT` and `LOSING THEM ALL LOSES THE DATA`; the approval rows come last, after the step's |
    | 22 | `REQUEST A DISK PIN CHANGE`, `THE NEXT BOOT BY THE PIN`, `ASKS FOR A NEW PIN` |
    | 23 | `REQUEST A TPM CLEAR`, `THE NEXT BOOT ASKS TO CLEAR`, `THE TPM AND ALL ITS KEYS`, `BOOTS AFTER NEED A SECURITY KEY`, `OR THE RECOVERY KEY` (the two boots that take the lockout and offer the reseal) |
    | 24 | `REQUEST TPM RE-ENROLLMENT`, `THE NEXT BOOT BY A SECURITY KEY`, `OR RECOVERY KEY ASKS TO RE-ENROLL` |

    The choose step's list is `CHOOSE THE KEY TO REMOVE`, one row per
    token, then `TYPE ITS NUMBER`. A token's row is its position,
    `PRIMARY KEY` or `RECOVERY KEY`, and its fingerprint (`2: RECOVERY
    KEY 1a2b3c4d`), followed by ` - NEEDED` when the token is not
    removable. The pending row, shown when the pending byte is not `0`,
    is `REPLACES THE PENDING PIN CHANGE`, `REPLACES THE PENDING TPM
    CLEAR` or `REPLACES THE PENDING RE-ENROLLMENT`, the last exactly 34
    columns.

    Tag 25 is reserved for operation byte `0a`, and decodes as unknown
    until its increment fixes it.

    **The selector update.** At `20 01` root starts the storage worker,
    which sends its baseline: the volume's state, its tokens and the
    running deployment's standing. Root derives the form from that state
    (device-bound 1, FIDO2 primary 2, TPM primary 3) and, before any
    description, ends the operation with the storage worker's own kinds
    from its baseline: `20` for no encrypted td volume or a header td's
    reader refuses, `21` for an upgrading volume, whose upgrade writes
    the direct form itself, and `22` for a running deployment that is
    not an acknowledged `current` with no pending attempt. An
    unencrypted volume takes no selector update: none of ENCRYPTION.md's
    forms names one. Otherwise it sends tag 16's description; the worker
    repeats it as its one step, which root presents (`91 04`) and
    acknowledges once the compositor's receipt arrives, sent only after
    Enter approves it; then
    the commit round, in which the worker stages or writes the pair
    (ENCRYPTION.md, "Selector update"). Its ceiling is 240 seconds,
    fixed at the selection: 120 for the worker's reads and the approval, which
    the worker presents only once its reads are done, and 120 for the
    writes, which the worker begins only with 120 seconds of its
    deadline left (its commit margin). Root enforces only the whole 240
    seconds, accepting the approved receipt until then; the split is
    the worker's, which answers a receipt with under 120 seconds left
    with `08` before its commit invitation. After root's `13` an outcome
    other than `14` and a successful exit is uncertain (`91 0e`), as a
    login write's is, but for the worker's `28` CHANGED with its
    successful exit: the worker sends it only from its checks after
    `13` and before its first write, so it is definite and root reports
    it as `91 0d 28 00`. Success shows the form's result
    (`td-compositor/DESIGN.md`, "Storage operations").

    **The upgrade.** ENCRYPTION.md's phase 1 ("Re-encrypting upgrade").
    `20 02 S N` chooses the shape `S`, `01` TPM primary or `02` FIDO2
    primary, and the new key count `N`, 0 to 2 for the TPM shape, each a
    fido2-recovery token, or 1 to 2 for the FIDO2 shape, each a
    fido2-primary token; a volume takes more by token addition (`04`)
    once protected. Root reads the login state at the start, as it reads
    the protected marker for a disk proof, and ends the operation with
    `87` unless it reads enrolled, before any worker starts: an upgrade
    makes login-key consent the volume's, which an unenrolled or
    unavailable state cannot give. Otherwise it starts the storage
    worker, whose baseline must show a device-bound volume (`20` for no
    encrypted td volume or a refused header, `21` for any other state)
    and an acknowledged `current` with no pending attempt (`22`); root
    ends the operation with that kind before any description. It then
    sends tag 17's review; the worker repeats it, after its reads, as its
    first `10`, which root presents (`91 04`) and acknowledges once the
    compositor's receipt arrives, sent only after Enter approves it.
    The worker's reads may end it first,
    with `20`, `21`, `23`, `24`, `28` for a header that is not the one
    its baseline read, definite since nothing is written, or, for the
    TPM shape, `27`, which root forwards as it forwards a selector
    update's `23` and `24`. Each
    later step is a first enrollment's key ceremony, which the worker
    presents and root admits as `admit_login_step` admits one
    (`Request::admit_upgrade_step`: tag 17's successor rule, a created
    fingerprint no fido2 token of the baseline names, nor an earlier new
    key's created credential, and no credential reported before prove),
    taking a PIN step's `1c` as a login step's.
    After the last step's acknowledgement comes the commit round, in
    which the worker writes phase 1's header commits and the selector
    (`td-install/DESIGN.md`, "Storage operation worker"). The ceiling is
    fixed at the selection: 240 seconds and 120 more for each new key,
    240, 360 or 480, the reads and the review taking the first 120, each
    key ceremony its 120 and the writes the last 120 behind the worker's
    commit margin, as for the selector update; `LOGIN_LONGEST` (480)
    remains the largest operation. The outcome rules after `13` are the
    selector update's: anything but `14` and a successful exit is
    uncertain (`91 0e`), but for `28` with a successful exit (`91 0d 28
    00`).

    **The cancellation.** `20 03` takes no operand. Root starts the
    storage worker, whose baseline must show an upgrading volume (`20`
    as above, `21` for any other state), and sends tag 18; the worker repeats it once its read of the header
    finds the upgrade cancellable, still in state U0 (`21` otherwise,
    `20` for a header gone or refused, or `28`, definite, for one that
    is not the baseline's), which root forwards, and the presentation,
    receipt and commit round are the selector update's. Its writes
    remove the `td-upgrade` token, then the pending fido2 tokens, each a
    key-less header commit. Its ceiling is 240 seconds, split as the
    selector update's, and its outcome rules are the same.

    **Protector management.** ENCRYPTION.md's "Protector management".
    At `20 04` to `20 09` root starts the storage worker. Its baseline
    must show a protected volume, `03` or `04`; otherwise root ends the
    operation with `20` (no encrypted td volume, or a refused header) or
    `21` (any other state). Root also ends it, before any description,
    with `21` and a detail when the baseline cannot take the operation:

    | Detail | Meaning |
    | --- | --- |
    | `00` | The proof is not offered: the recovery key without a marker, a disk key without a FIDO2 protector, or a removal by disk key without two |
    | `01` | An addition the shape cannot take: a role it does not take (the TPM shape takes fido2-recovery only), or no room (four fido2 tokens, or in the FIDO2 shape three of that role; every fido2 token counts, an orphan too, as td-protector's reader counts them) |
    | `02` | A removal with no removable protector (`Removal::removable`), or a recovery-key removal outside the FIDO2 shape, without the marker, or without two FIDO2 protectors, one of them fido2-primary |
    | `03` | A request without the protector it acts on: a PIN change without the TPM shape, a tpm-pin token naming a keyslot and a lockout token, which anchors the new seal; a TPM clear without a lockout token; a re-enrollment outside the TPM shape, or with neither a tpm-pin token naming a keyslot nor a lockout token, since it would find nothing stale |

    A FIDO2 protector is a fido2 token naming a keyslot: an orphan opens
    nothing. When the operation's own detail also applies, root sends
    that one, not `00`.

    Otherwise root builds the first description from the baseline: the
    proof's first step, the choose step for a removal, or tag 21's
    identify step, awaiting approval as a login disclosure does.
    Consent's `Protected` holds the baseline's protected
    volume:
    `refusal` gives the detail, `first` the description, and `admits`
    is the worker's check that root's description is that one. The
    worker repeats it as its first `10`, before any token I/O.

    Root admits each later step as a disk proof's
    (`Request::admit_protector_step`):

    - by the tag's successor rule;
    - a disk key's fingerprint must name a FIDO2 protector of the
      baseline, for a removal one other than the chosen token (the
      worker ends with `04` detail `01` otherwise), with retries of one
      nonzero byte, and a removal's list must be the baseline's
      protectors in order;
    - a recovery-key step only where the baseline has the marker;
    - for an addition's new key, what `admit_upgrade_step` admits of a
      new key, its fingerprint no FIDO2 protector's.

    A choose step's `1c` must hold exactly one ASCII digit naming a
    removable token (`Removal::entry`). Root parses it, forwards it to
    the worker as a one-byte `16` frame, and requires it as the next
    step's chosen position (`admit_protector_step`'s `chosen`); the
    compositor and the worker check the same. Anything
    else is a protocol violation that ends the generation, since the
    compositor's field admits nothing else. A recovery-key step's `1c`
    is a disk proof's.

    After the last step's acknowledgement comes the commit round, in
    which the worker writes (`td-install/DESIGN.md`, "Storage operation
    worker"). The outcome rules after `13` are the selector update's.
    The ceilings are fixed at the selection:

    | Operation | Ceiling | Split, in seconds |
    | --- | --- | --- |
    | Addition | 360 | proof 120, the new key's ceremony 120, writes 120 |
    | Removal | 360 | the choose step's reading 120, proof 120, writes 120 |
    | Recovery-key removal | 360 | the disclosure's reading 120, proof 120, writes 120 |
    | Each request | 240 | proof 120, writes 120 |

    The writes run behind the worker's commit margin, as for the
    selector update, and `LOGIN_LONGEST` (480) remains the largest
    operation. As for a disk proof, a stop closes the worker's channel
    and kills it only after `GRACE`. Its proof's keyslot test holds a
    header copy.

    **Kinds.** An end carries the worker's `15` kind, from the storage
    worker's table in `td-install/DESIGN.md` for its own, or root's:
    `80` cancelled, `86` above, `87` an upgrade without an enrolled
    login state, and `08` and `10` as for a login operation. Root admits
    an upgrade's and a cancellation's worker kinds only where they
    stand: the reads' while root's description awaits the worker's first
    `10`, a new key's while a key step is current before the commit, and
    `0e` and `28` after it; any other ends the operation as `10`.
    Protector management's kinds stand where a disk proof's do: `20`
    before the commit and a definite `28` anywhere; the proof's `01` to
    `07`, `25` and `26` from its first presented step to the commit,
    never at a choose step; a removal's `04` detail `01` there too; a
    new key's `11` while its step is current; and `0e` after the
    commit. The compositor accepts exactly the kinds each selection can
    produce where it stands, as for a login operation, and anything
    else ends the generation.

    **The login state.** The `9a` answer gains one byte after the
    admission byte: `01` when root read the protected marker for this
    answer, or could not read it, and `00` when it is absent. The
    compositor offers the disk-proof choices only while the last answer
    reads `01`; root decides from its own read at the operation's start,
    so the byte is a display choice and authorizes nothing. A live boot
    answers `00`, having no installed volume, and a compositor built
    without disk proofs ignores the byte. Both peers shipped it
    atomically, still `TDLA003`, with 8e's disk-proof commit.

    Until ENCRYPTION.md's item 9 a production build answers every `20`
    with `a0 00` before starting anything (`storage::OPERATIONS`, false
    there as `login::WRITES` was before activation), so nothing in
    production starts the storage worker; test builds set it, so host
    fixtures drive every implemented operation.

TOKEN-LOGIN.md, "Placement", owns the rule that only physical input starts
a login operation.

### Login-key operation supervision

`login.rs` is root's side of the worker's frames (`td-secret/DESIGN.md`,
"Login-key worker"). It owns one fixed `/bin/td-secret login-operation
--uid 1000` child, launched as the token workers are (empty environment,
`/` cwd, null stdout and stderr, a private stdin socketpair, the
authority's process group), over their nonblocking framed endpoint and
its five-second partial-frame deadline. It generates the operation's
nonce and accepts no caller-selected executable, path, account or
argument. The paired requests are:

| Request | Response |
| --- | --- |
| `1a` | `9a`, then the state: `00` unenrolled; `01`, a slot count of 1 to 8 and that many four-byte fingerprints in canonical slot order; or `02` and the cause `0a`, `0b` or `0c`, TOKEN-LOGIN.md's failure kinds, or `0d`, an unenrolled protected machine; then a length byte of 1 to 32 and the primary username, a length byte of 0 to 63 and the hostname, the revocation byte, `00` to `03`, the admission byte, `00` or `01`, and the protected byte, `00` or `01` (amendments 1, 7, 9 and 10) |
| `1b 07` unlock; `1b 08 01` or `1b 08 02` one- or two-key first enrollment; `1b 09` addition; `1b 0a N` and N slots, each a position and a four-byte fingerprint, positions strictly increasing within 1 to 8 | `9b 01` and the 32-byte nonce: started |
| `1c`, one length byte, the current step's canonical description, then the secret: 1 to 63 printable ASCII bytes, a PIN step's at least 4 (amendment 10's secrets below) | `9c 00` PIN queued; `9c 01` the operation had already ended, or root's deadline has passed, which ends it as TIMEOUT: the PIN is dropped |

Amendment 10 extends these rows: `1b 0b P` and an enrollment, addition
or removal selects a login operation authorized by a disk proof, with
the `9b` answers; `20` and an operation byte selects a storage
operation, answered `a0`; a recovery-key step's `1c` carries the 48
digits; and a choose step's `1c` carries one digit. So `1c` decodes 1
to 63 printable ASCII bytes, and the supervisor holds them to the
current step's profile: shorter than four bytes on a PIN step ends the
generation, as a malformed `1c` does. A `1c` for an operation that has
already ended is dropped first, as any secret for it is.

`1a` needs completed preparation, or it ends the generation, but neither
needs nor takes the operation slot. The compositor admits only an answer
of exactly that shape, with a username under `primary_account`'s rules
and a hostname empty or under `Hostname::parse`'s; anything else, a
revocation byte past `03`, an admission byte past `01` or a `01` in any
answer but the generation's first included, is a protocol violation
that ends the paired generation.

`1b` needs completed preparation and an empty operation slot, as `12`
does, or it ends the generation. The login operation then holds the
single slot until its terminal status is polled: no unlock, enrollment,
write, installation, inspection or second login begins beside it, and it
begins beside none of them. Poll `11` answers:

- `91 0b`: waiting for the worker's baseline, with no description yet;
- `91 03`, `91 04` or `91 05` and the current description: the worker at
  work, a presentation required, a commit required;
- `91 0c` and the description: the presented PIN step waits for `1c`;
- `91 06` and the final description: the worker sent `14` and root
  observed its successful exit;
- `91 0d KIND DETAIL`, then the current description if one exists: the
  operation ended and wrote nothing;
- `91 0e KIND DETAIL` and the description: uncertain (amendment 2).

KIND is the worker's failure kind with its detail byte, zero when the
kind has none, or root's own: `08` TIMEOUT for root's deadline or an
expired acknowledgement window; `09` NO RECORD when an unlock, addition
or removal meets an unenrolled baseline; `10` INTERNAL for a malformed,
unexpected or out-of-order frame, a stale acknowledgement or PIN, a lost
channel or a failed exit; and six kinds the worker never sends, `80`
cancelled, `81` an addition to a record that already holds eight keys,
`82` a first enrollment over an enrolled record, `83` removal slots
that are not the record's, and on a protected machine (amendment 9; the
marker and `/run/td-admit` read at the operation's start, a read
failure counting as present) `84` a first enrollment, which needs the
disk proof this build does not offer, and `85` a removal of every key.
A terminal status is delivered once and retires the operation. `15` with
the operation's nonce cancels it as it does a secret operation (`95 00`,
or `95 01` completed, `95 02` already ended); a stale acknowledgement or
PIN, as for secret operations, also ends the generation.

Root reads the baseline (`18`) first, and the baseline, not the request,
fixes the counts. An unlock, addition or removal against an unenrolled
baseline, an enrollment against an enrolled one, an addition at eight
keys, which consent cannot encode, and a removal set `begin_login`
refuses each end before root sends a description, so the worker reaches
no presentation or token I/O. Otherwise root sends `begin_login`'s first
step and requires the worker's first `10` to repeat it exactly; it admits
each later `10` with `admit_login_step` against the baseline, taking a
prove step's fingerprint as the created credential and keeping it through
repeat and probe. A `12` is accepted only after a step admitted `Last`
and only for that exact description, and `14` only after root's `13`.
Any other frame, a malformed baseline or failure frame, a second
baseline, a frame while root waits for the compositor, a frame before
root has written a PIN it accepted, an UNCERTAIN failure before a
write's `13` or in an unlock, or any byte after `14` included, ends the operation as INTERNAL;
the worker's typed `15` ends it with its kind at any point before `14`.
Success needs `14`, then the worker's successful exit and the clean end
of its channel. The end may lag the exit. Another process may hold a
copy of the worker's side: a descendant of the worker, or, in a
multi-threaded host such as the test harness, a process a sibling
thread forked while root still held that side, until it execs. So after
a successful exit root waits up to one second (`GRACE`) for the end. A
channel still open then ends the operation as INTERNAL. Root answers
the compositor's `13` and `14` with `11` and `13` as for secret
operations, within three seconds of the invitation against the
worker's five. A disclosure step (TOKEN-LOGIN.md increment
5's A4) is the exception: root waits for the compositor's `13`, and the
worker for root's `11`, until the operation's deadline, since no token
I/O is open while the person reads and approves. A disclosure
step's `13` that arrives at or after root's deadline ends the operation
as TIMEOUT, and the generation continues, as a late `1c` does; one for
an operation that has already ended is answered and dropped, as such a
`1c` is. Both are defence in depth: the compositor's attempt lifetime is
the same ceiling and counts from before root's `1b` started, so it
normally ends first, cancelling with `15` and showing TIMED OUT. Root
reads the clock once per receipt, so one that crosses the deadline while
root handles it is either acknowledged or TIMEOUT, never a stale
acknowledgement. After a
PIN step's `11`, root sends one `16`
frame from the `1c` that names that step. The PIN stays in a clearing
owner from decoding to the queued frame; root zeroes the received
request and the written frame and never formats it, `Debug` included.
The shared channel reads every request into a buffer it zeroes when the
receive fails, partway or after the whole frame, so a refused `1c` leaves
no PIN behind either. A `1c` that arrives at or after root's deadline is
answered `9c 01`: the operation ends as TIMEOUT and the generation
continues, since a person may submit just after root's last poll.

A cancel, and every refusal or violation, kills and reaps the worker and
runs no `lock-session`: a login operation releases nothing, so nothing
relocks and an application-store release is untouched. Generation
teardown reaps a live login worker before its own cleanup, as it does
every worker. Root's deadline is the selected operation's ceiling,
amendment 6, fixed when it spawns the worker; waiting for a PIN spends
that deadline, not the acknowledgement window. The first poll that finds
the deadline passed makes one last look. It first zeroes and discards
every frame root still has queued, a PIN included, so nothing more
reaches the worker; it then reads what the worker has already written,
without blocking, and checks once for the worker's exit. Only a `14`,
successful exit and clean end all seen by then complete the operation,
and a worker's typed `15` seen then keeps its kind; anything else,
including a malformed frame or an expired frame window in that look,
ends it as TIMEOUT, uncertain after a write's `13`.
The worker starts a commit round only with five seconds of its own
deadline left and refuses a late acknowledgement, which makes a late
`14` unlikely but cannot exclude one.

Since activation (TOKEN-LOGIN.md increment 5's A5) every production
build starts the worker for each `1b`: an enrollment, addition or
removal as the `K` screen selects it, and an unlock, which ends as NO
RECORD while no record exists and which the production compositor sends
only from its lock surface while the state is enrolled. `9b 00`, the
refusal before activation, is gone with `login::WRITES`; the compositor
takes any `9b` but `9b 01` and a nonce as a protocol violation. Only
this supervisor's `91 06` for an unlock's committed last step leaves the
lock surface.

Host tests drive the controller and the paired Session against scripted
exec children playing the worker's frames: each operation's exact step
sequence, the PIN relay, success only with `14` and a successful exit,
each refusal from the baseline with the worker killed before any step,
uncertainty after a write's `13` for a typed failure, a lost channel, a
failed exit, root's deadline and a cancel, an unlock's failure after `13`
staying a failure, malformed and out-of-order frames, stale
acknowledgements and PINs, the slot, teardown and production's
admission of every operation to its worker; root's key on exactly the
disclosure steps, a disclosure's receipt after ten seconds still
proceeding, the ceilings with and without the reading allowance and
their narrowing at the first step, a receipt judged at one reading of
the clock, and the defensive late receipt ending as TIMEOUT through the
Session with the generation going on. The ignored root fixture
`session::tests::root_login_supervision_meets_the_production_worker`,
qemu-secret's `supervise-login` authority case, runs the production
worker through the real Session in the disposable root VM: a missing
login directory's typed refusal before any baseline, NO RECORD from an
unenrolled baseline for an unlock and an addition, and a one-key
enrollment whose first step reaches the worker, which refuses it as
VERSION since the guest has no `/run/td-volume` and so no retained
deployment's marker reads. No token takes part; the worker's own guests
cover token I/O with simulated root acknowledgements. The same case
reads `1a` between those operations: the missing directory's damage,
unenrolled once it exists, and a record name the production
`inspect-login` helper reads as a damaged record, held by the cache
until the next operation ends.

Request `1a`'s host tests (`tests/login_status.rs`, and the paired
Session's in `tests/session.rs`) run the predicate over a temporary root
owned by the test and play the helper with child fixtures: an unenrolled
or damaged directory that never runs it, a missing, failing, malformed,
oversized and two-second-stalled helper, each unreadable, one and eight
keys and a damaged record with their exact `9a` bytes, the cache and its
refresh after a login operation's end, the helper's pause while
unreadable, the live boot, every hostname rule and the username rule;
and through the Session, `1a` refused before preparation, answered beside
a login operation without its slot, and refreshed only by a login
operation's end or request 19.

Request 19's host tests (`tests/session.rs`, `tests/deployment.rs`) drive
a real admitted intake over a deployment directory whose archive the
test builds with `login_tier.rs`'s fixtures, not the system recipe's
(whose marker, written since C10b, a recipe test pins): a marker-less
deployment, one whose marker lists another version, and one listing the
record's, each under an unenrolled machine (always selectable), an
enrolled record of version 1 or 2, and a damaged record, a damaged
directory and an unreadable helper (any marker selectable); a refusal's
exact `99 01`, no description, no operation, the requester's completion
byte `00` and nothing left to select; the cache refreshed by the read;
a deployment whose marker admits the record, refused with `99 01` when
its read's budget is spent and admitted with it unspent, through the
selected update's `give_up` field, which production sets only from
`MARKER_GIVE_UP`, whose two seconds beside the helper's keep the
request inside the receive; and the marker read afresh through the
held directory, never a link, a FIFO, another owner's file or one past
its bound.
