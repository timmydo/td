# td-login threat model

td-login is the credential-switching half of td's login chain: the
`login` applet, replacing busybox's, plus the `exec-as` and
`exec-service-as` subcommands that run supervised daemons as another user.
`login-primary` and `exec-primary` select the deployment's UID-1000 human
and then call the existing `login -f` and ordinary `exec-as` front ends;
root's boot-health probes drop to that human through `exec-primary`.
td ships no `su`: `APPLICATIONS.md` §L.1's L6 deleted it.
It is the only
td-owned general-purpose program on a td image whose job is to *change* a
process's Unix credentials, so a
bug here is not a malfunction — it is privilege escalation. This
document states what it defends, what it does not, and which invariant
each defence rests on. It is normative: the crate's tests assert the
invariants named here, and widening the surface means amending this file
in the same landing.

The release/source administrator (`td-update/ADMIN.md`) is a separate SSH
principal, UID/GID 986, with a forced fixed-command interface and root-owned
keys enrolled through one physical settings approval. It remains admitted in both SSH policy forms. Its password-field marker is
`td-admin-key-only`, an unsupported hash to td-login, not an empty password
or service marker. td-sshd uses public-key authentication alone; td-login
cannot authenticate that marker. The build/download account UID/GID 985 uses the
existing `!td-service` path and cannot log in. Neither account belongs to
another account's supplementary groups. The existing credential-switching
implementation is unchanged.

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
- **A2 — a hostile or corrupted account database**: `/etc/passwd` and
  `/etc/group` with duplicate, malformed, or crafted
  entries. The read-only EROFS deployment supplies the base tables. Before
  starting consumers, firstboot may publish a read-only account projection
  for the validated root-owned saved username; `td-install/INSTALLER.md`
  specifies its complete admission and publication. A build-time tailoring
  mistake or damaged saved setting must refuse, and readers still validate
  the live files. Numeric identities and authentication fields are preserved.
  The workload account ledger, `/var/lib/td/accounts.tsv`, is part of the
  database: it alone names application, sandbox and workspace accounts
  (§7).
- **A3 — a hostile caller of td-login**: whatever execs `login`,
  `login-primary`, `exec-as`, `exec-primary` or `exec-service-as` chooses
  argv, the environment, the current directory, and the open file
  descriptors, including which file is on fd 0.
- **A4 — a person at a locked machine** without an enrolled login key and
  its PIN. This adversary exists only under the login-key tier in
  [`TOKEN-LOGIN.md`](TOKEN-LOGIN.md), which owns that tier's scope, and
  only where a key is enrolled or the login directory is invalid, the
  states the compositor starts locked on; td-login's part is the console
  refusal in §3.

Explicitly **not** in the model: an attacker who already has uid 0
(nothing here can constrain them), physical DMA, and the kernel itself.

td-sshd is a separate, deliberately narrow credential-changing boundary
(`td-ssh/DESIGN.md`). It starts as root, switches its pre-auth worker
to the locked `sshd` privilege-separation identity, and switches authenticated
sessions to their target account without going through td-login, through a
credential switch of its own that `td-ssh/DESIGN.md` §4 states. It has only
Ed25519 public-key authentication: password, interactive, host-based and
forwarding paths are code it does not have, not settings. This document does
not extend td-login's syscall or post-condition claims to td-sshd; the image
recipe instead pins its configuration, locked account, empty volatile chroot,
and a real unprivileged loopback login, and boot health reads a live pre-auth
worker's uid, seccomp filter and `no_new_privs` from `/proc`.
Before entering the verified deployment, firstboot validates its complete
root-owned account tables and publishes the server policy itself as root:root
mode-0600 `/run/td-sshd.conf`, renaming into place a temporary it created
exclusively without following a link, then writes the volatile cutover
record naming the form it rendered (`td-authd/DESIGN.md` amendment 7).
Only the admitted UID/GID-1000 account name selects its SSH Match
block and, in the enforced form below, the one `AllowUsers` entry. The
policy's remaining bytes are compiled from the shared source used by the
realized `td-ssh-test`. The daemon requires this configuration file
explicitly; there is no optional include or fallback configuration. A
failed validation, boot ID read, or either write stops boot before user
processes. Within a boot, `td-firstboot render-ssh-policy`, which td-authd
runs as a cutover's first step, publishes the same way and writes no
record; td-authd writes the record through the same publication once the
cutover is complete. The render's failure is td-authd's failed
revocation (amendment 7), not a boot stop. Publication is serialized by the account
tables' read-only binds, from `prepare-primary-profile` to shutdown; by an
exclusive lock on a persistent root-only mode-0600 file beside the target
(`/run/td-sshd.conf.lock`, `/run/td-login-cutover.lock`), held from
clearing a stale temporary through the rename and the directory's fsync,
so overlapping renders cannot interleave and no account can stall one;
and by the rename, which no `sshd` reader sees half done. This changes no
credential transition or authentication method. Neither form admits root
(`TOKEN-LOGIN.md`, "SSH", states the rule). Since
`TOKEN-LOGIN.md`'s increment 4 (C6) the render reads the login state
through the shared predicate (§3) under the root it renders, and
renders the ordinary policy only where that state is unenrolled.
Enrolled, and every unavailable cause including a failed
read, renders the enforced form, which excludes owner-managed remote keys
and adds `AllowUsers` naming only
that admitted account after the root line (`TOKEN-LOGIN.md`, "SSH",
pins the bytes). The stock image never contains a record and firstboot
ensures the directory at every boot, so it renders the ordinary policy;
the enforced form acts only where a record or an invalid directory
exists.
The ordinary form also consults `.ssh/authorized_keys` in that account's
passwd home. The owner may authorize remote Ed25519 keys there; private-key
possession grants that account's session authority, including access to its
files, but grants no elevation or hardware-backed credential release.
td-sshd's strict modes (`td-ssh/DESIGN.md` §5) apply to the home and authorization
file. A process already running as that owner may add keys; same-UID
isolation is not claimed. Enforced and unavailable login states never consult
this file, and enrollment's SSH leaf teardown revokes existing sessions.
A server image's policy also names root-owned keys its volume's builder
provisioned (`TOKEN-LOGIN.md`, "SSH"), in both forms. Possessing one grants
the same session authority and nothing more. The account cannot change them,
and its render refuses a keys file the account could have written. Revoking
one takes a new persistent volume: a new image over the same volume keeps
these keys, and anything the account added to its own `authorized_keys`.
Keys a server's instance metadata offers (td-svc/DESIGN.md §10, "Instance
metadata") are root-owned too, grant the same, and live in volatile `/run`,
written each boot from the attached seed. Whoever can attach a disk can
therefore admit one: the instance's owner on a cloud, and physical access on
hardware, which can already boot a medium of its own (the image's Secure Boot
is off) and read a server's unencrypted volume. That holds for a live medium
too, whose provision is unsigned like the seed's keys: the medium the person
chose to boot is all that vouches for either. Removing the key from
the seed, or the seed, revokes it at the next boot. With no seed, a server whose
firmware's DMI names EC2 takes them from the instance metadata service instead
("Cloud metadata"): the account that launched the instance admits one with its
key pair, fixed for the instance's life. On Azure, whose firmware's chassis
asset tag names it, whoever set the SSH keys the VM was created with admits one.
The boot-health login uses a fresh volatile key for the unprivileged UI account,
but its root-owned authorization line is constrained by `restrict` and
`from="127.0.0.1"`; possession of that key cannot create a network-reachable
login. Root has no SSH login, and no other interactive one: its password
field is `!` (§3) and td-login is no administrative path (§4). The
disposable, unenrolled QEMU volume is preseeded with a loopback-restricted
root public key in the persistent authorization file and a root-only
matching private fixture; under the autotest token boot health reads
both, never rewriting live authorization state, and requires the daemon
to refuse that key (§8).

