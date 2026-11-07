# td-ssh

td-ssh replaces OpenSSH in td images with three td-owned programs from the
std-only `td-ssh` crate, which depends on `td-primitives` for its primitives:

- `td-ssh`, the client, installed as `/bin/td-ssh` and aliased `/bin/ssh`;
- `td-sshd`, the server, installed as `/bin/td-sshd`;
- `td-ssh-keygen`, key tooling, installed as `/bin/td-ssh-keygen` and aliased
  `/bin/ssh-keygen`.

It is a deliberately small profile of the SSH protocol, not a reimplementation
of OpenSSH. The goal is the smallest surface that serves every td caller and
speaks to current standard peers. Anything not named here is absent.

## 1. Goals, and what this trades

**Goals.** Keep every td caller and every persistent on-disk artifact working
(host keys, `authorized_keys`, `known_hosts`, `openssh-key-v1` private keys);
remove forwarding, agents, multiplexing, compression, password, keyboard
interactive, host-based and certificate code by never writing it; keep the
client's command line and configuration familiar enough that `git`, muscle
memory and a plain `~/.ssh/config` work.

**The trade, stated plainly.** td previously chose externally maintained
OpenSSH over a td-owned daemon (`fb96f5260`). This reverses that for surface
area and dependency reasons, and it moves real risk to td:

- OpenSSH has decades of adversarial review and an upstream security process.
  td-ssh has neither. td owns its vulnerability response.
- Its cryptography is new, hand-written and unaudited (§7).
- It parses unauthenticated network input as root's child, and boot health
  depends on it: a broken `td-sshd` rolls a deployment back.

What it buys is that no C parses network input, a code base small enough to
read whole, and no feature exists for an attacker to reach or misconfigure.
Rust removes memory-unsafety classes; it does not remove logic errors, which
are what most recent OpenSSH advisories in comparable code were.

## 2. Peers and protocol profile

There is exactly one algorithm suite, compiled in:

| Slot | Names |
|---|---|
| KEX | `mlkem768x25519-sha256`, then `curve25519-sha256` |
| Host and user keys | `ssh-ed25519` |
| Cipher | `chacha20-poly1305@openssh.com` |
| MAC | never used; see below |
| Compression | `none` |
| Strict KEX | `kex-strict-{c,s}-v00@openssh.com`, **mandatory** |

`mlkem768x25519-sha256` follows draft-ietf-sshm-mlkem-hybrid-kex: the shared
secret is SHA-256 of the ML-KEM secret then the X25519 secret, carried as an SSH
`string`, not an `mpint`. `curve25519-sha256` follows RFC 8731.

Strict KEX is required of the peer, in both roles. A peer that does not offer it
is refused during algorithm negotiation: ChaCha20-Poly1305 without it is
exposed to prefix truncation (Terrapin, CVE-2023-48795). This sets the
interoperability floor at OpenSSH 9.6 and comparable peers. Under strict KEX any
packet other than the expected KEX sequence before the first `NEWKEYS` is a
fatal error, and sequence numbers reset at each `NEWKEYS`.

The KEXINIT MAC lists name `hmac-sha2-256-etm@openssh.com` so strict peers that
insist on a non-empty intersection are satisfied. Because the only cipher is an
AEAD, no MAC is ever computed: RFC 4253 §7.1's negotiation result is ignored
exactly as OpenSSH ignores it. If negotiation somehow yielded a non-AEAD cipher
it would have failed first, since there is only one cipher name.

td-ssh always sends `first_kex_packet_follows` false. Receiving true, it
discards one packet only if the peer's guess was wrong, per RFC 4253 §7.

**Transport limits.** Maximum packet 64 KiB; version exchange preamble bounded
to 64 lines of 255 bytes (client) and absent (server); identification string
`SSH-2.0-tdssh_1.0`. Reads are exact: length then remainder, so no process holds
read-ahead when transport state moves between processes (§4).

**Rekeying is implemented in both roles.** OpenSSH clients rekey after about 1
GiB; refusing would break large Git transfers. Either side starts a rekey after
1 GiB or 2^28 packets in a direction. After sending its KEXINIT a side sends only
KEX packets, and its sources stop taking window credit until that exchange's `NEWKEYS`
(§6). It still
accepts application packets until the peer's KEXINIT arrives (RFC 4253 §7.1: an
in-flight `CHANNEL_DATA` is legitimate), and only KEX packets from then to
`NEWKEYS`. The client requires a rekey's host key to equal the connection's first.

**Ignored without error:** `SSH_MSG_IGNORE`, `DEBUG`, `UNIMPLEMENTED` and
`EXT_INFO` after the first `NEWKEYS`. Unknown global requests with `want_reply`
get `REQUEST_FAILURE`; unknown message numbers get `UNIMPLEMENTED`.
`hostkeys-00@openssh.com` is therefore ignored, and neither role offers host-key
rotation. These hold during authentication too, including while the client waits for
its service request to be accepted: the client ignores `UNIMPLEMENTED` there and
answers a `want_reply` global request with `REQUEST_FAILURE`.

**Authentication** is `publickey` only. The client sends `none` (some servers
need it to list methods), then a query, then a signed request.
`publickey-hostbound` is not implemented; the signature binds the session ID as
RFC 4252 specifies.

**Connection layer.** Channel types: `session` only. Requests admitted by the
server: `pty-req`, `window-change`, `shell`, `exec`, `subsystem` (only `sftp`).
`eow@openssh.com` is honoured (§4). Everything else (`env`, `signal`, `x11-req`,
`auth-agent-req@openssh.com`, `break`, `direct-tcpip`, `forwarded-tcpip`,
`tcpip-forward`) is refused. The
client additionally sends `env` for `SendEnv`-listed variables (GitHub honours
`GIT_PROTOCOL`), and tolerates a refusal. Window 2 MiB, maximum channel packet
32 KiB.

`no-more-sessions@openssh.com` is ignored; `keepalive@openssh.com` and other
unknown global requests take the failure reply above, which is what keeps
`ServerAliveInterval` peers alive. The client's own `ServerAliveInterval` uses
`keepalive@openssh.com`.

## 3. Threat model

An unauthenticated network peer, an authenticated peer of a lower-trust account
(`td-admin`, a key restricted by `from=`/`command=`/`restrict`), and a process
already compromised at some stage of the chain below. The client also faces a
malicious server.

Explicitly **not** claimed:

- that the cryptography is constant-time at the machine level (§7);
- that account existence cannot be inferred from timing;
- protection of a user's session from that same user's own processes, beyond
  what the kernel's non-dumpable state provides;
- post-quantum protection for `curve25519-sha256` connections or for any host
  and user authentication, which remain Ed25519. Hybrid KEX protects recorded
  traffic only;
- that a compromised pre-auth worker cannot use its one host-key signature to
  impersonate the host to a client it is simultaneously intercepting (§4);
