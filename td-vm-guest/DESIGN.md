# Guest development identity helper

`td-vm-guest serve` is a dependency-free, source-built service in the standard
image. `td-svc` launches it with the existing `td-login exec-as tester` path,
after seat setup and networking, requiring seat setup and firstboot. It runs
as UID 1000 in the session cgroup. This mode has no root operation and adds no unsafe
surface. The separate root power mode below has no Git or credential job. A running process is not a claim that a workspace is ready.

The compositor owns the VM carrier and writes one public 32-digit lowercase
hexadecimal instance ID to `/run/td-compositor/1000/vm-git-identity`. The helper
validates that directory and request's owner (993), type and write permissions.
It refuses symlinks, multiply linked records, extra bytes and invalid IDs.
Without an assignment it waits quietly. This is host-selected identity data,
not an arbitrary command or pathname.

The helper holds a private lifetime file lock under
`/home/tester/.local/share/td-vm`. Its immutable `git` directory contains the
instance ID, `id_ed25519` and `id_ed25519.pub`. It creates these in private
staging using the image's fixed `/bin/ssh-keygen`, an empty environment,
bounded output and execution time. Generation and public-key extraction use
closed stdin. Before publication it also signs a fixed local challenge and
verifies the signature against the public key using OpenSSH. Verification
receives only that challenge on stdin; private proof staging is removed after
the check. Extracting the embedded public key alone cannot detect damaged
Ed25519 secret bytes. Private
files are never returned or logged. File and directory syncs precede atomic
publication; interrupted staging may be removed under the lock. Published
state is never regenerated to repair corruption or to bind another identity.
An operator needing another identity creates a fresh VM.

The helper publishes only `TDVM-GIT-KEY-1`, the instance ID and an Ed25519
public key in `/run/td-guest/1000/git-key`. Seat setup prepares the root-owned
0755 `/run/td-guest` and tester-owned 0755 child. The file is 0644; the
compositor can read it but cannot read the private home state. The shared
codec in `td-compositor/src/vm_wire.rs` limits replies to 256 bytes and accepts
exactly one canonical public key bound to the requested ID. It grants no
host authority. The compositor validates the opened response's type, owner,
link count and write permissions before returning it over the carrier.

The helper clears the previous public reply on startup and on a failed
exchange, so a damaged private pair cannot leave a successful reply behind.
It polls every 500 milliseconds, reusing its verified result while
the request and published response remain unchanged. It reports each distinct
failure once. After failure, unchanged filesystem metadata suppresses another
key-tool invocation. Changes to the request, response, their directories,
private key records or tool allow a retry. Directory stamps track identity,
type, owner and permissions, excluding timestamps, size and link count:
the jobs create proof and response children in shared directories and must
not make each other retry. Changing an untracked child alone does not retry
an attempt; change the relevant request or restart the helper. A helper restart
also retries and revalidates the pair. A transient tool failure without a
filesystem change requires restarting the helper. Metadata caching is an
operational optimization, not protection against the state-owning human UID.
The unconfined human UID owns its state and can tamper with it; this does not
claim isolation from that UID. Confined applications do not receive these
paths. The key exchange does not copy host credentials or enroll keys. Workspace
cloning below has its own typed request and completion; terminal launch and
provider authentication remain separate.


Real OpenSSH key tests run in the host preflight with `--include-ignored`.
The compiler-only sandbox runs the pure wire/identity tests; it does not
supply OpenSSH. The target recipe checks its realized static helper's startup
interface, and boot evidence must exercise the standard image's OpenSSH tool.
Tool stderr is discarded deliberately; failure diagnostics expose only the
fixed operation and exit status, without private material or tool output.
The fixed tester/compositor identities are the standard image contract.


The helper follows the target-wide frame-pointer and debug-companion policy.
Its transitive assembly marker includes the Rust runtime as well as glibc
and libgcc. The recipe requires both the companion and marker in its output.


## Private SSH clone provisioning

