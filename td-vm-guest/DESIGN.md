# Guest development identity helper

`td-vm-guest serve` is a dependency-free, source-built service in the standard
image. `td-svc` launches it with the existing `td-login exec-as tester` path,
after seat setup and networking, requiring seat setup and firstboot. It runs
as UID 1000 in the session cgroup. It has no root operation and adds no unsafe
surface. A running process is not a claim that a workspace is ready.

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
private key records, state directory or tool allow a retry. A helper restart
also retries and revalidates the pair. A transient tool failure without a
filesystem change requires restarting the helper. Metadata caching is an
operational optimization, not protection against the state-owning human UID.
The unconfined human UID owns its state and can tamper with it; this does not
claim isolation from that UID. Confined applications do not receive these
paths. The helper neither copies host credentials nor enrolls keys, clones a
repository, changes a terminal's directory or executes host-supplied commands.
Those later workspace steps must acknowledge their own completion separately.


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
