# td-login threat model

td-login is the credential-switching half of td's login chain: the
`login` and `su` applets, replacing busybox's, plus the `exec-as` and
`exec-service-as` subcommands that run supervised daemons as another user.
It is the only
td-owned general-purpose program on a td image whose job is to *change* a
process's Unix credentials, so a
bug here is not a malfunction — it is privilege escalation. This
document states what it defends, what it does not, and which invariant
each defence rests on. It is normative: the crate's tests assert the
invariants named here, and widening the surface means amending this file
in the same landing.

## 1. Assets and adversaries

Assets, in the order an attacker wants them:

1. **uid 0.** Root on a td image owns `/dev`, the Btrfs `@var` volume,
   the deployment bookkeeping td-boot writes, and the ability to
   `kexec` a new kernel.
2. **gid 0 and supplementary groups.** A residual group from the
   *previous* credential holder is the classic quiet escalation: the
   uid drops, nobody notices the gid did not.
3. **Another user's session.** The terminal a login session owns, and
   anything reachable through it.

Adversaries considered:

- **A1 — an unprivileged local user** with a shell on the image (the
  auto-login user, or an SSH session), trying to gain uid 0 or another
  user's credentials.
- **A2 — a hostile or corrupted account database**: `/etc/passwd`,
  `/etc/group`, `/etc/shadow` with duplicate, malformed, or crafted
  entries. On a td image `/etc` is immutable EROFS, so this is
  primarily a *build-time* adversary — a tailoring mistake in the
  `SYSTEM` const — but the parser must not trust the file either way.
- **A3 — a hostile caller of td-login**: whatever execs `login`, `su`,
  `exec-as`, or `exec-service-as` chooses argv, the environment, the
  current directory, and the open file descriptors, including which file
  is on fd 0.

Explicitly **not** in the model: an attacker who already has uid 0
(nothing here can constrain them), physical DMA, and the kernel itself.

OpenSSH is a separate, deliberately narrow credential-changing boundary.
The externally maintained `sshd` starts as root, switches its pre-auth process
to the locked `sshd` privilege-separation identity, and switches authenticated
sessions to their target account without going through td-login. Its immutable
configuration permits only Ed25519 public-key authentication and disables
password, interactive, host-based, and forwarding paths. This document does
not extend td-login's syscall or post-condition claims to OpenSSH; the image
recipe instead pins its source, configuration, locked account, empty volatile
chroot, split helpers, seccomp sandbox, and a real unprivileged loopback login.
The boot-health login uses a fresh volatile key for the unprivileged UI account,
but its root-owned authorization line is constrained by OpenSSH `restrict` and
`from="127.0.0.1"`; possession of that key cannot create a network-reachable
login. The persistent administrator path is exercised separately only in the
disposable QEMU volume: it is preseeded with a loopback-restricted public key
and a root-only matching private fixture before boot. Boot health reads those
fixtures but never rewrites live administrator authorization state.

The resource boundary is also an asset. The application identity must enter
the delegated `td-user-1000/session` cgroup before it loses root, otherwise a
later unprivileged td-jail cannot move its child into a sibling per-instance
leaf: cgroup v2 requires write authority at the source/destination common
ancestor.

## 2. The core invariant: ordering, and then proof

The bug class this program exists to not have is **partial credential
change**. Every one of these is a real, historical escalation:

- `setuid(2)` before `setgroups(2)`/`setgid(2)` — after the uid drops,
  the process no longer has `CAP_SETGID`, so the group changes silently
  fail and the session keeps the *previous* holder's groups.
- Not calling `setgroups(2)` at all when the target has no
  supplementary groups, leaving root's set attached to an unprivileged
  uid.
- Ignoring a return value. `setuid(2)` can fail (RLIMIT_NPROC); a
  program that carries on has kept uid 0 and is about to `exec` the
  user's shell with it.
- Checking the uids and stopping there. A capability is a credential
  too. The kernel normally clears the permitted and effective sets when
  every uid leaves 0, but `SECBIT_NO_SETUID_FIXUP` turns that off — and
  unlike `SECBIT_KEEP_CAPS` it survives `execve`, so an ancestor can set
  it and every descendant inherits the changed rule. Under it,
  `setuid(1000)` produces a *perfect* four-column readback and a full
  `CapEff`.

td-login answers this in three layers.