The service also consumes the compositor-owned `vm-workspace` plan described
in [td-vm/DESIGN.md](../td-vm/DESIGN.md#explicit-guest-clone-provisioning).
It runs as tester and never invokes a root operation. The plan's ID and public
key must match the locally generated, signature-verified private key. The
private `ssh` directory beside `git` contains the immutable canonical plan,
`config` and `known_hosts`, all mode 0600. They are synced before atomic
publication; state ancestors through the existing human home are synced before
Git starts. Existing configuration must match exactly. A changed host key or
profile requires an explicit future migration or a new instance; this helper
does not silently replace trust data in a used workspace.

`td-vm-ssh` is a source-built alias of the same Rust executable. It requires
tester UID 1000, validates the private configuration, clears the environment,
and execs the fixed `/bin/ssh -F` configuration path with Git's argument vector.
Only exact `GIT_PROTOCOL=version=2` is preserved. The configuration selects the
instance key, `IdentitiesOnly`, no agent, batch public-key authentication,
strict host-key checking and the pinned `td-host` host-key alias. System/global
known-hosts additions, host-key updates, proxy commands/jumps, local commands,
agent and network forwarding are disabled by the generated profile. Connect
has a ten-second timeout; server keepalive has a fifteen-second interval and
three-failure ceiling. The unconfined human UID can also invoke ordinary SSH
or edit its own Git configuration; this is not a new isolation boundary
against that UID. Confined application identities do not acquire these paths
or the tester-only launcher through this change.

Git stdout is drained with a 4096-byte retained-output bound. Trusted Git
execution and filesystem work have no absolute completion deadline: a carrier
timeout does not kill workers or delete their staging. Git retains the service
lifetime lock on stdin. Git and its descendants also inherit a separate
private `git-worker.lock` lease on stderr, including workers such as
`index-pack` that redirect stdout. The helper drops its own copy after spawn
and must reacquire through an independent open description after reaping Git
and draining stdout before the attempt can finish. A restarted helper checks
that worker lease before any workspace mutation, so it cannot reclaim staging
while surviving workers finish. Process supervision retains the existing
td-svc process-group contract. Git/SSH diagnostics go to `git-worker.lock`;
each command truncates it only after exclusive acquisition.
The public result exposes only a fixed
operation/error summary. As with retained VM logs, disk-backed diagnostic
size is not yet capped; object transfer and history size follow host Git and
available VM disk limits. No private key or provider credential is sent over
the compositor bridge or included in a template.

Provisioning uses empty trusted hooks/templates, no inherited Git settings,
no automatic maintenance, no recursive submodules, no file transport, and
object verification. It clones without a checkout or shallow history, fetches
and verifies the retained starting commit, sets the author with literal argv
values, and creates a task worktree with relative Git links. Both directories
and their immutable plan are synced and atomically published under
`/home/tester/src/td-vm`. The private `.td-vm.tmp` staging name is reserved for
the helper; failed/interrupted unpublished work there may be removed on retry.
Published work is never removed, recloned, reset or silently rebased. A later
request validates its plan and Git common-directory link and preserves the
human's commits, dirty files, untracked files and other local Git settings.
Clone completion is not proof of an agent-ready toolchain or writable build
store, and it does not yet launch td-term in the task directory.

The service reports each failed observed request/state once and waits for a
changed request/state before retrying. The compositor replaces the request
on an explicit Clone action; a healthy network returning by itself does not
cause an unrequested retry. Status echoes the full plan and is only a record
of that completed attempt. The service clears status on startup and before a
new attempt; the compositor refuses a mismatched plan or malformed status.

The host preflight includes a real disposable SSH server under the test user's
private home, preserving OpenSSH StrictModes. It proves a moved origin main,
explicit retained-ref fetch, atomic relative worktree publication, same-plan
retry preserving edits and new commits, and an ordinary SSH push. Killing the
owned provisioning helper while Git's server is held verifies that surviving
Git/SSH retain the lease until completion; restart then reclaims unpublished
staging. A separate regression lets the Git parent fail while a descendant
keeps stderr and redirects stdout; the same helper must wait for it. A changed retention ref refuses publication and can be repaired for
a same-plan retry. A wrong host-key probe is refused. These fixtures use
host-only fixed Rust test adapters; the standard-image proof exercises the
actual tester-only `td-vm-ssh` alias and source-built Git/OpenSSH.

The combined service-loop regression retains both requests across a damaged
key's signature self-test failure. Each job attempts once, then remains idle
while the other job's temporary proof/response writes change directory
metadata. An explicit clone request retries that job without waking the key
job; a tracked private-key permission change remains observable.


## Fixed root power worker

`td-svc` starts `td-vm-guest power-serve` as a separate root `vm-power` unit
after successful seat setup. It has no arguments beyond that mode and opens
no private home or key state. It holds a lifetime lock in root-only
`/run/td-vm-power`; the ordinary tester helper cannot acquire that lock.
It additionally requires the kernel-owned `/sys/class/virtio-ports` to name
`org.td.vm.1`, scanning at most 64 entries and 129 bytes per name. Unnamed
ports and concurrently removed name attributes are skipped; other discovery
errors refuse the attempt. Without
that carrier or a compositor request it idles, including on non-VM boots.

The root worker validates root-owned, non-writable `/run` and
`/run/td-compositor`, the compositor-owned non-writable runtime directory,
and the opened request's regular-file type, UID 993, single link and absence
of group/other write permission. Final symlinks and blocking special files
are refused; reads are bounded to the fixed record plus one byte. Only exact
`TDVM-POWEROFF-1\n` is accepted. A compromised compositor can request VM
poweroff; the human UID and confined applications cannot author the request.
This is host lifecycle authority on its own VM, not a general elevation or
human authentication interface. No arbitrary service command is exposed.

The worker validates the private root-owned td-svc runtime and launches only
`/bin/td-svc poweroff`, with a cleared environment, cwd `/`, and an inherited
lifetime lease on stdin. It captures at most 128 bytes of stdout and discards
stderr. The source-built client connects to its existing fixed control socket;
the worker's three-second deadline covers connection, reply, and client exit.
On error or timeout it kills and reaps that recorded child. If the worker dies,
the client retains the lease so another worker cannot overlap it. The fixed
client creates no descendants. Only successful exit with exact poweroff
acceptance or already-running poweroff counts as confirmation. No child
command, environment, executable, path or credential comes from the request.
Each observed request inode is attempted once per worker lifetime; an explicit
Stop replaces it to retry. A supervisor/helper restart can replay the request,
which td-svc's monotonic shutdown makes idempotent. Errors are logged; the
compositor's reply acknowledges publication, not this result. The worker stays
supervised and shutdown stops it through the ordinary unit path. No unsafe
surface or dependency is added.

Host fixtures compile a tiny Rust control-client adapter and prove the exact
argument, accepted/rejected/oversized replies, nonzero exit, a client that holds
stdout open, and a real Unix socket connect stalled behind a full backlog.
Timeout reaps the recorded client. The standard image uses the actual shipped
td-svc client. These host-tool fixtures stay out of the compiler-only gate.