The resource boundary is also an asset. The application identity must enter
its delegated session cgroup before it loses root, otherwise a
later unprivileged td-jail cannot move its child into a sibling per-instance
leaf: cgroup v2 requires write authority at the source/destination common
ancestor.
The human UID 1000 selects `td-user-1000/session`; the Flathub runtime
account, UID 998, selects `td-runtimes/session`; the ledger's application
UIDs, 65536 through 98303, select `td-app-UID/session`, and its sandbox
and workspace UIDs, 98304 through 131071, `td-workload-UID/session`. Every
other non-root UID reports that no delegation is configured. Firstboot
prepares only the installed, durably reserved application accounts'
cgroups and, on the desktop, the runtime account's at sysinit, and a
sandbox or workspace account's when root allocates or prepares it,
as specified in `td-authd/DESIGN.md`; this selector alone activates
nothing.
The root-configured td-authd application launcher, and its workspace
launch intake for a sandbox's or workspace's account, make a failed placement fatal
before workload code runs, as the terminal launcher does for the human
session. Its active-account admission and post-switch checks are specified
in `td-authd/DESIGN.md`; it uses this existing service-only credential path.

The human's files reach an application account only through a directory
grant (WORKLOADS.md, "Grants"), and asking for one takes the human's
kernel credentials at the compositor control socket, no PIN, token or
secure attention (td-authd/DESIGN.md, "Grant requests"). So any program
running as UID 1000, not only td-software, can grant any directory of
the home outside the reserved and sensitive trees, read-only or
writable, to any application account the human owns, a networked one
such as Firefox's included, from that account's next launch. Such a
program already reads and writes those files itself, so a grant gives it
no authority it lacked; what it adds is a second party that holds them.
A read-only grant lets that account read the directory and send it over
its network. A writable one also lets the account change what the human
later runs or trusts: a granted repository's `.git/hooks`, its build
scripts, a `Makefile` or a script the human executes, so an escaped or
hostile application can turn a writable grant into code running as the
human. Same-UID isolation is not claimed, as for `.ssh/authorized_keys`
above.

The same holds for a sandbox's account (`tdw<uid>`) once the ledger
holds it: any program running as UID 1000 can grant it any such
directory, read-only or writable, and give it the host's network
namespace, which reaches the network, every service listening on the
host's loopback, abstract-namespace AF_UNIX sockets and netlink,
through the same socket, credentials and writer. That is the same
trust class: the program already holds the human's files and the
host's network itself, and a grant adds a second party holding them,
with the writable grant's way back into what the human runs. Only the
primary human's session may ask, since a grant's source is that
human's home. A grant does not make the account, which allocation
alone does, so grants spend none of the ledger's sandbox rows; they
spend the grant table's sandbox and workspace budget instead, half its
rows and bytes (td-authd/DESIGN.md, "Workload grant table"), so such a
program can use it up, refusing further sandbox grants until the human
revokes some, but cannot crowd out an application's. Nor can a request
set an application's network, which the application's declaration
fixes. A sandbox's grants and network take effect at its launches
through root's workspace intake, its directory views executable by its
account's kind (td-authd/DESIGN.md, "Workspace launch intake"), the
only way a sandbox runs.