Before Layer 1, `session::enter` performs one non-credential root operation:
for uid 1000 it writes its own pid to the fixed
`/sys/fs/cgroup/td-user-1000/session/cgroup.procs` and requires
`/proc/self/cgroup` to read back exactly `0::/td-user-1000/session`. Root
sessions do nothing; any other nonroot uid has no configured delegation. No
argv or account-database field selects the path. This uses safe filesystem I/O
and does not widen the unsafe surface. It must precede `creds::apply`; after the
uid drop, a process still outside the delegated subtree cannot acquire the
common-ancestor permission needed to enter it.

Placement failure is diagnosed but does not withhold a login session. The
cgroup is an application-resource boundary, not credential authority, and the
serial console must survive a failed delegation setup. td-jail independently
requires the delegated root and exact child membership before release, so an
unplaced session can use the console but cannot launch an application while
claiming resource enforcement.

**Layer 1 — one site, correct order.** `creds::apply` is the *only*
function in the crate that changes credentials, and it does so in this
order, checking each return value:

1. `setgroups(2)` — the full supplementary set, while still root.
2. `setgid(2)` — the primary group.
3. `setuid(2)` — last, because it is the call that removes the
   privilege the other two need.

`Credentials` cannot be constructed without all three of uid, gid and
group list, so "forgot to set the groups" is not a reachable state.

**Layer 2 — a post-condition, not an assumption.** After the three
calls, `apply` re-reads `/proc/self/status` and asserts the kernel's own
view: all four uid fields (real, effective, saved, filesystem) equal the
target uid, all four gid fields equal the target gid, the supplementary
set equals the requested set exactly, and — when the target is not root
— `CapPrm`, `CapEff` and `CapAmb` are all empty. Two capability fields
are deliberately NOT checked. `CapBnd` is a *bounding* set, not a held
privilege. `CapInh` survives the uid drop but grants nothing until an
`execve` of a file carrying inheritable file capabilities, and td ships
none — they are xattrs, and NAR does not carry them; it is also
routinely non-zero for ordinary processes, so requiring it empty would
refuse real sessions to defend against a conversion that cannot happen
here. `CapAmb` is the set that needs no file capability, and it *is*
checked. Only then does
the caller `exec`. A partial switch therefore *cannot* reach a shell — it
fails closed with a diagnostic naming what did not take. This is the
defence that does not depend on having reasoned correctly about
Layer 1.

**Layer 3 — the confinement tests.** `main.rs`'s `confinement` module
asserts against the crate's own source that there is exactly one
`syscall2` body, exactly one scoped `#[allow(unsafe_code)]`, exactly the
three credential syscall numbers, and exactly one call site for each of
`setgroups`/`setgid`/`setuid` — all inside `apply`, in that order. It
also asserts the crate never uses `CommandExt::uid`/`gid`/`groups`, so
there is no second, unverified credential mechanism, and that the only
two file modes it ever writes are the pinned `TTY_MODE` and a mode read
back off the same inode (§6), so no path through this program can set a
setuid bit. The compiler cannot
check any of that.

### Why raw syscalls rather than `Command::uid()`

`std`'s pre-exec path applies credentials in the correct order, but on
stable Rust `CommandExt::groups` is unstable (`feature(setgroups)`), so
the only reachable behaviour is `setgroups(0, NULL)` — every
supplementary group dropped. That is safe but not faithful: a user in
`wheel` would silently lose it. More importantly the changes happen in a
forked child that then execs, so there is no moment at which td-login
can *verify* them; Layer 2 would be impossible. Three confined syscalls
in our own process buy the fidelity and the proof.

The cost is recorded: this is the fourth target-side `unsafe` exception
in UNSAFE.md, and it is exactly three syscalls through one `syscall2`
body.

### Single-threadedness

The raw syscalls change the calling *thread*'s credentials; glibc's
wrappers broadcast to all threads. td-login never starts a thread, and
`apply` refuses to proceed unless `/proc/self/status` reports
`Threads: 1`. If a future change introduces a thread, this check turns a
silent partial switch into a refusal.

### Dependence on `/proc`

Layer 2 requires `/proc`. If `/proc` is not mounted, `apply` fails and
no session starts. This is deliberate: an unverifiable credential switch
is refused rather than trusted. On a td image `/etc/inittab` mounts
`/proc` at sysinit, long before any tty session.

