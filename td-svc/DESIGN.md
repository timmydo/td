# td-svc — the service supervisor

This file is the normative specification for `td-svc`. Where the code and
this document disagree, one of them is a bug. Several invariants below are
not checkable by any compiler or lint — they are recorded here because the
failure modes they prevent are silent, and because each was found the hard
way rather than reasoned out in advance.

## 1. Why the split exists

PID 1 (`td-init`'s `init` applet) supervises with **no signals at all**: a
blocking `wait4(-1)` is its event loop. That invariant is worth keeping — it
means PID 1 has no handler surface, no self-pipe, and no reentrancy — but it
costs three things the system needs:

- **Declared ordering.** The inittab encodes start order positionally, in
  line order plus a comment. `netup` must follow `rootcheck`, which must
  follow `td-firstboot` (it asserts the minted identity is readable), and
  `sshd` is fail-closed on the host key `td-firstboot` mints. None of that
  is expressed anywhere a machine can check.
- **An owner for shutdown.** Because PID 1 has no `::shutdown:` action,
  every script that decides to reboot runs `/etc/shutdown` *itself*. There
  are two such initiators, and the only thing holding the rule together is
  a test that scans every generated `/etc` file.
- **Ctrl-Alt-Del.** Both available kernel states are wrong for td: the
  default hard-resets a mounted read-write Btrfs `/var` with no userspace
  teardown, and the alternative delivers SIGINT to PID 1, which discards it.

td-svc takes ordering, restart policy, log capture, shutdown, and
Ctrl-Alt-Del. PID 1 keeps mounts, reaping, and respawning td-svc.

## 2. Hard invariants

**I1. No `unsafe`, ever.** The crate `#![forbid(unsafe_code)]`s. Every
capability td-svc needs is reachable through safe `std`; where that required
a non-obvious route, §4 records which. td-svc is deliberately *not* a
target-side unsafe exception, and adding one would be an AGENTS.md
amendment.

**I2. No `pre_exec`, ever.** td-svc is multithreaded (log drains, waiters).
A `pre_exec` closure runs between `fork` and `exec` in a multithreaded
process, where only async-signal-safe operations are legal — an allocation,
or a lock another thread held at fork time, deadlocks the child. Everything
needed is reachable through `Command`'s own safe setters, which `std`
implements inside its async-signal-safe post-fork path.

**I3. Liveness is read from `/proc`, never inferred from an exit status.**
`kill -0` reports through an exit code, so a spawn failure, an ENOENT, or a
rejected argv is indistinguishable from ESRCH — i.e. from "not running". A
liveness test that reads *any* failure as "gone" would declare a live
service dead and let the teardown unmount underneath it. `/proc` fails
closed: unreadable is an error, not an emptiness. `kill(1)` is used only to
*send* signals, and even its exit status is discarded.

**I4. A service is stopped only when both its leader is reaped and its
containment (§4) is empty.** Neither half suffices. An unreaped
leader is a zombie; a zombie is still a task in its process group and
immune to SIGKILL, so membership alone never empties. Reaping alone says
nothing about descendants, or about a session `setsid()` moved out of the
group.

**I5. The console is never skippable — and never indefinitely delayed.** A
dependency failure does not prevent a `tty=` service from starting, no
`requires=` edge may make it do so, and no dependency that simply never
settles may hold it either. This codebase treats "a machine that is up and
cannot be repaired from its own console" as the worst outcome; a skip
cascade inverts that, and so does an unbounded wait, which reaches the same
place more slowly. Hence four defences, not three: the table refuses
`requires=` on a `tty=` unit, the plan refuses to skip one, the runtime
refuses to skip one, and a console's wait for its dependencies is bounded
(`CONSOLE_PATIENCE`) after which it starts with its ordering ignored. The
first three reason about the GRAPH; only the fourth catches a stall.

**I6. Shutdown is a persisted, monotonic transition.** PID 1 respawns
td-svc unconditionally, so a td-svc that dies mid-teardown must not come
back and start services while an orphaned `/etc/shutdown` is unmounting
filesystems.

## 3. The table

One file, `/etc/td-svc.conf`, stanza per service. One file rather than
drop-ins: the image is immutable and recipe-generated, so there is no
third-party drop-in to serve, and a single file validates as one unit at
build time the way the inittab already does.

```
[netup]
type=oneshot
exec=/etc/netup
after=rootcheck
timeout=60

[sshd]
type=daemon
exec=/bin/sshd serve --listen 0.0.0.0:22 --host-key <KEY> --authorized-keys <AUTH>
after=netup,td-firstboot
ready=/bin/td-netd reach 127.0.0.1 22
ready-timeout=30
log=/var/log/svc/sshd.log   # rejected until log capture lands — see below

[greeter]
type=daemon
exec=/etc/tty-session
after=netup
tty=ttyS0
restart=always
```

Keys: `type` (`oneshot` | `daemon`), `exec`, `after`, `requires`, `restart`
(`always` | `on-failure` | `never`), `tty`, `timeout`, `ready`,
`ready-timeout`, `stop-timeout`.

**Nothing is accepted-and-ignored.** A key the supervisor would silently
drop reads in the table as a guarantee it does not make, so each is
rejected instead:

- a key that does not apply to the unit's `type` — `ready=` and `restart=`
  on a `oneshot`, `timeout=` on a `daemon`;
- a key whose behaviour has not LANDED — `log=` and `console=` arrive with
  log capture and are refused by name until then, so `log=` never names a
  file that will not exist. (When they land, so does the rule that `tty=`
  and `log=` are mutually exclusive: a pipe would break job control on a
  terminal.)
- a key whose *value* was refused — which is why one bad key fails its
  whole stanza rather than leaving the unit admitted with that key's
  default. A refused `type=` used to run the unit as the wrong kind.
- a LINE that is not `key=value` at all. Its intent is unknown, so
  admitting the unit without it silently drops that intent: `requires
  firewall` (no `=`) would have started the service with no strict
  dependency, complaint logged and ignored.

Parsing collects diagnostics rather than aborting, exactly as
`parse_inittab` does: a machine that refuses to boot over one bad stanza
cannot be repaired from its own console.

`td-svc check -f FILE` prints the resolved start order and every complaint,
exiting non-zero on any complaint. The image build runs it, so a bad table
reds the build rather than the boot.

### Ordering

A `requires=` that is not satisfied — the dependency failed, or has
crash-looped into the capped hold — skips the dependent **permanently**,
not until the dependency recovers. That is deliberate: re-evaluating it
would mean the dependent never reaches a decision itself, and everything
ordered after *it* would then wait forever, which is the stall
`timeout=`'s default exists to rule out. A strict dependency that was not
met is an answer, not a pause; restarting the dependent once its
dependency comes back is the control socket's job.

`after=` is **ordering**. A dependent waits for its dependency to reach a
*decision*, not to succeed: a failed dependency is logged and skips nothing
— matching today's `sysinit`, where a failed job is reported and later jobs
run anyway. `requires=` is the opt-in strict form; no shipped unit needs it,
and per **I5** it can never apply to a `tty=` service.

Strictness is not the default because the shipped scripts do not support the
reasoning that would justify it: `/etc/rootcheck` ends in
`if …; then echo MARKER; fi` and exits **0** whether or not its checks
passed — it signals by withholding a marker, not by status — while
`td-firstboot` exits non-zero on a transient failure and `/etc/netup` exits
non-zero under the nettest token when `reach` fails. Strict-by-default would
therefore never engage where it would help, and would strip the console
exactly where it would hurt.

Kahn's algorithm gives the order; what it leaves behind — the residual — is
**not** the cycle. The residual holds cycle members *and* everything
downstream of them, so neither starting it nor reporting all of it as a
cycle is right. Each residual node is therefore asked whether it can reach
itself: those that can are `InCycle`, the rest are `Blocked` and named for
what blocks them. (A full SCC decomposition would answer the same question;
these graphs are a dozen nodes, and self-reachability is a shorter thing to
be sure of.) Cycle members and their dependents are skipped, the rest
started — except a `tty=` unit, which per **I5** is started anyway, last,
with a complaint recording that its ordering was ignored.

### Readiness

`oneshot` is ready when it exits 0. `daemon` is ready when spawn succeeds,
unless it declares `ready=`, which is polled until it exits 0 within
`ready-timeout`. Each probe attempt is a supervised child in its own process
group with its own deadline, **group**-killed on timeout — a probe that
hangs must not hang the supervisor, and killing only the leader would leak a
set of helpers per attempt.

A `ready=` that never succeeds leaves the unit **not ready** — it is marked
failed, which settles ordering so dependents proceed, but never reports
ready. Promoting it on timeout would make `ready=` decorative and a dead
listener indistinguishable from a healthy one, which is the exact confusion
the key exists to prevent.

A readiness probe must target the **running instance**. `sshd selftest` is
not valid: it stands up its own in-process server on an ephemeral loopback
port, so it passes while the real listener is dead.

## 4. Process handling

| need | route |
|---|---|
| spawn / wait | `Command::spawn`, `Child::wait`/`try_wait` |
| capture output | `Stdio::piped()` + a drain thread per stream |
| service on a real tty | `Stdio::from(File)` |
| own process group | `CommandExt::process_group(0)` — **not for `tty=`**, see below |
| signal a service | exec `/bin/kill -TERM -<pgid>` / `-KILL -<pgid>` |
| liveness / membership | scan `/proc/*/stat` fields 5 (pgrp), 6 (session), 7 (tty_nr), 3 (state) |
| control channel | `std::os::unix::net::UnixListener` |
| Ctrl-Alt-Del | write `/proc/sys/kernel/ctrl-alt-del` and `/proc/sys/kernel/cad_pid` |
| identity across restart | `/proc/<pid>/stat` field 22 (starttime) |

### Process groups, and why `tty=` is exempt

`setsid(2)` returns EPERM iff the caller is already a **process-group**
leader. That has two causes which are not equivalent:

- already a *session* leader — harmless, the caller is what it wanted to be;
- a process-group leader that is **not** a session leader — fatal and
  permanent, because `TIOCSCTTY` requires session leadership.

`process_group(0)` produces the second. So grouping every service uniformly
and having the terminal program treat `setsid` EPERM as success — which
looks like a tidy unification, and which `cttyhack`'s comment appears to
license — yields a console with **no controlling terminal on every boot**.
`cttyhack` only ever meets the harmless branch because PID 1 groups nothing.

Nothing is lost by not imposing a group on a `tty=` service: `setsid()`
itself creates a new process group with `pgid == pid`, which td-svc already
knows from `Child::id()`. td-svc **observes** that group rather than
imposing one. Services that will *not* `setsid` still need a group imposed —
without one they share td-svc's, and a group-wide signal would hit td-svc
itself.

### Stopping

A process group is not a containment boundary. `getty` calls `setsid()`,
creating a new session *and* group, so the login tree leaves whatever group
td-svc assigned; a login shell then creates further groups within that
session for job control, and POSIX offers no kill-by-session primitive.
Killing the wrapper does not help via terminal hangup either — hangup fires
when the *session leader* dies, and after `setsid()` that is `getty`, not
the wrapper td-svc holds.

So stopping is **session-aware and enumeration-based**: scan `/proc/*/stat`,
collect every pid in the service's containment, signal that set. The
`killall5` shape, entirely in safe `std`.

A service's containment is one thing or the other, never the **union** of
both. A unit td-svc grouped is still in td-svc's *session*, so a
group-or-session match would select every other service and the supervisor's
own parent — a stop request for one unit tearing down the machine. So:

- no `tty=` — the imposed process group, `pgid == pid` by construction;
- `tty=` — **the leader together with the device the unit named**
  (`Console { leader, tty }`); or, if that device cannot be resolved, the
  session it leads, else the group it leads, else the pid alone.

**The terminal is what solved the shipped greeter, and the obvious answers
did not.** `/etc/tty-session` is a `#!/bin/sh` wrapper that runs `getty`;
the *shell* never `setsid`s or `setpgid`s — `getty` does, one level down,
and unconditionally (`loginutils/getty.c`: "Create new session and pgrp,
lose controlling tty"). So the direct child td-svc holds leads neither a
session nor a group, and `login` and the user's shell are in a session
td-svc never created and cannot name from the pid it spawned. Every
containment keyed on that pid — `Process`, `Group`, `Session` alike —
stops at the wrapper, TERMs one shell, and satisfies **I4**'s "containment
empty" vacuously while the login tree runs on.

Running the wrapper through `cttyhack` was the recorded plan and is the
trap: it makes the wrapper a session leader, and getty leaves that session
a moment later, so it buys a `Session` holding exactly the one process
`Process` already held — at the cost of moving `setsid`/`TIOCSCTTY` onto
the one boot path whose only verification is the daily oracle.

What getty does not escape is the terminal. `setsid()` drops the
controlling terminal, and getty immediately re-acquires the SAME one with
`TIOCSCTTY(1)`, so getty and everything it goes on to exec carry that
device in `/proc/<pid>/stat` field 7.

**But the leader does not, and that is the trap.** td-svc opens the tty
`O_NOCTTY` — deliberately, so the supervisor can never acquire a console
by side effect — so the direct child inherits no controlling terminal and
its own field 7 stays 0 for its whole life. Reading the device off the
CHILD therefore yields 0, which matches nothing: a first draft did exactly
that and the containment was inert, while its test fabricated a wrapper
that already held the terminal and so passed. Verified against a real pty:
the leader's field 7 is 0, and a `setsid`-ing grandchild holds the device.

So the device is read from the descriptor `attach_tty` ACTUALLY opened, at
spawn, and recorded on the service (the same 32-bit `new_encode_dev`
packing field 7 uses). Not the unit's `tty=` re-resolved later: that path
falls back to `/dev/console` when the configured terminal cannot be
opened, so the name and the device diverge exactly when something is
already wrong, and a containment built from the name would then signal a
terminal held by somebody else. A unit whose spawn recorded NO device got
no terminal at all, and is contained by its pid alone rather than by a
device it does not hold. The containment carries the leader alongside it.
Both halves are
needed: the terminal alone misses the one process td-svc waits on, and the
leader alone misses everything getty exec'd. It is also the one variant
with two members, and the "never a union" rule still holds where it was
aimed — that rule forbids group-OR-session because either half would name
the supervisor, and neither half here can.

Device `0` is never matched: it would select every daemon on the machine.
`tty_device` will not build one, the scan will not match one, and because
td-svc's own field 7 IS 0 the ordinary `contains_self` check catches it for
the right reason rather than as a special case.

### When a stop is finished

**I4 is two conditions and the leader's exit is only one of them.** For the
greeter it is the misleading one: the leader is a shell whose `getty` child
holds the console, so it exits on TERM while the login tree runs on. A
draft that marked the unit `Stopped` there cancelled the pending KILL and
reported a stopped service with a live console.

The containment a stop was issued against is therefore RECORDED, because
once the leader is reaped its pid is gone and nothing could otherwise be
asked. On the leader's exit that scope is re-scanned: empty means stopped;
anything remaining keeps the unit stopping with its KILL still armed. An
unreadable scan is not an empty one (**I3**) — `proven_empty` is the only
reading that fails closed, and a ZOMBIE is not a member: it has already
exited and holds nothing, but it keeps its pgrp and session, so counting
one would make a stop wait on whoever gets round to reaping it.

That re-scan cannot be driven by events alone. Survivors are not td-svc's
children, so nothing wakes the loop when the last of them goes, and a unit
whose containment outlived its leader would sit `stopping` for good — the
KILL fires once and no later look ever confirms it worked. So a stop that
finds its containment occupied schedules a **sweep**, and keeps scheduling
one until the scope is empty. That timer is the only path from `stopping`
to `Stopped` for anything that outlives its leader.

Two things follow from the leader's pid stopping being ours the moment it
is reaped. First, everything keyed on it must be dropped before the scope
is used again: a `Process` scope names only the leader and is therefore
provably empty, and `Console`'s leader half goes the same way, leaving the
terminal. A group or session id STAYS — Linux keeps a pid number reserved
while it is still in use as a pgid or sid, so it cannot name a different
group while that group still has a member. Second, the KILL addresses the
RECORDED scope rather than a freshly derived one. Deriving needs a live
pid, so a re-derived target is `None` exactly when the leader is gone —
which is the case the KILL exists for — and it can also be the wrong SET,
narrowing a `tty=` unit whose device stopped resolving to the wrapper the
TERM did not go to alone.

A stop in flight is neither running nor stopped, and the phase deliberately
does not move until it COMPLETES. That is right for **I4** and wrong for
everyone reading it, so both readers consult the flag instead: `status`
reports `stopping`, and `requires=` treats the unit as absent — a strict
dependent must not start against a service that is being torn down.

The initial TERM carries the same `(pid, starttime)` identity check as the
delayed KILL, and for a sharper reason. A waiter may already have reaped
the child while its `Exited` event sits in the channel, and the kernel can
have handed the pid on; a `tty=` unit would then derive a containment from
a STRANGER and signal everything on its terminal. It fails closed and
signals nothing, which costs a stop that must be retried.

The pre-`setsid` case is the sharp one. A `tty=` child is deliberately not
grouped, so until it calls `setsid()` it inherits td-svc's process group
*and* session — and both of those name the **supervisor**. An earlier draft
fell back to "the group it is in", so a `tty=` oneshot that hit its
`timeout=` before reaching `setsid` would have sent `kill -TERM -<td-svc's
own pgid>`: td-svc, every service in its group, and the machine with it.
Every containment must be one td-svc is *not* in, and the send path
re-checks that before signalling — the two being out of step once costs the
machine, which is worth a redundant comparison.

The `/proc` read happens at **query** time, not at spawn. A read taken as
`spawn` returns almost always precedes the child's `exec`, let alone its
`setsid`, and would record td-svc's own ids. That is about the child's
pgrp and session; the TERMINAL is the opposite case and is recorded at
spawn, because it is read from td-svc's own open descriptor rather than
from the child, and because what the child got is not recoverable later.

Sequence per service, in reverse topological order: `TERM` the set, sweep
until **I4** is satisfied or `stop-timeout` elapses, `KILL` the set, sweep
again. Then close log handles, then `/etc/shutdown`, then the power applet.
("Sweep" is the scheduled re-scan above, not a busy poll: the loop keeps
serving events between them.)

The `timeout=` path already uses the first half of that sequence, and must
use both: a `TERM` alone leaks a process that ignores it, and its waiter
thread with it. So a unit that overruns `timeout=` is TERMed, keeps its pid
(the only remaining handle on something that has not died), and is KILLed
`stop-timeout` later. Its dependents are released at the TERM, not the
KILL — the decision is already made.

That delay is exactly why field 22 is read. `stop-timeout` separates the
TERM from the KILL, and in that window the child can die, be reaped by its
waiter, and have its pid handed to something new — which the KILL would
then hit. A pid does not identify a process; a pid plus its start time
does. With no recorded identity nothing is signalled at all: failing
closed there leaks at most one process, and failing open kills an
unrelated one.

The scan behind all of this reports what it read AND what it could not,
because signalling and liveness want opposite things from a failure.
Signalling is best effort: one unreadable stranger must not stop td-svc
TERMing the processes it did find. Liveness is **I4**, and must fail
closed: a scan that dropped what it could not read would report the
containment empty while a service was still running.

### `/proc/<pid>/stat` parsing

`comm` is parenthesised and may contain spaces *and* `)`. Fields are found
by splitting after the **last** `)`. One parser serves three callers —
starttime (22) for identity, pgrp (5) and session (6) for stopping — and is
tested against a hostile `comm`.

"That process is gone" arrives two ways and both must be read as gone:
`ENOENT` when the `open` loses the race, and **`ESRCH` when the read does** —
`stat` is a seq_file, so a process that exits between the two fails at the
read. Rust maps errno 3 to no named `ErrorKind`, so a `NotFound`-only test
misses it and the scan errors out exactly during a teardown, when processes
exit fastest. Everything else (`EACCES`, `EIO`, an unmounted `/proc`) stays
an error: per **I3** an unreadable `/proc` is a fault, never an emptiness.

## 5. The event loop

No signals, no `pidfd` (unstable on the pinned rustc), no `wait4(-1)` (that
is PID 1's). So: **a thread per blocking wait, reporting to a single-owner
main loop over an mpsc channel.**

- One waiter per running service, blocking in `Child::wait`; on return it
  sends the exit — or, distinctly, the wait *error* — and stops. This reaps
  promptly, which **I4** depends on, without polling.
- One probe thread per daemon that declares `ready=`, for as long as it is
  probing.
- Two drain threads per captured service, and one control thread in
  `UnixListener::accept`, when those land.

The main loop is the sole owner of all supervision state, so there is no
state mutex and nothing to hold across a blocking call: threads block, then
send. It blocks in `recv_timeout` with the timeout set to the nearest
pending deadline — a retry, a `timeout=`, a KILL, a console's patience —
so there is no timer thread. Two floors bound that: a `TICK` minimum, so a
deadline a microsecond away cannot spin the loop, and a one-second ceiling
when nothing is pending at all. Both are arbitrary; neither is a poll
interval that decides how promptly anything happens, because every real
wake-up comes from the channel or from a computed deadline.

Every event carries the **generation** of the instance it describes, bumped
on each spawn; an event whose generation is stale is dropped. Without it a
probe launched for an instance that has since died marks its *replacement*
ready — the failure is silent and looks exactly like success.

## 6. Restart policy

```
delay = min(BASE * 2^(consecutive_failures - 1), CAP)
BASE = 100ms   CAP = 5min   MIN_UPTIME = 1s
```

The counter resets only on a run that lasted **and** ended cleanly — both,
not either. Resetting on any run longer than `MIN_UPTIME` lets a daemon that
crashes just *above* it restart at the base delay forever: ~50 restarts a
minute, escalating never and, because the log gate fires only on an
escalation, saying nothing. Resetting on any clean exit has the mirror
problem for a daemon that exits 0 immediately.

No jitter: jitter spreads a herd of clients across a shared server, and
local supervision has no herd. Log the first failure and the transition into
the capped hold, then hold silently — a job that can never start must not
scroll a serial console until the diagnostics are gone.

`restart=on-failure` does not restart on exit 0. `restart=never` is not a
oneshot: its dependents are released when it *spawns*, not when it exits.

A unit that could not be **spawned at all** — a missing binary, a bad
interpreter — settles as failed immediately rather than sitting at "not yet
started" behind a retry. Downstream, a binary that does not exist and a
binary that runs and exits non-zero are the same news, and they used to
differ by the ~7 minutes it takes backoff to reach the hold — during which
every dependent waited, the console among them. A oneshot or a
`restart=never` daemon says so once and stops; nothing would retry it.

The **spawn** diagnostics ride this same gate — the terminal a `tty=` unit
could not open, the console its stderr could not reach. That is what stops a
greeter with a missing terminal from scrolling the console at the restart
rate with the one message that would have explained it, and it was a real
bug, not a hypothetical. But the cost is worth naming, because it differs
from the restart message's: the restart message really is the same sentence
every time, while these describe the *environment*, which can change between
attempts. A unit already deep in a crash loop for an unrelated reason whose
terminal then disappears will not say so until the escalation attempt. The
gate is keyed on the failure count alone, so it suppresses the first
occurrence of a *new* diagnostic, not only repeats of an old one.

## 7. Log capture

Drain threads must never block the service, which rules out writing straight
through a shared writer — a stalled write backs up into the pipe and blocks
the service before any error surfaces. Drains feed a bounded per-service
queue emptied by one writer thread; when the queue is full, lines are
**dropped and counted**, with a `... N lines dropped` marker when it drains.

Rotation is by size, N bytes × M generations. `/var` is a persistent Btrfs
volume and an unbounded log is a way to fill it.

Two ordering rules:

- **Close every `/var` log handle before `/etc/shutdown` runs.** `umount
  /var` fails EBUSY against an open file, `/etc/shutdown` withholds its
  marker on a failed unmount, and the boot oracle greps for that marker — so
  a stray handle presents as a mount bug. This is best-effort by nature: a
  writer wedged in `write(2)` on a stalled filesystem still holds its fd
  after a deadline-join abandons it. The marker tripwire is what catches
  that case.
- **Never join a drain thread without a deadline.** A descendant holding the
  pipe write end keeps it open after the leader exits.

`tty=` and `log=` are mutually exclusive: a pipe would break job control. A
`tty=` service's stderr points at `/dev/console` so its own startup failures
are not swallowed — and if its own terminal cannot be opened at all, that is
where its stdin and stdout go too. **I5** is why it falls back rather than
refusing to start: a console that is missing a device must still be
attempted. The fallback also has to exist because `build` sets stdin to
`null`, so leaving a failed open alone gave a greeter a shell that read
immediate EOF, exited, and turned a missing device into a restart spin.

## 8. Control socket and shutdown

`/run/td-svc/control`, a `UnixListener` inside a `0700` directory created
*before* bind. Newline-delimited commands: `status`, `start|stop|restart
NAME` (landed), `reload`, `reboot|poweroff|halt` (with shutdown).

The directory carries the protection, not the socket: a path that cannot be
traversed cannot be connected to whatever the socket's own bits say. It is
created AT `0700` by `mkdir(2)` rather than created and then narrowed —
`create_dir_all` uses `0777 & ~umask`, and td-svc inherits PID 1's umask,
which PID 1 never sets, so a create-then-narrow leaves a window in which
anything can open a descriptor and keep traversal rights forever after.
umask can only remove bits from the mode `mkdir` is given, so `0700` is a
ceiling as well as a floor. A directory that already exists was not created
here, so it is adopted only if this process owns it; narrowing does not
revoke a descriptor somebody already holds.

A socket left by a supervisor that DIED is unlinked — PID 1 respawns
td-svc, so finding one is the ordinary case after a crash, and `bind` fails
with `EADDRINUSE` on an existing path whether or not anyone is listening.
But unlinking unconditionally is worse than refusing: `0700` keeps out
other USERS, not a second root td-svc, so a second instance would make the
first unreachable while both went on supervising and every later `stop`
addressed the wrong one. Connecting is the test, because it is the same
question a client asks — refused means a corpse, accepted means an owner.

One thread serves connections one at a time; a request crosses to the main
loop as a message and the reply crosses back the same way, so nothing
outside the loop touches a `Service` and there is still no lock (§5).
Serving serially is what makes the bounds load-bearing rather than tidy.
The READ is bounded in time and length, so a client that connects and never
finishes a request cannot hold the thread or grow a buffer inside PID 1's
only supervisor. The WRITE is bounded for the same reason at the other end
of the exchange: a client that asks and then stops reading would otherwise
block `write_all` once the reply outgrew the socket buffer. The client's
own wait is bounded too. An operator reaching for this socket is usually
doing so BECAUSE something is wrong, and a control tool that hangs then is
the least useful thing it could do.

Each bound is pinned by a test that can tell it apart from the others,
which is harder than it sounds: a client that half-closes is refused by the
incomplete-request check without the timeout or the length cap ever
running, so a test written that way passes with either of them deleted.

A requested stop is not a failure. `Phase::Stopped` is distinct from
`Failed` precisely so the restart policy cannot see it: a `restart=always`
daemon that an operator stopped has to stay stopped, or `stop` is a no-op
with extra steps. It still SETTLES, so a dependent waiting on it proceeds
rather than hanging on a decision that has already been made. Only an
explicit `start` leaves that phase, and both `start` and `restart` clear
the failure count — an operator asking again is plainly trying to interrupt
a backoff, and serving it out anyway would make the verb useless exactly
when it is most wanted.

`stop` does not WAIT. The TERM goes out, the KILL is scheduled at
`stop-timeout` through the same `kill_at` the `timeout=` path uses, and the
phase changes only when the stop COMPLETES in the sense **I4** means it —
see §4, "When a stop is finished". Replying "stopped" before that would
claim a death that has not happened, so the reply says what was sent.

Nothing is recorded unless the signal actually went out. An earlier draft
set `stopping` and armed the KILL before knowing whether the containment
could be derived or would be refused, so a stop that signalled nothing
still replied without an `error:` prefix and the client exited 0.

`Stopped` settles for `after=`, which asks whether a decision has been
reached, and an operator's stop is one. It does NOT satisfy `requires=`,
which asks whether the dependency is THERE — a service someone stopped is
exactly as absent as one that failed. `Held` was added to that test with
the same argument.

A `start` cannot rescue a unit the PLAN dropped. `start_eligible` walks the
resolved order, and a unit in a dependency cycle is not in it, so setting it
`Down` would reply "starting" and leave it there forever; it answers with
the reason instead. §3 assigns cycle recovery to this socket, and that
remains unbuilt — it needs a `reload`, which lands with shutdown.

`reload` is transactional: on any diagnostic the last-known-good table is
kept rather than applying the valid fragments.

Shutdown writes `/run/td-svc/shutdown` before stopping anything (**I6**). A
replacement instance that finds it **resumes to the power applet** — parking
forever is a hung machine. Two constraints make that hold:

- The teardown must not destroy the state it is gated on. `/etc/shutdown`
  runs `umount -a -r`, which would take `/run` with it; `/run` is tmpfs
  holding nothing that needs releasing, so it is excluded.
- Once the transition begins, `start`/`restart`/`reload` are rejected and a
  second shutdown request is a no-op.

### The greeter's exit status is meaningful

`/etc/tty-session` ends `getty … && <tail>`. That `&&` is a safety property,
not sequencing: if getty/login fails to start a session *at all*, the
non-zero exit short-circuits the reboot so the wrapper is respawned visibly,
rather than firing `reboot` and letting QEMU's `-no-reboot` mask a broken
greeter as a clean exit-0 shutdown. **"The greeter never started" and "the
user logged out" must remain distinguishable** — losing that yields a green
boot oracle on a machine with no greeter, the worst shape a regression can
take, since the oracle is what would otherwise catch it. td-svc never reads
a non-zero greeter exit as a shutdown request; that is a restart.

## 9. Ctrl-Alt-Del

`ctrl_alt_del()` does `kill_cad_pid(SIGINT, 1)` when `C_A_D == 0`, and
otherwise schedules `kernel_restart(NULL)` — which runs reboot notifiers and
`device_shutdown()`, but no userspace teardown, no `sync(2)`, no unmount.
`C_A_D` is `/proc/sys/kernel/ctrl-alt-del` (0644); the SIGINT recipient is
`/proc/sys/kernel/cad_pid` (0600), which accepts any live pid.

So PID 1 need not be the target, and the target needs no *handler* — it
needs to **die, observably**. td-svc:

1. writes `0` to `ctrl-alt-del` **first**, reading it back. Arming in the
   other order leaves a window in which a press still hard-resets;
2. spawns `td-svc cad-sentinel` with a pipe on its stdin, **retaining the
   write end**, so the sentinel's `read` blocks rather than seeing EOF;
3. writes the sentinel's pid to `cad_pid`. A failed *write* of ESRCH means
   the sentinel is already gone — retry. A successful write and read-back
   prove only that the kernel stored the pid, not that the sentinel lives;
   liveness comes from `try_wait`;
4. watches it like any other child. Death by **SIGINT specifically** is the
   press; any other death is a bug to log and re-arm from.

Re-arming is required after each press — `cad_pid` holds a reference to the
reaped sentinel's `struct pid`, so delivery finds no task until a new pid is
written — and is suppressed once a shutdown is in flight, so a second press
cannot restart the sequence.

**Two honest limits.** The trigger is not authenticated: any root process
can `kill -INT` the sentinel, so the console should say "shutdown requested
via CAD sentinel", not "Ctrl-Alt-Del pressed". And on td's current
`allnoconfig` kernel there is no `CONFIG_VT` and no input stack, so
`ctrl_alt_del()` has no caller at all — the mechanism is testable and
correct, but a real key press needs a separate kernel-config increment.

Testing separates what is proven from what is not: the **arming** is
verified by reading both sysctls back; the **reaction** by `kill -INT` on
the sentinel; the kernel path joining them is *not* covered, and only a
VT-enabled `sendkey` test would cover it.

## 10. Status

Landing 1 implemented the crate: table, validator, ordering, readiness,
backoff, supervision, the `/proc` layer, and the event loop.

Landing 2 (this one) is the cutover. `/etc/td-svc.conf` is generated by the
system-x86-64 recipe, `/bin/td-svc` is packed, and PID 1's inittab is
reduced to the three pseudo-filesystem mounts plus `::respawn:/bin/td-svc
run`. Every service — hostname, td-firstboot, rootcheck, netup,
bootsuccess, bootfail, sshd, the greeter — is now a unit with declared
edges, and `td-svc check` validates that table at image-build time, so an
ordering regression reds the build rather than the boot.

One thing about the running system DID change, and it follows from **I5**
rather than from the table: under the inittab the greeter waited for every
`::sysinit:` job unconditionally, because `respawn` lines simply did not
start until `sysinit` finished. Under td-svc a console waits at most
`CONSOLE_PATIENCE` and then starts with its ordering ignored. On a first
boot under TCG, td-firstboot's keygen plus rootcheck's process farm plus
netup's DHCP can exceed that together, so the forced start is a normal-path
outcome rather than an edge case, and getty's output interleaves with
rootcheck's and netup's markers. That is the trade I5 asks for — an
interleaved console beats a machine that is up and cannot be repaired from
its own console — and the boot oracle latches markers order-independently,
so it does not depend on the ordering either way.

The mounts stay with PID 1 deliberately: td-svc reads `/proc` for its own
group and session and for every containment and liveness query, and one
started before `/proc` exists comes up unable to signal a group at all.
`sysinit` runs to completion before any `respawn` line, so that ordering is
the guarantee.

Landing 3 added the control socket and the stop path: `/run/td-svc/control`
with `status`, `start`, `stop` and `restart`; `td-svc <verb> [NAME]` as the
client so the socket is usable from the image without another tool; and
`Containment::Console`, which is what finally reaches the greeter's login
tree (§4). `reload` and the power verbs land with ordered shutdown, since
they are requests to begin a transition that does not exist yet.

Ordered shutdown, Ctrl-Alt-Del arming, and log capture land subsequently,
in that order. Sections above describe the completed design; anything not
yet built is specified here so the later landings implement a reviewed
target rather than an improvised one.

### A table with no units, and the rescue td-init has but td-svc does not

`td-init` falls back to a built-in `::respawn:/bin/cttyhack /bin/sh` when
`/etc/inittab` is unreadable, precisely so a broken table still leaves a
shell on the console. After the cutover the console is a *unit*, so td-svc
handed an unreadable or missing table has no such floor: it prints the load
error, comes up with zero services, and — having no exit path — idles
forever while PID 1 never respawns it. One line, then silence
indistinguishable from a healthy boot.

The build-time guards cover the *generated* table (`td-svc check` runs over
it during the image build, and the recipe derives the inittab's `-f`, the
generated file, and that check from one constant), so reaching this state
takes a corrupted erofs. It is still the worst outcome in this document, so
until there is a rescue unit td-svc at least keeps *saying* so, on a slow
throttle rather than once: a supervisor with no units repeats that fact
every `SILENT_TABLE_COMPLAINT` and names the file to go look at. Repeating
is not recovering. A built-in fallback console — td-init's answer, one
level up — is the fix, and it belongs with the landing that owns rescue.

### Nothing reconciles a supervisor that died

Inserting td-svc between PID 1 and the services buys one failure mode the
inittab could not have: **if td-svc dies, its children do not.** They
reparent to PID 1, which reaps them but knows nothing about them, and the
respawned td-svc starts from `Down` and launches its own. An orphaned
`sshd` keeps `:22` while its replacement takes `EADDRINUSE` and
crash-loops into a hold — the service works, the supervisor reports it
broken — and two greeters read `ttyS0` at once, which is worse.

What keeps this narrow is that **td-svc has no exit path at all**:
`Runtime::run` is `-> !`, there is no `process::exit`, no `exec`, and a
table that fails to parse still enters the loop with no units rather than
exiting into a respawn spin (§5 records the `Disconnected` arm choosing to
idle for exactly this reason). Nothing here voluntarily terminates, so
reaching this state takes a signal or an abort.

That is a reason it is unlikely, not a reason it is handled. The fix is
not adoption — orphans are PID 1's children and td-svc cannot `wait4` them
— but *eviction*: record each spawned `(pid, starttime)` under `/run`, and
on startup kill anything still alive whose starttime still matches. The
`(pid, starttime)` identity check that closes the TERM→KILL pid-reuse
window (§4, "Stopping") is already the primitive that needs; what is
missing is the state file and the policy for a partial one. It is a
feature with its own failure modes, so it gets its own landing.

Two things the next landings must resolve before this is correct, both
found in review and recorded above rather than left to be rediscovered:
the supervisor-restart eviction contract just described, and `log=` /
`console=` being refused by name until log capture exists, so the
`tty=`/`log=` exclusion has nothing to enforce yet (§3).

A third — the shipped greeter's containment reaching only its wrapper
process — was the reason for this landing and is closed.

The greeter one is RESOLVED, and not the way this document previously said.
The recorded fix was to spawn the wrapper through `td-init`'s `cttyhack`
so it would lead a session. That does not work, and the reason is worth
keeping: `/etc/tty-session` runs `getty` as a CHILD, and busybox getty
opens with an unconditional `setsid(2)` — `loginutils/getty.c`, "Create
new session and pgrp, lose controlling tty". So `login` and the user's
shell end up in a session getty made, not the wrapper's. `cttyhack`
would have moved `setsid`/`TIOCSCTTY` onto the boot path to buy a
`Session` containing exactly the one process `Process` already contained.

What getty does not escape is the TERMINAL: it re-acquires the same one
with `TIOCSCTTY`, so it and everything it execs carry the device in
`/proc/<pid>/stat` field 7. Hence `Containment::Console`, which is keyed
on the device the unit's `tty=` names — not on the child's own `tty_nr`,
which is 0 because td-svc opens the console `O_NOCTTY` — and so reaches
the login tree without touching the boot path at all.
