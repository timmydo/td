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

**I1. One `unsafe`, and it is `kill(2)`.** The crate `#![deny(unsafe_code)]`s
— `deny`, not `forbid`, because `sys.rs` carries a single scoped `#[allow]` on
the `syscall` body and `forbid` cannot be relaxed from the inside. Every OTHER
capability td-svc needs is reachable through safe `std`; where that required a
non-obvious route, §4 records which.

This was `forbid` until the supervisor stopped shelling out to `/bin/kill`.
The exec was not free of risk, it just moved the risk somewhere unguarded: the
ability to stop anything became a runtime dependency on a third-party
multicall being present at an absolute path and reading `-<pgid>` as a group
rather than a flag, and nothing tied the two together, so removing that applet
from the image would have disarmed every `stop`, every `restart` and the whole
ordered teardown with no build-time complaint. It also cost a `fork`+`exec`
per signal during shutdown, and made seven of this crate's stop-path tests
skip on a host without `/bin/kill` — the code most needing coverage, least
often run. One confined syscall is the smaller surface, and `main.rs`'s
confinement tests hold it to one. A SECOND is an UNSAFE.md amendment.

**I2. No `pre_exec`, ever.** td-svc is multithreaded (log drains, waiters).
A `pre_exec` closure runs between `fork` and `exec` in a multithreaded
process, where only async-signal-safe operations are legal — an allocation,
or a lock another thread held at fork time, deadlocks the child. Everything
needed is reachable through `Command`'s own safe setters, which `std`
implements inside its async-signal-safe post-fork path.

**I3. Liveness is read from `/proc`, never inferred from a signal's result.**
A `kill -0` probe answers through one channel for two questions, so "the
target is gone" is indistinguishable from "the signal was refused" — and,
when it was a `kill(1)` exit code, from a spawn failure, an ENOENT, or a
rejected argv as well. A liveness test that reads *any* failure as "gone"
would declare a live service dead and let the teardown unmount underneath it.
`/proc` fails closed: unreadable is an error, not an emptiness. `kill(2)` is
used only to *send* signals. Its one policy — that **ESRCH reads as success**
— follows from the same rule rather than bending it: the target may die
between the `/proc` read that chose it and the signal, so "nothing was there"
is not news about liveness and must not be reported as a failure. Every other
errno is, because a signal that could not be sent means the stop it was part
of never happened.

Two targets are refused outright, before the syscall: `0` and `-1`. `kill(2)`
accepts both and means by them something td-svc never does — `0` is the
caller's own process group, and `-1` is a broadcast to every process this one
may signal, which is *not* "process group 1" however it was arrived at.
Neither is reachable by naming it; both are reachable by ARITHMETIC, because a
containment is signalled as `-pgid` and a pgid that read back as 1 negates
into the broadcast. Nothing downstream would report it, because the signal
succeeds. The refusal lives in `send_signal` rather than in the callers: that
is the one point every signal passes through.

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

I5 has exactly one limit, and it is **I6**: once a shutdown begins, nothing
starts, a `tty=` unit included. The two invariants want opposite things there
and I6 wins, because the machine a console exists to rescue is being taken
apart — its filesystems are going away, and a console handed to someone at
that moment can only mislead. Everywhere else I5 is absolute.

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
log=/var/log/svc/sshd.log
console=no