## 3. Authentication policy

td-login **does not verify password hashes** and does not prompt for a
password. Turning off terminal echo needs `termios`/`ioctl`, which safe
`std` does not reach and which would widen the `unsafe` surface for a
capability no shipped td image uses. The policy is therefore fail-closed
by construction — the shadow field is classified, and only one class
authenticates:

The `login -f` and `su`/`exec-as` columns below are the *forced* paths,
which skip authentication because the caller has by then already
established the right to start the session. **"Reachable
only by root" is how that used to be stated here and it is not quite
true**, which is worth correcting rather than repeating: what root is
needed for is a switch that CHANGES something, and §4's
`creds::may_switch` is what refuses one, over all four uid columns. A
caller asking to become who they already are takes `creds::apply`'s early
return instead — `su tester` as tester, or `exec-as tester` as tester —
and starts a program with no privilege that caller lacked. The forced
path skips a password; it does not hand one out.

Ordinary `exec-as` shares `su`'s column exactly, and by construction rather
than by coincidence: all three human-session front ends reach ONE decision,
`login::authorize`, which `the_session_policy_is_decided_in_one_place`
holds them to by refusing any module but `db` and `login` to name
`may_start_session` at all. So a locked or service-only account is refused on
every one of them. `exec-service-as` reaches the same account lookup and the
same credential switch, but a disjoint policy decision accepts only the exact
`!td-service` shadow marker. It rejects an empty field, a hash and an ordinary
lock, so a unit cannot turn the service path into a second way to enter a
human account.

| `/etc/shadow` field | class        | interactive `login` | `login -f` | `su`, `exec-as` (forced) | `exec-service-as` |
| ------------------- | ------------ | ------------------- | ---------- | ------------------------ | ----------------- |
| empty               | `NoPassword` | allowed             | allowed    | allowed                  | denied            |
| `!td-service`       | `Service`    | denied              | denied     | denied                   | **allowed**       |
| `!`, `!!`, `*`      | `Locked`     | denied              | **denied** | **denied**               | denied            |
| anything else       | `Hashed`     | **denied**          | allowed    | allowed                  | denied            |
| no entry / no file  | —            | denied              | denied     | denied                   | denied            |

Consequences worth stating plainly:

- An account with a real password hash **cannot log in interactively**.
  This build cannot verify one, and treating an unverifiable secret as
  absent would be the escalation. Root can still start a session for it
  (`login -f`, `su`) — that grants nothing root did not already have,
  and denying it would only break the ordinary "root may become anyone"
  semantics.
- `Locked` is denied even on the forced paths, which is stricter than
  busybox (whose `-f` skips the account database entirely) and stricter
  than `su(1)`. A locked account is an explicit administrative statement
  that no session may run as it; the cheap way to honour that is to make
  `/etc/autologin` naming a locked account fail loudly instead of
  quietly working. Nothing on a td image needs the other behaviour.
- **A service account is a distinct credential class, not an ordinary lock
  with a special caller.** `system-x86-64` writes the exact
  `!td-service` marker only for identities a unit starts through
  `exec-service-as`. The marker still begins with `!`, so software that does
  not know td's extension sees a conventional lock. td-login distinguishes it
  before the general `!` rule, refuses it through every human-session path,
  and refuses every other class through the service path. Making the audio
  account passwordless to satisfy ordinary `exec-as` would therefore red both
  the image contract and the credential policy.

  The shipped image has three locked service identities. OpenSSH's `sshd`
  privilege-separation account and `td-profiler`'s `profiler` account are not
  td-svc `exec-as` targets; each daemon performs its own fixed-purpose drop.
  The `sshd` account has a `/bin/false` shell and an empty root-owned
  `/run/sshd-empty` chroot recreated before the daemon starts. The `audio`
  account has a `/bin/false` shell and a volatile `/run/td-audio` home and is
  the sole current `exec-service-as` target. None of the three can be entered
  through td-login's human forced modes or an interactive login.
