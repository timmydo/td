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