- that a client's signature "over this session" (§4) binds to a session td-sshd
  chose. It binds to the session ID the worker named: a compromised worker can relay
  a signature a client made for a hostile server's session, as with OpenSSH's
  monitor;
- that a recorded key in another type pins a host. td-ssh can compare only
  Ed25519, so a host recorded under nothing else is unknown (§6): it warns, then
  `yes` refuses, `ask` asks, and `accept-new` and `no` take the Ed25519 key beside
  the other. A first connection to such a host trusts what it is shown;
- that a fatal signal leaves the terminal as it found it. td-ssh has no signal
  surface (§6);
- that the log's filter (§5) binds a compromised worker or signer. Their allowlist
  has `write`, descriptor 2 is their logging, and so such a process can write raw
  bytes there, a forged line among them. The filter binds what a peer or an account
  can say through code that is still doing what it was written to do.

**Keys of small order are refused where td-ssh takes key material.** Ed25519
verification here is cofactorless and takes such a key, and `(R = identity, S = 0)`
verifies under one on every message, so an entry holding one would be a universal
key. The one parser of a public key blob, `kex::parse_ed25519_public`, refuses all
fourteen encodings. In `authorized_keys` and an allowed-signers file that line is
unusable, as any other unusable line is, and the rest of the file stands. `known_hosts`
compares pins as text, so it decodes only to ask this: the line is skipped, standing
neither as a match nor as a pin another key would mismatch. A host key of small order
is a protocol error in KEX, before any lookup. Offered in `USERAUTH`, the worker answers
`USERAUTH_FAILURE` without asking the monitor, and the monitor refuses it again if a
worker asks. A key of mixed order is not of small order and is not caught.

td-login's syscall and post-condition claims do not extend to td-sshd, which has
its own credential switch and states its own post-condition (§4).
`td-login/THREAT-MODEL.md` records that boundary.

## 4. Server process model

```text
td-svc
 └─ td-sshd                 root; listener; holds no keys and parses no SSH
     └─ td-sshd --connection    root; monitor; host key; auth decision
         ├─ td-sshd --preauth       uid sshd, chroot, seccomp; all pre-auth parsing
         └─ td-sshd --session       target account; post-auth (after auth)
         (monitor then execs td-sshd --signer: uid sshd, chroot, seccomp)
```

Each stage is a fresh `exec` of `/proc/self/exe`, so address-space layout is
re-randomized per connection and no stage inherits memory from another; the
host key never exists in a process that parses unauthenticated bytes. Descriptors
pass through `std::process::Command` stdio only: 0 is the socket, 1 is the
parent's `UnixStream`, 2 is inherited logging. There is no descriptor passing
and no `pre_exec` on this path.

**Credential switch.** One `credentials` module, used by every stage that changes
identity: setgroups, setgid, setuid, in that order, in a process that has started
no thread, then a readback of all four uid and gid columns, the supplementary
groups and the permitted, effective and ambient capabilities from
`/proc/self/status`, parsed by td-login's `status.rs` mounted by `#[path]`. The
status descriptor is opened before `chroot`, since the jail has no `/proc`, and
closed before seccomp. It follows `td-login/src/creds.rs`, deliberately not sharing
it: the descriptor and td-sshd's wider syscall layer would both be amendments to
td-login's pinned three-syscall surface.