- **A class is a property of a name; the uid is what the kernel
  enforces.** `classify` reads one account's shadow field, so
  `!td-service` says that the NAME `audio` is refused by every human
  path. It says nothing about uid 994. A second passwd entry sharing
  that uid carries its own shadow field and its own `/etc/group`
  membership. An empty field is `NoPassword`, which is admitted on
  EVERY path in the table above — interactive `login` included, where no
  caller has established anything and none is asked to. So the alias is
  not merely a root-operated `exec-as` shortcut: an unauthenticated
  console caller reaches the daemon's uid, with any argv and any
  supplementary groups, never consulting the service class at all.

  `account_in` therefore refuses to resolve a uid two names claim, in
  either direction: asking for `audio` fails as surely as asking for the
  alias. A second name for one uid is a second account policy for one
  kernel identity, and refusing only the alias would leave "which name
  did you ask for" deciding which policy applies — the thing being
  removed. `the_service_class_is_reached_by_a_uid_no_other_name_claims`
  is what holds the class and the uid together.

  This is not a defence against root, which §4 places outside the threat
  model: root writes `/etc/passwd`. It is what lets the rest of this
  crate reason about a uid rather than a name, and what makes "who may
  be 994" answerable by reading the file. The generated image never
  contains such a pair — `system_def_is_self_consistent` requires each
  declared uid to belong to exactly one user, and a separate assertion
  keeps every declared account off the `sshd` privilege-separation uid —
  so the parser refuses a case the image generator already cannot
  produce, on any machine and however `/etc/passwd` got there.
  `the_generated_passwd_gives_each_uid_exactly_one_name` is the one that
  parses the generated file rather than the declaration, which is what
  covers a line `build_passwd` appends outside its loop over
  `SYSTEM.users`.
- `system-x86-64`'s `system_def_is_self_consistent` test refuses to ship
  a `SYSTEM` definition whose auto-login user is not passwordless, so the
  image cannot be tailored into a machine that will not let anyone in.
- Both interactive accounts on the stock image, `root` and `tester`, are
  `passwordless: true`, so the console is trusted **by image
  configuration**, not by td-login. The service identities above are
  locked. These are properties of the shipped `SYSTEM` const, and the
  interactive behavior is unchanged from the busybox chain this replaces,
  which also accepted the empty shadow field without prompting.

## 4. Privilege can only be dropped, never gained

td-login is **never installed setuid-root**. `system-x86-64` packs it as
a plain `/bin` symlink into a store path, exactly like busybox; no step
in its recipe sets a setuid bit, and the crate's own confinement test
(`no_mode_this_crate_sets_carries_a_setuid_bit`) refuses one in the only
mode td-login itself ever sets.

That is a promise about packaging, so it is *checked* in two places
rather than assumed:

- The recipe's shape check inspects the **packed artifact** and refuses
  a build whose `/bin/td-login` carries a setuid or setgid bit. The
  crate's own tests cannot see this — they only see modes td-login
  constructs, never the one the packer left on the file.
- `creds::may_switch` requires **all four uid columns** to be 0, not
  just the effective one. Under a setuid-root exec the real uid stays
  the caller's while the effective one is 0, so an "is the effective uid
  0" gate would admit an unprivileged caller — and since `su` takes the
  forced policy path (§3), that caller would reach root *without
  authenticating*. The two checks are independent: one would have to
  fail silently and the other be edited for the boundary to move.

It follows that:

- A1 cannot use `su` to become another user. `creds::may_switch` refuses
  a caller that is not root in every uid column, and the kernel would
  refuse the syscalls anyway; the check exists so the failure is a named
  diagnostic rather than an `EPERM` from somewhere in the middle of a
  switch.
- The `-s SHELL` and `-c CMD` options of `su`, which in a setuid
  program would be an escalation surface (choose the program root
  runs), are inert: only root can reach the credential switch at all,
  and root can already exec anything.
- **`su` permutes its options**, so a flag may appear after the user
  name — `su -s /bin/sh tester -c '…'` is the form td's own boot
  scripts use, and it is what busybox's `getopt32` does. The
  consequence is worth stating: in `su USER $UNTRUSTED`, a word in
  `$UNTRUSTED` that looks like an option is consumed by `su` rather
  than passed to the shell, so an injected `-s /bin/sh` would override
  the account's own shell. That is reachable only from a *root* script
  interpolating untrusted words — root can exec anything anyway — td
  ships no such script and no restricted shell, and `--` ends option
  parsing for a caller that wants it. Kept for busybox compatibility;
  revisit if td ever grows an account whose shell is a confinement.
- `su` to *yourself* is a no-op switch — `apply` returns early when the
  kernel's view already equals the target — so it neither needs nor
  attempts privilege. The session-cgroup join is independently idempotent: a
  same-user invocation already in the leaf performs no write.

