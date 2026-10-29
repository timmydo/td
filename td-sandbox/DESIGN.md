# td-sandbox

## Contract and first increment

`td-sandbox` manages named development environments for the invoking user.
It creates no account and gains no privilege. td-jail's workspace kind owns
namespace, mount, identity, capability, seccomp and descendant confinement;
APPLICATIONS.md section C is normative for that boundary. The frontend
forbids unsafe code and depends only on td-fs and td-ui. Its executable is
the workspace's immutable entry; the target recipe stages engine's one
shared host-PATH alias implementation too.

The first increment provides create, list, inspect, share, unshare, network,
shell, run, stop and delete. One active session or metadata mutation per name
holds a kernel file lock. `delete` explicitly removes an inactive private
home and all its state; it never deletes a shared directory. No credentials,
profile scripts, environment variables, desktop sockets or working directory
are imported implicitly. A shell starts in the private home. Users may cd
to an explicitly shared directory at its original absolute path.

```
td-sandbox create tools
td-sandbox share tools /home/user/src/project --write
td-sandbox network tools host
td-sandbox shell tools
td-sandbox run tools -- command argument
td-sandbox stop tools
```

The jail is the companion binary beside the launcher, the installed host
companion under ../lib/td, or td-jail on PATH. `--jail PATH` explicitly selects
another build. Failure to launch the jail is fatal, with no unconfined retry.
`./install-apps` installs the host launcher; the desktop image includes the
target-built launcher. This increment uses host tools. A compatibility
runtime supplying conventional dynamic-loader paths and additional shells,
and GUI socket grants, are subsequent increments. No general distribution
installer or package manager is implemented or promised.

## Persistent state and authority

State is under `$HOME/.local/share/td-sandbox/NAME`, owned by the caller and
private. Names contain 1–32 lowercase ASCII letters, digits or hyphens.
Only its `home` directory is mounted into the jail. `policy`, `lock`,
`launch-spec` and `control` remain outside every grant. The policy is a
bounded exact keyfile with network=off/host and at most 30 directory grants.
Writes replace the policy atomically. Existing symlink/private-directory
violations fail. The jail additionally admits each mount source by canonical
path and mount identity, refuses whole-home and reserved-tree aliases, and
refuses writable access to its own binaries or the launch specification.
Granting any of the frontend's management storage or an ancestor is refused.

The same UID is used inside and outside: this confines the launched workload,
not an already-unconfined process of the user's identity. Writable grants
entrust their entire contents to that workload, including executable files
and repository hooks. System trees are read-only. The private home and grants
are executable under the development policy, retaining a source's noexec
restriction if one exists. User-local installations and replacement binaries
persist across sessions; HOME/.local/bin comes first on PATH. The frontend
does not download, validate, install or update third-party payloads itself.
Those runtime files never become target recipe inputs or distribution seeds.
A future managed foreign-runtime admission needs its own reviewed policy.

Network defaults to off. The `development-offline` workspace profile retains
the development syscall policy and executable home while creating a private
network namespace. `host` shares the host namespace, including localhost,
LAN and abstract Unix sockets. Neither choice promises to block pathname
sockets a user explicitly shares. Off is not a hostile-kernel attack-surface
minimization profile. Resource caps are inherited; td-jail prints its existing
aggregate-memory/task/CPU-cap limitation.

## Process and terminal ownership

The outer launcher runs td-jail with an unnamed connected stream socket on
stdin/stdout. td-jail ties itself to its direct launcher parent and reaps the
whole PID namespace when the immutable entry ends. No host terminal descriptor
crosses that boundary. The entry opens a fresh PTY in the private devpts via
td-ui, then spawns the command as a new session leader on its slave. The
workspace's generic /dev/tty (5,0) refers to that session's terminal, not the
host's. PID 1 and the relay entry have no controlling terminal.

The duplex relay has a one-byte kind and little-endian u32 length, capped at
8192 bytes, with bounded 128-frame queues per reader/writer. Inbound kinds are input, resize
(exactly four bytes, positive u16 rows/columns) and EOF (empty). Outbound kinds
are stdout, stderr and exit (exactly one byte). Framing and payload shape are
validated; no message chooses a host operation beyond terminal output. PTY
output merges stderr; pipe mode preserves it and forwards stdin EOF. Exit
codes are retained, signals represented as 128+signal. Blocking input and host-output writes run on dedicated workers, outside
lifecycle polling. After command exit, output readers receive a 200ms drain
window each; descendants retaining stdin or idle output descriptors cannot
delay teardown. Delivering queued output and the exit frame can still wait
for the host output consumer, but stop remains responsive even then.

The outer foreground terminal guard uses td-ui's audited termios and signal
operations. It disables local canonical/echo/signal processing, reads with a
100ms timeout and polls sizes; bytes such as Control-C go to the inner PTY's
line discipline and foreground process group. HUP/INT/QUIT/TERM/TSTP are
blocked after jail spawn and consumed synchronously. They terminate the
session after restoring the original terminal, rather than suspending a raw
relay. SIGKILL cannot restore a terminal. Original attributes and signal mask
are restored on ordinary return and errors. Reader threads end with the
launcher process and are never joined on a blocking read.

`stop` connects to a private control socket outside all grants. Its filesystem
address is opened through a retained directory descriptor's procfs alias to
avoid sockaddr_un's pathname limit. The listener carries no general command
protocol: an accepted connection requests termination of this session only.
The child guard kills/reaps the retained td-jail child; its parent-death chain
also handles abrupt outer death. Stale control/spec files are replaced only
while the environment lock is held.

## Validation

Committed tests name no foreign application binary. Unit tests cover bounded
frames, invalid policy, private state and serialization; td-ui tests verify
exact terminal restoration and pin its raw surface. Live opt-in namespace
tests use only their own Rust executable, copied into the private home. They
prove ungranted-file absence, read-only and writable shares, offline versus
host loopback reachability, executable replacement/restart, terminal input,
resize, /dev/tty, stop and restored echo. Backpressure fixtures cover saturated
terminal input, unread host output and descendants retaining stdin. Their trees use td-fs::TempDir.

Run live qualification with TMPDIR under the worktree's agent scratch and
TD_SANDBOX_TEST_JAIL naming its built td-jail, selecting each outer test by
name with --ignored. Inner fixture tests must never be selected wholesale.
Foreign CLI install/login/update acceptance is manual, outside the committed
suite and build graph. Startup alone is not authenticated task acceptance.