**Listener.** Validates config and host key at start and refuses to start if
either is wrong (the unit's readiness and thus boot health see that). Binds, and
on each accept spawns a connection and closes its own copy of the socket; one that
cannot be spawned costs only that connection. Holds at most 64 unauthenticated
connections in total and 8 per source, where a source is one IPv4 address, a mapped
one counting as IPv4, or one IPv6 /64; excess are closed before any byte is written.
A connection counts until its monitor writes to a status pipe on authenticating or
exits, which its worker's exit causes at once. It raises the umask it inherited to
at least `022`, as OpenSSH does, since `td-svc` passes on PID 1's and nothing sets
that: a file made with no mode of its own, by a command or SFTP, is not group or
other writable. The cap is on unauthenticated connections alone: authenticated ones
are not capped, here or by OpenSSH, and each is its account's to spend. No stage
handles a reload:
config is re-read per connection, and it is published by rename
(`td-login/THREAT-MODEL.md`), so a connection sees one whole file. Revocation
remains `td-svc` stopping the whole `stop=leaf` leaf.

**Monitor (root).** Single-threaded. Parses config, account tables and host
key; spawns the worker; and answers a fixed request vocabulary over a framed
socket with 64 KiB frames. `LoginGraceTime` is an absolute deadline from spawn,
re-armed as the read timeout before each read, so a worker cannot extend it by
staying busy or by sending a frame a byte at a time. It kills the worker on expiry,
a malformed frame, a second signature request or answering `TOO_MANY`, after which
no request is answered: the worker has the same bounded wait as at hand-off to tell
its peer why. A worker that hung up is given that wait too. One a signal ended is
logged with the signal, when the monitor did not send it, since that is what a failed
exploit looks like. Once the filter is on, a fault logs as `SIGSYS` too, not `SIGSEGV`:
`std`'s handler for it calls `rt_sigaction`, which the filter kills. The session
has no such watcher: no root process remains to see how it ended.

| Worker request | Monitor behaviour |
|---|---|
| `HostKeySign{hash}` | Once only. Records `hash` as session ID, returns signature. |
| `KeyAcceptable{user, key}` | Evaluates account and key policy (§5); a boolean, identical for unknown users. A false answer counts against `MaxAuthTries`, so it is no unbounded enumeration oracle; reaching it answers `TOO_MANY` and ends the connection. |
| `Authenticate{user, key, signature}` | Rebuilds the RFC 4252 signed data itself from the recorded session ID, verifies Ed25519, re-evaluates policy; a failure counts against `MaxAuthTries` likewise. |
| `Authenticated{transport state}` | Accepted only after a successful `Authenticate`; must carry the recorded session ID. |

The monitor never accepts signed data or a verdict from the worker. A compromised
worker can refuse service, and can spend its single signature (§3), but cannot
authenticate without a client's valid signature over this session, which means the
session ID it named itself (§3).

**Pre-auth worker.** Starts as root; opens `/dev/urandom` and the status
descriptor; checks that `/run/sshd-empty` is a directory, root-owned and not group
or other writable, failing the connection otherwise; `chroot`s to it and
`chdir("/")`s; switches to `sshd` (uid and gid 995, with a group list of exactly that
gid); goes non-dumpable, which the kernel does at that uid change only by a sysctl's
leave; sets `no_new_privs`; installs a kill-process seccomp allowlist of `read`, `write`, `close`, `exit`, `exit_group`, `brk`,
`munmap`, `mremap`, `madvise`, `futex`, `sigaltstack`, `rt_sigreturn`,
`rt_sigprocmask`, `clock_gettime`, `clock_nanosleep` (the signer's rate limit),
`getrandom`, `sched_yield`, and `mmap` and `mprotect` denied `PROT_EXEC`, with the
architecture and the x32 syscall range checked. The signer takes the same steps. The worker therefore
uses no `poll`, `shutdown`, `setsockopt` or vectored write after the filter is on.
`tgkill` and friends are deliberately absent, because they could signal other
workers: a panic writes its message, then dies by `SIGSYS` instead of `SIGABRT`. It
handles version exchange, KEX, transport and the `USERAUTH` layer, and starts no
thread or process. After the monitor accepts, it sends `USERAUTH_SUCCESS`, sends
its transport state, and exits, with no bytes read past the last packet. Failure to
apply any step exits before reading a byte.

**Hand-off.** The monitor waits for the worker to exit before it spawns the
session, and kills it if it has not exited promptly, so a compromised worker cannot
consume packets meant for the session. At most one process ever reads the socket.

**Session.** The monitor spawns it as root with the socket, a fresh pair to the
signer, and logging. Its first frame carries transport state and the resolved
account: uid, gid, groups, home, shell, forced command, `permit_tty`,
`max_sessions`, and peer address. It refuses a grant of uid 0, whatever the monitor
says, and switches credentials before reading anything from the network. It sets `PR_SET_DUMPABLE` 0 (the kernel has already cleared it
across the uid change; this states the requirement and survives kernel policy) so
the user's own processes cannot ptrace it or reach the signer through its
descriptors, changes to the account's home directory, and only then starts threads.
It holds no host key.

**Keepalive.** A session probes its client, with compiled constants and no
configuration: every 300 s it sends `keepalive@openssh.com` with `want_reply`, and
any answer to a probe it sent resets the count. So does any packet at all arriving
since the last tick: ticks and answers queue behind output in one FIFO, so the reader
stamps each packet as it comes off the socket, and on a slow link a client that is
receiving and answering is not taken for one that has gone. The clock runs from the
tick that is served, so several late ticks count once, and each can add up to one period
to the limits below, as the next tick after a late one finds its probe not yet due. A
rekey in flight sends none but
counts the tick, and nothing that arrives during it, asked for or not, resets the count:
only finishing the rekey does. The third tick
with nothing arrived ends the connection, so at most 900 s after a client vanishes or a
rekey stops moving, and never before 600 s, its session, signer and any PTY shell go;
that same 900 s is the write timeout on the session's socket, so a stalled write ends
it too, the one thing a client that still sends but no longer reads can be held to. A laptop asleep for less than ten minutes keeps its session. On a link
slow enough that a probe waits behind the output in its buffers, what arrives is the
client's window grants. `td-ssh` sends one per 256 KiB it has consumed, so it is safe at
above 256 KiB in 600 s, about 440 B/s; under that its answers cannot be relied on to come
in time, and a link that slow can be cut. Stock OpenSSH and
`td-ssh` both answer, by the unknown-request failure of §2. No TCP keepalive and no
new syscall are involved.

**Channel lifetime.** A closed channel keeps its `MaxSessions` slot until its program
has gone, so closing and reopening runs no more programs at once than the limit
allows. Without a PTY the closed pipes are all the program gets, and it owns its exit.
On a PTY the source thread's blocked read holds a master open, so nothing would hang
the terminal up: the peer's close sends the session leader `SIGKILL` through its
`Child`, and the kernel then sends the foreground group `SIGHUP`. A PTY's output counts
as over at its master's EOF or, once the child has gone, after 200 ms of quiet, a clock
that does not run while the peer's window is empty or a rekey is holding the source
back. `eow@openssh.com`, a peer saying nothing reads this channel's output any more,
closes our read of the program's stdout, so its next write but one gets `SIGPIPE`; a
source blocked reading cannot be told sooner, so a program writing nothing runs on. A
PTY has no `SIGPIPE` to give, so there it ends the leader as a close does. Stock `ssh`
sends `eow@openssh.com` only to a server whose banner it recognizes as OpenSSH, which
`SSH-2.0-tdssh_1.0` is not, so through stock `ssh` to td-sshd a program writing to a
stdout that has closed goes on, its output discarded, as with any other server. Nothing
is sent for a channel after our
`CHANNEL_CLOSE`, because OpenSSH clients treat data on a freed channel as fatal. A
grant taking a channel's outstanding window past 2^32 - 1 ends the connection.

Sessions stay in td-sshd's cgroup leaf. They do not join the session cgroup,
because login-key cutover revokes them by emptying that leaf. Every child gets all
three standard descriptors replaced, so the socket and signer descriptors never
reach an account process; the recipe test pins that with a listing of `/proc/self/fd`.

**Signer.** After spawning the session, the monitor `exec`s `td-sshd --signer`,
handing the host key over a pipe on stdin and the session's socket on stdout, so
none of the parsed policy or account tables survive into it. It takes the worker's
jail and then answers `Sign{32-byte hash}` until EOF, at most once per second. This
serves rekeying, which needs a host-key signature. No root process remains per
connection after authentication. The oracle is the same class OpenSSH's monitor
exposes to its post-auth child; reaching it requires compromising the non-dumpable
session process, whose authority is the authenticated account's.

**Unprivileged single-user mode.** Started as non-root with `--single-user NAME`,
the stages skip `chroot` and credential changes (they have nothing to lose), keep
`no_new_privs` and seccomp, and admit only NAME as the calling uid. It is also the one
mode in which the jail is not non-dumpable, so that a test of that uid can read what the
jail holds. As root the flag is refused. This exists so unprivileged tests and the recipe test can run
real logins; `Port 0` is accepted only in this mode, and prints the bound port.

## 5. Server configuration and policy

`td-sshd -f FILE` reads a strict subset of `sshd_config` syntax; an unrecognized
keyword, value or `Match` condition refuses the whole file. Keywords:

`Port`, `ListenAddress`, `HostKey`, `AuthorizedKeysFile`, `AllowUsers`,
`Subsystem sftp internal-sftp` (the sole value), `PermitRootLogin no` (the sole
value), `LoginGraceTime`, `MaxAuthTries`, `MaxSessions`, and `Match User NAME` containing only
`AuthorizedKeysFile`, `ForceCommand`, `PermitTTY`, `MaxSessions`.
`AuthorizedKeysFile` takes several space-separated files; a `Match` value replaces
the global one. OpenSSH's `none` and `%` tokens there and `ForceCommand none` are
refused rather than read literally. Every keyword may appear once in its scope, the
global part or one `Match` block, and a repeat refuses the file rather than being taken
last-wins, so that a line far down cannot quietly undo one at the top. `AllowUsers` takes
all its names on its one line.

The rest of today's rendered policy becomes compiled behaviour, so it cannot be
misconfigured: publickey only, root refused for every account with uid 0, no
forwarding, no user environment or rc, no DNS, strict modes. `-T [-C
user=,host=,addr=]` parses the file and prints the effective policy, reading no key;
`-t` also validates the host key. There is no daemon mode: it stays in the
foreground, logging to stderr for td-svc's capture.

**Admission**, in the monitor, is: account exists in `/etc/passwd`; uid nonzero;
its password field classifies as `NoPassword` or `Hashed` under td-login's
`db::classify`, so `*`, `x` and any `!`-prefixed marker including `!td-service` are
refused and `td-admin-key-only` is not; `AllowUsers` names it if present; shell
exists and is executable; key found in one of the account's files. `td-login/src/db.rs`
is mounted by `#[path]`, so accounts parse fail-closed exactly as td-login reads
them. This is the account-locking rule `td-login/TOKEN-LOGIN.md`, "SSH" relies on
for refusing non-primary accounts in the ordinary form.

**Key files.** A root-owned absolute path is read by the monitor, after
canonicalizing it, since `/etc/ssh/authorized_keys` is a reviewed symlink into
`/var/lib/td/ssh`. A relative path resolves against the passwd home and is read by a
child, so root never opens an account-controlled path. The monitor spawns it as root;
it opens its status descriptor, takes the account's identity through the one
credential switch and its readback, goes non-dumpable, and only then reads (in
single-user mode there is no identity to take). The account can stop that child, so
the monitor takes at most 256 KiB from it through a stream, under one deadline of 5 s
and never past `LoginGraceTime`, then kills it and treats the file as empty. The child
reports by exit status alone, with stderr null, so nothing it says reaches root's log.
Strict modes apply to the resolved file and each ancestor: owned by root or the
account, not group or other writable, and the file regular. Where the account is not
root, a sticky directory with such an owner passes the writable test, since nobody
else can replace an entry there that they do not own: an account's own tree, and
`--single-user`, whose test sandbox has a `/` and `/tmp` of mode 1777 owned by the
test's uid. Stricter again for the absolute form and the real daemon's host key:
root-owned only, ancestors included, and a sticky world-writable step is refused,
since anyone can plant a missing entry or a symlink in `/tmp`. A missing file is
ordinary and says nothing; a failed check treats that file as empty and logs why. A
line that is not UTF-8 costs that line, not the file. The host key resolves the same
way and must be root-owned and not group or other accessible.

Stock `sshd` read every key file as the account, so a root-owned file of mode 0600 named
for the account denied by its mode. td-sshd reads an absolute file as root, where that
file is as good as any: naming one in `AuthorizedKeysFile` grants its keys to whoever
the block applies to, and what stands between it and an account that should have no
login is the password field's admission, not the file's mode.

**The log** is one line per event on stderr, and every line passes one control-character
filter on its way out, U+0085, U+2028 and U+2029 included, so nothing a peer or an
account supplied (a KEXINIT name-list, a symlink target) can begin a line of its own. No
stage's log write can panic it. A compromised stage is another matter (§3).

**`authorized_keys`** accepts plain `ssh-ed25519` lines with options `restrict`,
`from=`, `command=`, `no-pty`, `no-port-forwarding`, `no-agent-forwarding`,
`no-X11-forwarding`, `no-user-rc`. An unknown option or other key type makes the
line unusable, never unrestricted, and so does a repeated option: last-wins would let
a second `from=` undo the first. `from=` takes comma-separated exact addresses,
`*`/`?` patterns and `!` negation over the peer's address, a v4-mapped peer being
matched as its IPv4 address. There is no reverse DNS and no CIDR. A negated entry
that cannot match widens access, so a negated entry must be a literal address in its
canonical form, and the whole line is unusable otherwise, a negated pattern above all:
`!300?.*` looks like a fence and excludes nobody. Any other entry that cannot
match the one form an address prints in does the same: a `/`, a hostname, any character
outside digits, `.`, `*`, `?` and, with a `:`, hex digits; a mapped or non-canonical
literal; or an impossible octet. `restrict` also disables PTY. `ForceCommand` overrides `command=`, and both apply
to `shell`, `exec` and `subsystem`, setting `SSH_ORIGINAL_COMMAND`: a forced
account reaches SFTP or another command only through that command.

## 6. Session, SFTP, client and keygen

**Children.** Run as `SHELL -c COMMAND`, or for `shell` as the login shell with a
leading-dash `argv[0]`, in the home directory, in a new session; on a PTY, that
terminal becomes controlling. Environment is exactly `HOME`, `USER`, `LOGNAME`,
`SHELL`, `PATH`, `SSH_CONNECTION`, `SSH_CLIENT`, optional `SSH_TTY`, `TERM`, and
`SSH_ORIGINAL_COMMAND`. Exit is reported as `eof`, `exit-status` or `exit-signal`,
then `close`. The client's `pty-req` carries only `TTY_OP_END` and the server reads
and ignores any encoded modes, so a new terminal keeps the kernel's defaults.

**Concurrency.** One thread reads packets and one owns the transport and channels,
fed by a bounded `mpsc` channel; each child stream has a thread that blocks for window
credit. This is why no `poll` or signal surface is needed. A full channel
backpressures to the child pipe.

The queue is bounded in bytes, not only messages, since one authenticated connection
could otherwise pin 4096 packets of 64 KiB behind a core blocked in a write. Sources wait
above 2 MiB of queued packets and output, and the reader above 4 MiB, one message past
that always fitting an empty queue; the core's draining releases them. The reader's
higher limit is what keeps output filling the queue from stopping reads, which would
leave the peer's arrivals unstamped and could leave two td peers each blocked on the
other's reader. Nothing else charges against it, so ticks and exits always get through.
This cannot deadlock with a blocked write: that write has its limit (§4 and below), and
the core's end releases every waiter.

Rekeying does not disconnect a healthy peer. Sources, the client's stdin among them,
stop taking credit from the first KEXINIT, either side's, until `NEWKEYS` has gone both
ways and what was held has gone out. Only what they had already taken and control
messages are then held back: at most the queue, a packet in hand per source and whatever
replies a peer asks for meanwhile. So the 8 MiB bound on what is held remains for what
is left, a peer flooding requests during a rekey, and it fails the connection rather
than grow. A peer that never answers a rekey is caught by keepalive on the server, there
being no separate timer, and by the client's `ServerAlive` or a person's `~.`.

**SFTP v3**, in-process on its own thread as the account: `INIT`, `OPEN`,
`CLOSE`, `READ`, `WRITE`, `LSTAT`, `FSTAT`, `SETSTAT`, `FSETSTAT`, `OPENDIR`,
`READDIR`, `REMOVE`, `MKDIR`, `RMDIR`, `REALPATH`, `STAT`, `RENAME`, `READLINK`,
`SYMLINK`, and the extensions `posix-rename@openssh.com`, `fsync@openssh.com`,
`limits@openssh.com`, `expand-path@openssh.com`. Reads and writes at most 256 KiB,
at most 64 open handles. Paths are not jailed: it has the account's ordinary
filesystem permissions, as `internal-sftp` has today. The image still ships no
`scp` or `sftp` client. `FX_OPEN` or `SETSTAT` on a FIFO with no peer blocks that SFTP
channel, whose thread does its filesystem calls in turn, and holds its `MaxSessions`
slot until the connection ends. That is accepted: it costs the account only its own
slots, and the connection is its own to end.

**`ssh` command line.** `ssh [options] [user@]host [command...]` and
`ssh://[user@]host[:port]`. Options: `-F -G -i -l -n -N -o -p -q -t -T -v -V`; `-4` and
`-6`, which Git passes for `git fetch -4` and which keep only that family's addresses, the
last given deciding; and `-a -x`, which `td-remote` passes for a stock `ssh`'s sake: `-a`
is `ForwardAgent no`, first obtained like any option, so a person's `ForwardAgent yes`
after it reads, and `-x` disables X11, which td-ssh has none of. Anything else exits 255
as an unknown option. A port is 1 to 65535, by `-p`, `Port` or a URL. A host, given or by `HostName`, is held to OpenSSH's `valid_hostname`, and
further refuses `*` and `?`: a leading `-`, whitespace, controls, and any of
`` '`"$\;&<>|(){},*? ``, so `a,b` cannot record as a pin for two hosts. Git's `-o SendEnv=GIT_PROTOCOL` and `-G` probe both work. The remote
command is carried as bytes; options and the destination must be UTF-8 and are
otherwise refused. A remote command is one `exec` request and so one packet: at most
65,472 bytes, past which td-ssh says `remote command is too long` before it connects,
naming that limit. The limit is §2's and is not raised. Of `-l`, `-p`, `-o User`,
`-o Port` and the destination's own, the first given wins, in command-line order and
ahead of any configuration, as in OpenSSH, and a later `-p` is not even parsed. As there,
the last `-F` wins. Over `USER` and `LOGNAME`, both
unset, the remote user is the name `/etc/passwd` gives the real uid, and `HOME` likewise;
no such entry and no `-l` or `User` refuses by uid. The destination host is lowercased once, before configuration,
lookup and recording, as OpenSSH does, so a hashed pin cannot be missed by case; a
`HostName` is lowercased likewise. `-n` leaves stdin unread and sends
`CHANNEL_EOF` as soon as the session starts.

Exit status is the remote status, or 255 for a local or protocol failure, and 255
after a `~.`. A status already received stands if the connection is then lost: the
reader shuts the socket down when it ends, so that a write the core is blocked in ends
too, and the way out looks through what was queued behind that write for an
`exit-status` or `exit-signal` before dropping it. Only the last of those counts, so a
peer's flood costs no memory. If output was queued with it, that output is lost and
the status does not stand: the client says so and exits 255, since a success would
claim all of it arrived.
Two things can still lose a status. A reset, as with stock `ssh`: a server that closes
with client data unread resets the connection, the kernel throws away what nobody had
read, and the command reads as 255. And a queue already full behind the core, of 4096
packets or the reader's budget, which keeps the reader from the end of the connection
until the write limit below. These
diagnostics stay byte-compatible, since td-agent, td-pinentry and health probes
match them: `Permission denied (publickey).`, `Host key verification failed.`,
`read_passphrase: ` (when a prompt cannot be asked), and at `-v`, `debug1: Offering
public key: PATH ED25519 SHA256:FP`. So does the question beginning `The
authenticity of host ` and ending `(yes/no/[fingerprint])? `, which also goes to
`SSH_ASKPASS` under `SSH_ASKPASS_REQUIRE=force`; td-pinentry recognizes that exact
shape to show the answer unmasked. A wrong answer re-asks at most three times,
whichever source answers, and an end of input at any prompt is no answer rather than
an empty one. At the default `LogLevel`, a keepalive timeout says so on stderr, which
`td-remote` relies on.

Every diagnostic is printed at one point, which replaces control characters but the
line breaks and tabs of its layout, and which ignores a failing stderr. Text a peer
supplied (a name list, a disconnect reason, an open failure, a signal name) is also
cut to one line where it is quoted; only a banner keeps its line breaks. While the
terminal is raw it ends each line `\r\n`. A tool's
primary stdout that cannot be written gives an ordinary failure status, not a panic.

A stdout whose consumer has gone is reported as OpenSSH does, by `eow@openssh.com`
without `want_reply`, once, so `ssh h yes | head -1` ends. What still comes for it is
discarded and its window granted, since stderr shares that window and the status follows
both: a server that takes no notice of eow can finish, and one with no end to its output
is a connection that does not end, as with stock `ssh`. A stderr that has gone is only
discarded. A stream inherited with `O_NONBLOCK` is retried on `EAGAIN` after 5 ms on
stdin, stdout and stderr, by the session's sinks, by diagnostics and by a tool's primary
output, losing nothing and reading as no false end of input: `std` has no `poll`, and
td-ssh adds none.

**Time limits.** `ConnectTimeout` is one absolute deadline over the identification and
the first key exchange, re-armed before each read, so a server dripping a byte at a time
does not extend it, and time at the host-key question is added back. With none set, each
read there has `ServerAliveInterval` times `ServerAliveCountMax`, if set, and otherwise
nothing. Authentication keeps a limit on each read instead, that keepalive bound if set,
else `ConnectTimeout`: not the deadline's rest, as it may reach a person at a prompt,
whose time never counts. Every session has a limit on each write, of 900 s and
`ServerAliveInterval` times at least one if that is shorter, since a stalled write blocks
the only thread that would send a keepalive; it fails with the keepalive's text and
status 255, and the terminal restored. A farewell has its own limit of 1 s, set before
it is written. td-ssh has no `ProxyCommand` transport to exempt (refused below): one would have
no socket, and be bounded by its pipe alone. A server may start a rekey during
authentication, as during the session. The client waits for output already received
to reach its consumer before exiting, with the terminal restored first: a consumer
that never reads holds it there, as it holds OpenSSH.

A PTY session asks for its terminal with `want_reply`, and switches stdin to raw mode on
the answer, with a restore guard, and restores on every
exit path but a fatal signal: SIGTERM, SIGHUP or SIGKILL does not restore, by design,
since td-ssh has no signal surface. It is stdin's terminal, the one that was tested. A
refusal leaves the terminal as it was, says OpenSSH's `PTY allocation request failed on
channel 0`, and goes on with the command, with no escape character, as there is none to
need without a raw terminal. Stock `ssh` goes on only where it was not asked explicitly:
under `-t` or `-tt` it takes the refusal as fatal, and this does not.
Window changes come from a thread polling `TIOCGWINSZ` every 250
ms, so no signal handler is needed. The `~.` escape disconnects, after a newline,
and `~~` sends a tilde; nothing else is an escape. Keys are read and looked at on a
thread of their own, so `~.` is seen while the window is shut or a rekey is stuck, up to
256 chunks of typing waiting behind them, past which reading stops as OpenSSH's does. It
works on a session whose server stopped reading: it tells the core, gives it the
farewell's second, and then shuts the socket down, the one thing that ends a write the
core is blocked in. A drain of output already under way, for a stalled local consumer,
lasts no more than a second from the key. A core that has still not ended 3 s later is
printing into a stderr nobody reads, out of any socket's reach, so the keys' thread puts
the terminal back and ends the process with 255 itself. The character is always `~`.
A PTY follows OpenSSH's default, for no command when stdin is a terminal, and
`RequestTTY`, which `-t` and `-T` override, as `td-remote`'s `-T` needs. It is requested
only when stdin is a terminal, `-n` counting as none, or `-tt` or `RequestTTY force`
insists; `-t` without one
says OpenSSH's `Pseudo-terminal will not be allocated because stdin is not a terminal.`

An unechoed prompt takes the same route with no guard to lose: for its duration it
clears `ECHO`, `ICANON` and `ISIG` and reads byte by byte, itself handling line end,
erase, `^W`, `^U`, `^D` and `^C`, so Ctrl-C reaches it as a key, and aborts, and restores.

**`ssh_config`.** `-F FILE` alone, else `~/.ssh/config` then `/etc/ssh/ssh_config`;
first obtained value wins. `-F /dev/null`, which most td callers pass, therefore
reads nothing, and `-F none` reads no file at all. As OpenSSH, `~/.ssh/config` must be
owned by the caller or root and not group or other writable, else
`Bad owner or permissions on PATH`; `-F FILE` and the system file are not checked.
`Host` patterns with `*`, `?`, `!`, ignoring case. `Match` and `Include` refuse the file
wherever they stand, inactive blocks and `IgnoreUnknown` included, since skipping one
would apply what followed to the wrong host or lose what it held.

A keyword is here because a td caller passes it or a person at a terminal needs it, and
no other is read: a copied configuration may need editing.

- *Honoured:* `HostName`, `User`, `Port`, `IdentityFile`, `IdentitiesOnly` (accepted, and
  how td-ssh behaves anyway, with no agent to prefer), `BatchMode`,
  `StrictHostKeyChecking`, `UserKnownHostsFile`, `GlobalKnownHostsFile`, `HostKeyAlias`,
  `HashKnownHosts`, `ConnectTimeout`, `ServerAliveInterval`, `ServerAliveCountMax`,
  `SendEnv` (patterns, against the local environment, sending valid names only; OpenSSH's
  `-PATTERN` clearing is not implemented, so such a word clears nothing and, being no
  valid name, sends nothing), `LogLevel`, `RequestTTY`, `PreferredAuthentications` (must include
  `publickey`), and `RekeyLimit` in bytes, which can only lower the compiled limit and
  refuses a time.
- *Algorithm lists* (`KexAlgorithms`, `HostKeyAlgorithms`, `PubkeyAcceptedAlgorithms`,
  `Ciphers`), which the boot probes and tests pin: `+` and `^` forms keep the compiled
  set; `-` subtracts; a plain list intersects. An empty result is a hard error. Unknown
  names are not errors, so a `+ssh-rsa` line is harmless.
- *Stated, each only at the one value saying what td-ssh already does:*
  `ClearAllForwardings yes`, `RemoteCommand none`, `ControlPath none`, which `td-remote`
  passes because it also runs over a stock `ssh` on a host, with a person's configuration
  behind it; and `IdentityAgent none`, `CheckHostIP no`, `UpdateHostKeys no`,
  `ForwardAgent no`, `PermitLocalCommand no`, `CanonicalizeHostname no`, `ProxyCommand
  none`, `ProxyJump none`, which stand in the file `td-vm-guest` wrote into a VM's state.
  It compares that file byte for byte on every start and refuses a difference, so an
  existing VM's must keep reading. OpenSSH's other spellings of that value (`true`,
  `false`, quoted) count as it; `on` and `off` do not, since OpenSSH would read
  `ForwardAgent off` as an agent socket named `off`. Another value is refused when it is the first obtained,
  and `IgnoreUnknown` does not waive it: for `ProxyCommand`, `ProxyJump`,
  `RemoteCommand`, `ForwardAgent` and `ClearAllForwardings` connecting anyway would
  silently go somewhere else, run something else, or leak, and for the rest td-ssh does
  not do what the other value asks. After the stated value, as after any first value, a
  later one is not looked at, so `td-remote`'s stated keywords override the same ones in
  a person's file. A person's file is otherwise held to these rules behind `td-remote`
  too: a `ControlMaster`, a `Compression` or a `ForwardX11` in it refuses that link.
- *Discarded:* `LocalForward`, `RemoteForward` and `DynamicForward`, only where
  `ClearAllForwardings yes` is in force, on whatever line, as in OpenSSH; without it the
  connection refuses, there being no forwarding to give.

Anything else, X11 and passwords included, is `Bad configuration option` unless an
earlier `IgnoreUnknown` names it, in a `Host` block that applies: an inactive block's
keywords are not looked at, as in OpenSSH. `IgnoreUnknown` takes one comma-separated list
of keywords and refuses a pattern, as stock does not, so that no `IgnoreUnknown *` can hide everything. It does not waive a
keyword whose absence would trust another key, leave from another address or run something
other than asked: `AddressFamily`, `BindAddress`, `BindInterface`, `CertificateFile`,
`ForkAfterAuthentication`, `KnownHostsCommand`, `PKCS11Provider`, `RevokedHostKeys`,
`SecurityKeyProvider`, `SessionType`, `SetEnv`, `StdinNull`, `Tunnel` and
`TunnelDevice`.

**No agent.** `SSH_AUTH_SOCK` is ignored. A key held only in an agent elsewhere
does not authenticate through td-ssh.

**Identities.** `-i` and `IdentityFile`, else `~/.ssh/id_ed25519`. A private key
file must be owned by the caller and not group or other accessible. That is stricter
than OpenSSH, which checks the mode only of a key the caller owns.
Encrypted `openssh-key-v1` with `bcrypt` and `aes256-ctr` is read, never written (§7), with the
passphrase obtained from `SSH_ASKPASS` when `SSH_ASKPASS_REQUIRE=force` or there
is no terminal, else `/dev/tty` with echo disabled; `BatchMode` prompts for
nothing. Three wrong attempts move to the next identity, and Ctrl-C at the prompt
ends the connection. Loaded identities are dropped once authentication completes
rather than held for the session; what the crate owns of a key or a passphrase is
wiped as it goes (§7).

**`known_hosts`.** Reads plain and `|1|` hashed entries, wildcard and negated
patterns, `[host]:port`, and `@revoked`; `@cert-authority` lines and other key
types are skipped. The name looked up and recorded is `HostKeyAlias` verbatim, with
no port decoration, if set. A missing file is empty, a line that is not UTF-8 is
skipped alone, and a file that cannot be read fails the connection, since it may hold
any pin. `UserKnownHostsFile none` and `GlobalKnownHostsFile none` mean no file, so a
new host is recorded nowhere, while `/dev/null` stays an ordinary empty file. The
modes are `yes`, `accept-new`, `ask` (default, prompting)
and `no`, as OpenSSH, and a mismatch with a recorded Ed25519 key is a hard failure
in every mode. A line naming the host whose key does not decode fails the same way, but
says that line of that file is corrupt: only a key that does decode and differs is
reported as the host's identity having changed. A host recorded only in other key types prints OpenSSH's warning with
each such entry, and is otherwise unknown (§3). New entries are appended under
`HashKnownHosts`, on a line of their own, and only once the key's signature over the
exchange has verified, so a key a peer merely sent cannot be pinned. If hashing cannot
get its salt nothing is recorded: a line in the clear is what `HashKnownHosts` refuses.

**`td-ssh-keygen`.** Only what callers use: `-t ed25519` (and no other type) with
`-q -N '' -C -f`, refusing before it makes a secret if either file of the pair exists,
writing private then public and removing the private half if the public cannot be
written, with `~/.ssh` made 0700 for the default path, as OpenSSH does; `-y [-P pass] -f`,
asking for the passphrase of a protected key without `-P`, which td-firstboot (every boot)
and td-vm-guest pass as `-P ''`, so a protected key fails rather than prompting on a
closed stdin, and which nothing else takes; `-l [-E sha256] -f`, printing
exactly one line `256 SHA256:B64 COMMENT (ED25519)`, its comment through the filter
above; `-q -Y sign -n -f`; `-q -Y
verify -n -I -f -s`. SSHSIG uses SHA-512 with allowed-signers files of plain
`principal[,principal] ssh-ed25519 KEY` lines, with no options. It writes no protected
key: every td caller makes an unprotected one, so any other `-N`, or none, refuses, rather
than a tool that prompts handing back a key in the clear. A protected key made elsewhere
is read (§7). Existing `openssh-key-v1` files, host keys above all, remain valid; a
td-ssh update changes no machine's host identity.

## 7. Cryptography (`td-primitives`)

td-ssh writes no primitive of its own. All of them are `td-primitives`, whose
`td-primitives/DESIGN.md` owns what each one claims and what none of them claims,
source-level constant time and erasure included, and its `unsafe` around CPU
instructions a runtime check found (`UNSAFE.md` §26). They reach a jailed stage as
ordinary code: asking the CPU what it has takes no syscall, so the jail's allowlist
(§4) is unchanged by which path runs.

td-ssh takes SHA-256, SHA-512, HMAC, X25519, Ed25519 and `ct` as they stand there,
shared with TLS and td's other consumers. It added these, for itself: `OpenSshAead`
(`chacha20-poly1305@openssh.com`), ML-KEM-768 and the Keccak it is built from,
`hmac_sha1` (hashed `known_hosts` only), Blowfish with `bcrypt_pbkdf`, AES-256-CTR over
`aes_gcm`'s AES, and `ed25519::is_small_order` (§3). What belongs to td-ssh and not to
that crate is `openssh-key-v1`, the wire encodings, and the following.

**Randomness** comes through `KexEnvironment::random`, and so as arguments to
everything in the exchange, from a caller-held `/dev/urandom` descriptor, since the
worker's chroot has no device nodes. td-ssh never calls `td-primitives`' `random`, or
`generate_secret` or `generate_seed`, which read the device themselves.

**The seed.** `SigningKey` cannot give its seed back, and the signer hand-off and key
writing need it, so `keyfile::PrivateKey` keeps a copy beside the signing key, wiped
with it. A protected key file's cipher and KDF run only on a user's own machine, on
that user's own file, where no network peer can reach or time them: the transport uses
neither. `bcrypt_pbkdf` has no round limit, and the key-file reader sets one.

Writing a key file builds what holds the seed in room made for all of it up front, so
that no shorter copy is left behind by growing.

**Not claimed.** Hashing a secret leaves `Sha256` and `Sha512` state unwiped, in
`td-primitives` and so here: `sha` has no `Drop`. So the KEX's intermediate hash states,
and the seed's expansion inside `SigningKey`, stay on the stack until overwritten. td-ssh
wipes the keys and secrets it keeps, not every stack copy on the way to them. The
primitives have had no external audit. Bugs in them are security bugs in td, and td is their maintainer.

**Evidence.** The vectors `td-primitives/DESIGN.md` lists, which it runs in its own
tests, and end-to-end interoperation with real OpenSSH in both roles (§9), which is
the check that a wrong KEX or AEAD cannot pass.

## 8. `unsafe`

`td-primitives` has its own, for CPU instructions alone (`UNSAFE.md` §26). The `td-ssh`
library denies it, with one function-scoped allowance around one syscall instruction
and one around one pre-exec hook (`UNSAFE.md` §27). Its eight syscalls are `setgroups`, `setgid`,
`setuid` (one call site each, in that order), `prctl` (pinned to
`PR_SET_NO_NEW_PRIVS` with 1 and `PR_SET_DUMPABLE` with 0), `seccomp` (one operation,
no flags), `ioctl` (pinned to `TIOCSPTLCK`, `TIOCGPTN`, `TIOCSWINSZ`, `TIOCGWINSZ`,
`TCGETS`, `TCSETS`, and `TIOCSCTTY` in the hook only), `setsid` (in the hook only) and
`umask` (one floor operation, which can only tighten). `chroot` is safe `std`.
Confinement tests pin the module, the request list, each fixed call's arguments, the
seccomp allowlist's numbers, the credential order and that only `td-sshd`'s stages
reach the jail.

## 9. Tests

- Unit: every parser against malformed and boundary input, the packet and strict
  KEX state machines against hostile sequences, policy tables.
- Host integration, unprivileged: `td-ssh` against `td-sshd --single-user` on
  loopback, covering exec, PTY, large transfer across forced rekeys, the live
  stages' seccomp state, the listener's caps, and refusal of every removed feature.
  `td-ssh` has no subsystem option, so SFTP's operations are unit-tested and its
  channel runs against stock `sftp` and `scp` in the recipe test.
- Host integration, a hostile peer (`tests/client_peer.rs`): the real `td-ssh`, with a
  real terminal for its stdin, against a server built on the crate's key exchange and
  packet layers that stops reading, holds its window shut, leaves a rekey hanging or
  floods. The session loop is reached no other way: `~.` under each of those, the
  terminal after each, a refused PTY, a closed stdout's `eow@openssh.com`, inherited
  `O_NONBLOCK` streams, and a flood's bound by what the socket would take.
- The fences that only a run as root would otherwise show, the uid 0 refusals, the
  locked account and `--single-user` as root, are pure functions with tests of their
  own, whose call sites `tests/confinement.rs` pins.
- Realized recipe test `td-ssh-test`, with `openssh-x86-64` retained as a
  **test-only** build input. A build has no loopback, so the recipe's own steps
  hold only the binaries' static shape; its check's runner enters one `store-ns`,
  which has a loopback and an account the driver writes, and `td-ssh-interop`, a
  std-only program no image holds, starts both ends of every connection there.
  Stock `ssh` against `td-sshd --single-user` and `td-ssh` against stock `sshd`:
  each KEX forced in turn, by what `-v` says ran; exit status, stderr, stdin and its
  EOF; 4 MiB each way across forced rekeys, by digest, with `-v` saying a rekey
  happened; refused forwards and an algorithm outside the profile, each by the words
  of the refusal, since a dead daemon gives 255 too and a server that permits a forward
  gives its own; stock `ssh` asking for a password and for keyboard-interactive, shown
  only `publickey`; a command that is not UTF-8, which the shell refuses with its own
  status, for each client against td-sshd; a plain key nothing authorizes, for each
  client against each server, since a key the client cannot load shows nothing of the
  server; a changed host key;
  and an encrypted stock key through askpass. `td-ssh` against a stock `sshd` allowing
  only each KEX in turn, and against one with `RekeyLimit` that starts the rekeys itself.
  Each daemon ends the phase with a login, to show its refusals were its own. td-sshd
  starts no rekey, having no such setting. Stock `ssh` as `ControlMaster` with `RekeyLimit=4M`, six sessions
  sending 8 MiB each at once over one connection, by digest. `td-ssh` with a stdout that
  closes early, against stock `sshd` and `td-sshd`; stock `ssh` has no such leg, not
  sending `eow@openssh.com` to td-sshd (§4). A refused terminal: stock `ssh -tt` with
  `no-pty` on its key, and `td-ssh -tt` against `PermitTTY no`. Plain keys from each generator read by the other, and a stock encrypted one by td's,
  with one fingerprint, and td's refusal to write a protected one; `-Y sign`/`verify` crossed. Stock `sftp` and modern `scp`
  against `td-sshd`: a batch of mkdir, put, rename, `ls`, get, rm and rmdir, and `scp`
  both ways with absolute, home-relative and `~/` paths, 4 MiB by digest on the far
  side too; `mkdir` and `scp -r` with their modes. `scp -r` makes a directory then chmods
  it to the local mode, as an explicit request that the floor does not touch, as
  `sftp-server`'s does not. `openssh-x86-64` builds those two clients only because no image holds it:
  a catalog test holds that no shipped image and no other recipe names it, and the
  image's root-tree check holds `/bin/sshd`, OpenSSH's helpers and any OpenSSH store
  item absent. Every form of the image's rendered policy (`td-firstboot`'s
  `ssh_policy.rs`, which the driver mounts) goes to the real `td-sshd`: `-T` over the
  file as it is, for the primary, another account, root and `td-admin`, and `-t` with
  the test's host key standing for the machine's. `-T` runs no admission, so it says
  only what the file selects for each name. The enforced and ordinary forms then run
  real logins, with the self-test key file redirected to one the test holds: a key only
  in the account's own file is refused under the enforced form, and the file's own key
  logs in under it, and the first key logs in under the ordinary form. The check's
  runner holds the driver's count of checks to the number it has, so a leg cannot quietly
  stop running.
- Boot: `TD-SSHD-OK`, `TD-ROOT-SSH-REFUSED` and the Git clone, push and reclone legs;
  `td-vm`, `host_login` and other host oracles keep driving the guest from a stock
  host OpenSSH, so that interoperation is exercised on each integration run.
  `TD-SSHD-JAIL-OK`, under the autotest token and required by the oracle on both
  profiles, comes from the running daemon's `/proc`: boot health connects with a
  client that does not yet trust the host, so it asks about the host key after the
  exchange and before authentication, and while that question is open its
  `SSH_ASKPASS` reads every process of uid 995, of which at least one must exist: the
  pre-auth worker waiting on that connection is among them, along with any other
  connection's worker or signer. Each must show `Seccomp: 2`, `NoNewPrivs: 1` and the
  jail's group alone, and its `/proc/PID/root` must read `/run/sshd-empty`. A process
  that fails one is read again, up to four times a second apart, since a concurrent
  connection between the switch and the filter shows the same and must not roll a boot
  back; one that stays so fails the boot. `TD-SSHD-OK` cannot reach the child that reads
  an account's own keys, since its key is in the first file and the search stops there,
  or the signer's jail, since `exec` has no handshake and only a rekey asks it for
  anything. So on each boot `qemu-boot-server` logs in, its acknowledged boot, the
  image's and the live medium's, the host's client also runs these, each failing by
  name: a key the account writes to its own `~/.ssh/authorized_keys`, then a login by
  it; a session through `cat` under `RekeyLimit=1M`, 2 MiB by digest, with the client's
  log saying a rekey happened; `ssh -tt`, whose program finds `/dev/pts/`; and `sftp`
  put and get, by digest. That last needs a stock `sftp` on the host, as this tier
  needs QEMU, and fails saying so where it has none. The account's key needs the
  ordinary form of the policy, so a boot whose login directory is invalid fails that
  login by name. No other boot leg runs these. These boot legs are the first time the
  root paths run for real: `--single-user` exercises no `chroot`, credential switch or
  uid change, and neither do the tests above.

## 10. Migration

Principle 4 applied at the final increment, which cut every image over and removed
OpenSSH from them in one landing. Prior increments added code no image shipped. In
order, each independently green: this document; the new primitives in
`td-primitives`; client and keygen; server and SFTP with `UNSAFE.md` §27; recipes and
interop test; cutover.

The cutover: `/bin/ssh`, `/bin/ssh-keygen`, `/bin/td-ssh`, `/bin/td-ssh-keygen` and `/bin/td-sshd` come from the `td-ssh`
recipe, which is static and so copied into the image rather than staged as a runtime
closure root; the `sshd` unit name stays, with `exec=/bin/td-sshd -f
/run/td-sshd.conf`, keeping `td-authd`'s revocation and `td-svc`'s ordering as they
were; `ssh_policy.rs` renders §5's subset; the `sshd` account, its uid and
`/run/sshd-empty` stay; `/bin/sshd` and the `libexec` helpers are gone;
`/etc/ssh/ssh_config` is `HashKnownHosts yes`. A host key an earlier deployment minted
stays valid, and a machine keeps its identity across the cutover.

`openssh-x86-64` stays as `td-ssh-test`'s peer alone. `openssh-x86-64-test` is gone:
`td-ssh-test` logs in through every program it covered, and what else it held (the
shipped ELF's RUNPATH, `_FORTIFY_SOURCE` and absent libraries) were claims about an
artifact no image now carries. Its `sshd -T` reading of the policy became that test's
`td-sshd -T`.

`PermitRootLogin no` stays a rendered line even though the behaviour is compiled:
refusing it would otherwise be invisible in the policy the threat model describes.

Host OpenSSH (`td-vm`'s registrar with `AuthorizedKeysCommand`, host oracles) is
not td's image and is unchanged.