That intake is reachable by every program running as UID 1000, its
socket the human's at mode 0600 and its peer check the kernel's
credentials, with no PIN, token or secure attention. Such a program can
start a shell or a command of its choosing in any sandbox the ledger
holds, as that sandbox's account, with its grants and network, and
speak to it over the connection; change those grants and that network
as the grant request above can; create a sandbox under any name that is
not held, a creation over a sandbox holding files or grants being
refused `exists`, so a creation destroys nothing; and delete any
sandbox, which clears its grants and empties its home.
That is the same trust class as the grant request: the program could
already grant the sandbox those directories and that network, and holds
them itself; what a launch adds is code it chooses running as the
sandbox's account, which can do no more than the account already may,
and the writable grant's way back into what the human runs is the
grant's, not the launch's. A deletion destroys what the sandbox's home
held, which no program of the human's could reach before: the human's
programs act for the human, and the human may delete a sandbox. A
creation adds a permanent ledger row, so such a program can spend the
sandbox rows, half of the sandbox and workspace rows, by cycling names,
after which a new sandbox is refused for good, but not crowd out a
workspace's row or an application's (td-authd/DESIGN.md, "Workload
account ledger"). The intake creates workspace slots too (WORKLOADS.md
step 8), so such a program can spend the workspace rows, but root admits
only td-agent's own slot names for them, so it can make td-agent's
slots before td-agent does, which td-agent adopts, and not deny it one
unless an earlier deployment's sandboxes already hold `WORKLOAD_ROWS`.

The same program can create, delete and start an instance in any
workspace slot, with any plan root admits of the human's home: any
directory a grant may name, read-only or writable by its role, and
td-agent's own repositories, object stores and conversation scratch
trees, td-agent's own state that no grant reaches, each in its one role;
deleting a slot ends every process its account runs and empties its
home and scratch tree, so such a program can end another conversation's
instance's leftovers and destroy what its slot held. That is the same
trust class as request `2c` and td-ctl: the program already reads and
writes everything a plan can name, td-agent's trees among them, as the
human, and what an instance adds is td-agent's tool host, root's fixed
entry, running as the slot's account with those directories, whose
writable ones are the same way back into what the human runs as a
writable grant's (below). A plan also holds the slot account's views for
as long as its instance runs, so such a program can keep a directory
from another instance of the same slot at the other access until its
own ends, and fill a slot's 32 instances and the 128 in all, but not a
sandbox's launches.
At most 148 connections are served at once, at most 16 of them sandbox
launches and 128 workspace instances, so such a program can hold the
intake's launches up until its shells and instances end and its other
verbs for as long as each takes, but not an application's
launch. Root admits only the request's fixed grammar, writes the jail's
spec itself and runs only td's own td-sandbox relay in the jail, with
the shell root names or the command the request names inside it, or
td's own td-agent with td-txt beside it and the arguments the request
names; a request names no environment, descriptor or spec, and a path
only as a grant row the writer validates or a plan entry root
validates. The account a sandbox or slot runs as cannot reach the
intake: its socket's mode admits the human alone, and the peer check
refuses any other uid.

A writable grant is what lets a sandbox reach back into what the human
runs, and its view's execute permission does not bear on that. The
account can write scripts, git hooks, Makefiles, build files and
entries in the human's `~/bin` or `.local/bin`, were the human to grant
those, into a writable view; through the idmap the files land on disk
owned by UID 1000, as if the human had written them, and run with the
human's authority when the human later runs them outside any jail. The
view's nosuid keeps a setuid or setgid bit from taking effect within
the view, not when the human runs the file from its own path. Only the
grant decides this; the human's later runs are not confined.

The same intake makes one more socket that admits the human's kernel
credentials and acts on them. Every workload that still runs as UID
1000 (mail and news and jailed td-agent workspaces, WORKLOADS.md steps
8 and 9) passes its peer check, so what
keeps them from it is only that no jail binds `/run/td-authd`, its
`1000` directory or the intake: the next paragraph's `/run`
reservation, which a td-jail test pins for these paths as it does for
the compositor's control socket. An unconfined td-agent workspace can
reach it, as it can the control socket.

Which workloads can ask is narrower than "anything but the human", and
not yet by account. Firefox and td-web run as accounts of their own,
which the control socket refuses, as do sandboxes, each its own
account. Mail and news and jailed td-agent workspaces still run as UID
1000 (WORKLOADS.md steps 8 and 9); what keeps them from asking today is
only that no jail
binds `/run/td-compositor/1000/td-control` or its directory: td-jail
reserves `/run` from every workspace, by path and by alias, and binds an
application nothing of the compositor's but its Wayland socket, which a
td-jail test pins. An unconfined td-agent workspace runs as the human
with no jail and can ask. Root's writer refuses a source that is not the
human's alone or is reached through a link, whoever asks.

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
sessions do nothing; application UIDs select their canonical
`td-app-UID/session` path, sandbox and workspace UIDs
`td-workload-UID/session`, and the runtime account `td-runtimes/session`.
Other nonroot UIDs have no configured delegation.
No argv or account-database path field selects the path. This uses safe
filesystem I/O
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
by construction — `/etc/passwd`'s password field is classified, and only
one class authenticates. There is no `/etc/shadow`: td's accounts hold
markers, never a secret, so nothing about them needs a root-only file, and
td-sshd's admission reads the same field through this crate's `db.rs`
(`td-ssh/DESIGN.md` §5):

The `login -f` and `exec-as` columns below are the *forced* paths,
which skip authentication because the caller has by then already
established the right to start the session. **"Reachable
only by root" is how that used to be stated here and it is not quite
true**, which is worth correcting rather than repeating: what root is
needed for is a switch that CHANGES something, and §4's
`creds::may_switch` is what refuses one, over all four uid columns. A
switch to the credentials the caller already holds takes `creds::apply`'s
early return instead and starts a program with no privilege that caller
lacked. So `exec-as tester` or `exec-primary` run as tester just runs the
program as tester, which tester could have done, and non-root `login tester`
or a service account's `exec-service-as` of itself likewise, while on a td
image root's field is `!`, which the forced path refuses (below). Such a run
works only if the caller's groups already match, resets its environment and
directory as any session does, and tries the session-cgroup join first, which
is the caller moving itself where the kernel lets it, a warning otherwise.
The forced path skips a password; it does not hand one out.