This is why the crate is `#![deny(unsafe_code)]` with a three-syscall
exception rather than a program with a carefully audited setuid entry
point: the entire "gain privilege" half of a traditional `su` does not
exist.

## 5. Environment, and what the session inherits

A3 controls the environment td-login is handed. For a **login session**
(`login`, or `su -`/`su -l`) that environment is discarded: the session
starts from `PATH=/bin`, plus `HOME`, `SHELL`, `USER`, `LOGNAME` derived
from the account database, plus `TERM` carried through because the
terminal type is a property of the terminal, not of the caller. `login
-p` preserves the rest on the caller's explicit say-so — it is a flag
only root can reach, and root already owns the machine.

For a **non-login `su`** the environment is preserved (this is what
makes `su -c` usable from a script), but `HOME`, `SHELL`, `USER`,
`LOGNAME` and `PATH` are still overwritten so they describe the target
account rather than the caller's.

For **`exec-as` and `exec-service-as`** the environment is discarded
ENTIRELY — these are the only front ends where nothing at all of the
caller's survives, `TERM` included. What they start is a SUPERVISED
DAEMON, whose environment ought to be a property of its unit; td-svc has
no `env=` key to make it one, so preserving would hand a long-lived
unprivileged service whatever the supervisor happened to inherit from the
boot path — undeclared, and not necessarily the same on a restart as on
the first boot. Discarding is also the direction A3 argues for: these are
the front ends whose caller is a root supervisor and whose target runs
indefinitely, so anything carried across is carried into every restart
too.

`TERM` is the exception to the exception and is worth stating, because
the login rule above justifies keeping it. There it is right: the
terminal type is a property of the terminal rather than of the caller.
A daemon has no terminal — td-svc gives a unit with no `tty=` a null
stdin — so `TERM` there is not a terminal's property but the
supervisor's, and it is exactly the kind of value that quietly changes a
program's output between one boot and the next. So the session gets the
five identity variables and nothing else.

`PATH` is `/bin` in all cases. td images have no `/usr` and no `/sbin`;
`/bin` is a pure symlink farm into `/td/store`. A relative or
attacker-influenced `PATH` reaching a root session is the reason this is
set rather than inherited.

Those overrides are **final, not advisory**, which takes one deliberate
step: `environ` is a plain array and `execve` never deduplicates it, so a
caller may pass `PATH` twice. An override that patched only the first
occurrence would leave the second — and the map `Command` builds keeps
the LAST, so the caller's copy would win every one of the five. So
`environment` collapses each name to its first occurrence (what
`getenv(3)` answers with, so the session sees what it would have seen)
before applying the overrides, and `set` removes every entry for a name
before appending its replacement.

Not defended: td-login does not reset `umask`, because safe `std` has no
`umask` API and the syscall is not worth the surface. The session
inherits PID 1's `022`; `/etc/profile` is the place to change it.

## 6. The terminal

`login` gives the terminal to the user it logs in: `chown` to
uid:gid and `chmod 0600`, so the *next* session cannot read the
previous user's terminal.

That `chown` is the sharpest edge in the program. A3 chooses what is on
fd 0; a `login` that blindly chowns "whatever fd 0 is called" hands the
target user ownership of any file the caller can open — `/etc/shadow`,
a store path, a device node. td-login therefore validates before it
chowns, and treats a failed validation as "do not chown" rather than
"do not log in":

1. Resolve fd 0 through `/proc/self/fd/0`. No path is accepted from
   argv — busybox `login`'s `-h HOST` and friends never name a file
   here.
2. The resolved path must be under `/dev/`, with every remaining
   component a plain name (no `.`, `..`, or empty component).
   `/dev/../etc/shadow` starts with `/dev/` and is not a device.
3. The *open file* must be a character device. A regular file, a
   directory, a socket, or a pipe is refused.
4. The named path and the open file must be the same object — same
   device and inode. This is what rejects a `/proc/self/fd/0` that
   resolves to a name someone has since re-pointed.
5. That object must be this process's **controlling terminal**: its
   `rdev` must equal `tty_nr` from `/proc/self/stat`. Without this step
   `/dev/null` — a character device, correctly named, under `/dev` —
   passes 1–4 and gets handed to the user. A caller with no controlling
   terminal (`tty_nr` 0) is not an error: there is simply nothing to
   hand over.