[greeter]
type=daemon
exec=/etc/tty-session
after=netup
tty=ttyS0
restart=always
```

Keys: `type` (`oneshot` | `daemon`), `exec`, `after`, `requires`, `restart`
(`always` | `on-failure` | `never`), `tty`, `log`, `console` (`yes` | `no`),
`timeout`, `ready`, `ready-timeout`, `stop-timeout`.

**Nothing is accepted-and-ignored.** A key the supervisor would silently
drop reads in the table as a guarantee it does not make, so each is
rejected instead:

- a key that does not apply to the unit's `type` — `ready=` and `restart=`
  on a `oneshot`, `timeout=` on a `daemon`;
- a key whose value cannot be honoured — a relative `log=` would resolve
  against whatever directory PID 1 left td-svc in, and `console=` takes
  `yes` or `no` and no synonyms, because every spelling admitted is one the
  next reader has to know;
- a PAIR that cannot both be honoured — `tty=` with `log=` (a captured
  stream is a pipe and job control needs a terminal), and `console=yes`
  without `log=` (there is nothing to copy, and a service that inherits
  td-svc's stderr already reaches the console);
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
| signal a service | `kill(2)` via `sys::kill`, negative target for a group (§2 I1) |
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
and unconditionally (`td-init/src/getty.rs`; busybox's `loginutils/getty.c`
did the same, and this reasoning predates the applet moving to td-init). So the direct child td-svc holds leads neither a
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
controlling terminal, and getty immediately re-acquires the SAME one, so
getty and everything it goes on to exec carry that device in
`/proc/<pid>/stat` field 7. td-init's getty claims it with `TIOCSCTTY(0)`
where busybox used `(1)` — it does not steal from a live session — which
changes nothing here: td-svc opened the device `O_NOCTTY`, and the kernel
clears the association when a session leader exits, so the terminal a
restarting greeter claims is free. What the non-stealing form buys is that
a terminal somebody else genuinely holds produces a refusal the supervisor
restarts, rather than a session yanked out from under its owner.

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
the service before any error surfaces. A service that stops making progress
because its LOG is slow is a worse failure than losing the log. Drains feed a
bounded per-service queue emptied by one writer thread; when the queue is
full, lines are **dropped and counted**, with a `... N lines dropped` marker
written where the gap actually is. `CAPACITY` is 1024 lines: unbounded would
move the failure from "the log lost lines" to "td-svc grew until the machine
died", and td-svc is the one child PID 1 has that must not do that.

**One writer per SERVICE, not per instance.** A restarting daemon would
otherwise acquire a second writer with its own handle on the same path, and
two writers rotating one file race — both rename, and one then reopens a file
the other has already moved. The queue and its writer are created on first
spawn and reused; each new instance's drains push into the queue already
there. Drains ARE per instance and end at EOF.

Rotation is by size, `MAX_BYTES` (256 KiB) × `GENERATIONS` (4), a ceiling of
1.25 MiB per service. `/var` is a persistent Btrfs volume and an unbounded log
is a way to fill it — at which point every service that writes anywhere stops
working, which is a far bigger outage than a truncated log. The live file is
closed BEFORE the renames: writing through a handle whose name has moved
appends to the rotated generation, so the live file would stay empty and the
cap would never bind again. A single line larger than the whole budget is
written anyway rather than rotating forever trying to make room for it. The
file is opened before the size is consulted, because the size of a PERSISTED
log is only known once it has been stat'd: a supervisor restarting onto an
already-full file would otherwise append one line to it before the first
rotation, and if that were the only line it wrote the file would stay over the
ceiling for the life of the boot.

**Neither the reader nor the mode is left to the producer.** `read_line` is
unusable on both counts: it fails on output that is not UTF-8, so a service
printing a binary blob would end its own capture, and it has no length bound,
so a service printing without newlines would allocate until the machine died.
Invalid bytes are replaced and lines longer than 8 KiB are split. The file is
opened 0600 and a directory `log=` names is created 0700, for the reason the
control socket and the eviction record already state: td-svc inherits PID 1's
umask, which PID 1 never sets, so `create_dir_all`'s `0777 & ~umask` would
leave anyone able to replace the file td-svc then appends to.

**No writer means no pipe.** The pipes are wired only once the capture exists,
which is why the capture is created BEFORE the spawn rather than after it. A
pipe td-svc holds and never reads is the worst outcome this feature has
available: the service blocks in `write(2)` the moment it fills the buffer,
which is precisely the wedge the bounded queue exists to prevent. Without a
writer the unit inherits td-svc's own stdio, exactly as it did before capture
existed — degraded, but running. (A DRAIN that cannot start is past that
point: the pipe is already wired, so its stream is dropped and the service
gets EPIPE. That is the better of the two failures still available.)

**A reload retires a capture it no longer owns.** Two cases, both of which
would otherwise leave a writer waiting forever on a queue nothing can reach —
still holding the `/var` descriptor, and no longer reachable from `captures()`
for `close_logs` to stop. A service DROPPED by a reload is the last holder of
its handle. A service whose `log=` or `console=` CHANGED is keyed to a
destination that is no longer the one the table names, so keeping it would send
output to the previous file and make the reload silently not apply; it is
retired and the next start opens the new one. Retiring does not wait for the
writer: the main loop is answering a control request, and a wedged filesystem
must not hold the supervisor there.

**Two units may not share one `log=`.** The same race this module refuses for
restarts, reached across units instead — two sinks, two writer threads, two
size counters, two rotators on one file. It needs the whole table to see, so it
is refused where duplicate unit names already are.

**The queue is bounded in BYTES as well as lines.** A line count is not a
memory bound: a drained line can be `MAX_LINE` plus whatever the reader had
buffered, and a lossy conversion of non-UTF-8 input trebles it, so 1024 lines
is tens of megabytes per service in the worst case — reachable by any service
printing binary faster than a slow `/var` drains. `CAPACITY_BYTES` (1 MiB) is
what makes the claim above true.

**The log path is opened `O_NOFOLLOW`.** The modes protect only what td-svc
CREATES, so a `log=` naming a file inside a pre-existing writable directory is
otherwise a way to have root-owned service output appended to a file of
somebody else's choosing. Refusing to follow the link is cheaper than reasoning
about which directories are safe.

**A sink that cannot write says so once.** Latched, not rate-limited: a
read-only or full `/var` fails on every line, and a complaint per line would
scroll the console with the message that explains it. Once is what the reader
needs, and the log's own absence is the rest of the evidence.

`console=yes` copies each line to `/dev/console`, prefixed with the service
name so a console carrying several services can be read. Each copy is ONE
`write_all` of one buffer: `writeln!` issues a write per format fragment, and
two copying services would interleave mid-line, defeating the prefix that makes
a shared console readable. The shipped sshd sets it — capture alone would take
its failures out of the serial output the boot oracle prints when sshd is why
the boot failed. It is refused without
`log=`: there is nothing to copy, and a service that inherits td-svc's stderr
already reaches the console.

Two ordering rules:

- **Close every `/var` log handle before `/etc/shutdown` runs.** `umount
  /var` fails EBUSY against an open file, `/etc/shutdown` withholds its
  marker on a failed unmount, and the boot oracle greps for that marker — so
  a stray handle presents as a mount bug. `close_logs` asks every writer to
  finish and waits `LOG_CLOSE_GRACE` (3s) for all of them together, then
  proceeds regardless and NAMES what would not let go. Best-effort by
  nature: a writer wedged in `write(2)` on a stalled filesystem still holds
  its fd after the deadline abandons it, and blocking the shutdown on it
  would trade a lost log for a machine that never powers off. The marker
  tripwire is what catches that case, and the named service is what saves
  the next reader from diagnosing it as a mount bug.
- **A drain thread is never joined at all.** The original rule said "not
  without a deadline"; building it showed there is nothing to wait for. A
  descendant that inherited the pipe write end holds it open after the
  leader exits, so the drain is blocked in `read(2)` on a process td-svc
  does not supervise and cannot signal. It is left running and the writer is
  stopped instead — the writer is the one that holds the `/var` fd, which is
  the only part of this the unmount cares about.

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
*before* bind. Newline-delimited commands: `status [NAME]`, `start|stop|restart
NAME`, `reload`, and `reboot|poweroff|halt`.

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
the reason instead. §3 assigns cycle recovery to this socket: the route is
`reload`, which re-reads the table, so a cycle is repaired by fixing the
file and reloading rather than by a verb that pretends the plan says
something it does not.

`reload` is transactional: on any diagnostic the last-known-good table is
kept rather than applying the valid fragments. A table is a graph, so a
partial apply is not a partial configuration but a different one — half of
a renamed dependency edge resolves to an order nobody wrote.

Shutdown writes `/run/td-svc/shutdown` before stopping anything (**I6**). A
replacement instance that finds it **resumes to the power applet** — parking
forever is a hung machine. Two constraints make that hold:

- The teardown must not destroy the state it is gated on. `/etc/shutdown`
  runs `umount -a -r --exclude /run`; without the exclusion it would take
  `/run` with it, and the replacement would see a clean start and bring
  services up against filesystems already released. The exclusion is EXACT,
  so the moved btrfs at `/run/td-volume` is still unmounted; `/run` is tmpfs
  holding nothing that needs releasing.
- Once the transition begins, `start`/`restart`/`reload` are rejected and a
  second shutdown request is a no-op. Two presses, or a greeter and a park
  handshake racing, are the ordinary way it arrives twice — so the second
  reply reports the transition already under way rather than an error.

The teardown walks BACKWARDS, one unit at a time, through
the ordinary `stop` path — the same TERM, recorded containment, scheduled
KILL and I4 sweep an operator's `stop` uses. A second teardown path would be
a second set of bugs, and this one is already the harder-won code. A unit
that will not go is waited for `stop-timeout`, doubled and with the sweep
interval and a little slack added, and then left behind with a log line: a
machine that refuses to power off because one service will not die is worse
than one that powers off with it still running.

What it walks is captured once, when the request arrives, and it is NOT simply
the start order. A unit a `reload` dropped from the table is in no plan and no
start order — but if it is still on its way down it is still a running
process, and walking only the order would run `/etc/shutdown` while it held a
filesystem open. Those go LAST in the captured walk, so the reverse pass
reaches them FIRST: nothing still declared can depend on a unit that is no
longer declared. A `reload` cannot change the walk underneath the teardown
because `reload` is refused once the transition has begun.

Reload's other half of that bookkeeping: a removed unit that is NOT running is
dropped immediately rather than kept as a corpse. Keeping it would leak a
`Service` per removal per reload, leave undeclared units in `status` forever,
and — worse — re-declaring the name later would match the corpse and adopt its
`Phase::Stopped`, which is a standing operator decision `start_eligible` will
not override, so the re-added unit would never start.

One word is the verb, the marker's contents and the applet's basename. That
is deliberate: a resume reads back what the request wrote, and a mapping
that disagreed anywhere would reboot a machine an operator asked to power
off — silently, since nothing compares the two. A marker that is torn or
unrecognised reads as `reboot` rather than as no shutdown at all, because
the state it was written from is a system already being torn down.

`finish_shutdown` runs `/etc/shutdown` and then `exec`s the applet, so it
does not return on success. It opens `/dev/console` for the teardown rather
than passing on whatever td-svc inherited: the teardown's last act is a
marker the boot oracle latches, and by then the greeter's session leader is
gone, so writes through any descriptor inherited from that terminal return
EIO after the kernel's vhangup. The marker would simply vanish, which is
indistinguishable from a teardown that never ran. If the `exec` fails the
applet is missing or refused; td-svc says so on a timer and PARKS rather than
resuming supervision of a system whose filesystems `/etc/shutdown` has just
released. Exiting would be worse: PID 1 respawns td-svc unconditionally, so a
fresh instance would read the marker, resume straight back to the same missing
applet and exit again — a hot respawn loop through PID 1, no likelier to
succeed on the hundredth try than the first. Parking leaves the machine
diagnosable and the marker in place, so the handoff still completes if what
was wrong is repaired.

**Nothing on the image calls a power applet directly.** `/etc/tty-session`
and `/etc/bootfail` — the two things that decide a boot is over — each
`exec /bin/td-svc reboot`, and the `reboots_run_the_teardown_first` recipe
test holds that over every generated `/etc` file: no direct
`/bin/{reboot,poweroff,halt}`, no inlined `/etc/shutdown`, and exactly two
initiators. Before td-svc they inlined `{ /etc/shutdown; exec /bin/reboot; }`
themselves, which was right when nothing was supervised and resets a machine
with live services now that something is.

### The greeter's exit status is meaningful

`/etc/tty-session` ends `getty … && exec /bin/td-svc reboot`. That `&&` is a
safety property, not sequencing: if getty/login fails to start a session *at
all*, the non-zero exit short-circuits the reboot so the wrapper is
respawned visibly, rather than firing `reboot` and letting QEMU's
`-no-reboot` mask a broken
greeter as a clean exit-0 shutdown. **"The greeter never started" and "the
user logged out" must remain distinguishable** — losing that yields a green
boot oracle on a machine with no greeter, the worst shape a regression can
take, since the oracle is what would otherwise catch it. td-svc never reads
a non-zero greeter exit as a shutdown request; that is a restart.

Delegating the reboot makes the greeter DEPEND on td-svc being reachable,
which it did not before. If the control socket is gone the client exits
non-zero, the wrapper exits non-zero, and the greeter is respawned — a retry
loop at the console with td-svc's own error printed to `/dev/console` beside
it. That is the right direction to fail: a supervisor that cannot be reached
is broken, and under QEMU's `-no-reboot` this shows up as a boot that never
ends rather than as a clean exit 0. A hung test is a worse experience than a
failing one but a far better signal, and it is the same trade the `&&` above
already makes. What the cutover must never do is fall back to `/bin/reboot`
when td-svc cannot be reached: that would reset the machine with services
running and the marker unwritten, which is exactly the outcome the invariant
test forbids.

The `exec` matters as much as the `&&`. The greeter is a `restart=always`
unit, so the process td-svc watches for it BECOMES the client asking to
reboot — and stopping the greeter is one of the steps that request triggers.
td-svc must not read that exit as a crash: `stopping` short-circuits the
restart policy before it is consulted, or a `restart=always` greeter would
come back mid-teardown, forever, and the walk would never reach the handoff.

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
3. writes the sentinel's pid to `cad_pid`, reading it back. A failed *write*
   of ESRCH means the sentinel died between spawn and here, and is reported
   as such rather than retried in place: the answer is a whole fresh arming,
   which is what the re-arm path already does. A successful write and
   read-back prove only that the kernel stored the pid, not that the sentinel
   lives; liveness comes from the watcher below;
4. watches it like any other child — a thread blocked in `wait`, so a death
   *wakes* the event loop rather than being noticed on its next pass, which
   can be minutes out when a crash-looper sits at the backoff cap. Death by
   **SIGINT specifically** is the press; any other death is a bug to log and
   re-arm from.

Re-arming is required after each press — `cad_pid` holds a reference to the
reaped sentinel's `struct pid`, so delivery finds no task until a new pid is
written — and is suppressed once a shutdown is in flight, so a second press
cannot restart the sequence. Two things shape it:

- **It is scheduled, not immediate, and backs off.** A sentinel that dies for
  a persistent reason dies again as fast as it can be spawned; re-arming
  inline made that a fork loop that starved the event loop and filled the
  console. It rides `backoff::delay`, the same curve a crash-looping service
  gets, so repeated failure settles at `backoff::CAP` instead. The pending
  re-arm is a wake reason like any retry — the machine is unarmed *and* the
  kernel's hard reset is off until it fires, so the loop must not sit on the
  channel through it. The count is CONSECUTIVE: a sentinel that lived at
  least `backoff::MIN_UPTIME` clears it, the same evidence and the same
  threshold that clears a service's restart count. Without that it only ever
  grows, and one unrelated death hours later is answered at the cap — five
  minutes unarmed for a fault that has nothing to do with the boot-time ones.
  A timer already pending when a shutdown begins is DROPPED rather than
  fired: refusing to schedule new ones cannot retract one already made, and
  firing it would spawn a sentinel into a teardown whose walk is computed.
- **A death is only news about the sentinel currently held.** Retiring one is
  done by closing its pipe, so every re-arm leaves the previous sentinel's
  watcher about to report a death. Acting on that report would drop the
  sentinel just armed, whose watcher then reports *its* death — an unbounded
  loop that forks a process per turn and in which every sentinel is correctly
  armed at the moment it is destroyed. The pid in the event is what closes it.

A press whose shutdown is **refused** re-arms too. The arming is spent either
way, but if `begin_shutdown` fails, nothing has been stopped and the machine
is still live — now catching no presses while the kernel's own hard reset is
disabled, which is worse than either behaviour this section chooses between.

Arming that fails **part way** is the same hazard, so it is handled the same
way. Once step 1 has taken, the reset is off; every later failure therefore
schedules another attempt rather than giving up. Step 1 failing is the one
exception, and only when the sysctl is *absent*: on the current kernel that is
the ordinary case and the file will not appear later, so retrying it would be
a timer that can only fail. A sysctl that is present and refuses the write is
retried like anything else.

Two consequences for the sentinel spawned along the way. It must be **reaped**
— td-svc is not PID 1, and a dropped `Child` is never waited on, so an
abandoned sentinel is a zombie for the supervisor's lifetime. And the watcher
thread is therefore started *before* step 3, so that from that point the
reaping is already somebody's job and a failure need only close the pipe. The
one path that spawns a sentinel with no watcher to hand it to is a thread that
would not start, which reaps it in place; the child is passed to the watcher
through a slot rather than moved into the closure, because `Builder::spawn`
consumes the closure whether or not the thread starts.

**Two honest limits.** The trigger is not authenticated: any root process
can `kill -INT` the sentinel, so the console should say "shutdown requested
via CAD sentinel", not "Ctrl-Alt-Del pressed". And on td's current
`allnoconfig` kernel there is no `CONFIG_VT` and no input stack, so
`ctrl_alt_del()` has no caller at all — the mechanism is testable and
correct, but a real key press needs a separate kernel-config increment.

Testing separates what is proven from what is not, and the split is
deliberate rather than a gap someone forgot to close:

- **Arming** is proven, and proven to *happen*. Both writers read the sysctl
  back and report a value that did not take as a failure, because the write
  SUCCEEDS either way and nothing else distinguishes an armed machine from one
  that will hard-reset with `/var` mounted. The mismatch branch is exercised
  against `/dev/null`, which is exactly that shape — the write returns `Ok`
  and the read-back gives nothing. Separately, a test runs the real `arm_cad`
  and asserts both sysctls hold what they should and that the runtime is
  holding a sentinel: every other test here starts from a *death*, and an
  `arm_cad` that quietly gave up on every path would satisfy all of them.
- **The reaction** is proven: a sentinel death by SIGINT begins a reboot, any
  other death does not, a death arriving once a teardown is already in flight
  neither restarts the sequence nor re-arms, a *retired* sentinel's death —
  including a SIGINT — changes nothing, a refused shutdown still re-arms, and
  repeated failure schedules rather than spins.
- **The sentinel applet** is proven, and it needs saying where: a unit test
  cannot run it, because under a test harness `current_exe` is libtest, which
  reads `cad-sentinel` as a filter and exits. So the applet is covered twice
  over instead. In-crate, the read-to-EOF shape it is built from is driven
  against a real pipe — held, it must not finish; closed, it must finish —
  and `route()` is asserted to map `cad::SENTINEL_VERB`, since a verb that
  stopped routing would make every arming "succeed" into a sentinel that
  exits at once. End to end, a `td-svc-test` leg runs the SHIPPED static
  binary against a FIFO with the write end held on fd 9, which is td-svc's
  own mechanism: it must still be alive and not a zombie after two seconds,
  and must exit 0 once the fd closes.
- **The syscall itself** is proven, which the confinement tests cannot do:
  they assert things about `sys.rs`'s *text*, and a `kill` that returned
  `Ok(())` without issuing anything satisfies all of them and every stop
  td-svc performs. Signal 0 is the kernel's own probe for this — it delivers
  nothing and runs the existence and permission checks — so the wrapper is
  exercised against our own pid, against a pid above the kernel's ceiling
  (ESRCH), and against an invalid signal number (EINVAL, the one refusal that
  does not depend on whether the suite runs as root).
- **The kernel path joining them is not covered.** `ctrl_alt_del()` delivering
  SIGINT to `cad_pid` is taken on the kernel's word; only a VT-enabled
  `sendkey` test would close it, and on td's `allnoconfig` kernel there is no
  caller to test.

Because the sysctl paths are `Runtime` fields rather than the constants used
directly, the tests cannot reach the real ones — a suite that did would disarm
Ctrl-Alt-Del on the machine running it and point that kernel at a pid
belonging to the test harness.

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
tree (§4).

Landing 4 (this one) added the ordered shutdown, and with it the three
remaining verbs that were requests to begin a transition that did not exist:
`reload`, `reboot`, `poweroff` and `halt`. td-svc now owns the end of a boot.
The teardown reuses landing 3's stop path over the reversed plan,
`/etc/shutdown` runs once every unit is down, and the applet is `exec`ed
from there. The
transition is persisted at `/run/td-svc/shutdown` before anything is stopped,
so a supervisor that dies mid-teardown and is respawned by PID 1 resumes to
the handoff instead of starting services (**I6**) — which is also why
`/etc/shutdown` now unmounts with `--exclude /run`, a `umount` flag this
landing added to `td-init`.

The two things that decide a boot is over — `/etc/tty-session` when the
greeter's session ends, and `/etc/bootfail` when a candidate image fails its
park handshake — were cut over in the same landing, per directive 4: both
`exec /bin/td-svc reboot`, and neither inlines the teardown any more. The
invariant test that used to demand each initiator run `/etc/shutdown` itself
now demands the opposite, because the teardown became a sequence only td-svc
knows.

Landing 5 armed Ctrl-Alt-Del and moved signalling in-crate. The
supervisor now issues `kill(2)` itself rather than exec'ing the uutils
`/bin/kill` — §2 I1 records why that is the smaller surface, not the larger —
and arms a sentinel at startup. Re-arming is for a sentinel that *broke*, and
for a press whose shutdown was refused; a press that lands begins a teardown
and wants no new sentinel. It is scheduled on `backoff::delay` rather than run
inline, and keyed to the sentinel's pid — §9 records the two loops those two
choices close. td-svc is consequently the fifth target-side unsafe exception
UNSAFE.md lists, for exactly one syscall.

Landing 6 is the supervisor-restart eviction contract §11
describes: td-svc records what it started under `/run` and, on startup,
kills whatever a previous instance left running before starting anything.
Without it every td-svc death produced a machine running two of
everything, unsupervised copies included.

Log capture (§7) is what remains. Sections above describe the completed
design; anything not yet built is specified here so the later landing
implements a reviewed target rather than an improvised one.

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

That is a reason it is unlikely, not a reason it was left unhandled. The
fix is **eviction**, and it is described in §11.

Landing 7 (this one) builds §7, and with it this document describes
nothing that is not implemented. `log=` and `console=` are accepted,
the `tty=`/`log=` exclusion has something to enforce, and the shipped
table captures sshd — the one service that talks to the network, and so
the one whose output a failed login has to be reconstructed from.

The shipped greeter's containment reaching only its wrapper process is
RESOLVED, and not the way this document previously said.
The recorded fix was to spawn the wrapper through `td-init`'s `cttyhack`
so it would lead a session. That does not work, and the reason is worth
keeping: `/etc/tty-session` runs `getty` as a CHILD, and getty opens with
an unconditional `setsid(2)` — td-init's applet and busybox's before it
both "create new session and pgrp, lose controlling tty". So `login` and the user's
shell end up in a session getty made, not the wrapper's. `cttyhack`
would have moved `setsid`/`TIOCSCTTY` onto the boot path to buy a
`Session` containing exactly the one process `Process` already contained.

What getty does not escape is the TERMINAL: it re-acquires the same one
with `TIOCSCTTY`, so it and everything it execs carry the device in
`/proc/<pid>/stat` field 7. Hence `Containment::Console`, which is keyed
on the device the unit's `tty=` names — not on the child's own `tty_nr`,
which is 0 because td-svc opens the console `O_NOCTTY` — and so reaches
the login tree without touching the boot path at all.

## 11. Evicting a previous supervisor's services

PID 1 respawns td-svc unconditionally, so a td-svc that dies leaves its
services running — reparented to PID 1, which does not supervise them. The
replacement knows nothing about them and starts its own copies: two sshds on
one port, two greeters on one terminal. It is the same duplicate `abandon`
refuses to create (§4), reached by a different road, and the same trade
applies — **better one degraded service than a duplicate nobody owns**.

The fix is not adoption. Orphans are PID 1's children and td-svc cannot
`wait4` them, so **I4** ("stopped" needs the leader REAPED) is unreachable
for them by construction; emptying the containment is the most that can be
observed, and saying so when it cannot is the rest of the contract.

**The record.** `/run/td-svc/started`, one line per live service, `pid
starttime tty name`. `/run` is tmpfs, so a fresh boot has no file and nothing
to evict — the ordinary case, not a special one. It is written from the live
set on every spawn, whole, rather than appended to: an append-only record
grows without bound under a crash-looping service and this lives in RAM.
Rewriting also makes it self-cleaning, so no exit path has to remember to
remove an entry — a stale one is filtered on the way back in.

Three things about what is in a line:

- **`starttime` is what makes this safe.** A pid alone is a promise the
  kernel does not keep: the recorded process may be gone and its number
  reissued to something td-svc must not touch. This file is a list of things
  the supervisor is about to KILL, so it carries the same `(pid, starttime)`
  identity check that closes the TERM→KILL reuse window (§4). Field 22 is set
  at fork and never changes, so a match means that process, not its number.
- **`tty` is recorded because it cannot be recovered.** The device a console
  unit actually got lived in the dead supervisor's memory, and the child never
  carries it — td-svc opens the console `O_NOCTTY`, so its field 7 is 0 for
  life. Re-deriving from `/proc` would narrow a console unit to its wrapper
  and leave the login tree `Containment::Console` exists to reach still
  running. Everything else in the line IS recoverable and is read fresh.
- **A service whose `starttime` could not be read is left out.** Recording a
  pid nothing can verify would hand the successor a number to kill on faith,
  which is the one thing this must never do. The cost is an orphan that
  outlives its supervisor; the alternative is signalling a stranger.

**A torn record is skipped line by line, never refused whole.** Refusing over
one bad line leaves every orphan it named running, which is the failure this
exists to prevent. Skipping is safe precisely because parsing is not the
check that matters: a line that parses still has to name a process that is
really there. Reader-side, a pid that is not positive is refused outright —
`0` and `-1` are the two `kill(2)` targets that mean something td-svc never
does, and the record is the one input here a foreign writer can shape.

**A missing leader is not an empty containment.** The dangerous shape is a
service that forks its children into its own process group and then dies: the
recorded pid is unfindable, but the group is exactly what `Containment` exists
to reach, and skipping the entry leaves those children running and starts a
second copy beside them. So a vanished leader is followed by a scan of its
containment — but only when the pid is WHOLLY ABSENT rather than reissued. A
process group keeps its id as long as a member lives, and nothing can create
group N without first holding pid N; with the number unused, members of group N
must be what is left of the recorded service. If it has been reissued that
argument collapses — the new holder may lead a new group N — and td-svc drops
the entry rather than guess. The scan must also have FOUND something: an
unreadable `/proc` must not turn into a teardown and a refused unit (I5).

**Identity is re-checked before the KILL, not only before the TERM.** The grace
period is long enough for the recorded process to exit, be reaped by PID 1, and
have its number reissued — and every containment but a console's is keyed off
that number. Escalating on the strength of a check that old is how this
mechanism would kill the stranger it was written to protect. A console keeps
its escalation because its device, not the pid, is what addresses it.

**Who may write the record is part of the design.** These lines name processes
td-svc signals as root, so writing them is choosing them. `/run/td-svc` is
created at 0700 by whichever of `control::bind` and the record reaches it
first, which since eviction runs before the socket is bound is usually the
record — hence not `create_dir_all`, whose `0777 & ~umask` would be world
writable under the umask PID 1 never sets. A record found in a directory that
fails that test is IGNORED rather than narrowed, for the reason `bind` already
gives: narrowing does not revoke a descriptor somebody already holds.

**Ctrl-Alt-Del is armed before the eviction, not after.** Until §9's sentinel
is in place the key combination is the kernel's own hard reset, and a boot that
can now spend `EVICT_GRACE + EVICT_SETTLE` killing orphans is exactly when
somebody reaches for it. Reading the I6 marker still comes first — it changes
no process, and arming depends on knowing whether this is a resume — but the
sentinel is armed before anything is signalled. It is this supervisor's child
and appears in no record, so the eviction that follows cannot touch it.

**Eviction runs first**, before anything starts.
Whatever the last instance left is running right now whichever way this boot
goes: if td-svc goes on to supervise, a second copy is the duplicate; if it
goes on to finish a teardown, an orphan holding `/var` is a mount that will
not come away. Orphans are signalled together and waited on together, so a
machine with eight wedged services is delayed once rather than eight times —
TERM, `EVICT_GRACE`, KILL, `EVICT_SETTLE`. Both bounds exist because this is
the boot path and **I5** forbids delaying a console indefinitely; the grace is
deliberately shorter than a unit's own `stop-timeout`, because an orphan has
already lost the supervisor a graceful exit would report to.

**A unit whose orphan survives is not started**, and is left `Failed` with no
`retry_at` — the one state `start_eligible` will not leave on its own, so
nothing brings the duplicate back on a timer, while an explicit `start` still
can once an operator has dealt with the copy that is there. An unreadable
`/proc` counts as surviving: **I3** says unreadable is an error, not an
emptiness, and not knowing whether the recorded process is there is not
permission to assume it is not.

This does not violate I5 when the refused unit is the console. A greeter that
survived eviction is still holding its terminal, so the machine is still
repairable from its own console — what it lacks is supervision, not a
console. Starting a second getty on that device would take the working one
away.

**An orphan that could not be cleared stays in the record.** Whole-file
rewriting would otherwise drop it on the next spawn, and the failure would
compound: this supervisor refuses to start the unit, then dies, and its
successor reads a clean file and starts the very duplicate the refusal was
protecting against. Only a process proven gone leaves the record, so the file
is never deleted — it is rewritten, and the difference is what the successor
inherits.

**The window this cannot close** is between `spawn` returning and the record
being written. A td-svc killed inside it leaves an orphan its successor never
hears about. It is unclosable rather than merely unclosed: the pid does not
exist to record until `spawn` returns.

`td-svc-test` proves this against the SHIPPED binary and not only in-crate: it
hand-writes a record naming a live process, starts td-svc on a table holding
that unit, and requires both that the process is gone and that the unit then
ran — the second half being what separates evicting from refusing everything.