Ordinary `exec-as` shares `login -f`'s forced policy, and by construction
rather than by coincidence: both human-session front ends, `login` and
`exec-as` (with the primary selectors that forward to them), reach ONE
decision, `login::authorize`, which `the_session_policy_is_decided_in_one_place`
holds them to by refusing any module but `db` and `login` to name
`may_start_session` at all. So a locked or service-only account is refused on
every one of them. `exec-service-as` reaches the same account lookup and the
same credential switch, but a disjoint policy decision accepts only the exact
`!td-service` marker. It rejects an empty field, a hash and an ordinary
lock, so a unit cannot turn the service path into a second way to enter a
human account.

`login-primary` takes no operands; `exec-primary -- PROGRAM [ARG…]` takes
the same absolute program and literal argument tail as `exec-as`. Neither
accepts a name, UID, account-file path or environment override. They resolve
the primary name through the shared `td-authd/src/primary_account.rs` reader:
a root-owned, regular mode-0644 `/etc/passwd`, bounded canonical records,
unique names and UIDs, UID/GID 1000 and its canonical named home. The normal
front end then independently authorizes that name's password field, resolves
groups and enters the existing credential switch. Publication must precede
sessions; concurrent privileged account-file rewriting is outside this model.
These selectors neither authenticate nor grant authority: `login-primary`
retains the all-root caller requirement of `login -f`; `exec-primary` retains
the ordinary forced policy and same-credentials no-op behavior. Locked and
service-only accounts remain denied. A missing or malformed primary record
refuses without a fallback name. Environment and terminal handling follow
their existing front ends exactly. The applet/symlink roster is `login`
alone.

The shared reader is compiled beneath `forbid(unsafe_code)` and is included
in the source-level confinement scan alongside the local modules. Exactly
three reviewed external paths are admitted: that reader, since
TOKEN-LOGIN.md's increment 4 (C5), the shared std-only login-state
predicate `td-secret/src/login_state.rs` (below), which forbids
`unsafe_code` on itself and is compiled under a reasoned `dead_code`
allowance, since td-login calls only its predicate; the confinement scan
holds that its temporaries sweep, the one place it unlinks, is named
nowhere else. The third is the workload account ledger reader
`engine/src/workload_accounts.rs` (§7), compiled beneath
`forbid(unsafe_code)` with a reasoned `dead_code` allowance, since td-login
reads the ledger and never seeds it; its two owner checks are its only
zero-argument UID and GID reads. Other path attributes and code inclusion
remain forbidden.
Metadata's zero-argument UID and GID reads are distinct from credential
setters and are pinned per file; the shared sources' and the terminal
tests' test-only mode fixtures are pinned separately from the two
production terminal mode writes, and neither shared source writes a mode
outside its tests. The predicate adds no credential syscall or policy
decision; the console refusal below adds a hand-back to root that reuses
§6's identification and mode write in its own order.

| `/etc/passwd` field | class        | interactive `login` | `login -f` | `exec-as` (forced) | `exec-service-as` |
| ------------------- | ------------ | ------------------- | ---------- | ------------------ | ----------------- |
| empty               | `NoPassword` | allowed             | allowed    | allowed            | denied            |
| `!td-service`       | `Service`    | denied              | denied     | denied             | **allowed**       |
| `!`, `!!`, `*`, `x` | `Locked`     | denied              | **denied** | **denied**         | denied            |
| anything else       | `Hashed`     | **denied**          | allowed    | allowed            | denied            |
| no entry / no file  | —            | denied              | denied     | denied             | denied            |
| ledger `tda<uid>`   | `Service`    | denied              | denied     | denied             | **allowed**       |
| ledger `tdw<uid>`   | `Service`    | denied              | denied     | denied             | **allowed**       |

The last two rows are no passwd entries: an application account, and a
sandbox's or workspace's, is the workload account ledger's row (§7), of
the service class by construction.

Consequences worth stating plainly:

- An account with a real password hash **cannot log in interactively**.
  This build cannot verify one, and treating an unverifiable secret as
  absent would be the escalation. Root can still start a session for it
  (`login -f`, `exec-as`) — that grants nothing root did not already have,
  and denying it would only break the ordinary "root may become anyone"
  semantics.
- `Locked` is denied even on the forced paths, which is stricter than
  busybox (whose `-f` skips the account database entirely) and stricter
  than `su(1)`. A locked account is an explicit administrative statement
  that no session may run as it; the cheap way to honour that is to make
  the primary selector resolving a locked primary account fail loudly
  instead of quietly working. Nothing on a td image needs the other
  behaviour.