A failed check means *do not chown*, not *do not log in*. Refusing the
session over a terminal-ownership detail would brick the console for a
cosmetic property; the caller warns and continues.

The two writes are ordered `chmod` then `chown`, and the pair is not
atomic. If the `chown` fails, the terminal is 0600 and still root's —
the session about to start cannot read or write its own console, which
is a *worse* outcome than not having touched it. So a failed `chown`
puts the previous mode back (read off that same inode before the first
write) and the warning then means what it says: nothing was handed
over. If the restore itself fails, that is said too, because at that
point the console really is unusable and the warning is the operator's
only notice. The order is `chmod` first on purpose: `chown` first would
leave a window in which the terminal is the user's with the *old*,
group-readable mode.

Residual risk, accepted and stated: steps 2–5 read the filesystem more
than once, so a sufficiently fast attacker who can create entries in
`/dev` could in principle swap the object between the check and the
`chown`. On a td image `/dev` is a root-owned devtmpfs and `login` runs
as root out of PID 1's respawn, so there is no unprivileged writer to
race with. Closing it properly needs `fchown` on the borrowed
descriptor, which safe `std` does not expose.

Also not done: ownership is **not** restored when the session ends —
td-login `exec`s the shell and is gone. getty re-runs `login` on the
next session, which re-chowns; a terminal between sessions stays owned
by the last user. That matches busybox and util-linux.

## 7. Account database parsing

Against A2, the parsers fail closed rather than fail soft:

- A malformed line is a **hard error for the whole lookup**, not a
  skipped line. Skipping is how a crafted entry gets to shadow a real
  one. The tradeoff is deliberate and worth naming: one bad line in
  `/etc/passwd` locks *everyone* out, which on a mutable-`/etc` system
  would be a denial of service. On td it is not one — `/etc` is
  immutable EROFS generated by `system-x86-64` and checked at build
  time — and between "nobody logs in" and "the wrong person logs in",
  this program picks the first.
- A duplicate user name in `/etc/passwd` or `/etc/shadow` is an error.
  Two entries for `root` with different uids is an ambiguity nothing
  downstream can resolve safely, and "first wins" versus "last wins" is
  exactly the kind of ordering assumption this document exists to
  refuse.
- `home` and `shell` must be absolute paths. `login` execs the shell by
  absolute path with no `PATH` search, so a relative shell would
  resolve against the current directory.
- uid and gid must parse as `u32`. No wrapping, no negative-as-huge.
- A `/etc/shadow` record must carry **all nine** `shadow(5)` fields, not
  merely the name and password the decision reads. The password field is
  the *second* of nine and an empty one means `NoPassword`, so a record
  that loses its tail keeps a name and an empty secret: accepting a
  short line is how corruption of a **locked** account turns into an
  account anybody may log into. `build_shadow` writes nine.
- A `/etc/group` line naming a user is only honoured if the group's gid
  parses; a malformed group file fails the lookup rather than silently
  granting a smaller set.

The supplementary set handed to `setgroups(2)` is the user's `/etc/group`
memberships **plus** the primary gid, sorted and deduplicated. Sorting is
not cosmetic: it makes the set comparable, which is what lets Layer 2
assert equality against `/proc/self/status`.

## 8. What proves this on a real image

- `cargo test` covers the parsers, the policy table, the ordering
  confinement, and the `/proc/self/status` reader against captured
  fixtures.
- The boot itself is the success-path oracle: `login -f` is how a td
  image reaches its greeter at all, and `su` is how every health leg in
  `/etc/bootsuccess` runs unprivileged. Neither can regress without the
  boot failing.
- `TD-LOGIN-RUN-OK` is the credential-specific evidence. `/etc/bootsuccess`
  runs `su` to the unprivileged user and has `td-login verify-credentials`
  read `/proc/self/status` back, asserting the exact uid, gid and
  supplementary set. A switch that "worked" but left a residual group
  attached prints no marker and reds the boot oracle — which is the one
  failure mode every other check on the image would pass.
- The jailed fixture is the cgroup-placement evidence. Its QEMU marker is
  withheld unless td-jail observes the active per-instance sibling leaf with
  the exact configured caps; that migration can succeed only when the
  td-login-started session entered the delegated subtree first.