- **A service account is a distinct credential class, not an ordinary lock
  with a special caller.** `system-x86-64` writes the exact
  `!td-service` marker only for identities a unit starts through
  `exec-service-as`. The marker still begins with `!`, so software that does
  not know td's extension sees a conventional lock. td-login distinguishes it
  before the general `!` rule, refuses it through every human-session path,
  and refuses every other class through the service path. Making the audio
  account passwordless to satisfy ordinary `exec-as` would therefore red both
  the image contract and the credential policy.

  The shipped image has eight infrastructure service identities in passwd;
  its application identities are the ledger's carried rows. td-sshd's `sshd`
  privilege-separation account, `td-profiler`'s `profiler` account and
  `td-netd`'s `td-netd` account are not td-svc `exec-as` targets; each daemon
  performs its own fixed-purpose drop. td-netd's keeps CAP_NET_ADMIN and
  CAP_NET_BIND_SERVICE, which this crate's drop could never do by design
  (UNSAFE.md §2), and has a `/bin/false` shell and a volatile `/run/td-netd`
  home.
  The `sshd` account has a `/bin/false` shell and an empty root-owned
  `/run/sshd-empty` chroot recreated before the daemon starts. The `audio`
  account has a `/bin/false` shell and a volatile `/run/td-audio` home and is
  an `exec-service-as` target. The compositor account `tdc1000` has UID/GID
  993, a `/bin/false` shell and `/run/td-compositor/1000` home; its paired
  service uses the same checked service path. The broker account `tdb1000`
  has UID/GID 992, a `/bin/false` shell and `/run/td-bus/1000` home, and
  also uses `exec-service-as`. The portal `tdp1000` has UID/GID 991, a
  `/bin/false` shell and private `/run/td-portal/1000` home, and uses the
  same service path. The live installer wizard's `tdi1000` has UID/GID 990,
  a `/bin/false` shell and a `/run/td-setup` home nothing creates, and is
  entered through `exec-service-as` on live boots only. None can be entered
  through td-login's human forced modes or an interactive login.
- **A class is a property of a name; the uid is what the kernel
  enforces.** `classify` reads one account's password field, so
  `!td-service` says that the NAME `audio` is refused by every human
  path. It says nothing about uid 994. A second passwd entry sharing
  that uid carries its own password field and its own `/etc/group`
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
  image cannot be tailored into a machine that will not let anyone in. It
  also requires that user to be the primary account `login-primary`
  selects, and `build_autologin` refuses the build otherwise: the console
  logs in only through that selector.
- The stock image's one interactive account, `tester`, is
  `passwordless: true`, so the console is trusted **by image
  configuration**, not by td-login. The interactive behavior is
  unchanged from the busybox chain this replaces, which also accepted
  the empty password field without prompting. Root's field is `!`
  (`APPLICATIONS.md` §L.1, L7): `Locked`, which every column above
  refuses, the forced paths included, and which
  `system_def_is_self_consistent` holds for uid 0. Root needs no session
  of its own: td-svc runs root's units and probes directly, and
  `exec-as` and `exec-primary` only drop from root. A failed boot opens
  none either: td-init fails the boot instead of starting a console
  shell when `switch_root` refuses or the inittab is unusable
  ([`APPLICATIONS.md`](../APPLICATIONS.md) §L.1, "Administration
  today"). The service
  identities above are locked. These are properties of the shipped
  `SYSTEM` const.

**Login keys: the console refusal.** Under
[`TOKEN-LOGIN.md`](TOKEN-LOGIN.md), enrolling a FIDO2 login key publishes
`/var/lib/td/login/1000`; since its increment 5 (A5) the session owner
enrolls one through the compositor's attention screen. td-login
reads no CTAP and does not parse that record; it asks only whether one may
exist, through the directory-and-name predicate it shares with firstboot
and td-authd, on the real root and before anything else that could start
a session: `login-primary` before it resolves the primary account,
interactive `login` before it prompts or looks a name up. Unless the
root-owned mode-0700 `/var/lib/td/login` opens with valid metadata,
neither the record name nor the protected marker (`TOKEN-LOGIN.md`, "The
login record") is in it, and the boot left no `/run/td-admit`,
interactive `login` and `login-primary` refuse
**every** account, root included, so no console path bypasses the key.
The gate concerns a caller root in some uid column. A caller root in none
can switch to nobody but itself (§4), so it reaches no session it lacked,
and it cannot read the root-only directory: it keeps its existing
behaviour, which is what keeps an unenrolled machine's unprivileged
`login` unchanged. Before refusing, a root caller takes a terminal that
passes §6's checks back for root:root with the pinned `TTY_MODE`, so no
unprivileged process can open the line afresh. It writes the owner first,
then the mode, then reads both back off the open file. The order is the
reverse of §6's handover, which grants: here the node still belongs to
the account the console last served, and an owner may chmod its own
node, so a chmod first would let that account set 0666 again before the
chown and leave the line root's and open to everyone. Once root owns it,
only root changes its mode. A hand-back that fails or reads back wrong is
warned about as §6's is, and the refusal goes on. It does not revoke a
descriptor already open (TOKEN-LOGIN.md, "Cutover"). Each then writes
the one fixed line
`td-login: login keys enrolled or unavailable; console login refused`
to standard error and parks until killed, on its own thread in safe `std`
(`std::thread::park` in a loop, starting no thread) and with no new
syscall surface, rather than exit: the greeter's wrapper reboots the
machine when its session chain succeeds (`&& exec /bin/td-svc reboot`),
and its td-svc unit restarts it whenever it ends (`restart=always`), so
any exit would be a reboot or a respawn loop. A signal still ends it:
Ctrl-C or Ctrl-\ typed on the serial line, `SIGHUP` or `SIGTERM` ends
td-login with a non-zero status, so the `&&` skips the reboot and td-svc
respawns the greeter into the same gate. td ships no `su` (§4), root's
password field is `!`, neither SSH form admits root (§1) and td-init
starts no console shell (§3's stock bullet), all before any
machine can enroll (TOKEN-LOGIN.md, "Enrollment requires §L.1
elevation"). The
forced paths (`login -f`, `exec-as`, `exec-primary`,
`exec-service-as`) keep this section's rules:
they change credentials only for an all-root caller (§4) and are
otherwise no-ops. A greeter must therefore use `login-primary`:
`build_autologin` has no `login -f` fallback for a non-primary autologin
account, refuses the build for one, and `system_def_is_self_consistent`
requires the autologin account to be the primary one. A console session
started before enrollment does not survive it: td-authd's cutover
(`td-authd/DESIGN.md` amendment 7) hands this line back to root and
restarts the greeter, whose stop ends every process holding the line as
its controlling terminal, and `sshd`, whose stop ends its whole leaf,
before it records the enforced form; a revocation that fails reboots
once, guarded, or holds. TOKEN-LOGIN.md's "Cutover" owns how. The stock
image ships no record and firstboot ensures the directory at every boot,
so until its owner enrolls a key its console behaviour and the table
above stand; the refusal acts only where a record or an invalid
directory exists, and removing the last key cuts back over to that
behaviour (TOKEN-LOGIN.md, "Cutover"). Since TOKEN-LOGIN.md's increment
4 (C7) the compositor starts every generation locked on exactly those
states, so this refusal and the enforced SSH form are what keep the
serial line and SSH from passing a lock screen there.

The future hardware-backed disk and session unlock contract lives in
[`td-install/ENCRYPTION.md`](../td-install/ENCRYPTION.md). It binds primary
authentication to one enrolled account and distinguishes recovery from login;
the current auto-login and credential-switching behavior below is unchanged.

Credential-store enrollment and token-gated session release are separate
root-owned operations in td-secret and td-authd, specified by
`td-secret/DESIGN.md`. They do not switch process credentials or replace
td-login's auto-login policy. Firstboot clears volatile keys and leaves
all sealed stores locked. The paired compositor selects physical enrollment
or unlock and presents each immutable request before token I/O. Only a
verified assertion and the recorded TPM platform state permit release;
file and TPM-only stores cannot serve applications. td-login's session
authorization table is unchanged. Firstboot reserves
compositor, broker, portal, and per-app identities in a persistent
ledger, and refuses account records that alias a reservation. The image
activates the compositor assignment as service account `tdc1000` at
UID/GID 993 and the broker as `tdb1000` at UID/GID 992. Firstboot
prepares the broker and portal runtimes after durable enrollment, whose
success td-svc requires before startup. The portal consumes UID/GID 991;
firstboot transfers credential-store ownership to it before human
sessions and clears volatile releases independently of application-home
validation. Sealed stores remain locked until a presented token operation
succeeds; file and TPM-only stores cannot serve application credentials.
That is a rule of td-secret's stores: td-mail's `password_file`
(`APPLICATIONS.md` §W.4) is a file the application reads itself, outside
them by its owner's choice, and nothing in this document protects it.
Foreign application assignments are active service accounts.
The broker loads an immutable deployment table binding each installed name
to its execution UID and exact bus grants. Personal mail and news share the
human UID, but each requires registered jail lineage before its name is
exposed to the portal. An unconfined human connection has no application
identity. Only names explicitly assigned to the human UID may be registered
by that UID. Foreign applications retain their private homes under
`/var/lib/td/applications/APP_UID`, runtimes and cgroups. Personal
applications use the human home only through the jail's per-application
state and explicit configuration grants; an escape from that jail would
have human-UID authority. The root launcher checks both the retained UID
reservation and the immutable execution assignment, then uses `exec-primary`
for personal applications or the service-only class for foreign ones.
`td-authd/DESIGN.md` specifies this reservation and launch contract. The
image's paired td-authd terminal launcher uses ordinary `exec-as` after
a read-only reservation check and private-channel authentication. It
fixes the human account in root-owned service configuration and replaces
every standard descriptor before spawning this helper. Its unprivileged
terminal-exec wrapper refuses a failed session-cgroup placement before
terminal code runs, and a new process group keeps the user terminal
independent of the authority generation. This adds no credential-switch
mechanism or human authentication policy here. The paired compositor now
reserves physical Ctrl+Alt+Esc for a trusted screen with exclusive input.
It selects enrollment with a second token or explicit unrecoverability,
and primary or recovery unlock. Each immutable step must be presented
before root token I/O; an assertion binds the exact request. Session
startup clears old releases before graphical input begins, and generation
teardown reaps any worker before clearing its release. Typed credential writes use a human-authenticated sealed descriptor and
physical W selection of one queued request; one presented token assertion
authorizes that operation. There is no root-console write bypass. No login or keyboard-consent
behavior substitutes for that authorization.

Local system installation is a separate named root operation,
`deploy-publish`, which the principal table must grant the account. The
paired compositor selects one queued build with physical I and presents
its full manifest ID; Enter, pressed first and at least 500 ms after
complete presentation, authorizes only that installation.
td-authd passes its pinned source directory to the fixed
installed helper, which verifies it and uses the existing boot
transaction. td-login does not elevate the
caller. This does not change human login, hardware authentication, credential
release or sensitive protector policy. See td-authd/DESIGN.md and
td-install/DESIGN.md for admission and lifetime limits.

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
  0" gate would admit an unprivileged caller — and since `exec-as` takes
  the forced policy path (§3), that caller would reach root *without
  authenticating*. The two checks are independent: one would have to
  fail silently and the other be edited for the boundary to move.

It follows that:

- A1 cannot use `exec-as` or `exec-primary` to become another user.
  `creds::may_switch` refuses a caller that is not root in every uid
  column, and the kernel would refuse the syscalls anyway; the check
  exists so the failure is a named diagnostic rather than an `EPERM`
  from somewhere in the middle of a switch.
- So no td-login front end is an administrative path: each drops
  privilege, and only root's use of one changes credentials. td ships no
  `su`. `APPLICATIONS.md` §L.1's
  L6 deleted the applet, whose only callers were root-run boot-health
  probes, and moved them to `exec-primary` (§5, §8); a stale `/bin/su`
  link or `td-login su` reaches usage and exit 2, and the image's shape
  check refuses a packed `/bin/su`.
- `exec-as`'s program and argument tail, which in a setuid program would
  be an escalation surface (choose the program root runs), are inert:
  only root can change credentials, and root can already exec anything,
  as an account naming itself can exec what it runs as itself. Its grammar has no options and a mandatory `--`, so no
  argument word is ever consumed as one.
- A switch to the credentials already held is a no-op — `apply` returns
  early when the kernel's view already equals the target — so it attempts
  no privilege change. An account naming itself reaches it and runs its
  program as itself; a td image's locked root is refused (§3). The
  session-cgroup join is independently idempotent: a same-user invocation
  already in the leaf performs no write.

This is why the crate is `#![deny(unsafe_code)]` with a three-syscall
exception rather than a program with a carefully audited setuid entry
point: the entire "gain privilege" half of a traditional `su` does not
exist.

## 5. Environment, and what the session inherits

A3 controls the environment td-login is handed. For a **login session**
(`login`) that environment is discarded: the session
starts from `PATH=/bin`, plus `HOME`, `SHELL`, `USER`, `LOGNAME` derived
from the account database, plus `TERM` carried through because the
terminal type is a property of the terminal, not of the caller. `login
-p` preserves the rest on the caller's explicit say-so — it is a flag
only root can reach, and root already owns the machine.

No front end other than root's `login -p` keeps the caller's
environment. The non-login `su`, which kept it and overwrote only those
five variables, was deleted with its applet (`APPLICATIONS.md` §L.1,
L6). The root-run boot-health probes it carried run through
`exec-primary -- /bin/sh -c '…'` under the rule below, which differs
in three ways: the probe's shell inherits only the five identity
variables, starts in `/` rather than the caller's directory, and sees
the account's `SHELL` rather than the `/bin/sh` that `su -s` named.
`system-x86-64`'s
`every_primary_probe_reads_only_what_it_assigns_or_its_identity` holds
every such body to reading only `HOME`, `USER`, `LOGNAME` and `PATH`
or a variable that a bare top-level `NAME=value` statement of the body
assigned first, and to never reading `SHELL`. It is a conservative scan
of the shell's own parameter reads, not a shell: an assignment inside a
subshell, substitution, group or conditional, or as a command prefix,
does not count, and a body using a construct it does not model —
`eval`, `.`, `[[`, a function, a `for` list, a here-document, `${!…}`,
any name inside `$((…))` (td-sh evaluates one recursively), a program
runner such as `env` or `exec` given an operand that expands, or a
shell's name anywhere but as `which`'s operand without exactly `-c`
and one literal script that holds no `$` and passes the same scan —
fails the test. A program the body runs that reads its environment
sees only what `exec-primary` gave and the body exported.

For **`exec-as`, `exec-primary` and `exec-service-as`** the environment
is discarded ENTIRELY — these are the only front ends where nothing at
all of the caller's survives, `TERM` included, and each starts in `/`.
What they start is a SUPERVISED DAEMON or a root-run probe, whose
environment ought to be a property of its unit or script; td-svc has
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
target user ownership of any file the caller can open — `/etc/passwd`,
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
group-readable mode. That reasoning is for a grant; the console
refusal's hand-back to root writes `chown` first, for §3's reason.

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
  would be a denial of service. The td deployment base is checked at build
  time; a saved-name projection is completely validated and made read-only
  before consumers start. Invalid retained state still stops boot and
  requires trusted recovery. Between "nobody logs in" and "the wrong person
  logs in", this program picks the first.
- A duplicate user name in `/etc/passwd` is an error.
  Two entries for `root` with different uids is an ambiguity nothing
  downstream can resolve safely, and "first wins" versus "last wins" is
  exactly the kind of ordering assumption this document exists to
  refuse.
- `home` and `shell` must be absolute paths. `login` execs the shell by
  absolute path with no `PATH` search, so a relative shell would
  resolve against the current directory.
- uid and gid must parse as `u32`. No wrapping, no negative-as-huge.
- A `/etc/passwd` record must carry **all seven** fields, not merely the
  name and password the decision reads. The password field is the
  *second* of seven and an empty one means `NoPassword`, so a record
  that loses its tail keeps a name and an empty secret: accepting a
  short line is how corruption of a **locked** account turns into an
  account anybody may log into.
- A `/etc/group` line naming a user is only honoured if the group's gid
  parses; a malformed group file fails the lookup rather than silently
  granting a smaller set.

Workload accounts come from the workload account ledger
(`td-authd/DESIGN.md`, "Workload account ledger") alone. `tda<uid>`, `tda`
followed by digits, is an application account's reserved form, and
`tdw<uid>` a sandbox's or workspace's; another name that merely begins
`tda` or `tdw` is an ordinary name.

- `exec-service-as tda<uid>` or `tdw<uid>` reads the ledger through the
  shared reader in `engine/src/workload_accounts.rs` (`Ledger::account`),
  which opens `/var/lib/td` without following links, requires it
  root-owned and writable by no one else, and requires `accounts.tsv` to
  be a single-link, bounded, root-owned regular file of mode 0644 at the
  canonical format. The uid must be spelled canonically and a row of a
  kind the form names must hold it: an application row for `tda`, a
  sandbox or workspace row for `tdw`. An application row never answers
  for `tdw<uid>`, nor a sandbox or workspace row for `tda<uid>`, and the
  ranges keep the kinds' uids apart. The account is then fixed, not read:
  gid equal to uid, home `/var/lib/td/applications/<uid>` or
  `/var/lib/td/workloads/<uid>` by kind, shell `/bin/false`, the service
  class, and no supplementary group. A missing, hostile or malformed
  ledger, or a uid it does not hold, refuses. Any application row
  resolves, a retired application's (a former install's `tda65539`) or a
  personal one's included, and any sandbox or workspace row: td-login
  reads no declarations or grants, and `exec-service-as`'s caller is root,
  which could run as any uid anyway. Such an account may have no home,
  runtime or cgroup delegation prepared, since firstboot prepares only the
  declared service applications at boot and a sandbox's delegation lasts
  from its allocation to shutdown; td-login only warns when the join
  fails (§1), and the root launchers that start applications, td-authd's
  among them, select only declared ones and make a failed placement
  fatal. Root's workspace intake launches, and empties a deleted
  sandbox's home, as a sandbox's account only after preparing its
  delegation again, and makes a failed placement fatal
  (td-authd/DESIGN.md, "Workspace launch intake").
- Every lookup, human or service, reads passwd and group whole and
  refuses when any entry is of either reserved form or holds a uid or gid
  in the ledger's ranges (65536 through 131071), or a group lists a
  reserved-form member. Such an entry would be a second account policy
  for a ledger uid, the class confusion the alias rule above refuses
  within passwd: an empty password field the forced paths admit, or
  supplementary groups the ledger's account does not have. The range
  check needs no ledger read, so an ordinary login does not depend on the
  ledger.
- `login`, `login -f`, `exec-as` and `exec-primary` refuse a reserved-form
  name before any lookup, and a primary name of that form stays refused
  by the shared primary validator.
- Reserving a form refuses a human who already holds a name of it. The
  `tdw` form arrived after installs whose primary name was chosen freely,
  so a human named `tdw` and digits (`tdw7`) on such an install loses
  every login lookup and fails primary preparation on each boot of a
  deployment that reserves the form, until rolled back or renamed. The
  risk is accepted as very unlikely: the installer refuses such names from
  then on, and the alternative, a passwd account that can speak for a
  ledger account, is the class confusion this section refuses.

The supplementary set handed to `setgroups(2)` is the user's `/etc/group`
memberships **plus** the primary gid, sorted and deduplicated. Sorting is
not cosmetic: it makes the set comparable, which is what lets Layer 2
assert equality against `/proc/self/status`.

## 8. What proves this on a real image

- `cargo test` covers the parsers, the policy table, the ordering
  confinement, and the `/proc/self/status` reader against captured
  fixtures.
- The disposable `td-authd/tests/launch_vm.rs` QEMU fixture exercises
  both primary selectors with `alice`, UID/GID 1000. Its child probes
  check all four UID/GID columns, supplementary groups, cleared held
  capabilities, environment replacement and working directories.
  A nonroot forced login, foreign-UID primary exec, locked/service-only
  password fields, missing UID 1000 and duplicate UID aliases must refuse.
  Its guest holds an unenrolled root-owned mode-0700 `/var/lib/td/login`,
  so the console gate admits its root `login-primary`.
  The foreign-UID case proves the credential gate itself refuses the
  switch. This fixture is a host diagnostic supplied with the target td-login binary,
  not image content.
- The boot exercises login's forced-session path through `login-primary`
  and credential health probes through `exec-primary`. Unit tests cover
  the login applet's basename route and refuse `su` by either entry
  form; the image shape check pins the login symlink and refuses a
  `/bin/su`. A recipe unit test holds every `exec-primary` probe body to
  the environment §5 gives it.
- `TD-LOGIN-RUN-OK` is the credential-specific evidence. `/etc/bootsuccess`
  runs `exec-primary` to the unprivileged user, and as root `exec-as` to
  the same account, and in each has `td-login verify-credentials` read
  `/proc/self/status` back, asserting the exact uid, gid and
  supplementary set. A switch that "worked" but left a residual group
  attached prints no marker and reds the boot oracle — which is the one
  failure mode every other check on the image would pass.
- Root's retirement (`APPLICATIONS.md` §L.1, L7) has two markers in the
  same `/etc/bootsuccess` run, under the autotest token. This is where
  their conditions are stated. `TD-ROOT-SSH-REFUSED` prints once root's
  record reads exactly `root:!:…` and `ssh -v` with the loopback root key
  the disposable volume seeds into the persistent authorization file
  exits 255, its output showing the client offering that key, by its
  pinned fingerprint, before `Permission denied (publickey)`: a
  connection that failed offers nothing, so it cannot pass.
  `TD-SU-ABSENT` prints once a listing of `/bin` holds `sh` and no `su`,
  so a dangling link counts as present. Either failure fails the
  deployment's health; `qemu-boot-system` and `qemu-install-system`
  require both markers. A recipe unit test pins root's generated record
  and that only the primary's field is empty, and a host test pins that
  the fixture's private and public halves are one key with that
  fingerprint. td-init's unit tests hold that neither PID-1 failure path,
  an applet returning or an unusable inittab, yields a job or starts a
  shell.
- The console refusal's host tests run the gate in spawned processes
  against temporary roots: for a caller root in some column, enrolled, a
  missing directory, a non-directory and a foreign owner each write the
  exact line last, waited for up to a 30-second deadline, and leave the
  gate alive past a grace period after it; unenrolled returns; a caller
  root in no column returns whatever the state. Every unavailable cause
  is refused by the decision, and the line is this section's. The
  handover's writes and the hand-back's (owner first, nothing written
  before a refused `chown`, and a wrong read-back refused) are watched
  on files the test owns. Its full-system evidence is
  `TOKEN-LOGIN.md`'s `qemu-login-system`.
- The jailed fixture is the cgroup-placement evidence. Its QEMU marker is
  withheld unless td-jail observes the active per-instance sibling leaf with
  the exact configured caps; that migration can succeed only when the
  td-login-started session entered the delegated subtree first.


The trusted notebook's mail adapter admits portal UID 991 at a root-owned
session-generation endpoint; the portal must first authenticate the caller's
`mail` AppId through td-busd. A human UID or caller-supplied tuple alone is
insufficient. The tuple selects only a previously approved entry password,
while the notebook is unlocked. The adapter exposes neither notes nor vault
keys and cannot unlock the service. Closing or locking the notebook cancels
outstanding requests, but cannot revoke plaintext already released to mail.
