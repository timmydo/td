# Applications on td

**Status: incremental implementation.** The milestone ladder records what has
landed; the remaining text is the normative design for running third-party
graphical applications (Firefox, darktable, GNOME apps) inside td's own Wayland
compositor: the package format, identity, confinement, permission model and
state contract, and the components that implement them — `td-jail`, `td-busd`,
`td-portal` and `td-audio`.

The one compatibility claim, deliberately narrow, which no commit message
may widen by accident:

> The curated Firefox package and its declared runtime ship in the image,
> launch without host `/usr` or host libraries, and display an
> interactive Wayland window in `td-compositor`.

That implies nothing about audio, printing, cameras, screencast, X11,
arbitrary document access, GPU acceleration, or any application beyond
the one named.

## The model — a curated hybrid

Applications are **built by td recipes**, not installed from an open
repository. A recipe either **seeds** from a prebuilt upstream artifact
pinned by URL and `sha256`, or builds from source; the two produce the
same kind of package and the choice is per application (§B.3).

That removes the network stack, the signature trust root and the repository
format from the **target**. There is no target-side OSTree client, GVariant
reader, OpenPGP verification, HTTP or TLS. §B.3.1 selects a bounded
control-plane reader for exact Flathub commit graphs; the signature that
authorizes each commit is still checked by a human when the pin is reviewed,
the same way td already trusts the kernel tarball and Rust bootstrap snapshot.

The cost is that the curated set is td's to maintain: a seed bump is a
pin review plus a rebuild, and no application td has not packaged can be
installed at all.

## Premises

Settled by the maintainer; these are premises, not conclusions.

1. **Packages ship in `/td/store` with the image**; only writable state
   lives in `~/.td/app`. There is no install step and no privileged
   installer — choosing which shipped application to run needs no
   authority beyond a user's own launcher table.
2. **The first application-capable `td-jail` carries seccomp**, as part
   of the same `UNSAFE.md` surface. The preceding skeleton and mount
   rungs launch no application; their public entry point fails closed.
3. **Portals are `td-busd` + `td-portal`**, outside the compositor
   process (§E).
4. **Per-app uids are the identity direction — v2, not v1.** An
   unprivileged process may write only an identity `uid_map`, so this
   needs `CAP_SETUID` in the parent namespace and therefore a privileged
   component. v1 runs applications at uid 1000 and records the identity,
   so the change is additive.
5. **GPU acceleration is required**, not eventual (§M).
6. **Resource caps are required** — *"I don't want firefox eating all my
   RAM"* (§P).
7. **Keep an open path to other architectures.** Every UAPI layout here
   is x86-64-specific and is marked as such.
8. **The kernel prerequisite (§0) and `UNSAFE.md` surfaces #9, #10 and
   #11 are approved.** Approving a surface approves its existence, not
   its contents: each still lands with its own `UNSAFE.md` section,
   value-pinned roster and confinement tests.

## Trust

A third-party application is a **third zone**. Zone one is the
source-bootstrapped target graph; zone two is the host control plane;
zone three is a foreign prebuilt application, confined to one jail.

Zone three lives **in** `/td/store`, admitted by the §B.8 marker rather
than excluded by a layout. The marker's job is to keep the property that
matters while giving up the one that cannot be kept: a foreign payload is
never a tool, compilation or execution input to anything td builds, its
`PT_INTERP` is absent from the image root so it cannot be executed
outside a jail, and it is excluded from the source-bootstrapped claim in
a way a closure query can check.

The claim that results is narrower than "the store contains no foreign
binary", and the narrowing is deliberate:

> td's bootstrap graph contains no foreign binary other than its
> declared bootstrap seeds, and no foreign APPLICATION binary is an
> input to anything td builds. A curated set of applications ships in
> the store as marked sandboxed-application outputs — foreign prebuilt
> payloads that bring their own root and libc, are consumed by nothing,
> and are reported as such. td RUNS them behind td-owned namespace,
> seccomp, D-Bus, portal and Wayland boundaries.

"Runs them behind" rather than "they execute only behind": §B.8 shows
that nothing prevents the bytes being executed another way, and a claim
this document cannot keep is worse than the weaker one it can.

The sandbox does not *make* an application trusted; it bounds what a
compromised one reaches.

## 0. The kernel prerequisite — the gate, now landed

This section is first because it gates every other one, and because it
was not visible from either design brief.

**Read the next few pages as the BASELINE this rung argued against, not
as the current kernel.** The pins below have landed, so the "today"
column of the table records what the kernel had before them and the
mechanism paragraphs explain how it got there — which is the part worth
keeping, since the same three-step resolution decides every future
symbol. What the kernel carries NOW is at the end of the section, under
the pin block.

`recipes/src/recipes/linux-x86-64.rs` resolves the kernel config in
**three** steps, and reading only the first is how an earlier draft of
this table got an answer wrong:

1. **`allnoconfig`** (`linux-x86-64.rs:331`) answers `n` to every
   *prompted* symbol regardless of its `default`. A symbol whose prompt
   is guarded `if EXPERT` keeps its default instead, because
   `allnoconfig` turns `EXPERT` off and an invisible prompt cannot be
   answered.
2. **An explicit pin list** (`:530-606`, 77 lines since this rung added
   nine) forces symbols on or off by name.
3. **`olddefconfig`** (`:614`) then "takes defaults for newly-visible
   symbols" — the recipe's own words. This is the step that matters: a
   symbol invisible at step 1 because its dependency was off becomes
   visible once step 2 pins that dependency, and **olddefconfig takes its
   `default`, which may be `y`.**

So a symbol's final value is not "whatever allnoconfig said". Checked
against the pinned `linux-7.1.4` source and against the resolved config:

| symbol | Kconfig shape | before this rung |
|---|---|---|
| `NAMESPACES` | `bool "…" if EXPERT`, `default !EXPERT` | **y** — the menu is on |
| `MULTIUSER`, `SHMEM` | `bool "…" if EXPERT`, `default y` | **y** |
| `USER_NS` | `bool "User namespace"`, `default n` | **n** |
| `NET_NS` | prompted, `default y`, `depends on NET` | **y** — see below |
| `PID_NS`, `UTS_NS`, `TIME_NS` | prompted, `default y` | **n** |
| `IPC_NS` | prompted, `default y`; `depends on SYSVIPC \|\| POSIX_MQUEUE` | **n**, deps also off |
| `SECCOMP` | explicit `prompt` line above `def_bool y` | **n** |
| `SECCOMP_FILTER` | `def_bool y`; depends on `SECCOMP` **and `NET`** | **n** |
| `CGROUPS`, `FUSE_FS`, `INOTIFY_USER`, `SYSVIPC` | prompted | **n** |
| `OVERLAY_FS` | — | **n**, and the recipe *asserts* it stays off |
| sound (`CONFIG_SND*`) | — | **n**, nothing enabled |

**`NET_NS` was already on** before this rung, and the earlier draft of
this table said it was off. It is invisible at step 1 (`NET` is off, so
its prompt never appears), the pin list turns `CONFIG_NET=y` on at
`:569`, and step 3 then takes its `default y`. Nothing pinned `NET_NS` —
it arrived as a side effect, which is why this rung pins it explicitly.
That is one fewer symbol to argue for, and more importantly
it is the mechanism to remember: **pinning a dependency can enable its
dependants.**

The same trap is latent in the other direction for `IPC_NS`, which is
`default y` and depends on `SYSVIPC || POSIX_MQUEUE`. Enabling SysV IPC
for some unrelated reason would bring an IPC namespace along without
anyone pinning one — the opposite of the "cannot be enabled alone"
consequence below, and worth knowing before someone pins `SYSVIPC`
casually.

The conclusions this section exists for survive unchanged: `USER_NS` and
`SECCOMP` were genuinely off, so `unshare(CLONE_NEWUSER)` returned
`EINVAL` and `seccomp(2)` returned `ENOSYS` on the kernel that shipped
before this rung — which is what made it the gate. `SECCOMP` is the
subtle one: its explicit `prompt` line makes an otherwise-`def_bool y`
symbol answerable, so allnoconfig says no and no later step revisits it.
Note also that `SECCOMP_FILTER` depends on `NET` as well as `SECCOMP`,
which is satisfied but should be stated rather than discovered.

**That SECCOMP claim was challenged in review and it holds; the check is
recorded here so the next reader does not have to repeat it.** The
objection was that `SECCOMP`'s prompt is `EXPERT`-gated like
`NAMESPACES`', which would leave it `y` under `EXPERT=n` and make
milestone 1 unnecessary for seccomp. It is not gated. In the pinned
`linux-7.1.4`, `arch/Kconfig:643`:

```
config SECCOMP
	prompt "Enable seccomp to safely execute untrusted bytecode"
	def_bool y
	depends on HAVE_ARCH_SECCOMP
```

The `prompt` carries no `if`, so `allnoconfig` answers it `n` — unlike
`NAMESPACES`, whose prompt *is* `if EXPERT` and which is therefore on.
The difference between those two symbols is one `if` clause, and it is
the whole of why one needs a pin and the other does not; a reader who
checks only one of them will draw the wrong conclusion about the other.
`SECCOMP_FILTER` (`arch/Kconfig:660`) is `def_bool y` with **no** prompt,
so it cannot be pinned directly at all — it follows from `SECCOMP && NET
&& HAVE_ARCH_SECCOMP_FILTER`, which is step 3 doing exactly what this
section is about. The pin list must therefore name `SECCOMP`, not
`SECCOMP_FILTER`.

**Milestone 1 is therefore a kernel-config landing, and it is a real
argument rather than a formality.** This is a distro that keeps legacy
`KEXEC` off expressly "to not widen the surface" and that reasons
explicitly about `SECURITY_DMESG_RESTRICT`. Unprivileged user namespaces
have historically been one of the largest local-privilege-escalation
surfaces in the kernel, and turning them on to run foreign binaries is
exactly the trade that deserves its own reviewed commit with its own
prose. If that argument fails, this design is dead and better to learn it
in one commit than in twenty.

**APPROVED by the maintainer, and LANDED.** The pin block below was a
landing to write rather than a case still to argue, and §J's first risk
retired with it.
Two things the approval does not do, stated so the commit that carries
it stays honest: it does not make the boot oracle optional — a kernel
that regresses `USER_NS` or `SECCOMP` must red the image rather than the
first application — and it does not extend to `SYSVIPC` or `FUSE_FS`,
which are deferred below and each need their own argument when something
wants them.

Consequences worth stating separately:

- **`IPC_NS` cannot be enabled alone.** It depends on `SYSVIPC ||
  POSIX_MQUEUE`, both off. Either enable SysV IPC — which nothing else on
  the image wants — or ship without an IPC namespace and record that.
- **No FUSE ⇒ no Documents portal in its standard form** (§E). The
  document store is a FUSE filesystem mounted at `$XDG_RUNTIME_DIR/doc`
  — not `/run/flatpak/doc`, which an earlier draft wrote — and without
  FUSE that exact mechanism is unimplementable rather than merely
  unimplemented. The narrower claim is the correct one: what is
  foreclosed is the *fd-passing document store*, not every conceivable
  FileChooser, since a portal that returns a real path under a granted
  directory needs no filesystem of its own. §E builds that one.
- **No inotify ⇒ GLib file monitoring degrades to polling.** Cheap to
  pin now, tedious to diagnose later.
- **Overlayfs is refused on purpose**, so `/app`+`/usr` composition is
  bind mounts. That is what bubblewrap does anyway; it just forecloses an
  alternative someone would otherwise reach for.
- **No sound of any kind TODAY** — not "no PipeWire", no ALSA and no
  `/dev/snd`. This is a statement about the shipped kernel, not a
  non-goal: §K designs the audio path and the pin block below turns the
  hardware on for it. An earlier draft said "audio is a non-goal" ten
  lines above its own `CONFIG_SND*` pins, which was left over from before
  §K existed.

The pins, with `MEMFD_CREATE`/`EPOLL`/`FUTEX`-class symbols included
defensively in the recipe's existing style even though they are
EXPERT-gated and already on. Everything but the audio block has landed;
the audio pins wait for td-audio at rung 25:

```
CONFIG_USER_NS=y  CONFIG_PID_NS=y  CONFIG_NET_NS=y  CONFIG_UTS_NS=y
CONFIG_SECCOMP=y                   # NOT SECCOMP_FILTER — see below
CONFIG_INOTIFY_USER=y
# audio (§K), landing with td-audio rather than with the jail:
CONFIG_SOUND=y  CONFIG_SND=y  CONFIG_SND_PCM=y
CONFIG_SND_PCI=y  CONFIG_SND_HDA=y  CONFIG_SND_HDA_INTEL=y  CONFIG_SND_HDA_GENERIC=y
CONFIG_SND_ALOOP=y                 # in-guest loopback, the audio test oracle
# CONFIG_SND_PCM_OSS stays OFF — present in 7.1.4, deliberately refused (§K.3)
# resource caps (§P) — NOT deferred after all, see below:
CONFIG_CGROUPS=y  CONFIG_MEMCG=y  CONFIG_CGROUP_PIDS=y
CONFIG_CGROUP_SCHED=y  CONFIG_FAIR_GROUP_SCHED=y  CONFIG_CFS_BANDWIDTH=y
# CONFIG_RT_GROUP_SCHED is not set  # fair-scheduler bandwidth only
# IPC_NS deferred: needs SYSVIPC, which nothing else on the image wants
# FUSE_FS deferred: lands with the Documents portal, not before
```

An earlier draft deferred `CGROUPS` on the grounds that "rlimits cover
the first need". They do not, and §P explains why: rlimits are
per-process and inherited, so a browser's content processes multiply the
cap rather than share it. Since the kernel landing is one reviewed
commit either way, the cgroup symbols belong in it.

The functional namespace probe has two deliberately different homes.
`td-jail`'s own recipe test runs the target-static binary against the
build host as a policy smoke test. The authoritative image assertion is
the QEMU boot oracle: as uid 1000 on the target kernel it creates the
complete user, mount, pid, UTS and isolated-network set, reads the
identity maps back, proves stage 2 is PID 1, uses exactly the one-capability
mount bridge, enters the fresh immutable root, clears and reads back every
capability set, installs and reads back no-new-privileges and the compiled
filter, reaps a filtered reparented descendant, and gates boot success on
`TD-JAIL-TRANSITION-OK`. A non-shipped target probe staged only into the QEMU
fixture exercises the exact filter's errno and kill actions. Failure disables
application launch with a named diagnostic; it never silently selects a weaker
sandbox.

**LANDED, in staged halves, and the split is worth reading before relying on
it.** The pins above (minus audio, which waits for td-audio) are in
`linux-x86-64.rs`, each guarded against the RESOLVED `.config` rather
than against the pin list; and the greeter carries a kernel-capability
farm that prints `TD-SANDBOX-KERNEL-OK` once the RUNNING kernel has been
observed to have every one that can be witnessed from `/proc` — all of
them but `MEMCG`, for the reason below. What did NOT land in the kernel
rung is the wording above taken literally: the image oracle READS
`/proc` and does not ISSUE `unshare(2)` or `seccomp(2)`. Those two calls
are surface #9's, and inventing an earlier prober would have meant an
`unsafe` surface outside the crate that owns it. The functional
namespace assertion landed with rung 8; rung 9 adds the mount,
capability-bridge, pivot and readback assertion in `td-jail` and the
target-system QEMU oracle. Rung 10 adds final capability removal and the
orphan-reaping assertion. Rung 11 installs and reads back the compiled filter
and proves its behavior with an interpreter, build-host probe, and target QEMU
probe.

What the `/proc` reads DO cover, beyond the features themselves, is the
one class of failure a config guard structurally cannot see: a value.
Each namespace has a ucount ceiling in `/proc/sys/user/max_*_namespaces`
that a compiled-in namespace cannot survive being set to zero — the
feature is present, its `/proc/self/ns/` node is there, and `unshare`
returns `ENOSPC`. All four td-jail asks for are read, not just the user
one.

The gap that leaves is narrow and worth stating precisely, because
"reads `/proc`" sounds much weaker than it is: procfs builds
`/proc/<pid>/ns/<kind>` from a table whose entries are `#ifdef`ed on
their namespace symbol, and writes `Seccomp:`/`Seccomp_filters:` into
`/proc/self/status` only under `CONFIG_SECCOMP`/`CONFIG_SECCOMP_FILTER`
— so those nodes exist *if and only if* the feature is compiled in. What
is unproven is the sysctl and LSM policy around the calls rather than
the features themselves, and `/proc/sys/user/max_user_namespaces` covers
the one sysctl that turns a compiled-in `USER_NS` into an `EPERM`.

**`SECCOMP_FILTER` must not be pinned, and the block above said to pin
it.** That line contradicted this section's own prose eight paragraphs
up and is corrected. `SECCOMP_FILTER` has no prompt at all, so kconfig
computes it from `SECCOMP && NET` and `olddefconfig` DROPS a line naming
it — leaving a pin list that reads like a guarantee nothing made. It is
guarded instead, which is the only place a derived symbol can be
observed. Resolving the pinned `linux-7.1.4` end to end confirms it:
with `SECCOMP` pinned and `SECCOMP_FILTER` absent from the list, the
resolved config carries `CONFIG_SECCOMP_FILTER=y`.

**`IPC_NS` is now guarded OFF rather than merely left out**, which is
this section's own `NET_NS` lesson applied in the other direction: a
symbol that is `default y` behind a dependency somebody may pin later
arrives unasked. `td-jail` omits `CLONE_NEWIPC` on the strength of it
being off, so the guard is what makes that a decision.

**`MEMCG` needed a different runtime witness, and the reason is a trap
worth carrying into §P.** `/proc/cgroups` is the obvious place to look
for a controller, and it does not list `memory` on this kernel even
though `CONFIG_MEMCG=y`: `proc_cgroupstats_show` skips any subsystem
where `cgroup1_subsys_absent()` holds — no v1 interface but a v2 one —
and memcg registers its `legacy_cftypes` under `#ifdef CONFIG_MEMCG_V1`,
which resolves to `n` here. The kernel says as much itself, once, on the
console: *"/proc/cgroups lists only v1 controllers, use cgroup.controllers
of root cgroup for v2 info"*. `pids` is listed because it registers v1
files unconditionally, so the greeter also asserts that legacy view — anchored on the
`enabled` column, since `cgroup_disable=pids` leaves the row and clears
it, which is the one failure no config guard can see.

The consequence for §P is that **`cgroup.controllers` on a mounted
cgroup2 is the interface that answers whether the v2 `cpu`, `memory`, and
`pids` controllers are available**. The resource-cap landings make all three
image assertions; the session leaf's CFS-only `nr_periods` row in `cpu.stat`
separately witnesses that bandwidth, rather than only CPU accounting, is
active. A first
draft of this rung probed `memory` in `/proc/cgroups` and would have
failed every boot; it was caught in review rather than in QEMU.

---

## A. Architecture and crate split

| crate | binary | deps | unsafe | role |
|---|---|---|---|---|
| `td-jail` | `/bin/td-jail`, plus one `argv[0]` symlink per application | none | surface **#9** | namespaces, mounts, capability drop, seccomp, PID-1 reaping, and resolving `argv[0]` to a spec |
| `td-busd` | `/bin/td-busd` | none | surface **#10** | the session D-Bus broker; SCM_RIGHTS forces the surface |
| `td-portal` | `td-portal -> td-compositor` | none | **none** (first landing) | a fourth `argv[0]` personality of the compositor multicall, in its own process (§E) |
| `td-audio` | `/bin/td-audio` | none | surface **#11** | the PulseAudio-protocol sound server (§K), running as its own `audio` uid |
| `td-login` | new `exec-as` subcommand (§A's Supervision — NOT an applet, so no `/bin/exec-as`) | none | no new syscalls | launch a literal argv as another uid without a shell |

There is **no application-manager binary**. With packages in the image
there is no install, no uid allocation in v1, and no immutable package input to
generate at run time — the builder compiles the spec at build time — so the
work that would have justified one is either gone or happens earlier.

### A.0 The launcher

**`/bin/firefox` is a symlink to `td-jail`, which reads its own `argv[0]`
and looks the name up in the immutable build-time application registry.**
`/etc/td-applications.tsv` maps the validated name to the exact
content-addressed package path; `td-init`,
`td-util`, `td-login` and `td-sh` are all multicalls keyed on `argv[0]`;
this is a fifth.

**`argv[0]` selects, it does not authenticate.** A caller controls
`argv[0]`, so the only thing it can do is name a *different shipped
application* — which that caller could have launched directly anyway. The
security property comes from the table being in the read-only image and
from the entry point resolving the spec itself.

**But this `argv[0]` is an OPEN key, unlike the four multicalls above**,
and the difference is the whole of the resolver's obligation. `td-init`,
`td-util`, `td-login` and `td-sh` dispatch on closed compiled-in name
sets, where an unknown name is an error. Here any name is a lookup key
into the store, so **the resolver re-validates it** — bounded character
set, bounded length, no path separator, no `..`, no leading dash — before
it is used to build a path. The identity was validated once at build
(§A); that is the wrong place to rely on, because the string arriving
here did not come from there.

**The `/bin/<name>` symlinks are a seventh applet farm and are now
registered as one.** `bin_farms()` enumerates the six closed farms plus this
open one, and
`applet_farms_are_disjoint_and_boot_names_stay_static` exists because a
name in two farms packs two conflicting symlinks for one applet.
Application names are a new, open-ended, recipe-authored class in the
same directory — authored by whoever packages an application — so they
are emitted through `real_root_steps` and added to `bin_farms()`, or the
collision guard goes stale for exactly the class of name a non-td author
picks. "A collision is a merge conflict rather than a security event"
(§A) was written about the application namespace alone and is not true of
its union with the applet farms.

**There is therefore no public `--spec <path>`.** Accepting a spec
pathname from an untrusted caller would let anyone hand `td-jail` a
handcrafted mount plan, which is the whole sandbox. Stage 1 resolves the
spec **by store path** from the table and reads it there; the stage-2
re-exec is internal, reached by a random synchronization token stage 1
sends over a descriptor, and is not a documented entry point. The token
detects a broken handoff; it is not launch authority, because the same
uid controls argv and can create its own namespaces. Stage 0's broker
registration supplies that authority before application launch is
enabled. No
per-instance copy of the spec is written and then re-read — that would
put a writable file back on the path this rule exists to keep immutable.

The registry names the package by its full path and the package's spec names
the runtime by its **full store path**, not by short
name, because td discovers closure edges by scanning output bytes for
store hashes (§B.8): a short name leaves the application→runtime edge
invisible to the closure query and to any future collector.

**Registration is the crate's own obligation, not a separate program's,
and it is split across the two stages the way §D describes.** Stage 0 opens
phase one — `{instance, app-id, permitted service names, owned bus names}`,
for an opaque one-shot token — before it unshares anything, because the pid
the record needs does not exist until after; stage 1 completes it with the
stage-2 pid it gets back from `Command::spawn`. §E rests `Unconfined` —
which grants full portal access — on the registry being complete by
construction, because nothing entered a jail without a registrar running
first. The registrar and the jail are now one binary and `/bin/td-jail` is a
documented entry point, so that premise has to be restated as an invariant
of the crate itself: **stage 1 refuses to proceed without the token stage 0
obtained**, and entering stage 1 without it is a refusal rather than an
unregistered jail. (An earlier draft called registration "stage 1's
obligation" flatly while §D and §E named stage 0, which are the two halves
of one protocol described as though they were rival answers.) The exposure
if it were not is bounded — an unregistered launcher is a uid-1000 process,
which is `Unconfined` anyway — but the argument that makes `Unconfined` a
positive result rather than a default is not.

**Not a shell script.** Directive 3 keeps shell out of these crates,
`/bin/sh` is td-sh, and a launcher composes the argv of the process the
whole sandbox is about — a quoting bug there is a confinement bug. The
symlink form is less code than a script, not more.

**What it launches is generated at build time.** The recipe declares identity,
runtime, entry, environment and typed permission defaults; the builder resolves
the runtime and compiles those values into `spec` beside the payload. The spec
`td-jail` parses is therefore trusted, immutable input. Machine-specific grant
sources and mount targets are still resolved at launch, but the package cannot
author a replacement plan.

**Nothing else moves in.** In particular `td-jail` never extracts an
archive: on a host (§X) the package is materialized by `td-builder` in
the control plane, which already has that code. A NAR reader inside the
crate that holds surface #9 would be exactly the parser-and-filesystem
bookkeeping this split exists to keep out.

**`td-jail` is a separate binary from anything that manages packages**
because `unsafe` belongs in the smallest binary that can hold it: the
sandbox is the security boundary and the thing an auditor must read
whole. That argument survives the manager's disappearance — there is now
simply nothing else to separate it *from*, and the resolver that joined
it is a few dozen lines over immutable input.

### Supervision

`td-svc` has no `user=` field; the existing units spell the credential
switch as `exec=/bin/su -s /bin/sh {user} -c '…'`. Rather than write more
shell (directive 3), add a **`td-login exec-as USER -- PROGRAM ARGS`**
applet using the credential syscalls and readback that crate already has,
and give the new units a literal argv:

```ini
[busd]
type=daemon
exec=/bin/td-login exec-as tester -- /bin/td-busd run --socket /run/user/1000/bus
after=seat
ready=/bin/td-login exec-as tester -- /bin/td-busd probe /run/user/1000/bus
ready-timeout=30
restart=always

[portal]
type=daemon
exec=/bin/td-login exec-as tester -- /bin/td-portal run \
     --bus /run/user/1000/bus --wayland /run/user/1000/td-portal-wayland-0
after=busd,compositor
ready=/bin/td-login exec-as tester -- /bin/td-portal probe /run/user/1000/td-portal.ready
ready-timeout=30
restart=always
```

`busd` needs only `/run/user/1000` (td-seatd's product); `portal` needs
both the bus and the compositor. Both are `restart=always`: a crashed
broker must return before the next portal call, and td-svc's backoff
bounds the loop.

**The `busd` unit has LANDED**, with two things this sketch did not
settle. It `requires=seat` as well as ordering after it, and not for the
reason a first draft gave: there is no race. td-svc will not start a
unit until every `after=` dependency has SETTLED, so `after=seat` alone
already keeps the broker behind td-seatd. What `requires=` adds is that
a FAILED seat settles too — with ordering alone the broker is released
onto a session that does not exist, binds a socket in a
`/run/user/1000` td-seatd never made, and prints a healthy marker on a
machine with no seat, no compositor and no way to run anything. The
marker would then mean less than it appears to, which is the one thing
this design is careful about everywhere else. (`requires=` supplies the
ordering edge by itself; the `after=` is kept because the declared edge
set is pinned by a test and reads better stated than inferred.) And the
boot asserts the outcome rather than trusting the unit: `/etc/bootsuccess`
probes the RUNNING broker in its health farm and prints a marker the
image oracle requires — §D below. `[portal]` is not landed: td-portal
does not exist yet.

**`exec-as` has LANDED** (rung 2), with three details this sketch did not
settle. It is a SUBCOMMAND rather than an applet — `td-login exec-as`,
never a `/bin/exec-as` symlink — for the reason `verify-credentials` is
one: the units above spell it out in full and never invoke it by
basename, so a symlink would put an unaccounted name in `/bin`, and this
particular name is one a reader could mistake for a general-purpose "run
this as anyone". The `--` is REQUIRED, which is the whole parser: with no
options of its own, a mandatory separator removes the only ambiguity
available and keeps a later option from colliding with an argument that
already works. And the environment is EMPTIED — not merely `su -`'s
fresh one, which keeps `TERM`, but the five identity variables and
nothing else. A supervised daemon's environment should be a property of
its unit and td-svc has no `env=` key to make it one, so anything
carried across makes what a daemon sees a function of the boot path;
`TERM` is the case that proves the rule rather than an exception to it,
since a process with no terminal has no use for one and its value
changes program output between boots. That is why the units above pass
every path as an explicit argument, and adding a variable is a
unit-level key rather than a flag on `exec-as`.

**No setuid helper and no new root daemon of its own.** One dependency is
not this design's: `td-authd` (§L.1), for elevation and, in v2, for the
identity transition per-app uids need. Root on this image is ordinary
rather than exceptional — `build_td_svc_conf` already runs seven root
oneshots and two root daemons — so the claim worth making is the narrow
one: **nothing here is more privileged than its caller**, no setuid
binary exists, and the runtime path is unprivileged end to end.

### Process tree

```
td-term ─ /bin/sh                          uid 1000, host pid ns
└─ /bin/firefox -> td-jail                 waits in the caller's session;
   └─ td-jail [stage 1]                    parent-death bound; detached from
      │                                    an ambient terminal, or kept in a
      │                                    proved no-terminal supervisor group,
      │                                    before argv[0] resolves
      │                                    the store spec (§A.0), writes the
      │                                    instance dir + /.flatpak-info and
      │                                    registers with td-busd;
      │                                    single-threaded;
      │                                    unshare(NEWUSER|NEWNS|NEWPID|
      │                                    NEWUTS[|NEWIPC][|NEWNET]);
      │                                    setgroups=deny, then uid_map
      │                                    and gid_map "1000 1000 1";
      │                                    prepare root + loopback; carry
      │                                    only ambient CAP_SYS_ADMIN;
      │                                    drop/read bounding set
      └─ td-jail [stage 2] <token>         PID 1 of the new pid ns;
         │                                 exact caps + CapBnd=0; pivot;
         │                                 fresh proc + dev/tmp; drop/read caps;
         │                                 NO_NEW_PRIVS, seccomp,
         │                                 readback, then spawn and reap
         └─ firefox                        PID 2; interpreter is the
            └─ content processes           RUNTIME's ld.so
```

**The launch parent closes the inherited-terminal boundary without escaping an
existing supervisor boundary.** `setsid(2)` fails for a process-group leader,
so the process selected by `/bin/<name>` cannot safely issue it itself. It
spawns a child that cannot yet be that leader, passes its own pid, and waits.
The child first sets `PR_SET_PDEATHSIG=SIGKILL`, proves that exact parent through
procfs, then reads its containment before it resolves authority or creates
application state. A child already in a no-terminal process group led by that
exact parent stays there and reads the group back; this is the dedicated group
td-svc records and later drains. Every other child creates and reads back a new
session with no controlling terminal. Killing the waiting launcher therefore
kills stage 1; the existing proof pipe then kills stage 2, and the independent
cgroup watcher drains the leaf. A containment snapshot predating the launch
cannot name its later-born child. A console snapshot in the brief
pre-detachment window can later kill stage 1, but it cannot name the still
later cleanup watcher, so the same parent-death and leaf-drain path holds.

**The two-stage re-exec is the answer to `pre_exec`.** A target crate has
one scoped `unsafe` allow, in `sys.rs`, and `CommandExt::pre_exec` is
itself an `unsafe fn` — calling it from the module that spawns would need
a *second* allow, which every `UNSAFE.md` confinement test counts and
refuses. So the process boundary is the fork instead: stage 1 unshares in
its own still-single-threaded `main` (`CLONE_NEWUSER` requires a
single-threaded process, so stage 1 spawns no thread before it), and the
first child it spawns through safe `Command` lands as PID 1 of the new
pid namespace with the prepared mount namespace inherited. Rung 8 proves
the default nonzero-identity exec strips every effective capability.
Rung 9 deliberately replaces that default for the real jail: stage 1
puts only `CAP_SYS_ADMIN` in the inheritable and ambient sets, drops and
reads back every bounding-set bit while `CAP_SETPCAP` is still effective,
then execs. Stage 2 reads back that exact one-capability state plus an
empty bounding set, mounts the procfs for the PID namespace it actually
inhabits, pivots, and detaches the old root. That order is load-bearing:
an unprivileged user namespace may mount procfs only while another procfs
is fully visible in its mount tree (`mount_too_revealing`), so mounting
after detaching the old root fails `EPERM`. Rung 10 clears and reads back
ambient, effective, permitted and inheritable, then exercises `wait4(-1)`
with a zero-capability internal child that leaves an orphan for PID 1.
`NO_NEW_PRIVS`, seccomp, application spawn, and survivor termination
remain later steps. Application launch stays disabled across all three
intermediate states.

**Stage 2 does not exec the app over itself.** A PID 1 that is Firefox
reaps only its own children, and orphaned grandchildren pile up as
zombies — so stage 2 stays resident as an init: spawn, `wait4(-1)` until
the app exits, preserve the status, give survivors two seconds after
namespace-wide `SIGTERM`, then force and reap them for at most two more
seconds with repeated namespace-wide `SIGKILL`. `wait4(2)` and `kill(2)`
are on surface #9 for it; PID-namespace teardown is the final hard stop.
The four seconds bound PID 1's userspace polling, not a task's exit from an
uninterruptible kernel sleep. An entry that backgrounds work and exits does
not transfer lifecycle ownership: those descendants enter survivor cleanup.

A draft called that "~40 lines" and used the number to argue the crate
holding surface #9 stays auditable. It does not survive §D: stage 2 also
opens the per-instance activation listener, validates a requested name
against the predeclared set, and spawns a literal `Exec=` argv — an
accept loop, a validator and a spawner. §P adds the cgroup move and §A
the watcher thread. Call it what it is: **a small supervisor, and the
second-most security-relevant loop in the crate after the mount plan.**
The listener's socket has to be created BEFORE step 15, in a directory
the broker can name and the jail does not mount, since `pivot_root` plus
`MNT_DETACH` leaves stage 2 unable to reach anything it did not already
hold — a descriptor, again, rather than a path.

**`PR_SET_PDEATHSIG` needs a liveness check that works in a new PID
namespace**, and the two usual ones do not: stage 2 is PID 1 of a fresh
namespace, so its parent is invisible and `getppid(2)` and
`/proc/self/stat`'s fourth field both return 0 whether or not stage 1
lives. The mechanism that works needs no syscall: **stage 1 holds the
write end of a pipe** and stage 2 the read end. If stage 1 dies the write
end closes and stage 2's read returns EOF; if stage 1 lives the read
blocks. Stage 1 maps the read end onto stage 2's stdin with `Stdio`; an
ordinary `Command` performs an exec and the anonymous pipe is
close-on-exec by default, so merely leaving its original descriptor open
would lose the proof. Stage 2 consumes the fixed-size proof before stdin is
replaced by the application's declared stream in the mount-plan rung.

**The read is on a WATCHER THREAD, and that is what makes it a check
rather than a deadlock** — a draft had stage 2 read inline to close the
`PR_SET_PDEATHSIG` race, which in the healthy case is a process that
blocks forever having launched nothing. A thread blocked on that read
answers the question the right way round: it does not ask "is my parent
alive", it waits to be told it is not, and tears the instance down when
the answer arrives. The race closes for free, since a parent that died
before the read makes EOF immediate rather than late. The thread is
created AFTER step 18 installs the filter, so it inherits it — a thread
made before would need `SECCOMP_FILTER_FLAG_TSYNC`, which is a flag to
get wrong for no gain — and after step 19's readback, so nothing runs
under an unverified filter. It is also why stage 1 must be the
single-threaded one: `CLONE_NEWUSER` refuses a multi-threaded caller,
and this thread lives on the other side of the unshare.

That pipe runs INWARD only, which an earlier draft got wrong by saying it
"doubles as the channel stage 1 uses to report stage 2's pid outward": a
pipe whose read end is held by stage 2 cannot carry anything to the
broker. Nothing needs it to. **Stage 1 completes the registration on its
own broker connection** (§D), with the pid `Command::spawn` just handed
it — and that pid is already expressed in the broker's namespace, because
`unshare(CLONE_NEWPID)` does not move the CALLER: stage 1 stays in the
outer pid namespace and only its first child lands as PID 1 of the new
one. That is why stage 2 is the child rather than stage 1 re-execing
itself, and it is worth stating because the natural reading — that a
process which unshared a pid namespace is in it — makes the reported pid
look unusable when it is exactly right.


### Application identity — short names, not reverse DNS

**An application's td identity is a short flat name: `firefox`,
`darktable`.** Not `org.mozilla.firefox`.

Reverse-DNS identifiers are a collision-avoidance convention for a
*decentralized* namespace — they are not an authentication mechanism, and
nothing verifies that whoever published `org.mozilla.firefox` controls
`mozilla.org`. td's set is curated: a reviewed list in this repository,
every entry a recipe, and a collision is a merge conflict. Paying
reverse-DNS's cost in every path, argv, directory name and diagnostic
buys nothing back.

The name is the identity everywhere td owns the namespace: the launcher
symlink, the store path, the per-app uid allocation, the permission file,
the state directory, the D-Bus `td.AppId` credential, the cgroup, and the
launcher table. Its build-time language has **LANDED**: 1–32 ASCII bytes,
each an alphanumeric, `.`, `_` or `-`, with no leading dash and no `..`.
The single name `.` is also refused. That is the `td-login` account language
narrowed for an open path key, and it excludes path separators without a
second rule. Runtime package names use the same language. The later launcher
resolver still re-validates its caller-controlled `argv[0]`; build-time
validation is not authentication.

**Foreign applications still announce reverse DNS on wires td does not
own**, so each manifest records at most one such string as its optional
**alias**, used only where a foreign protocol demands one: matching a toplevel
to its launcher
entry, scoping bus-name policy, resolving a `.desktop` reference. The
alias never becomes the identity, and an application with no alias is
ordinary rather than special.
The alias names the package on a foreign application protocol; additional
well-known bus names are permissions, not package identities.

`/.flatpak-info` and `FLATPAK_ID` keep their upstream spelling for one
reason: **the application reads them.** GIO decides whether to route
through portals by looking for that exact path, so a repackaged binary
that does not find it will try to open files directly and fail
confusingly. It is a compatibility surface td writes, not a name td uses.

### Launcher integration

**The image-side contract has LANDED.** Each application recipe supplies a
typed display name and bounded list of search terms. The outer builder binds
the final recipe identity and writes one canonical
`exports/launcher.tsv` row as builder-authenticated metadata:

```
name<TAB>display-name<TAB>space-separated-search-terms
```

Names keep §A's 32-byte application grammar. A display name is 1–128 UTF-8
bytes with no control character or edge whitespace. There are at most 32
distinct search terms, each 1–64 UTF-8 bytes with no control or whitespace.
An export is at most 4 KiB. Unknown JSON fields, duplicate keys and every
noncanonical TSV form are refused.

The native `compileApplicationTables` recipe step takes a literal expected
application identity plus package and paired runtime paths named through its
typed data channel. The expected identity is separate from the catalog key and
must equal the builder-authenticated recipe name. The compiler refuses symlinks
in each metadata path,
parses the canonical manifest, spec and export, and requires all three
identities to agree. The package itself and the exact runtime named by its spec
must both be declared payloads, and the spec's exact runtime must equal the
runtime paired with that package. It then emits two sorted, duplicate-free
immutable files:

- `/etc/td-launcher.tsv` keeps the three columns above for the compositor;
- `/etc/td-applications.tsv` carries `name<TAB>exact-package-store-path` for
  `td-jail`'s resolver.

Each table admits at most 256 applications and 1 MiB. Keeping the path out of
the presentation table gives each reader only its own input and closes the
previous design gap where a three-column row gave `td-jail` no way to locate a
content-addressed package. The image now selects the first jailed fixture into
both tables. Activation is always the literal argv `/bin/<name>`.

An image selection names the authenticated application identity plus its
package catalog key and runtime catalog key. The latter two are data inputs.
`StageRuntimeClosure` follows the exact runtime path embedded in the
spec and refuses it unless that runtime was declared; shared runtime roots are
deduplicated. Only application package roots feed `compileApplicationTables`.

Launcher names are checked at build against every `/bin` provider, including
the six closed applet farms and direct links such as `rg` and `td-netd`.
`applet_farms_are_disjoint_and_boot_names_stay_static` includes the open
application farm and refuses a second provider of a name, and a foreign
package must not be able to claim `sh`.

**The compositor never executes an upstream `Desktop Entry` `Exec=`
line.** Field codes are parsed only to learn file-forwarding intent;
shell syntax, command substitution, environment assignments and arbitrary
executable paths are refused. A `.desktop` file is foreign data in zone
three, and treating it as a command line would hand zone three an argv in
zone one.


## B. Packages

### B.1 Where packages live — `/td/store`, shipped with the image

Application packages are recipe outputs in `/td/store`, marked as
sandboxed applications (§B.8), delivered by the deployment. Only writable
state lives in `~/.td/app` (§B.4).

The reason is delivery, and it is worth one paragraph because it is the
constraint a later reader will try to relax. `/td/store` is inside
`root.erofs` — a read-only image staged at build time — so nothing can be
added to it on a running machine. That argues for a separate writable
tier, and a tier needs packages *delivered* to it: `system-x86-64.rs`
ships no `td-builder` and no toolchain, so a booted machine cannot build
one; §B.6 refuses a target-side fetcher; §Z refuses anything td would
have to serve. The deployment bundle is the only delivery td has
(§W.2), so packages ride it.

**The costs are real and are accepted rather than mitigated:**

- an application update is a whole-system deployment, so a browser point
  release ships the kernel and userland and spends a reboot;
- rolling an application back rolls the system back with it, which can
  undo an unrelated security fix;
- the boot-attempt counter does **not** cover application execution: the
  confined fixture is independent QEMU evidence, so broken application code
  or mutable application state cannot roll back the whole deployment;
- every account on the machine gets the same set of applications, and
  two accounts can no longer disagree about a browser version;
- the image carries every packaged application and runtime.

**Three size constants bound this today, and large applications still owe a
reviewed capacity landing**:

| constant | today | capacity consequence |
|---|---|---|
| `td-boot/src/protocol.rs`'s `MIN_VOLUME_BYTES` | 5 GiB | three deployment-sized copies are transiently live during publish. The profiling policy budgets one GiB of debug companions in each copy, one GiB for all three copies' non-debug payloads, and one GiB for Btrfs metadata plus `@var`. `td-install` enforces this as an admission floor, not a promise that every deployment fits; a larger update can still fail with `ENOSPC` while staging, before selectors change |
| the QEMU oracle's `PERSISTENT_VOLUME_BYTES` / headroom | 5 GiB / 1 GiB, `copies = 3` | permits four GiB of aggregate fixture payload across the three transactional copies. Three ceiling-sized debug trees consume three GiB of that, leaving one GiB total for their boot payloads (roughly 341 MiB per deployment). §H's small fixture fits, but a large browser can still exceed the oracle and therefore still needs an explicit capacity landing |
| `ESP_BYTES` | 512 MiB | **not** a problem — the ESP carries kernel and initramfs only. Named here because a reader would otherwise assume it is the partition that grows |

The disk cost is therefore **2× the browser permanently and 3× across an
update** — `current` and `previous` are what a machine retains, and the
third exists only between installing a new deployment and retiring the
old one. A draft said "3×, permanently" and that overstated the steady
state while understating nothing: it is the transient peak that has to
fit, so `MIN_VOLUME_BYTES` is still sized for three. Both numbers belong
beside the reboot in any honest accounting of this decision.

Updating one package without a deployment is the intended sequel, and
§W.2 specifies it. What that needs is a writable package root and a
pointer; what it does *not* have yet is the authority to write a system
location, which is `td-authd`'s (§L.1) and is the honest gap.

**What store placement buys back**, beside deleting the delivery
problem: a package is root-owned inside a read-only image admitted by a
signed manifest, so it is not writable by the uid that runs it; the tier
has the store's hashing and its collector rather than needing its own;
and a runtime is shared by every account instead of copied per home.

### B.2 What a package is

```
/td/store/<hash>-firefox-141.0/
  manifest            the td-owned declaration (below)
  spec                the builder-compiled jail input (§A.0)
  files/              becomes /app inside the jail
  exports/            launcher entry, icons, mime associations
```

The first seed is still deliberately not shipped or launchable: rung 7 has now
added its authenticated launcher export and the empty image tables, but the jail
does not exist yet. Putting it in the image before that boundary lands would
make an ordinary unconfined package path look like an application path.

**`manifest` and `spec` are two files with two jobs**, and keeping them
apart matters because they have different readers. The manifest is the
*declaration* — what this package is, written for a human and for the
recipe checks. The spec is the immutable compiler output derived from it at
build time — effective environment, typed grants/defaults, the entry point,
and the runtime's full store path. It is the only package metadata `td-jail`
parses; the jail resolves grants into the machine-specific mount plan later.

The manifest is td's own format — a small keyfile in the shape
`td-svc.conf` already uses, so the distribution has one config grammar
rather than two:

```
name=firefox                    the identity (§A)
version=141.0
alias=org.mozilla.firefox       what it calls itself on foreign wires
runtime=freedesktop-sdk-24.08   the runtime package it needs
entry=/app/bin/firefox
provenance=foreign              or `source`
```

**That format and its recipe-side generator have LANDED.** The file is at
most 16 KiB, UTF-8 with a trailing LF, and contains no CR or NUL. Blank
lines, `#` comment lines and layout whitespace around keys and values are
accepted and canonicalized away. The six declaration keys above are the
whole root vocabulary: `name`, `version`, `runtime`, `entry` and
`provenance` are required exactly once, `alias` is optional, and an unknown
or duplicate key is a refusal. Provenance has exactly the two spellings
shown. An alias has at least three non-empty reverse-DNS components; the
entry is an absolute child of `/app/`, with no empty, `.` or `..` component.

The manifest's only section is optional `[Environment]`. Its keys are bounded POSIX
environment names, its values are bounded single-line strings, duplicates
are refused, and at most 128 entries are admitted. The canonical writer
sorts them, so recipe construction order cannot change a package hash.
Those entries are immutable package content, not process environment. The
landed spec compiler refuses every `LD_*` loader-control name, including
`LD_PRELOAD`, `LD_AUDIT` and `LD_LIBRARY_PATH`, and constructs the jailed
environment from its own fixed base before the remaining entries apply. The
manifest validator already reserves td's own `TD_*` namespace.
`td_recipe::application::ApplicationDeclaration` carries only the authored
runtime, entry, optional alias and environment. `Recipe::application` attaches
that declaration to the package; it has no fields for identity, version or
provenance. The derivation assembler takes those three answers from the final
recipe JSON and **derives** provenance from its source-pin mark, then renders
the canonical manifest into the outer build contract; it is not passed into the
package PID namespace. An application declaration that contains a direct
`payload_inputs` edge is foreign too: containment deliberately does not taint an
image recipe, but it cannot make the contained application's own manifest claim
`source`. This provenance is deliberately an answer about the recipe's direct
staging, not its transitive runtime closure; the landed store-level closure
query answers that separate question.

`Recipe::application_launcher` separately carries the typed display name and
search terms. The derivation assembler binds the final recipe name into its TSV
row. Keeping presentation out of the manifest preserves the manifest's package
identity job, while making `exports/launcher.tsv` builder-authenticated rather
than a command-authored claim from a foreign package.

**The canonical compiled spec has LANDED.** It is a builder-owned keyfile of at
most 48 KiB, not an authored compatibility grammar:

```ini
format=1
name=ripgrep-seed
runtime=/td/store/<hash>-empty-runtime-1
entry=/app/bin/rg

[Environment]
HOME=/home/td
PATH=/app/bin:/usr/bin
...
```

The permission policy sections follow in the same file without a second
`format` key. `Recipe::application_permissions` supplies typed immutable
defaults; an application without that policy, or a policy without an
application, is refused. Runtime resolution requires one matching
`payload_inputs` entry, one typed `td-recipe-output` lock row, and one direct
canonical child of the active `/td/store`. The compiler currently has exactly
one reviewed runtime-major environment policy, `empty-runtime`; another runtime
is refused until its policy is added deliberately. The parser accepts only the
compiler's canonical bytes, including the full `/td/store` runtime path.

After PID-namespace teardown prevents any package descendant from running
further code, the outer sandbox parent recognizes the four application-capable
runners and materializes the manifest, spec and `exports/launcher.tsv` as mode
0644 metadata. It creates `exports/` as mode 0755 without following a symlink.
None of the three values is passed into the package PID namespace. The sealed
`stage0` seed and downloaded `rust-stage0` trust root refuse application
metadata at assembly; their outer post-build verifier only reserves the root
names and can never write metadata.
This is not a `Step`: applications built with GNU, CMake or Cargo get the same
declaration as a mesboot recipe, and there is no second generic file writer that
can evade step-level guards. The fixed writer decodes and re-parses all three
values, requires canonical bytes and equal identities, refuses to replace any
reserved path, and writes content verbatim, so an environment
value that resembles a build token cannot turn into a store path. Keyfile
parsing and derivation assembly enforce the aggregate byte bound; recipe JSON
shares the typed, per-entry and entry-count validators, and assembly refuses an
oversized declaration before it reaches the derivation.
The root `manifest` and `spec` names plus nested `exports/launcher.tsv` are
reserved in every output built by the six standard recipe phase runners: an
application declaration creates them only in `out`, and their absence
everywhere else requires that the package build did not create one. Builder ABI
4 invalidates realizations from before the launcher reservation existed.
Presence is therefore authenticated by the hashed derivation contract rather
than by self-asserted package bytes.

This is deliberately **not** a generic parser shared with the permission
file. The manifest is immutable package content; the permission file is a
separate policy language with filesystem, bus, socket, device and resource
semantics. Unknown policy keys are not admitted through a reusable string map.

Permissions are a **separate file**, deliberately not part of the
manifest, because they have a different lifecycle: the manifest is
content and changes only when the package does, while a permission grant
is a decision an operator revisits without rebuilding anything.

**The typed permission keyfile has LANDED.** `td_engine::permissions` owns its
parser, canonical writer and construction API. Version 1 remains accepted;
version 2 adds the CPU bandwidth key shown here:

```ini
format=2

[Context]
shared=network
sockets=wayland;pulseaudio
features=allow-devel

[Filesystem]
/mnt/archive=ro
xdg-download=rw:create
xdg-pictures=deny
~/Projects=rw:create

[Session Bus Policy]
org.freedesktop.FileManager1=talk
org.mozilla.firefox=own

[Resources]
memory-high=1073741824
memory-max=1342177280
pids-max=1024
cpu-max=100000 100000
```

The authored file and its canonical rendering are each at most 16 KiB, UTF-8
with a trailing LF and no CR or NUL. `format=1` or `format=2` is required
before the first section. Version 2 requires `cpu-max`; version 1 cannot spell
it. Full-line `#` comments, blank lines and ASCII space/tab layout are
accepted and canonicalized away; sections, keys and list members may not
repeat, unknown intent is refused, and the writer fixes section/key/list order.
Empty sections disappear. A compact boundary input whose normalized rendering
would cross the same cap is refused rather than producing an oversized policy.

`shared` admits only `network`; `sockets` admits `wayland` and `pulseaudio`;
and `features` admits only `allow-devel`. `devices=dri` is recognized but
refused until §M's hardware-rendering policy lands, while `devices=tty` names
the missing fresh-terminal acquisition policy. This makes both future changes
a policy-table decision rather than an unknown key becoming active by accident.

Filesystem keys are exactly the six `xdg-*` names §C lists, a lexical `~/`
subpath of the launching user's real home, or an absolute path. `~/` never
names the private `/home/td` assembled inside the jail. Their value is `deny`,
`ro`, `rw`, `ro:create` or `rw:create`; `create` is confined to XDG and
home-relative locations, and the keyfile delimiter `=` is not a path character
in this language. Blanket `host`/`home`, the Flatpak repositories,
`/.flatpak-info`, and the
`/app`, `/usr`, `/bin`, `/run`, `/proc`, `/sys`, `/dev`, `/tmp`, `/home`,
`/root`, `/var/home`, `/var/root`, `/var/run`, `/var/tmp`, `/etc`, `/boot`, and
td's `/var/lib/td` system state are refused here. The refusal includes a
recursive grant above one of those fixed trees — `/var/lib` cannot smuggle the
system Flatpak repository or td system state in, and `~/.local` cannot smuggle
the per-user repository in.
The package and application-state roots remain configuration, not paths baked
into this context-free parser. The rung-6 spec preserves these typed grants;
rung 9's immutable-base mount plan accepts no caller paths. Rung 12b now
resolves the builder-authenticated immutable defaults from the spec: `~/`, XDG
names and absolute sources are interpreted against the current configuration,
then every alias or overlap with reserved roots is refused before file-type
checks, mount-target separation and deny-wins merging. A mutable per-user
override file is not launch authority yet; its exact path, ownership and editor
lifecycle remain a separate landing. The format does not pretend a lexical
parser performed those filesystem operations.

Session-bus keys are exact well-known names, never unique names or wildcards,
and their values are the ordered capabilities `see`, `talk` and `own` — an
`own` entry confers the two below it, which is what `BusAccess::allows`
states and what the broker implements. Applications cannot own the broker,
the reserved `org.freedesktop.portal.*` and `org.freedesktop.impl.portal.*`
names, or the two bare namespace roots. Resource values are bounded positive
decimal integers. Memory is a byte count capped at
9,223,372,036,854,767,616, the largest 4096-byte-aligned value below the first
value the pinned kernel rounds to its unlimited page-counter sentinel, and
`pids-max` is a task/TID count capped at the kernel's 4,194,304-task limit.
`cpu-max` is a quota and period in microseconds, separated by one space. The
quota is 1,000 through 17,592,186,044,415 and the period is 1,000 through
1,000,000, matching the pinned kernel's CFS bandwidth bounds. The format has
no literal `max` spelling; its upper bound is kernel representability, not a
practical CPU allotment. `memory-high` must be below `memory-max` when both
appear.
The spec compiler embeds the immutable authored policy. At launch, omission
selects the reviewed non-unlimited baseline: `memory-high=1073741824`,
`memory-max=1342177280`, `pids-max=1024`, and `cpu-max=100000 100000` — one
fair-scheduler CPU. An explicit format-1 resource section is atomic over its
three keys and inherits that CPU baseline; format 2 requires those three plus
`cpu-max`. Both memory values must be aligned to the target's 4096-byte page
size so kernel readback is exact rather than a silently rounded value. The
compiled application spec stays format 1 and embeds the permission sections
without a second root key; the presence of `cpu-max` selects the embedded
version-2 permission grammar. Mutable operator overrides remain a later
lifecycle landing.

**The runtime/application split is the one piece of flatpak's
architecture worth adopting wholesale.** A runtime is just another
package, shared by reference between every application that names it, and
— more valuable — it gives an application a *stable ABI target* that is
not td's evolving userland. td's own libraries move when td decides; a
foreign binary compiled against a particular glibc, GTK and ICU needs
those exact ones to keep existing. Without the split every application
would pin its whole world separately.


### B.3 The seed path — how a foreign build becomes a package

This is what removes the network from the target. It happens entirely in
the control plane, and the target never speaks a repository protocol.

**A seed is pinned from what upstream already publishes, never from
something td assembled.** That rule is load-bearing rather than
stylistic: a tree somebody assembled locally has no URL anyone else can
fetch, so pinning one would require td to host it, which §Z refuses.

1. **Pin what upstream publishes** as a plain file at a stable URL, in
   `recipes/src/source_pins.rs` — a URL, a `sha256`, a filename. The
   mechanism the kernel tarball and Rust bootstrap snapshot already use,
   fetched by `td-net`, verified the same way. The pin carries the
   `foreign` taint (§B.8).
2. **A recipe transforms it** into a package: unpack, lay out `files/`, and
   declare the manifest, launcher presentation and typed permission defaults.
   The builder now emits the manifest, compiled full-runtime-path spec and
   launcher export only after the package PID namespace is gone. All three are
   deterministic derivation work, so the artifact is a build output rather
   than a thing needing distribution.
3. **The recipe's checks are ordinary recipe checks**: the entry point
   exists and is executable, the declared runtime resolves, the metadata
   normalization of §B.8 holds, and the entry is either proved fully static or
   its interpreter and needed-library assertions are made against the runtime.

**The first seed and its spec have LANDED.** `ripgrep-seed` packages upstream's pinned x86-64
musl release without executing it and names the td-built `empty-runtime` by
the payload-only channel. The native validation step resolves that runtime only
from `TD_PAYLOAD_MAP`, checks that the exact `/app/bin/rg` entry is an x86-64
world-executable `ET_EXEC` or static PIE whose entry point is in an executable
load segment,
proves it has no interpreter, needed library or run path, and rejects special
file types, special mode bits and every symlink. It also requires every
root-owned package directory to be world-traversable by the uid-1000 application.
Every foreign-source application requires exactly one check whose entry and
runtime are bound to the builder-authenticated manifest. Assembly admits only
native unpack/mkdir/copy steps before that terminal validation, so package code
cannot race or mutate the approved tree. The native archive reader and copy step
do not materialize device entries or xattrs, and the copy step refuses a symlink
in any source-path component or any special-file source rather than
dereferencing it; final validation rejects
setuid, setgid and sticky mode bits. Its empty typed permission policy and the
`empty-runtime` environment table compile into `spec`; its recipe check also
runs the registered-store closure proof described below.

This is the cheapest complete exercise of the seed/trust path. It did not
answer the graphical-runtime question; the package-only Firefox experiment
below now does.

#### B.3.1 The Firefox deploy-tree proof

The selected binary-distribution route is a bounded, control-plane-only
Flatpak deploy importer. It is not a Flatpak client on the target, and it is
not permission compatibility. The importer converts one reviewed application
commit and one reviewed runtime commit into the same td package/runtime shape
every other application uses.

The package-only experiment pulled Flathub into an isolated scratch
installation with host Guix's Flatpak 1.16.6, deliberately omitting every
related extension, including the locale and graphics extensions. It did not
execute Firefox and did not touch the system image. Mutable refs were used only
to discover the current commits; these are the immutable objects that were
inspected:

| role | ref | commit | deployed/download size |
|---|---|---|---|
| application | `app/org.mozilla.firefox/x86_64/stable` | `86ba63a1c2378a9525b495e1ba2c3ed9dc71ee92f67e45d8016cc4972024b410` | 333.8 MB / 125.6 MB |
| runtime | `runtime/org.freedesktop.Platform/x86_64/25.08` | `bd44a6230581917d04f89812a4c21090c304d390edb73995af1c2f9fd8abf4e8` | 659.9 MB / 257.1 MB |

Both commits have a good Flathub signature rooted at fingerprint
`6E5C 05D9 79C7 6DAF 93C0 8135 4184 DD4D 907A 7CAE`. Their OSTree content
checksums are respectively
`e511b540f42135f8703d6ea0f65abe3b798f93d4ab73ad27bf272d372a72fac3`
and
`e8c3f71b355e2248fba4e04492de33242355ddd4b552f809ea06292859200c72`.
Flathub marks `org.mozilla.firefox` manually verified and publishes a manifest
link into Mozilla's mutable `mozilla-central` tree. That link does not
authenticate this exact build. The stronger build-specific evidence is in the
signed deploy: its `application.ini` says Firefox 154.0, build ID
`20260812182057`, source repository
`https://hg.mozilla.org/releases/mozilla-release`, and source stamp
`9ce1ee6baeb9a3c326dbd180bdece65d8fc2eadc`, the
`FIREFOX_154_0_RELEASE` changeset.

This answers E1's layout question. The application deploy's `files/` tree is
the hierarchy td mounts at `/app`; the platform deploy's `files/` tree is the
hierarchy td mounts at `/usr`. The application tree had 480 children: 184
directories, 151 regular files and 145 symlinks. The runtime tree had 18,201
children: 1,947 directories, 13,744 regular files and 2,510 symlinks. Neither
tree contained a device, FIFO or socket node, and neither contained a setuid or
setgid regular file. The two base refs total 993.7 MB deployed, versus the
approximately 1.4 GB uncompressed closure of the host Guix `mpv` package. Their
separate compressed-transfer figure is 382.7 MB; it is not compared with the
uncompressed Guix closure. Both figures exclude every related extension. The
659.9 MB deployed runtime, whose transfer is 257.1 MB, is shared by every
application on the same runtime major.

The proof is deliberately not called a td package landing. Flathub publishes
an OSTree repository rather than a stable deploy tarball, and invoking the
caller's `flatpak` from a recipe would make the derivation depend on ambient
programs, remote configuration and a mutable ref. A locally exported tarball
would instead make td its distributor. Neither is admissible. The repository
workstream must implement and review the importer before these bytes can be a
recipe input.

The importer contract is narrower than Flatpak:

1. The input names an architecture, exact app/runtime refs and exact commit
   hashes. A branch name alone is a refusal. Pin review records the signing-key
   fingerprint and independently verifies the signatures; the fixed commit
   and every fetched object hash then preserve that decision during builds.
2. Only the two commit-reachable `files/` trees are materialized. Summary
   browsing, updates, installation state, exports, permissions and activation
   metadata are not imported. The target has no OSTree or Flatpak code.
3. The object count, aggregate decoded bytes, path count, path length and file
   size are bounded before publication. Hash, type, mode or tree-reference
   mismatches fail closed, and a temporary tree is renamed into place only
   after complete validation.
4. Device nodes, FIFOs, sockets, setid/sticky bits and file capabilities are
   refused. So are all xattrs and ACLs, rather than copying only the ones the
   importer understands. Every node is materialized as root-owned, timestamps
   are normalized to a fixed value, and directory and regular-file modes are
   canonicalized. Symlinks are retained only when lexical resolution stays in
   the assembled `/app` or `/usr` trees or names an explicitly synthesized
   jail path. The current application has 102 absolute locale links under
   `/app`; selecting or omitting the locale extension must be explicit rather
   than leaving an accidental set of dangling links.
5. td generates the manifest, spec and launcher from reviewed typed data after
   import. Flatpak's permissions are evidence for that review, never launch
   authority. Unknown or broader permissions are refused rather than copied
   as strings.
6. A dynamic-package validator binds the entry, application-private DSOs and
   runtime by the builder-authenticated declaration. It accepts a bounded
   shebang only when its interpreter resolves in the selected runtime, walks
   every ELF interpreter/needed-library edge without consulting the host, and
   refuses an unresolved edge or an ambient path. No imported executable runs
   during conversion or checking.
7. A commit carrying `xa.extra-data-sources`, or any other declaration that
   obtains payload bytes from outside the signed commit-reachable object graph,
   is refused. The inspected application and runtime do not use extra data.

The inspected Firefox entry is a short `#!/bin/bash` wrapper that sets
`TMPDIR=$XDG_CACHE_HOME/tmp` and execs `/app/lib/firefox/firefox`. Its ELF
interpreter is `/lib64/ld-linux-x86-64.so.2`, supplied by the platform under
`/usr/lib64`. Therefore dynamic Flatpak packages also require a reviewed
runtime-major environment policy, synthetic `/bin`, `/lib`, `/lib64` and
`/sbin` aliases into `/usr`, and a bounded `/etc` assembled from td policy plus
selected runtime data. Runtime links into Flatpak's `/run/host` are never
allowed to recreate that host view; DNS, certificates, fonts, timezone and
machine identity receive td-owned paths from §G. Those are jail/runtime tasks,
not reasons to reshape the imported trees.

#### B.3.2 The Firefox platform stop line

The application packaging workstream stops after this proof until the
following bus and Wayland capabilities land. This is a dependency boundary:
the bus and compositor workstreams own their implementations, while a later
application increment re-runs the proof against the then-current exact
Flathub commits.

Those capabilities are necessary, not a claim that Firefox becomes ready when
they land. The resumed application workstream still owns the bounded importer
and dynamic validator from §B.3.1, the runtime aliases and synthesized `/etc`,
the narrowed td permission and resource policy, and image-capacity accounting
for the 993.7 MB base deploy. E2's toolkit and nested-sandbox experiments are
now complete; the exact §H Firefox oracle remains. `td-portal` itself is a
separate platform prerequisite. None of those obligations is discharged by
the bus/compositor list below.

For `td-busd`, full Firefox fidelity needs:

- the per-caller policy and per-jail identity handshake, so Firefox cannot
  borrow the fixture, portal or another application's names and quotas;
- `RequestName`/`ReleaseName` with the bounded owner queue for
  `org.mozilla.firefox.*` and `org.mpris.MediaPlayer2.firefox.*`. The
  MECHANISM and the GRANT are both landed — see §D. `td.Jail1`'s registration
  carries the permission file's `[Session Bus Policy]` `own` entries, and a
  sandboxed application holds exactly the names they list. What is still owed
  is the `.*` on BOTH families here, and a first version of this paragraph
  got that wrong by claiming the bullet was discharged and attributing the
  residual to MPRIS alone. Session-bus keys are EXACT names, and both starred
  families carry a suffix the application picks at run time: MPRIS appends a
  per-instance number, and Firefox's remote-control name appends an encoding
  of the profile path. Neither can be written in a permission file, so what
  an `own` entry can express today is the BARE `org.mozilla.firefox` — enough
  to prove the path end to end, not enough for the two features this bullet
  is about. Media keys, player integration and remote control need an
  amendment admitting a suffix form for `own`, not more broker work;
- `AddMatch`/`RemoveMatch`, authenticated sender stamping and per-recipient
  broadcast filtering are landed — see §D. Portal, MPRIS, file-manager and
  accessibility delivery now depends on the corresponding service and grant,
  not on missing broker match machinery;
- bounded `SCM_RIGHTS` forwarding is landed for directed and matched-broadcast
  traffic between peers that negotiated `UNIX_FD`. Descriptor indices and
  counts stay tied to their message even when several descriptor-bearing
  messages share one `recvmsg`; portal file results now depend on the portal
  service rather than missing broker forwarding;
- per-instance portal activation and Request/Session handle routing are
  landed. The portal namespace remains reserved even while the portal is
  restarting; `td-portal` itself is still the service-side prerequisite;
- a deliberate decision for the imported requests to
  `org.a11y.Bus`, `org.gtk.vfs.*`, `org.freedesktop.FileManager1` and the
  system-bus `org.freedesktop.NetworkManager`. They are not silently granted.
  Accessibility additionally needs §S's second AT-SPI bus; NetworkManager
  either gets a narrow mediated status interface or Firefox runs without that
  integration.

The current broker has authentication, `Hello`, names-and-directed routing,
receive-side descriptor adoption, a global connection ceiling with
per-instance admission shares, and — since the landings
recorded in §D's "what is landed" — the per-caller filter and the per-jail
identity it reads, authenticated pending-reply ownership, and well-known
names with their owner queue, bounded match and descriptor forwarding, and
supervised portal activation. The first bullet above is therefore discharged
for connection admission and routing: a
confined connection is resolved to its instance and answered accordingly, so
all of that instance's processes share one admission key and it cannot see or
ADDRESS the fixture or another application's peers. Not "reach": the shared
descriptor budget below is still a way to affect a peer this filter will not
name. It can
see and reach the portal namespace, which is the grant that bullet exists to
scope rather than something withheld.

What remains of the quota issue is descriptor attribution, described below;
the connection table no longer grants one share per process.

The ordering constraint this paragraph carried has been DISCHARGED rather
than deleted, and it is worth keeping the record of why it was here. The
filter decided who may be addressed without deciding what may be sent, so a
sandboxed peer permitted to call the portal was equally permitted to send it
a signal or a forged method return — dormant only because no portal name
could be owned, and a spoofing path for the first owner of one. Both halves
are now closed in §D: pending-reply ownership makes a reply's origin
enforceable, and message-type policy refuses a confined peer's directed
signals. `RequestName` is no longer blocked on them.

The image's assertion of exactly one shipped application no longer rests on
missing broker substrate: the filter, names, match rules and portal routing
are all per-caller now. A second real application still needs reviewed service
decisions and exact grants from the remaining bullets; it must not weaken the
assertion merely because the broker mechanisms exist.

For `td-compositor`, the minimum useful Firefox target is Software WebRender
over `wl_shm`; dmabuf must remain unadvertised until it works. The platform
gap is:

- `wl_data_device_manager` v3 selection and `wl_subcompositor` are landed, so
  §F's two first-window core-protocol blockers are closed for the pinned
  runtime;
- popup outside-click dismissal and edge constraint solving are landed, so
  browser menus close and stay within the usable output;
- add `zxdg_exporter_v2`/`zxdg_importer_v2`, so Firefox and GTK can give portal
  dialogs an authenticated Wayland parent handle;
- add primary selection, then bounded drag and drop; core data-device
  clipboard transfer is landed;
- add `wp_viewporter` and `xdg_activation_v1` for correct scaling and launch
  focus, and relative-pointer plus pointer-constraints for pointer lock;
- add text-input/input-method only with an actual IME, and fractional-scale and
  presentation timing when td supports non-unit scale;
- before admitting a browser trace, replace the fixture-sized shm/object
  assumptions with §F's per-client pool, aggregate-byte, surface-dimension,
  hidden-subsurface and callback-queue bounds. A 1080p browser copying through
  fbdev is a functionality proof, not a video-performance claim;
- treat `zwp_linux_dmabuf_v1`, explicit synchronization and DRI as a later
  hardware-rendering increment. They are required for full GPU and hardware
  video fidelity, not for the first offline page painted by Software
  WebRender.

Flatpak's authored context is intentionally not the td policy: it asks for
X11/fallback-X11, `devices=all`, `features=devel`, PulseAudio, PC/SC, CUPS,
network, persistent `.mozilla`, and several filesystem and bus grants. td
refuses X11, blanket devices and devel for the first browser proof; uses the
private application home, Wayland, network and narrowly resolved download
grant; and adds audio, printing, smart cards and broader integration only in
their own reviewed increments.

**Replacing a seed with a source build changes nothing above the
recipe.** The manifest, identity, permissions, jail, store path shape and
every consumer stay as they are; the recipe stops consuming a pinned
archive and starts building from source, and `provenance` flips to
`source`. This is the property to protect in review: any design that
makes a seeded package *structurally* different from a source-built one
has broken it.


### B.4 Writable state stays in `$HOME`

Packages are shared between an application's runs; an application's own
data is not. Per-application state lives at:

```
~/.td/app/<name>/{home,config,cache,data,state}
```

bound into the jail as `$HOME` and the `XDG_*` directories (§C). td
spells this itself rather than borrowing upstream's `~/.var/app`, because
nothing reads it from outside the jail.

Keeping state in `$HOME` survived a falsification attempt worth
recording, because it is the argument someone will re-make: moving it to
a read-only volume was proposed on the ground that `$HOME` would
otherwise be the only user-writable persistent execution on an immutable
td. That is false — `td-sh`'s `read_profiles` sources `$HOME/.profile` on
every login shell (`td-sh/src/main.rs:298`), on the writable Btrfs
`/var`. uid-1000 persistence already exists, so moving application state
would not change whether an attacker can persist.

**Per-app uids (§L) are what change who can reach it.** Once each
application runs as its own uid, `~/.td/app/firefox` is owned by that uid
at mode 0700, so one application's escape no longer reads another's
cookies. That belongs to the identity workstream, and it is **not free of
privilege**: an unprivileged process may write only an identity
`uid_map`, so a distinct uid needs `CAP_SETUID` in the parent namespace,
and uid 1000 cannot `chown` a state directory to an unrelated uid without
`CAP_CHOWN` or idmapped mounts. `td-authd` (§L.1) is the natural home for
both as enumerated operations — `map-subuid` over a namespace the caller
already created, and the state chown — authorized once at **enrollment**
rather than prompting per launch, since a prompt on every application
start is exactly the fatigue §L.1's threat table refuses.

The state root is a **configuration value from the first landing**, never
a path baked into a manifest or into jail policy, so relocating it later
is not a migration. §X relies on that, and so does the package root.
The immutable `/etc/td-app.conf` now records the version-1 image contract:

```ini
format=1
package-root=/td/store
state-root=.td/app
registry=/etc/td-applications.tsv
launcher-table=/etc/td-launcher.tsv
```

The relative state root is resolved beneath the real user's home. Rung 7
does not create that writable directory; `td-jail` does so on first launch
once the ownership and confinement boundary exists.


### B.5 Activation and state — there is no install

There is no install step and no installer binary. The packages are in the
image; nothing is fetched, materialized or verified at install time
because there is no install time. What the earlier per-user tier needed
an installer for now happens in two other places:

| job | where it happens now |
|---|---|
| identity allocation | build time, in the recipe |
| the permission defaults | build time — part of the jail spec (§A.0) |
| the launcher and resolver tables | build time, compiled from every shipped package's builder-authenticated metadata |
| the state directory `~/.td/app/<name>/` | first launch, by `td-jail` |
| the per-user permission *override* | on demand, at first edit — a separate file, so the default stays immutable |

**Widening a grant must stay visible.** A default that is immutable does
not make an override safe, so a launch reports any grant beyond the
package's declared defaults. A permission model whose widening is silent
is not a permission model.

**Every account sees the same applications**, which follows from one
shared store. Where one account should not see one application, the
answer is a per-user hide list read beside the launcher table — not a
per-user package tier.

**Collection** is the store's, not this design's: a package is reachable
while a deployment names it, and deployments are retained two deep.

`verify` and `fsck` verbs do not exist. The deployment's signature is
checked once at publish and the payload is a read-only image; a userspace
hash sweep would re-check it with a weaker mechanism. That is a statement
about *publish-time* integrity, and it is worth being exact: plain EROFS
is not dm-verity, so nothing authenticates each read at run time, and a
process that can rewrite the backing file defeats it. Applications are in
exactly the position of the kernel, the initramfs and td's own userland
in that respect — an attacker who can rewrite the image has already won
more than an application — so a verb that singled applications out would
be theatre rather than defence.

### B.6 Deliberately not built

Recorded as refusals with their reasons, so that a later reader reaching
for one has to argue past this rather than rediscover it:

| not built | why |
|---|---|
| **target-side OSTree client** (repo modes, `.filez`/`.dirtree`/`.dirmeta`, GVariant) | Still refused. §B.3.1 selects a bounded control-plane deploy importer for reviewed exact commits; it does not put repository or installation machinery in the distribution. |
| **runtime summary browsing, refs and updates** | Still refused. The control-plane importer may resolve only the exact pinned commit/object graph. A pin bump plus a rebuild is td's update mechanism; a mutable ref is discovery input at review time, never derivation input. |
| **OpenPGP verification** | The signature is checked by a human at pin review, as it is for every other fixed-output input. An implementation on the target would be a parser for attacker-supplied input serving a trust decision already made elsewhere. |
| **HTTP/TLS on the target** | Nothing on the target fetches. The control plane's `td-net` already does this, under the existing dependency exception. |
| **a target-side `fsck`/`verify` verb** | Refused. A package is in a read-only image admitted by a signed manifest, so a userspace hash sweep would re-check it with a weaker mechanism (§B.5). This refusal has flipped three times as the layout moved, which is the entry's real content: **a refusal argued from a layout is only as settled as the layout.** |
| **OCI / container registries** | A second foreign format with the same objection as the first. The selected Firefox experiment uses Flathub's signed OSTree commits directly; an OCI translation would add a second parser without removing the first. |

### B.7 Reuse from the control plane

Much shorter than it was, since most of what would have been ported
served the repository client:

| source | decision |
|---|---|
| `builder/src/sandbox.rs` | **rewrite** as `td-jail` — the namespace mechanics are the model, but its build-user ids, offline network, `/gnu/store` paths and trust model are all wrong here |
| `builder/src/store.rs` | **read, do not port** — the store semantics are the control plane's, and with packages in the store there is nothing on the target that creates a store path |
| `builder/src/elf.rs` | **never** — see §G |
| `builder/src/gzip.rs`, `xz.rs`, `bzip2.rs`, `erofs.rs`, `tar.rs`, `oci.rs` | do not port — all of them served the repository client or the image writer, and decompression now happens in a recipe on the control plane |
| `builder/src/nar.rs` | **needed, but only in the control plane** — `read_nar` is how host mode materializes a package (§X.1). Nothing on the target extracts an archive. §X.6 records the arbitrary-file-write this design found in it, fixed on main before this landed |
| `engine` SHA-256 | **not needed** — nothing on the target hashes a package; the deployment's signed manifest is the integrity check |

### B.8 The sandboxed-application marker

A foreign prebuilt payload is admitted to the store by a **type**, not by
a location. The type exists because the safeguards it must not weaken
were built for a different threat: #469 is *undeclared* host ingress —
build-host bits leaking into recipes — which `AssertStatic`
(`recipes/src/types.rs:201`) reds by refusing a regained `PT_INTERP` or
`DT_NEEDED` on the pre-libc rungs. A reviewed pin is *declared* ingress.
A rule that cannot tell the two apart will either admit the leak or
refuse the pin.

**It is a recipe-level marker**, because there is nowhere else to put it:
`Recipe.outputs` (`recipes/src/types.rs:781`) is names-only and no recipe
calls it. It must also reach `Recipe::to_json`, or the rule is enforced
in only one of the two places a plan is built — `td-builder`'s own
`build_plan_auto` sees the emitted JSON and nothing else.

**It cannot reach it by riding the source pin, and that is a real
constraint rather than a detail of where to write the field.**
`Recipe::to_json` (`types.rs:1012`) deliberately drops `source_pins`, and
a test named `source_pins_are_recipe_metadata_not_build_json`
(`types.rs:1108`) exists to keep it dropped: a pin is how a recipe was
authored, and the build JSON is what a derivation hashes. Emitting pins
into it would change every derivation hash in the tree for a metadata
reason. So the taint still ORIGINATES at the pin — §B.8 needs that, for
the reasons below — and what crosses the boundary is a **derived
recipe-level flag**, computed from the pins during evaluation and emitted
as its own key. The pin is the source of truth; the flag is its shadow on
the side of the wall where `build_plan_auto` lives. A landing that adds
the field to the pin and stops has enforced half the rule and will pass
its own tests.

**Both halves have LANDED**, which is what that last sentence was
written to prevent: `SourcePin` carries `foreign`, `Recipe` derives its
own flag from the pins at the single funnel every pin reaches a recipe
through, and `to_json` emits `"foreign": true` — only when true, so
every recipe in the tree hashes exactly as it did and landing the mark
rebuilt nothing. Three things about the shape are worth recording
because each was a choice with an alternative:

- **The recipe's answer is COMPUTED from its pins, not cached beside
  them.** A builder method would be a second source of truth for a
  trust answer; so, it turned out, would a cached field, because
  `source_pins` is public and consumers hold it — appending to that
  vector, or flipping a mark on a pin already in it, desyncs a cache
  silently. Both cross-model reviewers found that independently. There
  is one source of truth and `is_foreign()` reads it.
  The taint is also STICKY under the pre-existing dedup: two pins under
  one key disagreeing about the answer is a conflict either arrival
  order could hide, so the mark joins the pin that is kept.
- **The pins name their marks in a ROSTER** rather than carrying
  `foreign: false` on all fifty-odd `PinDef`s, for UNSAFE.md's reason —
  the count is what a reviewer reads. A name-keyed roster has the hole
  this workstream has twice refused, a declaration that reaches nothing
  reading as enforcement, so the roster is checked to name pin KEYS and
  never aliases: `materialize` marks on the pin's own key, and a pin
  foreign under one name and ordinary under another is the same bytes
  admitted twice under two trust answers.
- **What the marker defends against is MISTAKE, not a hostile recipe
  author.** Recipes are compiled Rust in this tree, so anyone able to
  clear a mark could equally not declare the pin. The mark is a review
  artifact and a machine-checkable one; it is not a sandbox around the
  catalog, and reading it as one would be the same overclaim §B.8
  corrects elsewhere.

The roster was empty until the first application landed, which was exactly the
condition under which every test over it passed with the rule inverted. The
decision functions therefore still take their roster as an argument and are
driven with fixtures, including through the production lookup over a real pin
key. The real roster now contains exactly `ripgrep-seed-source`; tests assert
both the count and that this is the sole marked pin. The wiring that supplies
that roster remains pinned in source, so replacing `FOREIGN` with an empty slice
cannot silently return the seed to the ordinary trust class.

The planning-time refusal that reads this mark landed after it, and the
containment edge and closure query after that; both are described below.
One thing none of them closes: the mark does not
cross into the four-column source-pin TSV that `td-feed` and the
control-plane bootstrap parse into their own pin types, so the
fetch/warm side cannot yet tell a foreign pin from an ordinary one.

**The taint starts at the source pin, not at the output.** Marking only
the finished package would leave the packaging recipe free to execute the
pinned foreign binary while building it, and would let a second recipe
consume the same archive and emit an *unmarked* output. So `foreign` is a
property of the fixed-output source pin, it propagates to every output
derived from it, and an unmarked descendant of a marked source is a
planning-time refusal in the shape `seed_digests.rs` already uses for
`provenance rejected`.

**The refusal has LANDED, in both places a plan is built**, and what it
enforces is the table below rather than the sentence above — because
those turn out to say different things. "An unmarked descendant of a
marked source" suggests the taint should SPREAD to the consumer; the
table says a marked path on `inputs`/`native_inputs` is *refused*, which
means there is no unmarked descendant to catch. A consumer that names a
payload as a tool does not become foreign, it does not build. The
sentence describes what would be needed if the tool channel admitted a
payload; the table is why it does not.

So marked-ness travels by RECIPE and the refusal reaches as far as the
graph does: `mid` naming a payload in `inputs` is refused, and so is
`top` naming `mid`, because `mid` never builds. Both plan builders
enforce it, which is the reason the derived flag had to reach the build
JSON at all: at eval, `classify_graph_inputs` refuses in the
`provenance rejected` shape; in `td-builder`, `auto_topo` refuses while
walking the emitted JSON, which is all that side ever sees. A `foreign`
key present but not a boolean is an ERROR there rather than "not
marked" — a restriction read from a damaged declaration must not fail
open.

The two channel rosters are named constants and asserted to partition
the same set, so a fourth declaration channel cannot be added to one
alone: added to the walk only, it is a path the table never rules on;
added to the refusal only, it is a path the plan never resolves.

**A marked PIN named as a tool is refused too**, and review is what
found that a recipe-level check alone missed the case this section names
FIRST. A recipe writing `.inputs(&["app-archive"])` for a marked pin has
the payload's own bytes staged as a seed source and builds from them —
and the pin attaches, so 3b-i marks the recipe for it. Nothing refused
the recipe that actually runs the payload; only a *consumer* of it would
have been, and there need not be one. That is precisely "the packaging
recipe free to execute the pinned foreign binary while building it",
arriving through the tool channel rather than through the output. So the
question asked per edge is "is this name a marked recipe OR a marked
pin". `source_input` is admitted only for the owning packaging recipe:
the recipe JSON carries a separate `foreignSource` mark, the assembler
withholds that archive from `TD_INPUT_MAP`, and `Unpack.input` reads it
through the local `{payload:<recipe>-source}` entry. That name is distinct from
the `sourceInput` pin key when a recipe renames or shares a source. The
entry may not also be listed explicitly in `payloadInputs`; only the
source-specific mark moves it to DATA. The aggregate `foreign` mark cannot make
that decision because it may come from some other attached pin.

That half is enforced where the CATALOG is known — at eval, plus a
catalog sweep — and **not** in `td-builder`, which cannot enforce it: a
pin's mark does not cross into the build JSON, so a non-owned input is
just a name the map resolves. It is the same gap as the source-pin TSV
above and closes with it. Until then the division is: `td-builder`
refuses a marked RECIPE on the tool channel, and the pin case is refused
before any recipe JSON exists.

**"Propagates to every output derived from it" cannot be read over the
`payload_inputs` edge, and review caught the two rules colliding exactly
where the design needs them not to.** The image recipe consumes an
application through that channel by construction, so unrestricted
propagation makes `root.erofs` — and therefore the deployment, and
therefore td — a marked foreign output, which is both absurd and the end
of the closure query that was the point. The distinction is real rather
than a carve-out: **a derived output is one built FROM a payload, and an
image is one that CONTAINS it.** Deriving takes the taint; containing
takes a different mark, `contains_payloads: {paths}`, which is what the
closure query reads to answer "source-bootstrapped apart from these".
A containment edge is exactly `payload_inputs` and nothing else, which
is why the channel had to exist before this rule could be stated: the
graph could not otherwise tell the two apart, and neither could a
reader.

**Both have LANDED over the recipe graph**, as `td-recipe-eval
payload-closure [TARGET…]` (default `system-x86-64`). It is PURE — it
reads the catalog and builds nothing — so it is a question anyone can ask
of a checkout. Rung 6 added the complementary built-byte query,
`td-recipe-eval application-closure TARGET`; the distinction remains
important because only the latter can prove a store edge created by `spec`.

`contains_payloads` is the ANSWER rather than a declared key — no output
carries a `{paths}` field, and grepping for one finds this paragraph and
the Rust function that computes it. It is COMPUTED from the graph for the
reason `is_foreign` is: a declared set is a second source of truth for a
trust answer, and the graph already carries the edge. What it names are
recipe NAMES, not store paths — a name resolves to a path only in a plan,
and a query that had to build one would not be answerable of a checkout.
The answer is two counts and a line per marked path, TAB-separated and
fixed-arity — shown aligned here, and a parser should split on tabs:

```text
members	66
unmarked	66
audited-seeds	59
payload	recipe	firefox
payload	pin	firefox-archive
```

The counts come out of one walk on purpose. With nothing marked the list
is empty, which is exactly what a query that walked nothing prints — so
`66 of 66` is what tells a true answer from a broken one, and `0 of 0`
is the shape a test refuses. It is the same empty-roster trap the mark
itself had, answered with a number rather than with a fixture.

**The second line is NOT spelled `source-bootstrapped`, and that is a
correction review forced rather than a wording preference.** The count is
members minus marked payloads, and two members of this very closure are
prebuilt: `rust-stage0` transforms an upstream binary snapshot, and
`stage0` is the stage0-posix seed. Both are td's declared bootstrap trust
roots, which AGENTS.md's claim names as its own exception — "no foreign
binary other than its declared bootstrap seeds" — and §B.8's mark is
about APPLICATION payloads and says nothing about them. A line spelled
`source-bootstrapped 66` would therefore deny something td declares, and
a reader parsing it as "nothing prebuilt is in here" would be wrong. So
the wire says `unmarked`, the design sentence stays what it is, and
`audited-seeds` — the distinct seed inputs the planning pass classified —
is printed beside it so the seeds are visible rather than absent.

A marked pin is reported BESIDE the output built from it rather than
instead of it: the archive's bytes are a store path of their own, so the
two are two paths the claim does not cover. Neither is a closure MEMBER
in the count — a pin is bytes, not a recipe — so pins are listed and
members are divided.

The query REFUSES rather than reports when a marked path reached the
closure over a tool edge, and it does so by RUNNING the planning pass
rather than by re-checking after it: the same `classify_graph_inputs`
that decides provenance also carries the tool-channel refusal, so
"everything reported here arrived over a containment edge" is
established by the pass the query runs first. An earlier draft checked
it a second time in the reporting function, which after the
classification landed was a line the product could never reach.

What keeps the taint off the containment edge is one absence, and it is
asserted rather than assumed: `inputs`, `native_inputs` and
`source_input` all attach the named key's source pin to the recipe, and
`payload_inputs` does not. An image naming a payload as DATA is
therefore marked for nothing, which is what stops `root.erofs` — and the
deployment, and td — being a foreign output. The test uses a REAL pin
key, because with a made-up one nothing attaches on any channel and it
could not tell a channel that attaches from one that does not.

`td-builder` has no equivalent and is not getting one here, for the
reason 3b-ii recorded: a pin's mark does not cross into the build JSON,
so that side cannot answer the pin half at all. The query lives where
the catalog is known.

#### What "never a build input" can and cannot mean

Stated flatly it is **false**, and structurally so. The image recipe
declares everything the image contains in one input list and moves each
into the erofs tree by `{in:NAME}` template, so a shipped application
*must* appear there or it cannot be in `root.erofs`; `system-x86-64.rs`
states the rule itself — *"a store item reaches the erofs root by
CopyTree or StageRuntimeClosure; being a recipe INPUT only makes the path
resolvable at build time."* An application must likewise name its
runtime, or the spec cannot spell it.

Worse, the graph **cannot currently express the distinction**: `inputs`
and `native_inputs` are `.chain()`ed into one iterator by every consumer
(`check_runner.rs`'s `classify_graph_inputs`, `catalog_seed_universe`,
`builder/src/main.rs`'s `inputs_from_recipe_json`), and edges carry no
label saying *why* an input is there. A check would see "image recipe →
firefox" and be unable to tell payload from compiler.

So the marker adds a **separate declared channel** rather than a
predicate over the existing one:

| channel | permitted for a marked path | meaning |
|---|---|---|
| `payload_inputs` | **yes** | staged as data — copied into an image, or named as a runtime to be mounted. Never executed, never linked against |
| `inputs` / `native_inputs` | **refused** | the tool, compilation and execution channel — the ingress #469 exists to stop |

and the enforceable claim is **"never a tool, compilation or execution
input to a source-built output"**. The image-assembly and
runtime-reference edges are then ordinary `payload_inputs`, not exceptions
carved out of a rule they would otherwise break.

**Execution is not a graph property, and an argv scanner is not the
answer to it.** A step can name a payload legitimately and still run it:
`Step::Run`'s argv expands `{in:NAME}`, and the image recipe already
executes declared inputs this way. A draft answered that with a scanner
over every recipe's argv and templates, refusing any `payload_inputs`
path that reaches one — and a scanner is SYNTAX. A build tool that can
see the payload can concatenate the path, read it out of a file, walk the
store to find it, or exec something an earlier step already copied. Every
one of those satisfies the scanner.

**What actually enforces it is visibility: a marked payload is never
staged into a `Step::Run` sandbox at all.** Payload data resolve only for
the typed data operations — `Unpack`, `CopyTree`,
`StageRuntimeClosure`, and whatever spells a runtime path into a spec —
which are performed by the builder itself rather than by a program the
recipe chose. A step that
runs a command simply does not have the path in its filesystem, so
"never executed, never linked against" stops being a property to check
and becomes one the sandbox cannot express. That is the same move
`AssertStatic` makes: not "did anyone link against the host libc" but
"the host libc is not in there to link against".

**The argv/template scan has LANDED**, downgraded to what it is worth: a
cheap pure pass in `classify_graph_inputs` that catches the honest
mistake — a recipe naming a payload where it meant a tool — and reports
it at planning time with the recipe, step, field, payload name and line,
rather than as a step that fails inside a sandbox for no visible reason.
It is deliberately described as an audit rather than enforcement: the
separate input map and the `noexec` bind below remain the boundaries.
The pass is at `td-recipe-eval`'s production planning boundary; invoking
the runner-private `td-builder build-plan --auto` backend directly is
outside that boundary and still gets the expander's fail-closed error.

**The channel has LANDED (rung 3), and the paragraph above needed one
correction to become code.** "Never staged into a `Step::Run` sandbox at
all" is not implementable as written: every step of a build runs inside
ONE sandbox invocation, and `Unpack`/`CopyTree`/`StageRuntimeClosure`/
`CompileApplicationTables` are performed by `td-builder` *in that same
sandbox* — so a payload the data operations can read is necessarily mounted
while a `Step::Run` also runs. Per-step mount manipulation would close that,
and it would mean a new `unsafe` call site in `build.rs` outside the one the
control plane records. So the enforcement is split in two, and neither half is
a scan:

- **Resolution.** A payload is withheld from `TD_INPUT_MAP` at assembly
  and placed in `TD_PAYLOAD_MAP`. The outer spec compiler resolves an
  application's runtime through the declared payload set for every
  application-capable build system, then the sandbox strips the map before a
  non-mesboot package process starts. Such an application may declare exactly
  that runtime payload and no extra; mesboot reaches other payload data by a
  template token of its
  own, `{payload:NAME}`, which resolves ONLY in `Unpack`'s `input`,
  `CopyTree`'s `from`, `StageRuntimeClosure`'s `roots`, and
  `CompileApplicationTables`' `packages` and `runtimes`. A
  command-bearing step has no name
  for a payload — `{payload:…}` there is an error, not a miss, and
  `{in:PAYLOAD}` is an error naming the rule rather than the ordinary
  "no such input", which would send a reader hunting a lock entry that
  was withheld on purpose. **A `glob:` argv element is confined to the
  build's own tree** as part of the same half, and review is what found
  that it had to be: `glob:` splices directory entries straight into
  argv, and `{in:<any input>}/..` is the store directory — so
  `glob:{in:mes}/../*-firefox-140` names a payload on a command line
  with no `{payload:}` and no `{in:PAYLOAD}`, defeating this bullet
  through a template that resolves something else entirely. Every
  `glob:` in the tree already reads `{root}`, so the confinement costs
  nothing; what it shows is that "has no name for a payload" is a claim
  about EVERY way argv is built, not only about the expander.
- **Execution.** A declared payload's sandbox bind is `ro,noexec`. That
  is the half the scanner could not be for DIRECT execution:
  concatenating the path, reading it out of a file, or walking the store
  all still find the bytes, and the kernel refuses to run them from
  there.

**`noexec` is not the whole property, and saying so is the point of
writing it here.** A `Step::Run` can copy the payload into `{out}` or
`/tmp` — both writable and executable — and run the copy; a script can
be handed to an interpreter, which never execs the file at all. So the
enforceable claim is *narrower* than "cannot be executed": it is that a
recipe cannot NAME a payload in a command, and cannot execute one where
it lies. What defeats the copy is the payload not being in the
filesystem during a Run step at all — the mechanism §B.8 originally
specified, which needs the per-step mount masking above and a new
`unsafe` call site to implement. Until that lands, the three mechanisms
are in DEPTH rather than complete: resolution stops the accident,
`noexec` stops the shortcut, and the marker plus the closure query
(rung 3's other half) are what make the remaining case auditable rather
than prevented. This is the same shape as §B.8's own "direct execution
outside a jail is not prevented" — a claim about the path td provides,
not about what the bytes cannot be made to do.

Refusals hold the channel shut at both ends, and they are at both ends
because a rule enforced only where a recipe is WRITTEN is a rule a
damaged derivation walks around. At declaration: a name in both
`payloadInputs` and `inputs`/`nativeInputs` is refused outright rather
than resolved by precedence. An explicitly duplicated `sourceInput` is
also refused: a marked source joins the data map from `foreignSource`,
while an ordinary source would still reach the build as `TD_SRC` — the
same channel through another door. A `payloadInputs` that is not an
array, or holds a non-string, is
refused rather than filtered, since a malformed declaration of a
restriction must never be read as an absent one; and an entry no lock
entry resolves is refused, because a declaration that reaches no path
reads as enforcement and is none. At the builder: a `TD_PAYLOAD_MAP`
present but malformed is an error rather than "no payloads", an entry
matching no closure item is an error, and the two maps are checked
disjoint by PATH — a payload aliased under an input name would resolve
for a command-bearing step, since the runtime resolver compares names.
The drv's environment is refused outright if any key repeats, because
its readers disagree about which copy wins: a variable asked for by
name answers with the first, while the environment handed to the build
keeps the last, so a `TD_PAYLOAD_MAP` written twice would plan the
mounts from one map and resolve `{payload:…}` from the other. The
Mesboot alone exposes the map through typed data steps. Other build systems may
declare exactly one payload only for an application's outer spec compiler, and
the map is stripped before their package process starts.

State the limits, since this section's whole method is to. `noexec`
stops execution and NOT linking — a compiler handed the path could still
link against what is there, which is why the mount plan (§C) rather than
this is what makes "never linked against" true at run time. Only the
DECLARED payload roots are marked, not their transitive closure: §B.8's
case rests on a foreign payload being self-contained, carrying none of
td's store hashes, and a marked runtime being its own declared payload —
a payload that did drag td-built dependencies in would leave those binds
executable. And the payload set travels in the drv's own `env`, so it is
hashed derivation data and cannot change without changing the
derivation; every new key is omitted when unset, so landing the channel
left every existing derivation hash byte-identical.

The pass does not borrow `ladder.rs`'s `command_texts`: that helper
covers four of eighteen `Step` variants, is `#[cfg(test)]`, and omits
`Step::Run`'s `env` — exactly where an image recipe supplies `PATH`.
Instead an exhaustive match visits every field the builder's template
expander visits, including environment values, and deliberately skips
literal labels and `SubstituteText` edit text that the builder does not
expand. `{payload:NAME}` is admitted only in `Unpack.input`,
`CopyTree.from`, `StageRuntimeClosure.roots`, and
`CompileApplicationTables.packages` and `runtimes`; `{in:NAME}` cannot
launder a declared
payload through those fields either. That exhaustiveness makes a new
`Step` variant or field a compile failure until its template visibility
is decided, and the test table is checked against the production field
visitor and the builder's ordinary/data expansion-site counts. It is
still syntax; the point of the paragraph above is that it does not have
to be the mechanism.

#### Direct execution outside a jail is not prevented

A marked payload sits at a readable store path, so nothing stops
`execve` on it — a launcher is a convention, not a boundary, and
`/td/store` cannot be mounted `noexec` because td's own binaries are
there too. Three drafts of this subsection tried to close that with the
payload's own interpreter and each was refuted; the argument is recorded
because it is seductive and because the refutations are what a fourth
attempt has to beat.

*The argument.* A seed's `PT_INTERP` is `/lib64/ld-linux-x86-64.so.2`,
which resolves only inside the jail, where mount step 5 links `/lib64`
into the runtime at `/usr`. It does not resolve on td's image root — and
that much is true, though not for the reason a draft gave. It cited
`builder/src/elf.rs`'s interpreter retargeting, whose only production
caller is `toolchain_x86_64.rs`'s `relink_rust_interp` over `rustc`,
`rustdoc` and `cargo`, which are Rust stage0 and excluded from final
closures. The real reason is that td's shipped userland is **statically
linked**, asserted by `Step::AssertStatic` in about twenty-eight
recipes; nothing in the image has an interpreter at all. Same conclusion,
and the difference matters because the stated reason would survive td
shipping one dynamically-linked binary and the real one would not. So —
the argument went — a payload exec'd outside a jail fails `ENOENT`
before one instruction of its code runs.

*Four ways it does not hold.* **The loader is a program, and it is in
the store.** `ld-linux-x86-64.so.2 --library-path <runtime> <app>` loads
and runs an executable whose own `PT_INTERP` is never consulted; the
runtime ships that loader at a readable store path, so the bypass needs
nothing the payload does not already ship with. **A static executable
has no `PT_INTERP` at all** — `binfmt_elf` needs no interpreter for one,
so a Go binary or anything against musl runs with nothing to fail on.
**§0 hands every uid-1000 process the jail's own first move**: with
`USER_NS` on for the machine, anything can `unshare(CLONE_NEWUSER |
CLONE_NEWNS)` and bind a `/lib64` of its own, so "absent from the image
root" stops being a property of the machine and becomes one of a
namespace the attacker picks. And **a source-built package is strictly
worse off**, because its interpreter is a store path that exists by
construction.

**So the honest statement is the one §B.8 closes with: a marker is a
claim about provenance, and a jail is a claim about reach.** What runs
outside the jail is a uid-1000 process with exactly the authority the
invoking user already had — it reads that user's files because it IS
that user, not because confinement failed. td loses nothing it had; what
it does not get is a guarantee that the bytes cannot run except behind
§C, and no sentence anywhere may assume one. The `PT_INTERP` property is
still worth asserting, demoted to what it actually is: **a check that the
ordinary path is the jail** — `/bin/firefox` reaches §C or fails, rather
than half-working outside it and leaving an operator to wonder which
happened.

*What would make the strong claim true*, so a later landing amends a plan
rather than inventing one: per-app uids with each payload readable only
to its own, or a payload subtree bound `noexec` that td's own binaries do
not live under. Both are real mechanisms and neither is here. Until one
is, **`AGENTS.md` says "applications are RUN behind td's boundaries",
never "can only run" behind them.**

Deliberately **not** done: retargeting a payload's interpreter into the
store the way the Rust bootstrap snapshot's is. That would make the
payload no longer the pinned bytes, and it is the jail's job to supply
the runtime.

#### The library check is a smoke test, and is stated as one

`DT_NEEDED` holds sonames, not paths. Resolution depends on
`RPATH`/`RUNPATH`, `$ORIGIN`, the loader cache and the mount namespace,
and `dlopen` is invisible to any static scan — Firefox dlopens ffmpeg and
NSS, glibc dlopens NSS modules driven by the `/etc/nsswitch.conf` the
mount plan writes, and `StageRuntimeClosure`'s own documentation already
records that *"script interpreters and data-only `dlopen` paths are
outside this step's graph"*.

So the check is: every `DT_NEEDED` soname is satisfied somewhere in the
**application ∪ runtime** closure — not "inside the runtime", since
application-private DSOs under `/app/lib` are ordinary and legitimate.
The *guarantee* comes from the mount plan, not the scan: inside the jail
there is no host filesystem and no td `/usr`, so there is nothing else
the loader could bind to. The scan catches a package that would fail at
launch; the namespace is what makes it safe. Calling it "`AssertStatic`'s
inverse" would overstate it by an order of magnitude — that is a per-file
header check with no graph view.

#### The closure query, and the edge it would otherwise miss

Assertion 3 — that a closure query can answer *"source-bootstrapped apart
from these marked paths"* — is buildable today: `store_db_read.rs`'s
`closure`/`closure_roots` and `ladder.rs`'s existing graph-partition
invariant with its reviewed exception list are the machinery and the
precedent.

One gap has to be closed for it to be true. td discovers closure edges by
**scanning output bytes for store hashes** (`builder/src/scan.rs`), and a
self-contained foreign payload contains none of td's. The
application→runtime edge therefore exists only if the recipe-generated
spec spells the runtime's **full store path** — so it must, and §A.0 says
so. A spec naming the runtime by short name would leave the closure
under-reporting and a future collector free to reclaim a live runtime.

**Both halves have now landed.** `payload-closure` walks declared edges in the
catalog; it never opens the store DB and never scans a byte. Everything that
follows from that:

- It is answerable of a checkout, with nothing built — which is what
  makes it useful before the first application exists, and is why it is
  the half taken first.
- It cannot see the edge above. A spec that named its runtime by short
  name would still be *declared* as a `payload_inputs` edge, so this
  query counts the runtime while the built closure omits it. The recipe
  answer and the store answer would disagree, and only the store one is
  about what a collector will do.
- It is therefore not the collector proof. `application-closure TARGET` builds
  an application, reads its canonical `spec`, asks `td-builder store-closure`
  for the root in the accumulating td store DB, and refuses unless the app root
  and the exact runtime output selected by the declared payload edge are both
  present. It also binds every graph-marked recipe and pin to the output or seed
  path the same plan selected, reporting each as `retained` or `build-only`.

For the first seed the result is two retained store members: `ripgrep-seed` and
`empty-runtime`. The foreign recipe output is retained and the pinned upstream
archive is correctly `build-only`: the recipe copied its authenticated bytes
into the package, so the archive remains a provenance input but is not a live
runtime reference. This distinction is why the query prints both the registered
store closure and the reviewed graph marks instead of pretending every build
input is a collector edge. A short runtime name, a mismatched store output, a
missing DB edge, or a missing reviewed pin mapping is a refusal.

Its output is TAB-separated and exposes both answers explicitly:

```text
members	2
unmarked	1
recipe-members	2
audited-seeds	1
application	ripgrep-seed	/td/store/<hash>-ripgrep-seed-15.2.0
runtime	empty-runtime	/td/store/<hash>-empty-runtime-1
store	/td/store/<hash>-empty-runtime-1
store	/td/store/<hash>-ripgrep-seed-15.2.0
payload	recipe	ripgrep-seed	/td/store/<hash>-ripgrep-seed-15.2.0	retained
payload	pin	ripgrep-seed-source	/td/store/<hash>-ripgrep-seed-source	build-only
```

#### Metadata, exports, and what the marker does not do

The payload's metadata is normalized at build: no setuid or setgid bits,
no file capabilities, no security xattrs, no device nodes, no symlink
escaping the tree. Foreign `exports/` bytes are not trusted. The launcher row
is the first exception only because the outer builder creates it from typed
recipe metadata after the package namespace has gone; its name is checked
against reserved names and the `/bin` applet farms (§A.0), so a foreign package
cannot claim `sh`.

The fourth assertion is the ordinary one: a **reviewed pin with a
compiled expected digest**, so adding an application is a reviewed line
like every other seed.

**The marker does not make the payload trusted, sandboxed or safe** — §C
does that at run time, and a marker is a claim about provenance where a
jail is a claim about reach.

**And it does not make a seeded package structurally different from a
source-built one**, which §B.3 requires. The marker is *metadata on the
recipe*: the manifest, spec, layout, identity, permissions and jail are
identical either way. A source-built package satisfies neither the
interpreter assertion nor the payload-channel restriction, and does not
need to — it is not foreign. What §B.3 protects is the shape of the
package and everything above it, and that is untouched.

## C. `td-jail` — the sandbox

### Namespaces

After the proved application containment above, one `unshare(2)` in
single-threaded stage 1:

```
CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUTS [| CLONE_NEWIPC] [| CLONE_NEWNET]
```

All in one call so the pid namespace is owned by the new user namespace —
the kernel applies `NEWUSER` first, which is what grants the capability
for the rest. `NEWNET` only when metadata lacks `shared=network` (Firefox
has it, so Firefox keeps td's stack). Stage 1 reads back that an isolated
network changed or that a declared shared network did not. Only in the
fresh namespace is loopback **brought up** with a pinned
`SIOCGIFFLAGS`/`SIOCSIFFLAGS` pair on the name `lo` — leaving it down
turns "no internet" into "no sockets" for every app with a localhost
helper, while issuing that ioctl against td's shared stack would mutate
host policy. `NEWIPC` only
once `CONFIG_IPC_NS` is on (§0). uid/gid maps are **identity** — `1000
1000 1` — because an app that sees uid 0 mis-chowns its own files, and
`setgroups` is denied *before* the gid_map write (CVE-2014-8989); that
ordering is ported verbatim from `map_userns_id`.

### Mount plan

Stage 1 prepares the new root and performs the privileged entries below
that do not depend on inhabiting the child PID namespace. Before the
safe-`Command` re-exec it makes exactly `CAP_SYS_ADMIN` inheritable and
ambient, then drops and reads back the complete bounding set while
`CAP_SETPCAP` remains effective. Stage 2 reads back the exact one-capability
state and empty bounding set, mounts the fresh procfs for its own PID
namespace while the old procfs is still visible, pivots into the prepared
root, then drops every remaining capability before policy finalization.
The landed rung-12 application path is the closed static/empty-runtime
subset of this target table. It implements `/app`, `/usr`, fresh `/proc`,
the minimal `/dev`, tmpfs `/run`, the exact Wayland socket, the session
bus socket of step 12 and the five persistent state directories, and either a
read-back-up loopback interface in an otherwise-empty network namespace or the
unchanged td network namespace selected by `shared=network`. It deliberately
leaves `/etc`, `/sys`, `/var/lib`, `/var/cache`, `.flatpak-info`, extension
mounts, and mutable permission overrides absent. It accepts exactly optional
`shared=network`, `sockets=wayland`, the closed filesystem subset below,
resource limits, and `[Session Bus Policy]` `own` entries, which it forwards
to the broker at registration and does not itself act on; any other policy is
refused, and the refusal names the request so an operator holding a permission
file learns which line to change. Sharing is direct access to td's network
stack, not mediation, and this landing does not populate `/etc/resolv.conf` or
claim name resolution. Later rungs fill those named rows without making their
absence a degraded launch mode.

The bus is bound unconditionally and is deliberately not a `sockets=`
permission, which is what step 12 has said since it was written. It is
also the one landed row whose enclosing claim — that the broker is the
policy — is now PARTLY a description of what runs rather than wholly a
target: today's td-busd resolves a per-jail identity at accept, filters
what each caller may see, address and own, honours the `own` entries this row
forwards, and filters matched broadcasts per recipient. Its PRESENCE is a
launch precondition as well:
`plan_launch` resolves the socket before the jail unshares, so a missing
or non-socket `/run/user/1000/bus` fails the launch of every application,
including one that never opens D-Bus. §D records what all of that costs.

The compiled application environment crosses the internal exec only as
bounded, canonical argv data:
stage 2 starts empty and applies those entries only to the final child after
its capability and seccomp readbacks. The internal argv also carries the
one-use stage proof token. Before spawning the application, PID 1 becomes
non-dumpable, which denies the unprivileged child access to PID 1's
`fd`, `exe`, and `environ` procfs entries. Linux leaves `/proc/1/cmdline`
readable, so the stage-2 argv must never contain a secret.
The entry arguments and environment are applied to the child and remain
application-visible, so their values must never be treated as secrets. The app
cannot reopen PID 1's executable or proof descriptor, has no td-jail binary in
its namespace, and cannot create the required namespaces after the filter. In
launch mode, stage 2 receives its
proof pipe on stdin, null stdout and one bounded diagnostic pipe on stderr.
After its readbacks it makes PID 1 non-dumpable and verifies that state, opens
the application's null descriptors, and spawns it. PID 1 retains the proof
reader and bounded diagnostic writer: a filtered watcher treats proof EOF as
stage-1 death and terminates the namespace, while the diagnostic remains live
through the application's final status. PID 1 is non-dumpable before the child
exists, so the same-UID application cannot reopen either descriptor through
procfs. A separate trusted evidence unit probes post-frame readiness,
atomically publishes the root-owned evidence file, emits
`TD-JAIL-FIXTURE-BOOT-READY`, then atomically publishes a distinct completion
record that releases the autotest greeter. Its own bounded one-second probe
loop covers the second fixture cold-start attempt without inheriting td-svc's
exponential restart backoff. The greeter's separate allowance also includes
the first fixture ready timeout that can elapse before the evidence unit starts.
Deployment success does not depend on the fixture or its mutable state. Once
deployment health has finished, missing evidence or completion releases the
greeter when that complete bounded allowance expires, so
QEMU reports the missing marker instead of waiting for the full boot ceiling.
The fixture keeps its stdout and stderr on `/dev/null`, so it cannot forge the
console marker. Confinement failures instead use phase-specific exits 70--74
for status/capability, procfs/host-boundary, mount, loopback, and filesystem
grant verification;
PID 1 reports the raw application status through td-jail's bounded diagnostic.
The fixture is deliberately both autostarted and present in the launcher, so a
button press may create a second instance over the same state roots. This rung's
client leaves no persistent state on normal completion; a stateful application
must declare and
enforce its single-instance or multi-profile policy before using both paths.

Probe mode alone overlays one read-only executable bind at
`/tmp/td-jail-reaper-probe` after mounting `/tmp` noexec. It is the current
trusted `td-jail` inode, source-identity checked before the old root is detached,
and exists only long enough to prove that PID 1 naturally reaps a direct child
and its orphan, terminates and reaps a second long-lived orphan through the
production survivor-cleanup path, then force-kills and reaps a third through
the same hard-phase implementation. Application mode never creates that mount,
so its entire `/tmp` remains noexec.

Every bind is read-only unless marked **rw**, and a failed read-only remount
on a load-bearing bind is fatal, never degraded:

```
 0  CLOSE every inherited descriptor above 2, and REPLACE 0/1/2 with
        /dev/null or the caller's declared pipes. See "descriptors and
        the terminal" below: this step is not tidiness, it is the step
        without which every other one is decoration.
 1  mount MS_REC|MS_PRIVATE on /
 2  tmpfs <newroot>, mode 0755, nosuid/nodev where compatible
 3  /usr   <- runtime deploy files/            ro, nosuid, nodev
 4  /app   <- app deploy files/                ro, nosuid, nodev
 5  /bin /sbin /lib /lib64 -> symlinks into usr/
 6  /etc   tmpfs, populated selectively from the runtime's usr/etc, then
        immutable; over-written per instance: passwd, group, hosts,
        hostname, machine-id, resolv.conf, nsswitch.conf,
        ssl/certs/ca-certificates.crt
 7  extension mounts at authenticated extension-point directories
 8  /proc  mount point prepared by stage 1; after step 15, stage 2 mounts
        a fresh procfs for the namespace where it is PID 1, before the
        pivot while the old procfs is still fully visible (required by
        the kernel's unprivileged-userns `mount_too_revealing` rule),
        nosuid/nodev/noexec, then masks sys, sysrq-trigger, irq, bus,
        acpi, scsi, kcore, keys,
        timer_list, sched_debug, latency_stats -- as step 9 masks, and
        for step 9's reason. Most are already closed by DAC here (uid 0
        is unmapped by the identity map), but this section enumerates
        rather than resting on an incidental permission, and it does
        enumerate for /sys.
 9  /sys   RECURSIVE-BIND of the host /sys, read-only; debug, security,
        bpf, firmware AND fs/cgroup masked by over-mounting each with an
        empty ro tmpfs. cgroup is the one an earlier draft left off and
        it is not decoration: §P delegates a subtree that uid 1000 may
        WRITE, so an unmasked view lets the app move its own pid into a
        sibling cgroup and leave the memory.max §P put it under -- the
        cap defeated with memory.events reporting nothing. It is also a
        submount of a recursive bind, so the read-only remount below
        applies to it and the mask is what makes that moot.
        A FRESH `mount -t sysfs` is NOT possible here: the kernel refuses
        it in a user namespace that does not also own the network
        namespace, and `shared=network` apps (Firefox) omit CLONE_NEWNET.
        Bind-and-mask is therefore the only form that works for both
        cases, so it is the only form used.
10  /dev   tmpfs 0755, nosuid/noexec, containing ONLY bind-mounted host
        nodes:
        null zero full random urandom.  NO /dev/tty unless a policy
        explicitly asks for it — see "descriptors and the terminal"
        /dev/pts  devpts newinstance,ptmxmode=0666,mode=0620,gid=1000
        /dev/ptmx -> pts/ptmx
        /dev/shm  tmpfs rw, 512 MiB ceiling
        /dev/fd -> /proc/self/fd, stdin/stdout/stderr symlinks
        NO fb0, NO input/*, NO kmsg, NO disks, NO /dev/snd (the app's only
        route to sound is td-audio's socket — §K)
        `devices=dri` is PARSED and REFUSED with a named diagnostic from
        the first landing, so honouring it later (binding renderD128 only,
        never card0) is a policy-table flip, not a parser change (§M)
11  /tmp, /var/tmp   tmpfs rw 1777, noexec, 256 MiB ceiling each;
        /var/lib, /var/cache  empty tmpfs
12  /run   tmpfs, 64 MiB ceiling;  /run/user/1000 mode 0700
        wayland-0 <- bind, when sockets=wayland
        bus       <- bind, ALWAYS (the broker is the policy, not the mount)
13  /.flatpak-info  <- ro bind of a file on a jail-created tmpfs, written
        and remounted ro BEFORE pivot_root;  /run/flatpak-info -> it.
        NOT a file under the rw $HOME bind of step 14: a read-only bind
        exposes the inode's current contents, so an app that could
        rewrite the source would rewrite what every reader sees. Nothing
        AUTHENTICATES with it (§E says why), so this is about the app
        not lying to itself and to host tooling rather than about a
        boundary -- but a file the reader can edit is not worth writing.
14  $HOME (/home/td)  <- bind of ~/.td/app/<name>/home         rw
        /home/td/.config       <- bind of  .../<name>/config  rw
        /home/td/.cache        <- bind of  .../<name>/cache   rw
        /home/td/.local/share  <- bind of  .../<name>/data    rw
        /home/td/.local/state  <- bind of  .../<name>/state   rw
        (the five siblings of §B.4, each at the XDG path the
        environment names -- binding their PARENT as $HOME instead
        would leave all five unreachable and every XDG_* pointing at
        a fresh empty dotdir, which an earlier draft of this step did)
    Each granted --filesystem is a separate bind at its declared jail target;
    an absolute target need not be below $HOME.
15  stage 1 makes exactly `CAP_SYS_ADMIN` inheritable and ambient, drops
    every bounding bit while `CAP_SETPCAP` remains effective, reads the
    bounding set back empty, then spawns stage 2. Stage 2 reads back
    effective/permitted/inheritable/ambient = exactly `CAP_SYS_ADMIN`
    and bounding = empty; mkdir <newroot>/oldroot (pivot_root requires
    put_old to EXIST and to
    be under new_root; newroot is a fresh tmpfs, so nothing else makes it)
    mount the fresh procfs at <newroot>/proc while the old procfs is
    still visible;
    pivot_root(newroot, newroot/oldroot); chdir /; umount2(/oldroot,
    MNT_DETACH); rmdir /oldroot; perform step 8's procfs masks
16  PR_CAP_AMBIENT_CLEAR_ALL; capset permitted/effective/inheritable
    empty; then read back capget, PR_CAPBSET_READ per bit, and
    PR_CAP_AMBIENT_IS_SET -- capget does NOT return the ambient set, so it
    is the one that needs its own question
17  PR_SET_NO_NEW_PRIVS, then PR_GET_NO_NEW_PRIVS readback
18  seccomp(SECCOMP_SET_MODE_FILTER, 0, &prog)
19  READBACK: /proc/self/status says NoNewPrivs: 1 and Seccomp: 2,
    or stage 2 refuses to spawn anything
20  spawn argv; wait4(-1) as the namespace's reaper until the direct entry
    exits; preserve that status; kill(-1, SIGTERM), poll/reap for at most two
    seconds, then repeatedly kill(-1, SIGKILL) while polling/reaping for at
    most two more seconds; if PID 1 still has not observed ECHILD, fail and
    let PID namespace teardown provide the final hard stop
```

The two deadlines bound PID 1's polling, not kernel completion for a task in
uninterruptible sleep. Backgrounding work does not detach it from this
lifecycle; once the direct entry exits, every remaining descendant is a
survivor.

`pivot_root` and not `chroot`: chroot is escapable by design for a
process that can chdir out through a descriptor, and it keeps the old
root alive in the mount table. The `MNT_DETACH` is what actually removes
the host filesystem from the app's world.

**Descriptors and the terminal — two escapes the earlier draft left
open, and the second was a claim rather than an oversight.**

*An inherited descriptor outlives `pivot_root`.* Detaching the old root
removes it from the mount *table*; it does nothing to a descriptor
already open on it. A directory fd inherited from the launcher's caller is a
working handle to the host filesystem — `openat` from it walks anywhere,
and `fchdir` to it puts the process's cwd outside the new root, which is
precisely the chroot escape the paragraph above says `pivot_root` avoids.
The same is true of an inherited socket, and worse of an inherited
terminal. So **step 0 closes them**, and it must run in *stage 1 before
it spawns stage 2*, so that nothing unintended is ever in stage 2's
table: enumerate `/proc/self/fd`, close everything above 2 that the jail
did not itself create, and replace 0/1/2. `CLOEXEC` alone is not
sufficient — the caller controls whether its descriptors carry it, and a
jail whose confinement depends on its invoker's hygiene is not a jail.
This is one of the few places where the correct implementation is a loop
over `/proc/self/fd` rather than anything clever, and where getting it
wrong is invisible in every test that does not deliberately leak a
descriptor into the launch. §H owes it exactly that test.

**"That the jail did not itself create" is doing real work in that
sentence, and stage 1 holds exactly two such descriptors — which fixes
their order.** The close loop runs FIRST, and the pipe is created after
it; a pipe made before the sweep is a pipe the sweep closes, and stage 2
then reads EOF and concludes its parent is dead before it has done
anything. The broker connection is the other, and it goes the other way:
it must be closed BEFORE stage 2 is spawned, and not merely marked
`CLOEXEC`. A safe `Command` does exec stage 2 and close-on-exec is a
backstop, but closing the broker connection before spawn makes the
authority absent even if process creation fails before that boundary.
Leaking it would hand the confined side the channel that completes
registrations, which is the one channel in this design whose whole
authority is that only stage 1 has it. So: sweep,
create the pipe, register, close the broker connection, spawn. Stage 2
performs no sweep of its own — the pipe is the only descriptor it is
given above stdio, and a second blind loop would have to exempt it.

*The "no controlling terminal" claim was false.* Surface #9's original
exclusion said an app got no controlling terminal and therefore needed no
`setsid(2)`/`TIOCSCTTY`, but a controlling terminal is a property of the
**session**. Before the fixes in this section, stage 2 inherited the launcher's
session, the mount plan bound `/dev/tty`, and inherited descriptors survived.
An app could therefore reach the operator's terminal. Removing `/dev/tty` and
closing every inherited descriptor fixed the direct paths; the
application-containment bootstrap fixes the signal and future-reacquisition
path.

The remaining third is now closed before authority resolution. A later-born
stage 1 first recognizes one special safe input: a no-terminal process group
whose id is the exact waiting parent's pid. It stays in and reads back that
group so td-svc's recorded stop containment continues to cover stage 1, stage
2 and the application. Otherwise it enters and proves its own session while
its waiting parent keeps the launch lifecycle. Terminal-generated signals and
console containment do not reach that detached tree after the readback; a
console snapshot that names stage 1 before detachment may still deliver its
already-selected signal, and the parent-death/cleanup path handles it.
`CommandExt`'s stable `process_group` is only `setpgid` and would not provide
the terminal boundary.

**`devices=tty` remains refused.** The containment bootstrap prevents
accidental access; it does not define how a terminal application deliberately
acquires a fresh controlling terminal. That policy still needs a documented
`TIOCSCTTY` request, a device grant and readback rather than binding the
operator's terminal.

*A read-only bind is not recursive, and neither is a read-only remount.*
Binding a granted directory `:ro` makes that mount read-only and says
nothing about mounts nested underneath it, which come across writable
under `MS_REC` — so `--filesystem=$HOME` over a home directory with a
removable-media or network mount beneath it grants write access the
policy did not. `mount_setattr(AT_RECURSIVE)` is the one-call answer and
is deliberately not on surface #9 (the filter denies it, and adding it
would be an amendment). So the jail **enumerates**: after each recursive
bind it reads `/proc/self/mountinfo`, finds every mount whose root is at
or under the target, and remounts each read-only from the deepest
outward, failing the launch if any remount fails. That is the same
fail-closed rule the paragraph above the plan states, applied per
submount rather than per grant. The test is a grant over a directory with
a `tmpfs` mounted inside it, which is cheap to set up and is the only way
this bug is ever caught.

**`mknod(2)` is never called.** A user namespace cannot mint real device
nodes anyway; binding the host's nodes is both how that is avoided and a
guarantee the jail cannot produce a node the host does not already have.

**Capabilities are dropped and read back** (steps 15–16). Mount setup needs
namespace-local `CAP_SYS_ADMIN`; the app must not inherit it. Each
`capset` is followed by `capget` and each `PR_CAPBSET_DROP` by
`PR_CAPBSET_READ`, with any surviving capability fatal. The design does
not rely on `exec` incidentally clearing anything. The stage-1 bridge
sets only `CAP_SYS_ADMIN` inheritable and raises only that ambient bit,
then drops and reads back the bounding set while `CAP_SETPCAP` is still
effective. Stage 2 reads effective, permitted, inheritable, ambient and
bounding state before using `CAP_SYS_ADMIN`, then step 16 explicitly
removes and re-reads the four remaining sets.

**The ORDER across those steps is not arrangement, it is the difference
between working and `EPERM`**, and the step list spells it out because a
plausible reading of "drop the capability sets" fails: `PR_CAPBSET_DROP`
requires `CAP_SETPCAP` in the caller's EFFECTIVE set. Stage 1 therefore
drops and reads back the complete bounding set before exec reduces stage
2 to `CAP_SYS_ADMIN`; asking stage 2 to do it would fail every drop.
After the mounts, stage 2 explicitly clears and reads back the ambient
set before emptying permitted/effective/inheritable. The explicit clear
matters because `capget(2)` never returns ambient state, so relying on
the permitted-set side effect would leave the only set that outlives an
`exec` as the only one nothing observes.

Steps 16–19 are four readbacks in a row and that is the point: nothing
observable distinguishes a jail whose filter did not load from one whose
did — until an attack, which is the wrong place to learn it. Same
argument as `losetup` re-reading its read-only flag out of sysfs.

### Filesystem grants

Implemented for the builder-authenticated immutable defaults:
`xdg-download`, `xdg-documents`, `xdg-pictures`, `xdg-music`, `xdg-videos`,
`xdg-desktop`, subpaths of the launching user's real home, and explicit
absolute paths, each with `:ro`/`:rw`, and `:create` only below that real home.
`~/` resolves from the real home but is mounted under the corresponding
`/home/td` path; it does not name the private home while authority is resolved.
td creates every granted XDG directory on first use, even without an explicit
`:create`, since a fresh image has no `~/Downloads`; explicit `:create` is only
needed for a home-relative spelling. Creation walks one component at a time
and refuses links; a missing source without implicit or explicit creation, or
any invalid component, refuses the whole launch. XDG names are directory
contracts and refuse an existing non-directory; existing explicit sources may
be regular files or directories.

Deny is conservative: if a denied source or jail target intersects an allowed
grant, the whole allowed grant is removed before `:create` can mutate the
host, so denying a child cannot expose or create it through a granted parent.
Two spellings that canonicalize to the same source and target merge with
read-only winning. Deny and allowed-grant comparisons include every visible
mount identity below each source. A distinct bind alias or any other overlap
between allowed sources or targets refuses the launch rather than relying on
mount order. The current rung does not read a mutable per-user override; that
lifecycle remains pending.

**Refused, deliberately stricter than upstream:** `filesystem=host`,
`filesystem=home`, `/`, `/usr`, `/bin`, `/app`, `/run`, `/proc`, `/sys`,
`/tmp`, `/home`, `/root`, `/var/home`, `/var/root`, `/var/run`, `/var/tmp`,
`/etc`, `/boot`, `/.flatpak-info`, the flatpak repo itself, td's
`/var/lib/td` system state, socket paths, and device trees. The configured
package and application-state roots are also refused after source resolution,
whatever their spelling. An app that genuinely needs blanket home access gets
a reviewed per-app override rather than a default.
Sources are canonicalized before the app starts; source type, device and inode
are checked before and after the bind. Regular-file grants require a link count
of exactly one throughout the transition, conservatively refusing hardlink
aliases that path and mount identities cannot distinguish. Canonical paths are
not the only alias. The single-link rule applies to an explicit regular-file
grant root; a directory grant authorizes entries reachable inside that tree.
The source and every nested mount are also compared by mountinfo device/root
identity with every visible mount below the reserved trees and all users'
homes. An alias of the launching user's own real-home subtree remains
admissible. Every source and target is preflighted before any `:create`
mutation, then revalidated afterward. Targets in the
fresh root are checked component by component and may not replace or overlap
`/app`, `/usr`, `/run`, `/proc`, `/sys`, `/dev`, `/tmp`, `/var/tmp`, `/etc`,
`/boot`, `/.flatpak-info`, `/oldroot`, `/root-write-probe`, the private-home
root, or its fixed config, cache, data and local-state mounts. Directory grants
are recursive binds. Every mount
at or below a granted target is enumerated from mountinfo and remounted
`nosuid,nodev,noexec`; a read-only grant additionally makes every nested mount
read-only, deepest first. Stage 1 and stage 2 independently read those rows
back. Stage 2 exactly enumerates every fresh scaffold outside the dynamic
private home, stopping at each declared grant root. The shipped fixture proves
read-write Downloads, read-only Pictures, and a read-only regular-file grant.
The two XDG sources are made distinct self-bind mounts before `switch_root`:
they retain the graphical user's persistent storage and normal capacity while
their mount roots no longer alias the reserved state subtree on the same Btrfs
volume. The regular-file source lives on a separate dedicated tmpfs and is
bind-mounted at the fixture's existing `/var/td-jail-fixture-file` path. The
fixture also recursively binds
a read-only
`/mnt/td-jail-fixture-pictures` source containing a separately writable nested
tmpfs. The client requires the root and nested directory to have different
devices before testing both as read-only, so omitting `MS_REC` cannot satisfy
the oracle. The fixture root is `1777` on immutable EROFS so its write refusal
cannot be attributed to directory permissions. Deployment initialization
mounts the two self-binds and two fixture tmpfs instances once before
`switch_root`; it is not a restartable root service following mutable user
state. A process killed in the create/unlink window of stage 2's writable
grant probe can leave one randomly named `.td-jail-write-probe-*` file in that
host directory. The fixture client has the same window for one
`.td-jail-rw-*` file in Downloads. The application user can remove either;
normal error paths unlink them.

### Environment

Cleared, then rebuilt — no ambient `TD_*`, host `PATH`, compiler, store
path or build variable reaches the app:

```
HOME=/home/td  USER=td  LOGNAME=td  SHELL=/bin/sh
PATH=/app/bin:/usr/bin
XDG_RUNTIME_DIR=/run/user/1000  XDG_DATA_HOME=/home/td/.local/share
XDG_CONFIG_HOME=…/.config  XDG_CACHE_HOME=…/.cache  XDG_STATE_HOME=…/.local/state
XDG_DATA_DIRS=/app/share:/usr/share   XDG_CONFIG_DIRS=/app/etc/xdg:/etc/xdg
XDG_SESSION_TYPE=wayland  XDG_CURRENT_DESKTOP=td  XDG_SESSION_DESKTOP=td
WAYLAND_DISPLAY=wayland-0
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
FLATPAK_ID=<id>  container=flatpak
GDK_BACKEND=wayland  GTK_A11Y=none
```

plus the manifest's `[Environment]` group, then a **per-runtime-major**
software-rendering policy table (§G) rather than one global set, since
each yearly runtime rebases Mesa and GTK. `LD_LIBRARY_PATH` is absent
unless authenticated extension metadata requires one.

### The seccomp filter

**A Flatpak-derived, deliberately stricter deny list, not an allow list.**
This diverges from td's instincts and is argued rather than slipped in:
an allow list over the runtime glibc's whole syscall surface breaks every
time that glibc updates — a new `*_time64` or `statx` variant appears and
every app dies — and td does not control that glibc, Freedesktop SDK does. The deny
list is the part upstream has kept stable for a decade, it is small
enough to roster, and td pins it the way it pins ioctl requests: by
value, in one table, emitted by one function.

Return action is `SECCOMP_RET_ERRNO(EPERM)`, not `KILL`, because apps
*probe* these syscalls and a kill turns a feature probe into a dead app.
Two cases return `KILL_PROCESS`: a wrong `arch`, and an x32 syscall
number. `clone3` returns **`ENOSYS`** — its flags live in a struct cBPF
cannot read, and `ENOSYS` makes glibc fall back to `clone`, where the
flag check applies; `EPERM` would be believed and break thread creation.

| rule | reason |
|---|---|
| `arch != AUDIT_ARCH_X86_64` (0xC000003E) → KILL | a filter keyed on numbers is meaningless under another table |
| `(nr & 0xC0000000) == 0x40000000` (`__X32_SYSCALL_BIT` set, sign bit clear) → KILL | the x32 ABI aliases numbers past the filter. **TWO bits, not one, and not a magnitude compare** — both simpler forms kill `-1`, and review caught the second one after the first had been corrected to it. `nr >= 0x40000000` kills it because `0xFFFFFFFF` compares greater; `nr & 0x40000000` kills it because `0xFFFFFFFF & 0x40000000` is *also* nonzero. An x32 number is a small positive number with bit 30 set, so bit 31 is clear, and the two-bit mask names exactly that set. An invalid syscall number is something libraries genuinely probe with, and the kernel's own answer to it is `ENOSYS`; turning that into `SIGSYS` is a filter that kills for something the ABI permits. The interpreter test below asserts `-1` survives, which is what would have caught either draft |
| `socket` with `arg0` outside `{AF_UNIX, AF_INET, AF_INET6, AF_NETLINK}` → EAFNOSUPPORT | Flatpak's family filtering is the starting point, but td does not carry its `AF_UNSPEC`, `AF_CAN`, or `AF_BLUETOOTH` cases because td has no permission that needs them. Without this rule a jail reaches `AF_VSOCK`, `AF_XDP`, `AF_PACKET` and the rest of a large and unevenly audited surface. `EAFNOSUPPORT` is the answer a kernel without the family gives, and libraries already handle it |
| `ptrace`, `perf_event_open` → EPERM (dropped under `allow-devel`) | cross-process inspection; kernel attack surface |
| `userfaultfd` → EPERM | pause-kernel-paths primitive used by exploit chains |
| `personality` with arg0 outside `{0, 0xFFFFFFFF}` → EPERM | `READ_IMPLIES_EXEC` and historical bypasses. **`0xFFFFFFFF` must be allowed**: it is not a personality to set but the standard *query* form, which returns the current value and changes nothing, and glibc and several runtimes use it during startup. An earlier draft denied every nonzero argument and would have `EPERM`ed a read-only question |
| `ioctl` with `(arg1 & 0xFFFFFFFF) == TIOCSTI (0x5412)` → EPERM | terminal input injection. **The mask is load-bearing** — the kernel truncates the request to 32 bits, so an unmasked compare is bypassed by `TIOCSTI \| 1<<32`, the exact historical filter bypass |
| `ioctl` with `(arg1 & 0xFFFFFFFF) == TIOCLINUX (0x541C)` → EPERM | VT injection, same family |
| `clone` with `arg0 & CLONE_NEWUSER` → EPERM | see the open question below |
| `clone3` → ENOSYS | as above |
| `unshare`, `setns`, `chroot` → EPERM | namespace creation and joining |
| `mount`, `umount`, `umount2`, `pivot_root`, `move_mount`, `open_tree`, `open_tree_attr`, `fsopen`, `fsconfig`, `fsmount`, `fspick`, `mount_setattr` → EPERM | the whole mount surface, old and new API — filtering only the old one is a hole; `open_tree_attr` is the Linux 7.1 addition |
| `open_by_handle_at` → EPERM | the Shocker escape primitive |
| `add_key`, `request_key`, `keyctl` → EPERM | shared kernel keyring |
| `move_pages`, `mbind`, `get_mempolicy`, `set_mempolicy`, `migrate_pages` → EPERM | NUMA policy on other processes' pages |
| `kexec_load`, `kexec_file_load`, `swapon`, `swapoff`, `reboot`, `sethostname`, `setdomainname`, `init_module`, `finit_module`, `delete_module`, `acct`, `quotactl`, `syslog`, `uselib`, `vhangup`, `modify_ldt`, `ustat`, `sysfs`, `_sysctl`, `create_module`, `get_kernel_syms`, `query_module`, `nfsservctl`, `getpmsg`, `putpmsg`, `afs_syscall`, `tuxcall`, `security`, `set_thread_area`, `get_thread_area`, `epoll_ctl_old`, `epoll_wait_old`, and `vserver` → EPERM | privileged or the exact named obsolete x86-64 table slots; denied so the answer never depends on capability arithmetic |
| `bpf`, `io_uring_setup`, `io_uring_enter`, `io_uring_register`, `pidfd_getfd`, `process_vm_readv`, `process_vm_writev` → EPERM | **td additions beyond Flatpak**, each recorded as such: bpf and io_uring are the two largest post-2015 kernel attack surfaces, `pidfd_getfd` steals descriptors, and `process_vm_*` is ptrace by another number |

This is a policy lineage, not a byte-for-byte compatibility claim. Current
Flatpak also admits `AF_UNSPEC`, handles `personality` differently, and returns
`ENOSYS` for newer mount APIs. td keeps only the two query-only personality
values, returns `EPERM` across the mount surface, and adds the
`userfaultfd`, `open_by_handle_at`, privileged/obsolete, bpf, io_uring,
`pidfd_getfd`, and `process_vm_*` denials rostered above. Those stricter deltas
are deliberate and tested rather than inherited accidentally.

Everything else is allowed, including `seccomp(2)` and
`prctl(PR_SET_SECCOMP)` — Firefox installs its own filters in every
content process, and filter stacking under `NO_NEW_PRIVS` is exactly what
the kernel provides. A nested filter can only narrow.

Linux 7.1's `uretprobe`=335 and `uprobe`=336 entries are not ordinary
application syscalls and are not claimed as filter rules. The pinned kernel
unconditionally treats both as seccomp exceptions before running cBPF so an
external tracer can inject its kernel-owned trampoline; a direct `uretprobe`
call is rejected with `SIGILL` and a direct `uprobe` call with `ENXIO`.
Putting either number in the array would describe a denial the kernel does not
enforce. The ordinary tracing entry points remain denied through
`perf_event_open`, `ptrace`, and `bpf`.

**Expressing BPF with no dependencies.** `struct sock_filter` is
`{u16 code; u8 jt; u8 jf; u32 k}` — 8 bytes, built by one `insn()` whose
bit placement is a tested function, because a swapped `jt`/`jf` is a
well-formed program that jumps the wrong way and nothing at the call site
shows it. The program is a `const` array whose length is part of its Rust
type, so `sock_fprog.len` is derived from the array rather than written
beside it: `len` is the kernel's read bound over the pointer, and a `len`
larger than the array is an out-of-bounds kernel *read* from code the
compiler reads as safe — the `pollfd`/`nfds` argument in the read
direction. `seccomp_data` offsets are pinned constants (`nr`@0,
`arch`@4, `args[k]`@16+8k) and every argument load reads the low 32-bit
word, which is correct for every rule above by construction — and for the
two ioctls the truncation *is* the point.

**Testing a filter you cannot test by reading**, in three layers:

1. A ~120-line cBPF **interpreter** (test fixture, not shipped) executes
   the exact compiled array against synthetic `seccomp_data` for every
   rostered number, both ioctls with and without high bits set, `clone`
   with and without the flag, `clone3`, a wrong arch, an x32 number,
   `-1` and a second negative number — the case two drafts of the x32
   rule got wrong — and a page of allowed syscalls. This pins the
   program's *meaning* against
   its bytes. Three cases are there because a review found the rule
   above them wrong, and they are the ones a reader would not think to
   write: a **negative** `nr` (which must reach the kernel and return
   `ENOSYS`, not `SIGSYS`), `personality(0xFFFFFFFF)` (allowed) beside
   `personality(READ_IMPLIES_EXEC)` (denied), and an x32 number whose
   low bits collide with an allowed x86-64 one.
2. A **declared, non-shipped target probe recipe** — built by td's GCC, never
   in the closure — installs the production filter and issues the real
   syscalls in children, checking exact errno and termination status.
   This is where `ptrace` and `TIOCSTI` get proved, since safe td code
   cannot issue them; it is why the probe is a C-side test binary rather
   than a `cfg`-gated widening of `sys.rs`.
3. The **QEMU boot oracle** builds that helper-only recipe, rather than the
   host-policy smoke-test recipe, and runs the probe on the booted image,
   because layer 1 proves the program and layer 2 proves *a* kernel
   loaded it — only the target kernel proves the pinned config supports
   every piece.

Plus negative tests: omitting `NO_NEW_PRIVS` must make installation fail,
and a corrupted-jump or wrong-length program must be refused before
`seccomp(2)`. A build host that inherited no-new-privileges but no filter may
skip only that impossible negative leg. If it already has a seccomp filter,
the probe validates the bounded artifact but skips host behavior: filters
stack and the outer policy could change any result. The QEMU invocation passes
no allowance, requires both states to begin at zero, and always runs the exact
behavior checks on the target kernel.

### Firefox's nested sandbox

**The two source designs disagreed here; the resolution below required an
experiment.** One holds that upstream flatpak denies `clone(CLONE_NEWUSER)`
and that Firefox detects this and runs its content sandbox in fallback
mode, so td should match upstream exactly and inherit a decade of
ecosystem testing. The other holds that Firefox must be allowed to build
its nested sandbox, and adds a second reviewed `BROWSER_FILTER` profile
permitting namespace syscalls while still blocking ptrace, perf, bpf and
the ioctls.

The security argument for allowing it is real — a nested user namespace
grants capabilities only relative to itself, the host root and store are
absent, and stacked filters cannot be removed — but it is also a
per-app relaxation of the sandbox for the single most-attacked program on
the image, which is precisely where a relaxation is most expensive.

**One thing about that framing is wrong and a review named it: this is
not a free choice between two equally complete outcomes.** Filters
*compose* — a nested filter can only narrow — so whichever profile td
picks, Firefox's own sandbox must still INSTALL. Its content processes
call `seccomp(2)` and `prctl(PR_SET_SECCOMP)` (allowed above, and that
is why), and under the deny profile they additionally call
`clone(CLONE_NEWUSER)` and get `EPERM`. What "match upstream" actually
buys is Firefox's *fallback* content sandbox rather than none; what it
costs is the namespace layer of that sandbox. So the honest statement of
the choice is **"Firefox's content sandbox at full strength, or at
fallback strength"** — not "one filter or two", and never "Firefox's
sandbox on or off". A configuration in which Firefox's inner sandbox
fails to install at all is a bug under either profile, and the proof must
say so: **§H's Firefox item must assert `about:support`'s sandbox section
reports the expected level for EVERY process class** — content, GPU,
socket, utility — rather than merely that the processes exist. Without
that assertion, "Firefox runs" is compatible with Firefox having quietly
disabled its own defences inside td's jail, which is the one outcome
neither design wants and which nothing else here would detect.

**Resolution — E2, 2026-08-25:** td ships the one standard deny filter; there
is no `BROWSER_FILTER`. The development host had no seccomp filter and
`unshare -Ur true` succeeded, so the kernel and host policy supported an
unprivileged user namespace. Inside the exact pinned Firefox 154.0 deploy,
the stock Flatpak sandbox reported `NoNewPrivs: 1`, seccomp mode 2 with one
filter, and the same command failed `EPERM`. Firefox printed
`CanCreateUserNamespace() clone() failure: EPERM`, continued running, and
installed its own seccomp filters with thread synchronization in eight child
processes observed by `MOZ_SANDBOX_LOGGING=1`.

The same stock run's `about:support` report said Seccomp-BPF and seccomp thread
synchronization were true, user namespaces were false, content and media
plugin sandboxing were true, and both the configured and effective content
sandbox levels were 6. Its headless process list contained Socket, Fork
Server, RDD, and Extension processes. This is the upstream fallback outcome
the standard filter was intended to match: denying a nested user namespace
does not disable Firefox's content seccomp sandbox.

That bounded experiment selects the filter; it does **not** discharge §H's
stronger oracle. A headless `about:support` page does not exercise ordinary
web content, GPU, socket, and utility classes together under td-jail. The
Firefox milestone must still open ordinary content and assert the expected
sandbox result for every process class that the pinned build creates.

### `UNSAFE.md` surface #9 (target-state draft)

The normative `UNSAFE.md` roster grows with the implementation. Rung 9
landed the namespace, descriptor, mount and capability-bridge calls, and
rung 10 adds `wait4(2)` with final capability removal and its reparented-
orphan oracle. Rung 11 adds `seccomp(2)` plus no-new-privileges operations,
the exact compiled policy and its interpreter, build-host, and target-kernel
oracles. The remaining calls below are not authorized until their own launch
and policy rungs land with callers and tests.
The quoted block is the completed target mirrored by the implemented roster in
`UNSAFE.md`; `transition.rs` is the sole caller.

> ## 9. `td-jail` — the application sandbox
>
> The `td-jail` application sandbox, whose one `syscall5` body in
> `td-jail/src/sys.rs` carries EXACTLY FOURTEEN syscalls — `unshare(2)` with
> a value-pinned namespace set, `close(2)` for inherited descriptors,
> `mount(2)`, `umount2(2)` and
> `pivot_root(2)` for the validated mount plan, `capset(2)` with
> `capget(2)` for the one-capability exec bridge, capability drop and
> their readbacks, `prctl(2)` with
> EIGHT value-pinned operations (`PR_SET_NO_NEW_PRIVS`=38,
> `PR_GET_NO_NEW_PRIVS`=39, `PR_SET_PDEATHSIG`=1, `PR_CAPBSET_DROP`=24,
> `PR_CAPBSET_READ`=23 — the readback mount-plan step 16 requires,
> which an earlier draft mandated in the plan while forbidding it in the
> roster — `PR_SET_DUMPABLE`=4, `PR_GET_DUMPABLE`=3, and
> `PR_CAP_AMBIENT`=47 with its own three pinned
> sub-operations, `PR_CAP_AMBIENT_RAISE`,
> `PR_CAP_AMBIENT_CLEAR_ALL`, and `PR_CAP_AMBIENT_IS_SET`.
> That last one is a correction: a draft left 47 out on the grounds that
> clearing the permitted set clears the ambient set "which the `capget`
> readback confirms", and `capget(2)` does not RETURN the ambient set —
> it carries effective, permitted and inheritable only, so the one
> readback in step 16 that had no way to observe what it asserted was
> the one about the set that survives an `exec`. `PR_SET_SECCOMP` is
> still deliberately absent), `seccomp(2)` with
> ONE pinned operation (`SECCOMP_SET_MODE_FILTER`=1, flags 0), `wait4(2)`
> and `kill(2)` for the reaper, `prlimit64(2)` for §P's per-process
> backstop — which is here because `std` exposes NO rlimit API at all,
> and because setting only the soft limit is a bound the application may
> raise on itself, so both halves of the pair are written before `exec`
> and read back through the same call — `setsid(2)` for the application
> transition and cgroup cleanup processes' controlling-terminal detachment,
> and `ioctl(2)` with exactly TWO
> value-pinned requests
> (`SIOCGIFFLAGS`=0x8913, `SIOCSIFFLAGS`=0x8914, argument a pinned 40-byte
> `ifreq`, interface name pinned to `lo`) — reached only from `transition.rs`,
> the module that executes a mount plan. Two of those are amendments the
> maintainer's premise bought rather than avoided, and each replaced
> something worse:
> `kill(-1, SIGTERM)` then a bounded reap then `SIGKILL` replaces relying
> on the kernel's namespace teardown, which is correct for CLEANUP —
> PID 1 exiting does SIGKILL the namespace — but cannot give a
> reparented descendant the chance to exit gracefully; and the `ifreq`
> pair replaces leaving `lo` down. This is the builder's namespace jail
> arriving on the target side, and the port is a copy of
> `builder/src/sys.rs`'s shapes
> with the grandfathered parts left behind: no `pre_exec` (the stage
> boundary is a spawned process, so the namespace work runs in ordinary
> post-`main` code, and `pre_exec` is itself an `unsafe fn` — taking it
> would be a second scoped allow of a different shape), and no `fork`
> (`Command` spawns stage 2, which IS PID 1 of the pid namespace
> unshared before it).
>
> **`getppid(2)` is deliberately NOT here, and the reason is a corrected
> mistake worth recording.** Two successive drafts gave the PDEATHSIG
> re-check to `/proc/self/stat`'s fourth field and then to `getppid(2)`,
> arguing the second was safer because `comm` may contain spaces and
> parentheses. Both are WRONG for this caller: stage 2 is PID 1 of a new
> PID namespace, its parent is in an ancestor namespace and therefore
> invisible, and both report **0 unconditionally** — a check that cannot
> observe what it exists to observe. A pipe held open by stage 1 answers
> it with no syscall at all (§A), which is why the roster shrank by one
> rather than swapping one entry for another.
>
> `prctl` and `seccomp` are operation-carrying syscalls the way `ioctl`
> is request-carrying, so the operations are the surface: each is pinned
> by value and the two entry points refuse anything outside their named
> arrays before issuing. The seccomp program is compiled-in policy,
> never caller data — a caller-supplied filter would make this contract
> a formality — and its instruction count is part of its Rust type, so
> `sock_fprog`'s length is derived from the array it bounds rather than
> written beside it. The `sock_filter` word layout and the
> length-beside-pointer pair are each built by one tested function,
> because a swapped `jt`/`jf` is a well-formed program that jumps the
> wrong way. Capabilities are dropped and READ BACK, no-new-privs is set
> and read back, and the filter is read back out of the fresh
> `/proc/self/status` (`Seccomp: 2`, `NoNewPrivs: 1`) before anything is
> spawned, because nothing observable distinguishes a jail whose filter
> did not load from one whose did — until an attack, which is the wrong
> place to learn it. `wait4(2)` is here because stage 2 is PID 1 of its
> namespace and must reap the orphans a targeted `Child::wait` cannot
> see — td-init's argument, one applet narrower.
>
> Deliberately NOT in that surface: `fork`/`execve`/`pre_exec` (safe
> `Command` plus the two-stage re-exec cover all three); `mknod(2)` —
> device nodes are BIND-MOUNTED from the host `/dev`, which a user
> namespace permits where `mknod` it refuses, and which cannot mint a
> node the host does not already have; `chroot(2)` (the old root is
> removed with `pivot_root` and `MNT_DETACH`, which chroot cannot do);
> `setuid`/`setgid`/`setgroups` (the map is identity and written through
> `/proc` as ordinary files — there is no setuid helper ANYWHERE in this
> design, a jail more privileged than its caller being the thing user
> namespaces exist to not need); `setns(2)` (the constructor never joins
> a namespace a caller supplied); `sethostname(2)` (the UTS namespace is
> unshared and the name inherited); `TIOCSCTTY` — **excluded for a corrected
> reason**: the earlier draft said an app gets no
> controlling terminal, which was false before the application bootstrap
> established containment. It remains excluded because no terminal policy
> exists: the jail removes `/dev/tty` and inherited terminal descriptors, and
> the bootstrap either proves a dedicated no-terminal supervisor group or
> creates a new session; both paths prove `tty_nr=0`. A real terminal policy
> would need a
> documented ioctl-request amendment rather than an assumption; `statfs(2)`
> (nothing in this design issues it — with packages in the image,
> nothing writes gigabytes at launch); `mmap(2)` (nothing here maps anything —
> but see §M, which anticipates that the GPU path WILL need a
> mapping-shaped amendment, of a different class from this
> syscall-instruction layer); and any `ioctl` request beyond the two
> `ifreq` ones — in particular no terminal or device control, the
> jail's other relationship to `ioctl` being that its FILTER denies two
> of them. A FIFTEENTH syscall, a third ioctl
> request, a ninth prctl
> operation, a fourth `PR_CAP_AMBIENT` sub-operation, a second seccomp
> flag, an arch
> beyond x86-64, or a caller-supplied BPF program is an amendment here;
> `td-jail/src/main.rs`'s confinement tests assert the roster and its
> FOURTEEN value-pinned numbers, the eight prctl and one seccomp operation
> values, the namespace and mount flag rosters, the whole `asm!` block
> including which register each argument lands in and that
> `options(nomem)` stays absent (the kernel reads the filter program and
> the mount strings through pointers and writes the capability and wait
> buffers back through others), that the crate names the unsafe lint
> exactly twice, that `jail.rs` is the wrappers' only caller, that no
> module aliases or imports out of `sys`, and that every fixed buffer
> length is pinned in the SHIPPED build. `sys.rs`'s own tests then issue
> what can be issued, because every assertion above is about source TEXT
> and a wrapper that returned `Ok(())` without issuing anything would
> satisfy all of them.

---

## D. `td-busd` — the D-Bus broker

One **session** bus at `/run/user/1000/bus`, parent directory 0700,
socket 0600. There is no system bus: nothing on td speaks one, and the
first thing that wants one is a design review rather than a config file.

`td-busd` also **absorbs `xdg-dbus-proxy`**. Upstream starts a filtering
proxy per sandbox and bind-mounts the proxied socket; td bind-mounts the
real broker socket into every jail and the broker enforces per-connection
policy itself. One process, one socket, no per-app proxy to supervise,
and the policy input is the same file upstream's proxy reads.

### Auth

```
C: \0AUTH EXTERNAL 31303030      ("1000" hex-encoded)
S: OK <32-hex-guid>
C: NEGOTIATE_UNIX_FD
S: AGREE_UNIX_FD
C: BEGIN
```

Only `EXTERNAL`. The hex identity must **resolve to** `SO_PEERCRED.uid` —
equality today, since every peer shares the session uid, but *stated as
resolution rather than equality because equality breaks under per-app
uids* (§L). A sandboxed app in a user namespace believes it is uid 1000
and sends that; `SO_PEERCRED`, read outside the namespace, reports the
mapped uid. Comparing the two for equality would drop every sandboxed
connection the day per-app uids land, and the failure would present as
"D-Bus stopped working" rather than as anything about identity. So the
comparison goes through the instance's registered mapping from the
outset, which costs nothing while the mapping is the identity.

Two related notes on what `EXTERNAL` does *not* require. It does not
require the broker's own uid to match the client's — the mechanism
verifies a claimed identity against the peer credential, and the server's
identity is not part of that check — so a broker running as a dedicated
uid can authenticate a uid-1000 client (§L revisits what that buys).
Empty
`AUTH EXTERNAL` may enter the `DATA` exchange (sd-bus's spelling). A
client that skips `NEGOTIATE_UNIX_FD` is refused any message carrying a
descriptor, per spec. `REJECTED`: `ANONYMOUS`, `DBUS_COOKIE_SHA1`, and an
identity that decodes and is not this peer's — non-numeric, signed, or
another uid. The connection ends on: a second `BEGIN`, a `BEGIN` before
the handshake completes or carrying an argument, lines over 4 KiB, more
than 16 auth commands. Unknown commands get `ERROR` without changing
state, and so does a command whose ARGUMENT cannot be read — an
unreadable hex identity claims nobody, so nothing is refused, and the
specification reserves `ERROR` for a peer that "did not understand the
arguments to the command" and requires the sender to "continue as if the
command causing the `ERROR` had never been received". `REJECTED` there
would end an attempt no one made.

### Messages

16-byte fixed header (endianness `l` or `B`, type, flags, version 1, body
length, nonzero serial, header-field array length), then `a(yv)` fields —
PATH, INTERFACE, MEMBER, ERROR_NAME, REPLY_SERIAL, DESTINATION, SENDER,
SIGNATURE, UNIX_FDS — then the body at 8-byte alignment. **A
client-supplied `SENDER` is rejected**; the broker inserts the
authenticated unique name.

The signature grammar covers `y b n q i u x t d s o g h a( ) a{ } v`.
Alignment: 1 for `y`/`g`/**`v`**; 2 for `n`/`q`; 4 for `b i u h s o` and
**`a` (the array's own u32 length prefix)**; 8 for `x t d`, structs and
dict entries. Arrays and variants were missing from an earlier draft of
that list, which is the kind of omission that does not fail loudly — a
parser that guesses an array's alignment reads a plausible length from
the wrong offset — so the roster is one table with a test per entry
rather than prose. Strings carry a u32 length and a NUL; signatures a u8
length and a NUL; arrays a u32 payload length then padding to element
alignment, **and the padding is counted outside the declared length**,
which is the second thing that list has to say and the first thing a
fresh implementation gets wrong.

**D-Bus marshalling is the only wire format this design implements.** An
earlier draft had to argue that it must not share code with a GVariant
reader — the two look alike and differ in every load-bearing detail,
alignment-with-padding versus framing offsets, `g` versus `s`, a message
endianness flag versus fixed byte order — and §B.6 removed the GVariant
reader entirely, so the argument is settled by there being nothing to
share with.

Bounds, each a named constant with a refusal test: 64 KiB header fields,
16 MiB body, nesting 32, signature 255, 64 descriptors per message, 64
queued descriptor attachments per recipient, 256 match rules and 64 KiB
of rule text per connection, 128 pending replies, 64 connections — the last **per
instance as well as globally**, since every other bound in that list is
per-connection and a single global one is a denial of service any jailed
app can perform by opening 64: the portal, the compositor's own clients
and every other application are then locked off the bus by an
application that did nothing but connect.

**An array is bounded by BYTES, and a flat element cap was a bug in a
draft of that list.** "1,000,000 array elements" reads as a sensible
anti-exhaustion bound and breaks ordinary desktop traffic: `ay` is how
D-Bus carries a blob, so a notification icon or a clipboard payload is a
byte array of millions of elements, well inside the 16 MiB body the same
line permits. The specification bounds arrays in bytes for exactly this
reason and says nothing about counts. So the byte limit is the bound —
already implied by the body cap and checked against the array's own
declared length — and a COUNT limit applies only where elements are
containers, which is where per-element work is unbounded and where
nesting depth alone does not bound it.
Malformed padding, non-normal booleans, invalid UTF-8, invalid paths or
names, integer overflow, missing mandatory fields, zero serials,
signature/body mismatch, and descriptor-count mismatch all disconnect the
sender.

**Queue limits are in BYTES, not messages, and an earlier draft's
"4,096 queued messages" was a denial-of-service written as a bound.**
Multiplied by the 16 MiB body limit beside it, that permits 64 GiB queued
per connection — and the memory is charged to `td-busd`, not to the app's
cgroup, so §P's caps do not touch it and the machine dies rather than the
attacker. A message count cannot bound memory when message size is
separately bounded; only bytes can. So: **a per-connection outgoing-queue
byte ceiling and a global one**, both named constants, with a message
count kept only as a secondary guard against many tiny messages. Reaching
the per-connection ceiling **refuses the SENDER with
`LimitsExceeded` and leaves the recipient alone** — amended from
"disconnects that connection", for the reason the landed section below
gives: the ceiling is one maximum message, so two of them back to back
exceed it whatever the recipient does, and the sender picks the moment.
Reaching the global one is a broker-level condition that disconnects the
largest consumer rather than refusing service to everyone, and is logged
as a distinct diagnostic because it means a policy elsewhere is wrong.
The test is a client that subscribes broadly and never reads.

### `org.freedesktop.DBus`

`Hello` (assigns `:1.N`; anything before it disconnects), `RequestName`/
`ReleaseName` with `ALLOW_REPLACEMENT`/`REPLACE_EXISTING`/`DO_NOT_QUEUE`
and a bounded owner queue, `ListNames`, `ListActivatableNames`,
`NameHasOwner`, `GetNameOwner`, `GetConnectionUnixUser`,
`GetConnectionUnixProcessID`, `GetConnectionCredentials` (carrying the td
extension `td.AppId` for sandboxed connections), `AddMatch`/`RemoveMatch`,
`GetId`, `Peer.Ping`, `Peer.GetMachineId`, minimal
`Introspectable.Introspect`, and the `NameOwnerChanged`/`NameAcquired`/
`NameLost` signals.

**Those methods are FILTERED PER CALLER, not answered globally**, which
is the difference between a policy and a decoration. `ListNames`,
`ListActivatableNames`, `NameHasOwner` and `GetNameOwner` answer only
about names the caller may `see`; a name it may not is reported as
absent, in the one consistent story rather than as an error that
confirms it exists. `NameOwnerChanged` is filtered the same way, since a
signal is a subscription-shaped version of the same question. And
`GetConnectionUnixProcessID`/`GetConnectionUnixUser`/`GetConnectionCredentials`
answer only about peers the caller may see — a sandbox has no business
learning another instance's host pid, which is both an identifier for
`/proc` spelunking outside the jail and the input to the lineage walk
this broker's whole identity story rests on. A draft listed the methods
as permitted and said nothing about their answers, which would have made
the `see` policy a filter on calls the caller never needed to make.

Match rules parse `type`, `sender`, `interface`, `member`, `path`,
`path_namespace`, `destination`, `arg0`…`arg63`, `arg0path`…`arg63path`, and
`arg0namespace`, with exact D-Bus escaping. `eavesdrop=true` and
`BecomeMonitor` are refused. A signal is delivered at most once per
connection however many rules match.

### Sandbox policy

At accept: `SO_PEERPIDFD` → `fdinfo` → pid → the registered jail
instance, bracketed by a second read of the same pidfd (see the
peer-identity subsection below for why the number alone will not do). The
default sandboxed policy may own no name; may call the
`org.freedesktop.DBus` subset above; may call any
`org.freedesktop.portal.*` member and receive its replies and **directed**
signals (a `Request.Response` arrives as a directed signal, which is what
makes portals work with no other grant); receives broadcasts only from
portal-owned names; and gets `AccessDenied` for everything else, with
broadcasts silently undelivered rather than errored — answering a message
that was not addressed to you is worse than dropping it.
`[Session Bus Policy]` `see`/`talk`/`own` entries from authenticated
metadata widen that. Of the three, `own` is implemented: it reaches the
broker through `td.Jail1`'s registration and is what a sandboxed application
holds a well-known name by. `see` and `talk` are parsed and refused, because
which imported services a sandbox may address is a decision this section owes
rather than a mechanism it is waiting on.

That grant is one-directional and the implementation reads it so. A
sandbox may CALL a portal member and RECEIVE its replies and directed
signals; it may not ORIGINATE a directed signal at anything, which is the
reverse channel and is not among the grants above. Such a signal is
dropped in silence rather than errored, for the reason the sentence above
gives about broadcasts: a signal has no reply, so a refusal would be a
message its sender has no serial for. What the implementation refuses
with `AccessDenied` is narrower than "everything else" reads — a name a
caller may not SEE is reported absent instead, since an error confirming
the name exists is the disclosure the filter is there to prevent. See the
per-caller filter subsection later in this section.

**Portal names are RESERVED, not merely conventional.** "Broadcasts from
portal-owned names" is a trust statement, and nothing above made it one:
`org.freedesktop.portal.*` was a name like any other, so an *unsandboxed*
uid-1000 process — which this design deliberately leaves unrestricted —
could claim it after a restart and start receiving sandboxed apps'
portal traffic, including their FileChooser paths and whatever a Request
carries. That is not an escape from the jail; it is a same-uid process
doing what same-uid processes may do, which is exactly why the *broker*
has to be the one to refuse it. So the portal name set is a **compiled-in
reservation** owned only by the connection the supervisor registered as
the portal at startup, `RequestName` for a reserved name returns
`AccessDenied` to every other connection sandboxed or not, and the
reservation survives the portal's death — a restarted portal re-registers
through the same supervised path rather than racing for the name.
The same argument covers `org.freedesktop.DBus` itself, which is the
broker's own and was never claimable, and it is why the two are one
table.

**The ambiguous case fails closed.** A pid whose `/proc` is unreadable or
gone at accept refuses the connection, because failing that direction is
privilege up.

**Identity is lineage, not just `/proc/<pid>/root/.flatpak-info`.** This
is a correction to the obvious design and it matters for exactly the app
this project exists for: a nested Firefox child can change its mount
namespace and so change what `/proc/<pid>/root/.flatpak-info` resolves to.
So the broker holds `{instance, app-id, stage-2 pid, start time, permitted
service names, owned bus names}`, and a connection is authenticated by
**descent from that registered PID 1**. The pid the broker starts from is
expressed in the *broker's* pid namespace, so it stays meaningful even
though the app sees itself in a nested one — true of `SO_PEERCRED.pid`, and
true of the `Pid:` line a pidfd's `fdinfo` carries, which is what the accept
path actually reads and which reports in the namespace of the process doing
the reading. `NSpid:` is the per-namespace chain and is deliberately not
consulted: its innermost entry is 1 for every jail. A process whose lineage
cannot be proven is denied. Unsandboxed same-uid connections are
unrestricted — td's existing trust model, stated explicitly in §E — but
**"unsandboxed" is a proved answer here and not a fallback**, because the
two sentences you just read would otherwise resolve the same ambiguous peer
in opposite directions. The broker's three-valued reply, and why the middle
answer is provable, are in §E.

**Every EDGE of that walk is validated, not just its endpoint.** An
earlier draft checked the connecting pid's start time before and after
and called pid reuse closed; it is not. The walk reads
`/proc/<pid>/stat`'s ppid at each hop, and an *intermediate* ancestor can
exit and have its pid reused between two hops — after which the walk
continues up a lineage that is not the one it started in, and lands on
the registered stage-2 pid by a path that never existed. The endpoint
check cannot see this, because both endpoints are exactly what they claim
to be.

So each hop records `(pid, starttime, ppid)` and the whole chain is
re-verified after the walk completes, with **two invariants that make a
substituted ancestor detectable**: every recorded start time must be
unchanged on the second read, and a parent's start time must be **less
than or equal to** its child's, which a reused pid violates because the
reusing process necessarily started later than the child that named it as
parent. The second is the cheap one and it is what catches the race the
first can miss. A chain that fails either is `Unknown` — denied, per the
three-valued rule above — rather than retried, since a retry under an
active attacker is a loop rather than a resolution.

This is a walk over `/proc` and it is inherently a sampled view. The
durable fix is a kernel-maintained boundary — a delegated cgroup whose
membership the kernel maintains, or pidfds held from the moment each
process is created. A pidfd is a handle on a process rather than on a
number, so a boundary expressed in pidfds is not subject to pid reuse;
note that this is a claim about the HANDLE, and that the pid NUMBER a
pidfd reports does return to the allocator once the process is reaped,
which is why the peer-identity use below re-reads rather than assuming.
§P already
delegates a cgroup per instance for resource caps; **using
`cgroup.procs` membership as the identity oracle instead of a `/proc`
walk is the better design and costs nothing new**, since the cgroup must
exist anyway. Take it if the delegation lands before the broker does;
the `/proc` walk with validated edges is the fallback for the ordering
where it does not, and it is what this section specifies because the
design cannot yet assume that ordering.

**Registration is two-phase, because the pid does not exist when the
instance does.** Registering the whole record "at launch" is
chronologically impossible: stage 1 unshares and spawns stage 2, and only
then is there a stage-2 pid to name. So:

1. **Before unsharing**, stage 0 registers `{instance, app-id, permitted
   service names, owned bus names}` and receives an opaque one-shot
   token. The instance exists; it has no pid and accepts no connections
   yet.
2. **Stage 1 completes the registration** with `{stage-2 pid, start
   time}` under that token, on its own broker connection — the pid is
   what `Command::spawn` returned, already in the broker's namespace for
   §A's reason. (§A's parent-death pipe is not this channel: it runs
   stage 1 → stage 2, and the broker is on neither end.)
3. **Only then does stage 1 release stage 2**, and this is a requirement
   on the JAIL rather than a description of the broker. A draft of this
   list said "only then does the broker accept connections for the
   instance", which is not something the broker does or could do: it
   accepts every connection and answers the identity question about it.
   `Command::spawn` returns with stage 2 already runnable, so between
   phase two and the release there is a window in which stage 2 — or the
   application it execs — can connect. §E refuses a strict descendant of
   a pending registrant, correctly, and §D fixes identity AT ACCEPT, so
   that connection is denied for its whole life even though the
   registration completes a millisecond later. §A already has the gate:
   stage 2 blocks on the proof pipe, and the write that releases it must
   come after `Complete` succeeds. On any failure stage 1 kills and
   reaps stage 2 instead of releasing it — the same requirement as
   "stage 1 refuses to proceed without the token", stated at the point
   where it is actually enforceable.

That token is also what makes §A.0's completeness invariant checkable:
**stage 1 refuses to proceed without it**, so entering the jail without
having registered is a refusal rather than an unregistered instance.

**What the broker CANNOT check is the app id itself, and §L.1 has to be
read in that light.** Registration is authenticated by uid, and in v1
every session peer is uid 1000 — so nothing distinguishes a genuine
`td-jail` performing this protocol from any other uid-1000 process
performing it, and the app id is a string the registrant supplies. The
lineage walk is sound about *which instance a connection belongs to* and
says nothing about whether that instance is what it calls itself. Two
consequences worth naming rather than leaving to be discovered: a rogue
process can register as `firefox` and be NAMED `firefox` in an elevation
prompt whose whole value is naming the requester (§L.1 property 2 claims
the requester "never gets to say what it is", which is true of the pid
and false of the id); and registering a legitimate unconfined process's
pid as an instance's stage-2 pid mislabels that process, denying it the
portal access it should have. Per-app uids (§L, v2) are the fix for both,
since the registrant's uid then IS the claim — which makes this a third
argument for scheduling them, beside the two §L already gives.

A connection arriving between the two phases is refused rather than
queued — the ambiguous case fails closed, as above — and the token is
consumed on completion, so a second attempt to bind a pid to an instance
is an error rather than a takeover. Reading the pid out of stage 1 rather
than letting stage 2 announce itself is deliberate: stage 2 is inside the
jail by then, and a record the confined process supplies is a record the
confined process chooses.

### Activation

`StartServiceByName` for *host* services is refused: there are no
`.service` files on td and the portal is a supervised unit whose `ready=`
proves it is up. But **app-local services (dconf, and whatever an app
ships) must activate inside the app's namespaces**, so: the spec
compiler parses the authenticated app/runtime `.service` files at build
time, stage 2 opens a
private activation listener not mounted into the sandbox, the permitted
names are predeclared at registration, and a matching
`StartServiceByName` is forwarded to that instance, where PID 1 validates
the name against the predeclared set and spawns literal argv. Only
`Name=` and a bounded literal `Exec=` grammar are accepted — no shell, no
environment expansion, no `PATH` lookup. The broker queues at most 64
calls for five seconds while the service claims its name. An app cannot
activate another instance's services.

Because those listeners are per-instance and private, the collision that
one bus would otherwise have — two sandboxes each wanting to own
`ca.desrt.dconf` — does not arise: neither of them claims it on the
shared bus, each claims it on its own listener, and the broker's
predeclared set is per instance. That is name virtualization arrived at
by construction rather than as a feature, and it is worth naming as the
reason a global broker can serve app-local services at all.

**Auto-activation, not only the explicit call.** D-Bus activates on an
ordinary method call addressed to an unowned but activatable name — that
is the mechanism almost everything actually uses, since a client calls
`org.gtk.vfs.Daemon` rather than calling `StartServiceByName` first. A
broker that activated only on the explicit request would leave every
such call failing with `NameHasNoOwner`, and the failure would look like
a policy denial rather than a missing feature. So a call to a predeclared
activatable name that no connection owns enters the **same** queue-and-
activate path described above, with the same five-second bound and the
same per-instance validation; the difference is only what triggers it.
The reply is delivered when the service answers, or the pending call is
errored on timeout — never dropped, because a caller waiting on a serial
that will never be answered hangs rather than fails.

### `UNSAFE.md` surface #10 (landed)

The surface is landed and normative in `UNSAFE.md` §10. It is THREE
syscalls, not the four this section drafted:

```
| 10 | `td-busd` | `recvmsg(2)`, `sendmsg(2)`, `getsockopt(2)` |
```

`close(2)` came off between draft and landing. The draft carried it from
the hand-rolled descriptor owner the very next paragraph rejects; once the
`OwnedFd` adoption is taken, `std` performs every close and the crate has
no close of its own to make. Keeping the row as drafted would have
rostered a syscall no line of code issues, which is the failure this file
exists to prevent, pointed the other way.

`getsockopt` accepts only `SOL_SOCKET` with `SO_PEERCRED` or
`SO_PEERPIDFD`, each pinned at its own call site. `SO_PEERCRED` uses a
fixed `[i32; 3]` buffer whose length is pinned in the shipped build, since
the kernel writes exactly `sizeof(struct ucred)` through the pointer — and
the landed wrapper compares the length the kernel writes BACK against that
size before believing the words, because a short write leaves a zeroed uid
that reads exactly like `root`. `recvmsg` pins `MSG_CMSG_CLOEXEC` and a
control buffer sized for exactly 64 descriptors; `sendmsg` pins
`MSG_NOSIGNAL`. The body is `syscall5`, since `getsockopt` takes five
arguments.

The sole caller is `transport.rs`. The draft named `auth.rs` too, and that
turned out to be wrong: the handshake needs no syscall of its own, because
the transport reads the bytes and hands them to a `settle()` that is pure.
A confinement test pins the narrower list.

**How a forwarded descriptor is owned.** td-compositor reopens received
descriptors through `/proc/self/fd/N` rather than `from_raw_fd`, and that
trick is *unavailable* to a broker — opening a `/proc/self/fd` entry
naming a **socket** fails with `ENXIO`, and one naming an `anon_inode`
such as an `eventfd` with `EACCES` (both measured, not assumed), while the
compositor's descriptors are memfds and files, which reopen faithfully.
Reopening also yields a NEW open file description, so even where it
succeeds the receiver does not get the shared description `SCM_RIGHTS`
defines. A forwarded descriptor here is freight: it is recounted
against the message's `UNIX_FDS` field (mismatch disconnects the sender),
forwarded by number, and closed.

The first draft held it as a bare integer in a hand-rolled owner whose
`Drop` called the rostered `close(2)`, specifically so the crate could
assert it never names `from_raw_fd`. **That is reinventing RAII to keep
an `unsafe` count down, which is the exact trade the maintainer's premise
refuses** — a hand-rolled owner is a fresh chance at a double-close or a
leak, in a type `std` already ships correct. So td-busd instead takes
**one scoped `OwnedFd::from_raw_fd` adoption**, in one named function,
recorded as a second scoped allow of a *different shape* from the syscall
layer — a descriptor adoption — with its own confinement test asserting
that the adoption appears exactly once and nowhere else. A partial
`sendmsg` attaches SCM_RIGHTS only on the first write.

**The adoption happens BEFORE any validation, and the ordering is the
whole point.** An earlier draft adopted "immediately after the
`SCM_RIGHTS` count is validated", which leaks every descriptor on exactly
the messages that matter: the kernel has already installed them in the
process by the time `recvmsg` returns, so a message whose `UNIX_FDS`
disagrees with its ancillary data — the malformed case, the attacker's
case — disconnects the sender while the descriptors stay open forever. A
few thousand such messages exhausts the broker's descriptor table, and
the resulting failure is `EMFILE` on an unrelated later connection.
Validation cannot precede ownership because ownership is what makes the
cleanup path work at all: **every descriptor `recvmsg` returns enters an
`OwnedFd` first, in the same function, before `UNIX_FDS` is compared,
before the body is parsed, and before any policy runs.** Then a refusal
is an early return and the `Drop`s do the rest.

`MSG_CTRUNC` is checked and rejected explicitly for the same reason, and
it is rejected AFTER the adoption above rather than before — which is the
whole reason the ordering is stated as a rule rather than left to the
obvious reading. The flag means the kernel had more ancillary data than
the control buffer could hold, and the delivery is PARTIAL rather than
absent: the descriptors that fit are installed in this process exactly
like any others, and only the excess is closed by the kernel. A draft
said "closed by the kernel, so nothing leaks", which is true of the
excess and false of the rest — and a reader who believed it would write
the one bail-out that leaks, since the malformed case is precisely where
the flag appears. What the flag actually costs is that the message is now
a lie about what accompanies it, so treating a truncated control buffer
as a successful receive is how a `h`-typed body value ends up indexing a
descriptor that was never delivered.

Which is the last rule here: **a body value of type `h` is an INDEX, and
every one is bounds-checked against the descriptors actually received**,
not merely against the `UNIX_FDS` count. The count says how many arrived;
it says nothing about whether index 7 exists when three arrived, and a
parser that checks only the count will happily hand a caller the wrong
descriptor from an adjacent message.

**What enforces the 64-connection bound.** The global half is exact and cheap:
the listener reserves one place using `SO_PEERCRED` before it starts a lineage
walk. That reservation bounds the identification workers as well as live
connections, so a deep ancestry cannot serialize the listener or create an
unbounded thread queue. The worker then resolves the same once-at-accept
lineage identity that connection policy will use and atomically converts its
place to the final authority key. Every process proved to descend from one
registered jail is charged to that instance string, so forking does not mint
new shares. A proved unconfined process uses its process as a fallback key.
All peers whose lineage is unknown and therefore receive no instance
authority share one fail-closed key: rotating an already-reaped connecting pid
cannot mint shares for sockets it left behind. On a kernel that cannot answer
`SO_PEERPIDFD` at all, those peers collectively stop at one 16-connection
share; policy already makes every one unusable there. The per-key number is a
quarter of the global ceiling.

**A second bound the list above does not state, and needs.** "64
descriptors per message" is per message; a broker also has to bound what
is queued and unclaimed. Per connection that is 64, since one legal
message may carry that many — but 64 connections at 64 descriptors is
4096, past a 1024 `RLIMIT_NOFILE`, so a per-connection cap alone produces
exactly the `EMFILE` it was added to prevent. The queue is therefore
charged against a BUS-wide budget of four messages' worth, which leaves
the connection sockets, the listener and stdio their room.

**Three bounds this landing does NOT have, named so they are not
rediscovered.** A served connection has no idle or authentication timeout,
so a peer that connects and never writes holds its slot until it leaves.
That is bounded rather than open-ended — the per-instance share above means one
app can hold only its own quarter, not the bus — but the slot is held
indefinitely, and the bound that should replace "indefinitely" is a
deadline on completing the handshake. Second, `inbox` and `frame` retain
their high-water capacity per connection, so peers each sending one
near-maximum message pin their size in memory charged to `td-busd` rather
than to the app cgroup; the descriptor budget above is the pattern a byte
budget would follow. Third, the socket is umask-wide between `bind` and the
`chmod` that makes it 0600 — a window the 0700 parent covers, and one that
`umask(2)` would close at the cost of a syscall this roster does not have.

**The stale-socket check is narrowed, not atomic.** `bind` refuses a path
whose socket still has a listener, which is what stops a second broker
silently displacing a running one and stranding its peers. Two starts
racing inside the window between that `connect` and the `unlink` can still
both conclude "stale". Closing that needs arbitration this design has not
specified — a lock file with stale recovery, or a syscall not on surface
#10's roster — and `td-svc` supervises exactly one instance, so the
remaining race is a misconfiguration rather than an expected path. Stated
here rather than left for a reader to rediscover.

Deliberately absent: `socket`/`bind`/`listen`/`accept` and byte I/O (all
`std`), arbitrary socket options, `SCM_CREDENTIALS`, and any syscall a
D-Bus *service* would need. Also absent, and worth naming because a
broker looks like it needs one: `poll(2)`/`epoll_*`. Stable `std` exposes
no readiness API, so multiplexing would buy a FOURTH rostered syscall for
a concurrency the session bus does not have; `td-busd` serves one
connection per thread and blocks in `recvmsg`.

What is landed of the above is the complete bounded broker path.
Descriptors are adopted first, queued as freight, and claimed by the
message whose `UNIX_FDS` says so, with a bound on how many may sit
unclaimed. The `h`-index bounds check is in the codec: `wire.rs` refuses
an index at or past the count `decode` was given, so a body value naming a
descriptor that did not arrive is a decode failure rather than a lookup.
The broker preserves the `UNIX_FDS` header, transfers descriptor ownership
and its global quota charge with the frame, bounds queued attachments per
recipient, and attaches them on that recipient's writer thread. Directed
calls and matched broadcasts carry descriptors only to recipients that
also completed `NEGOTIATE_UNIX_FD`; a directed call that expects an answer
gets `NotSupported` when its recipient did not negotiate. If a reply would
carry descriptors to a caller that did not negotiate, the broker consumes
the authenticated pending-reply record and answers the original call with
`NotSupported` rather than silently losing both the reply and its record.

**Which layer counts the descriptors, and how pipelining is framed.**
"Mismatch disconnects the sender" is enforced in `message.rs`, which takes
the number that arrived and refuses any message whose `UNIX_FDS` is not
EQUAL to it. On a Linux stream, ancillary data is a barrier: one `recvmsg`
may return bytes sent before the control-bearing write together with that
write and its descriptors. The transport therefore never lets a receive
cross the end of the D-Bus frame it is assembling. It reads the fixed header,
then at most that frame's declared remainder, and only then decodes against
the freight accumulated inside those reads. Missing and surplus descriptors
both disconnect; neither can drift into an adjacent message. Two legal
descriptor-bearing `sendmsg` calls pipelined without waiting are an end-to-end
regression, beside cases that put the descriptor only in the following frame
or add an undeclared descriptor to this one.

The broker-wide 256-descriptor budget includes outbound queues now that
ownership follows a forwarded frame. If stalled recipient writers fill it,
the receive path disconnects the connection holding the most queued
descriptor attachments and retries once, mirroring the broker-wide byte
remedy. It does not make the next unrelated sender the victim of pressure it
merely observed.

### What is landed of the bus interface

Names, directed routing, the per-caller filter, authenticated
pending-reply ownership, well-known name ownership, the permission file's
`own` grant that widens it, and bounded match rules are landed. Of the
`org.freedesktop.DBus` roster above: `Hello` (assigning
`:1.N`, with anything before it disconnecting and a second one ending the
connection, and announcing the assigned name like any other name gained),
`RequestName` and `ReleaseName` with the owner queue, `ListNames`,
`ListActivatableNames`, `NameHasOwner`,
`GetNameOwner`, `GetConnectionUnixUser`, `GetConnectionUnixProcessID`,
`GetConnectionCredentials`, `AddMatch`, `RemoveMatch`, `GetId` and
`Peer.Ping`, plus `NameAcquired`, `NameLost`, and filtered
`NameOwnerChanged` signals. The private two-phase `td.Portal1.Prepare` and
`td.Portal1.Activate` supervisor capability is landed at `/td/Portal1`; its
exact process-and-name rules are recorded in the reservation subsection
below. A `Hello` that omits
its `DESTINATION` is accepted, because there is nothing else a connection
with no name could be addressing — the broker is the only peer it can
reach — and requiring the field made the no-destination case unreachable
in the routing match while turning a lenient one into a disconnect whose
stated reason was "Hello before Hello". Absent:
`Introspectable.Introspect` and `Peer.GetMachineId`.

**Absent means `UnknownMethod`, not silence.** A client told its match
rule was installed and then never signalled is worse off than one told
there is no such method: the first waits, the second fails. So the bus
answers every call it cannot serve rather than dropping it, and
`ListActivatableNames` answers with an EMPTY array rather than an error
because that is the true answer — td has no service activation, and
nothing on this bus starts because it was called.

A method called with the wrong arguments is `InvalidArgs` and the
connection continues. That is the same rule read the other way: a bad
CALL is not a bad peer, and a draft disconnected for a name lookup with
no name in it — which its own comment said it did not do. The check is
on the SIGNATURE, not on the first argument's type: a draft read
`args()[0]` and ignored the rest, so a lookup with a spare argument, or
a no-argument method called with a body, ran as though it had been
called correctly. A lookup's argument is also validated as a bus name,
because a string that cannot be owned by anyone is a malformed question
rather than a name with no owner. `Hello` is the one method whose
arguments are not checked, and cannot be: an error reply has to be
addressed, and a connection that has not said `Hello` has no name to
address one to.

**The broker's own methods are at the broker's own object.**
`org.freedesktop.DBus`'s methods are answered only at
`/org/freedesktop/DBus`; `org.freedesktop.DBus.Peer` is answered wherever
it is addressed, because the specification puts `Peer` on every object a
connection exposes. Without the distinction a call to an application's
own path, addressed to the bus name, would be answered by the broker as
though it were that application's object. `Hello` is held to the same
rule on all three counts — the bus's name, the bus's interface and the
bus's object — which is also what keeps the no-`DESTINATION` case above a
live branch rather than a dead one. A draft checked the name and the
object and left the interface out, so `org.example.Thing.Hello` earned a
unique name.

**The per-caller filter is landed.** It was the gap that mattered most
in this section, and the shape of it was: everything answered globally,
so `ListNames` reported every unique name on the bus to every caller and
`GetConnectionCredentials` reported any peer's uid and pid to any peer.
The broker was correct for a single-user session of mutually trusting
peers and was not the confinement boundary §D describes. It now is, for
the surface it has.

`policy` decides, from the three-valued identity and nothing else.
`Unconfined` is unrestricted, which is the positive grant §E specifies
rather than a fallback. `Unknown` is denied every other peer,
which is the ambiguous case failing closed: a peer whose lineage the
broker could not prove must not collect the grant it was unable to
demonstrate. It keeps the broker and its own unique name, and that is a
consistency requirement rather than a softening — the connection has
already been told both, and a first draft that hid them had the bus deny a
peer the name `Hello` had just handed it while still answering its
`GetId`. `Jailed` is the same plus the reserved
`org.freedesktop.portal.*` namespace, so an unplaceable peer is strictly
below a sandboxed one rather than differently placed.

Below Linux 6.5 no peer can be identified at all, so every connection is
`Unknown` and the bus routes nothing between peers. That is fail-closed
behaving as specified rather than a degradation to be worked around, and
it is one more reason the image pins 7.x. Nothing else — §D's default sandboxed policy grants the
portal and the `org.freedesktop.DBus` subset, and the portal is how a
sandbox asks for anything outside itself.

Three consequences worth stating rather than discovering. First, `td-portal`
does not exist yet, so a confined application on the shipped image can reach
the BROKER and no portal service. The activation and routing substrate is
exercised by wire-level broker tests rather than by a shipped service; this is
why the service-side portal bullets remain in §B.3.2. Second, an instance's
connections cannot see EACH OTHER: §D's default grants the portal and the
broker, and same-instance peer-to-peer traffic is not among them. If a real
application turns out to
need it, that is a reviewed widening with its own argument, not something to
assume. Third, this filter decides WHO may be addressed and not WHAT may
be sent; message type is a separate rule, and it now exists — a confined
peer may CALL and may not originate a directed SIGNAL. See the pending-reply
paragraphs later in this section.

What this filter does NOT make private is everything a peer shares with
every other peer rather than learns by asking. Unique names are handed out
from one sequential counter and never reused, so a peer that reconnects can
count the arrivals it did not see. Admission takes a cheap global and
provisional per-process reservation before the lineage walk, then atomically
converts it to the registered instance's bounded share; unprovable peers share
one fail-closed key. The outgoing budget is bus-wide, and §D's remedy for
exhausting it disconnects the LARGEST CONSUMER — which is a receiver, so a
peer permitted to talk to the portal can pressure the portal off the bus by
talking to it. These are shared-resource observation and denial of service,
not the directed-message boundary this section is about, and per-caller
quotas are where they get addressed.

**Which methods a confined peer may call is settled the other way, and
this is the part a draft got backwards.** The whole
`org.freedesktop.DBus` roster stays callable; it is the ANSWERS that are
filtered. A `see` policy expressed as refused calls is a filter on
questions nobody needed to ask, and it would also tell the caller that
the thing it asked about is the kind of thing that exists.

**A name the caller may not see is ABSENT on every path that ANSWERS A
QUESTION about it, the send path included.** What a peer can infer from
what it shares with every other peer is a separate matter, and the last
paragraph of this subsection is where that is accounted for. `ListNames` omits it, `NameHasOwner` answers false,
`GetNameOwner` and the three credential lookups answer `NameHasNoOwner`,
and a directed message to it that WANTS A REPLY is refused with
`NameHasNoOwner` — not `AccessDenied`. One that wants no reply is dropped
in silence, which is the rule the broker already followed for a name with
no owner and is the same story told to a caller who is not listening.

**A REPLY is exempt from this FILTER, because the filter is about who may
be ADDRESSED and not about what may be sent.** A method return or an error
is addressed by `reply_serial` to a caller that already reached this
connection, so filtering it by the sender's talk set drops the answer to a
call the broker itself delivered — the caller waits until it times out and
the callee is told nothing. §D grants a sandbox the portal's replies, so the
symmetric direction cannot be a denial.

Exempt from the filter is not exempt from every test. A reply is governed by
OWNERSHIP instead, which is the stricter one: it is carried only when the
broker's pending-reply table says this connection is the one that call was
routed to. That table is landed — see the pending-reply paragraphs later in
this section — and it closes what this passage used to record as a residual,
that a confined peer could address a forged reply anywhere.

A peer's own credentials, host pid included, are exempt with its own name.
The reason to withhold a host pid is that ANOTHER instance's is an
identifier for spelunking outside the jail and an input to the lineage
walk; neither argument reaches a peer's own number, and hiding it would
have two lookups disagree about one name.

§D asks for `AccessDenied` for what the default policy does not permit and
separately that an unseeable name be reported absent "rather than as an
error that confirms it exists". On a directed send the two rules meet, and
the second governs — but not for the reason a draft of this gave. The
reason is NOT that `AccessDenied` is inherently a disclosure: the policy is
consulted before the directory is, so a refusal is issued without the
broker having looked, and its timing does not depend on whether the name is
there. The reason is that `AccessDenied` would DISAGREE with the four
lookups. A caller told "no owner" by `GetNameOwner` and `AccessDenied` by a
send has learnt from the contradiction exactly what both answers were
shaped to withhold. Every one of these answers goes through one `may_see`
for that reason: a filter whose paths disagree is a hint.

**`td.Jail1` is the one live `AccessDenied`.** A confined caller is
refused the registration interface outright rather than told it is not
there. It is not a secret — the difference from a name is that a peer
which may not use an interface is better told so than left calling a
method that appears not to exist — and it is the interface that CREATES
confinement records: registration is authenticated by uid, every v1
session peer shares one, and a jailed peer that could register would name
its own instance and app id, which is the record every later answer about
it derives from.

What the filter depended on had already landed, which is what made this
increment reviewable on its own: the registration protocol and the
lineage walk were built and `GetConnectionCredentials` already carried
`td.AppId`, with nothing consulting any of it to decide an answer. An
identity that is wrong is worse than one that is missing, so the oracle
landed first and the first denial landed second.

What IS the broker's word rather than the caller's, already: the uid and
pid a credential lookup reports are `SO_PEERCRED` taken once at accept
and held in the directory, so a peer cannot describe itself into a
different answer. `td.AppId` joins them on the same terms — decided at
accept from the kernel's pid, stored, and never recomputed, because a
lineage is a statement about a process tree at an instant and re-walking
it later would answer about a tree that has moved on.

### What is landed of jail identity

**Registration is `td.Jail1` at `/td/Jail1`.** §D specifies the two
phases and their contents and does not name a wire interface, so this
landing chose one: `Register(s instance, s app_id, as services, as owned)
-> s token` and `Complete(s token, u pid)`, on td's own interface at td's own
object rather than as additions to `org.freedesktop.DBus`. That
interface belongs to the specification, and private methods hung off it
would be indistinguishable from standard surface to anything that
introspects the bus. The name carries a version from the start for the
same reason `org.freedesktop.systemd1` does: this is a private protocol
between two td programs, and one digit now is cheaper than a flag day
later.

The start time is NOT an argument to `Complete`. §D describes the record
as `{stage-2 pid, start time}` and the broker reads the second field out
of `/proc` for itself, because it is the field every later reuse check
rests on and a registrant-supplied value would be the one part of the
record the registrant chooses. A pid that is already gone completes
nothing.

**The pid is not taken on trust either, and a draft did take it.** Under
that version any session peer could open a registration and complete it
with any pid it could read; completing with pid 1 makes every later
connection in the session walk into the attacker's instance and be handed
its app id. Two checks close it, and both are things the broker sees for
itself rather than assertions about the caller: the connection that
completes must be the connection that opened, and the pid must be a CHILD
of that connection's process.

That is a requirement on td-jail rather than an observation about it,
and it is recorded here as one: stage 1 must be the same PROCESS that
performed phase one, and stage 2 must be its direct child, which is what
`Command::spawn` produces. A launcher that registered on behalf of a
sibling would be refused.

The same process, and deliberately not the same CONNECTION. A draft
claimed the connection, which is the stronger rule and would break the
only launcher there is: §A step 0 closes every descriptor above stderr
between the `unshare` and the spawn, so the connection stage 0
registered on is gone before stage 1 has a pid to report. Stage 1
reconnects. What holds across both phases is the pid — and, since the
landing below, the pid together with the start time the broker read at
phase one — because `unshare(CLONE_NEWPID)` does not move the caller. This also rules out a
tempting cleanup: dropping a pending registration when the connection
that opened it closes would discard every legitimate registration at
exactly the moment §A sweeps descriptors.

None of this makes the app id authentic: the v1 exposure above stands,
and a rogue can still call its own child `firefox`. It is the difference
between mislabelling a process you already own and relabelling somebody
else's.

**The app id is graded as a td identity, not as a bus name.** A draft
used the bus-name grammar, which is wrong in both directions: §B's
identity section defines an application's identity as a short flat name
— `firefox`, `darktable` — and a bus name requires an interior `.`, so
every real td identity would have been refused while `:1.7`, a unique
name the broker hands out and nobody may claim, would have been
accepted. The broker now applies the same 1–32-byte language td-jail's
`validate_application_name` applies. Predeclared service names are held
to the WELL-KNOWN name grammar for the matching reason: a service is a
name the instance intends to own, and a unique name is not ownable.

**The token is 16 bytes of `/dev/urandom`, not a serial.** A draft
reasoned that it need not resist guessing, because anything able to guess
it could call `Register` itself. That covers creating a new instance and
misses the move that matters: CONSUMING somebody else's in-flight
registration. The real stage 1's `Complete` then fails — and by then
stage 2 has been spawned, because its pid is what that call was going to
carry — so a live jail exists with no registration on record, which is
exactly the `Unconfined` answer §E exists to prevent. An unreadable
`/dev/urandom` refuses the registration, because every fallback is a
predictable token.

`NO_REPLY_EXPECTED` withdraws the reply, not the work. Both registration
methods are dispatched ABOVE the broker's no-reply guard: a `Complete`
dropped for want of a reply would leave a running stage 2 with no record,
which is the same `Unconfined` again. A draft had them below it, on the
strength of a comment stating that no bus method changed state — true
when it was written, false once these landed.

Moving them there cost a guard that had been holding by accident. Only
a METHOD_CALL may be dispatched as a method, and `wants_reply` is false
for every other type, so nothing below the no-reply guard could ever
run for a signal. Above it, a SIGNAL named `Register` at `/td/Jail1`
registered an instance. Both arms now test the message type the way
`Hello`'s caller always has.

**The walk validates edges, and one of §D's two invariants does the
work.** Both are implemented and both are red-checked separately —
recorded start times must be unchanged on a second read, and a parent's
must not exceed its child's — and the second is what catches the
substitution the first can miss, exactly as this section predicted. The
separately part was a review finding: one fixture killed both checks at
once, so weakening the re-read alone passed the suite, and staging a hop
that changes BETWEEN the two reads is what distinguishes them. A third check did not survive: a draft also
required each hop's recorded ppid to equal the parent the walk moved to,
which cannot fail, because the walk builds the chain by following that
ppid. Removing a check that cannot fire is worth recording, because a
reader counting defences would otherwise count three.

`/proc/<pid>/stat` is parsed by splitting at the LAST `)`. The second
field is `comm`, it may contain spaces and parentheses, and it is
attacker-controlled through `prctl(PR_SET_NAME)` — so a process named
`") 1 999999"` forges its own ppid against any parser that tokenizes the
line. This is the sort of thing that is obvious once written down and
absent from most implementations.

**Two lifecycle rules §D does not state, both availability first and
both with a security edge §D does not name.** An instance whose stage-2
process is gone is REAPED rather than refused for ever: the accounting
pass cannot distinguish "this instance ended" from "this instance's pid
may have been reused underneath the walk I just did", so without reaping
one ordinary application exit would make `Unconfined` unsayable for every
later connection until the broker restarted. Reaping is safe on this
section's own terms — stage 2 is PID 1 of the instance's pid namespace,
so killing it kills the namespace, and a dead instance has no live
descendants to misattribute.

But a dropped record travels with the walk. A draft pruned the registry
and then answered off the pruned copy, which quietly converted §E's own
`Unknown` case into `Unconfined`: "a registered stage-2 pid is no longer
where the registry says" is the observation §E denies on, because that
pid may already have been taken by something in the chain about to be
walked. A second draft refused every connection that observed any reap,
which is sound and too broad — a rogue can schedule it, by registering
its own child, completing, killing it, and letting the next connection
trip over the stale record, and because identity is fixed at accept that
connection is denied for its whole life.

What makes an answer unsound is narrower and checkable: the dropped pid
standing in THIS connection's lineage. That is the only way its reuse
could have bent the chain, and it is checked against the walked chain
rather than assumed. Peers elsewhere in the process tree are answered
normally, and the record is gone either way, which is what keeps a jail
that simply exited from denying the session for ever.

An instance the broker could not READ is a third case and is treated as
neither. `fs::read` fails for reasons that say nothing about the
process — `EMFILE`, `ENOMEM`, a line that does not parse — and a draft
read every failure as death: the connection that observed it was
refused, which
looks safe, but the record was dropped, and the NEXT connection from
inside that live jail resolved `Unconfined`. Only `ENOENT` licenses
dropping a record. Anything else refuses without reaping.

And a registration that stands between its phases for longer than
`PENDING_LIFETIME` is dropped, loudly. §E's rule is that a registration
in flight makes `Unconfined` unsayable, and what §E does not say is that
the party who could clean up a half-finished one is exactly the party
that is no longer there — so a stage 0 that died between the phases
would deny the session until the broker restarted.

Which peers it denies is narrowed to the registrant's strict
DESCENDANTS, and the narrowing matters in both directions. A first
version refused `Unconfined` to every peer while any registration was
open: safe, and wide enough that any uid-1000 process could deny the
whole session by opening a registration it never finished. It can be
narrowed exactly, because stage 2 is a CHILD of the registrant —
`unshare(CLONE_NEWPID)` moves the caller's children into the new
namespace rather than the caller — so every peer that could belong to a
pending instance descends from the pid that opened it. The registrant
itself is excluded, or the broker would deny the connection whose next
message is `Complete`.

The deadline is swept by `Register` and by `Complete` as well as by
`resolve`. Without that a token older than the deadline completes
whenever no `resolve` happened to sweep it first, which makes the
deadline a property of traffic rather than of time.

`Register` sweeps BOTH sides of the ceiling, pending and live. Otherwise
it is a one-way ratchet: 64 abandoned registrations, or 64 instances
whose jails exited without any connection ever arriving, refuse every
later launch until some connection happens along. A first fix swept only
the pending side and left the identical defect in the neighbouring
collection.

An expiry is fail-CLOSED only because of an invariant that lives in
another component: `Complete` then finds no token, and §D requires stage
1 to refuse to proceed without one. A stage 1 that ignored a failed
`Complete` and launched anyway would produce an application the broker
has no record of, which resolves `Unconfined` — full portal access for
the process that is certainly confined. That is the single most
important thing for the increment that wires td-jail to get right, and
it is why the expiry logs rather than passing quietly.

**The peer's own pid is proved by a pidfd, and this is how.** The walk
validates every ancestor edge and says nothing about its own starting
point. `SO_PEERCRED` cannot supply one. The kernel samples peer
credentials at `connect(2)` and holds a `struct pid` reference — which
keeps the struct alive without reserving the number, so `free_pid`
returns the number to the allocator when the connecting process is
reaped and `pid_vnr` still reports it. A peer that connects, passes its
socket to a sibling and exits can therefore have its pid recycled before
the broker reads it, with the delay under the peer's control through the
listen backlog. The walk would then describe whichever process now holds
that number, and the dangerous direction is a confined peer resolving
`Unconfined`.

So the broker takes the pid from `SO_PEERPIDFD` instead — a second
value-pinned `getsockopt` option on surface #10, recorded in `UNSAFE.md`
§10. **The argument is liveness, not reservation, and an earlier draft of
this section had it wrong.** That draft said a pidfd "cannot be recycled
by definition"; the claim is true of the HANDLE and false of the NUMBER.
Holding a pidfd across a reap does not stop the pid being handed out
again, which is measured rather than reasoned about. What the descriptor
buys is the ability to ask: `/proc/self/fdinfo/<pidfd>` reports the pid
while the process is alive AND while it is a zombie, and `-1` once it has
been reaped — and a reap is exactly what has to happen before a number
can be reused.

Hence the rule, and **both halves are load-bearing**: read the pidfd
before the walk, to learn which pid to start from and to refuse a peer
that has already been reaped; read it again after every `/proc` read that
an ANSWER rests on, and require the same pid. The qualifier is exact: a
lookup that refuses part-way — an instance the sweep could not read, a
broken chain — returns before the second read, and is refused on the
strength of having read something it could not vouch for rather than on
any conclusion drawn from it. The second read is the one the
soundness rests on — if the pidfd names the peer at the end, the peer was
never reaped in between, so its number was never free, so every `/proc`
read taken while the lookup ran was a read of this peer. One read alone
does not get there: the peer can be reaped and its number reused between
that read and the walk, and the walk's own re-read of hop zero would then
find the impostor's start time unchanged and pass. The check applies to a
`Jailed` answer as much as to an `Unconfined` one, since a recycled pid
that lands on a registered instance misattributes one application's
connection to another.

A kernel without `SO_PEERPIDFD` (below 6.5; td pins 7.x) answers
`ENOPROTOOPT`, which is `Unknown` — denied — rather than an accept
failure. Refusing the CONNECTION would take the session bus down
entirely, and would do it to the compositor and the terminal as readily
as to a jail.

**The two registration calls are told who is calling the same way, and
the pending record is a pid AND a start time.** The paragraphs above
fixed the identity walk and left the registry sampling: `Register` and
`Complete` were handed `SO_PEERCRED.pid`, so the recycled number that
would have misdescribed a connection could equally open a registration
in somebody else's name — and `Complete`'s "the same process that opened
it" rule compared two numbers across a gap the registrant need not
survive. The gap is not removable: §A closes every descriptor above
stderr between the phases, so the two calls necessarily arrive on
different connections. That is what makes a number the wrong instrument
for the comparison rather than merely an imprecise one.

Two changes, and neither is sufficient alone. First, the broker takes
the caller's pid from a fresh `SO_PEERPIDFD` at each of the two calls
and refuses a caller it cannot identify; that covers a registrant which
has already been REAPED, whose stale number the socket still reports.
Second, the record stores the registrant's start time and phase two
compares it; that covers a registrant whose number has already been
handed to a live process, which the liveness oracle by itself would
identify quite correctly as somebody — just not as the process that
opened.

**What the pidfd buys at a registry call is liveness and not a different
number**, and a review corrected a draft of this section that implied
otherwise. `SO_PEERCRED` and `SO_PEERPIDFD` render the same
`sk->sk_peer_pid`, so whenever the descriptor names a pid at all it is
the pid the socket already reported. The difference is what each can be
asked: the socket keeps reporting its number after the process behind it
has been reaped and the allocator has handed the number on; the
descriptor says whether that has happened. "Reaped" and not "exited" is
exact — a zombie's `fdinfo` still reports its pid, which is right,
because an unreaped pid is an unavailable pid and availability is the
only property at issue.

**The start time is read inside the same two-read bracket the walk
uses**, and a review found the version without it. The broker reads the
pidfd, reads `/proc/<pid>/stat`, and reads the pidfd again requiring the
same pid; only then is the pair a caller. One read is not enough for the
reason it was not enough for the walk: the peer can be reaped and its
number reused between the pidfd read and the `stat`, and phase one would
record the impostor's start time — while the peer that transferred its
socket away before dying still has a controller on the other end to take
delivery of the token. Both later checks would then agree with each
other about a process that was never this connection's peer. The
registry therefore reads no `/proc` entry for its own caller at all; it
is handed a pair, because a read outside the bracket cannot say whose
entry it read.

**The pending-registration rule matches that pair too.** A registration
in flight denies `Unconfined` to the registrant's strict descendants,
and matching the registrant by number alone handed that lever to chance:
a registrant that ended with its registration open would deny every
descendant of whichever process next received its number, for the rest
of `PENDING_LIFETIME` and without that process having registered
anything.

A kernel without `SO_PEERPIDFD` now refuses LAUNCHES and not merely
identity. `Register` and `Complete` are refused along with everything
else the descriptor is needed for, and §D requires stage 1 to refuse to
proceed without its token, so on such a host no jailed application
starts. That is the fail-closed direction and it is stated rather than
discovered: td pins 7.x, and a broker that cannot tell one process from
another should not be recording which application a process belongs to.

The pid `Complete` NAMES — stage 2's — stays a bare number, and soundly.
A registrant may only name a process whose parent it currently is, so a
stage-2 number that was reaped and reused can only be reused by another
of the registrant's own children. The worst available is mislabelling a
process it already owns, which is the v1 app-id exposure recorded above,
rather than a reach into somebody else's.

**td-jail performs this protocol, and the image's one application does
it at boot.** Every jailed application is a registered instance and
reports `td.AppId`; everything else on the session bus still resolves
`Unconfined`, which is right — the compositor and the terminal are not
in jails. The app-id caveat below is unchanged and is the reason the
filter still cannot land on this alone: registration is authenticated by
uid, every session peer is uid 1000, and the app id is a string the
registrant supplies. The walk is sound about WHICH instance a connection
belongs to and says nothing about whether that instance is what it calls
itself. Per-app uids are the fix and this is the third argument for
scheduling them.

**What the launcher sends is fixed by its own grammar, not by anything
the application controls.** The **app id is the application's td name** —
already the launcher key, the store path shape and the state directory —
so the bus credential names the same thing the rest of §B does instead of
introducing a second identity. The two crates cannot share a constant,
being separate dependency-free locks, so td-jail's
`validate_application_name` and td-busd's `valid_application_id` are the
same language written twice; a td-jail test reads BOTH of td-busd's
ceilings out of that crate's source rather than restating them, because
a name this side accepts and that side refuses is a launch that fails at
boot and nowhere earlier.

The **instance name** is that app id, a dash, and sixteen hex characters
of fresh randomness. It names a LAUNCH rather than a program: two windows
of one application are two instances, and the broker refuses a name a
live instance already holds. Hence random rather than the pid — a pid is
unique only among live processes, which is the property this section
spends its length not relying on. It is deliberately not the stage-2
proof token: that token is a one-shot secret, an instance name is an
identifier the broker compares and quotes back in its refusals, and a
secret spent as a name stops being one.

The **service list is empty**, and that is the honest statement rather
than a placeholder. Predeclared names are for app-local activation, which
remains absent and is separate from both well-known-name ownership and
supervised portal startup. `RequestName` and the permission file's exact
`own` entries are landed; the empty service list says only that td-jail
starts no app-local D-Bus service.

**Both orderings in the list above are pinned by a source-level test**,
in `td-jail/src/main.rs`'s confinement module, because no type expresses
either. What phase one must precede is the SPAWN: the pending
registration has to exist before anything inside the jail can connect.
Putting it before the `unshare` as well is the cheap half — a refused
registration then costs no namespaces, no mounts and no child to reap.

An earlier version of this paragraph gave a different and false reason:
that a registration opened after the `unshare` would name a pid the
broker could not see. It would not. `unshare(CLONE_NEWPID)` does not move
the caller, the identity map keeps the launcher at uid 1000, a pathname
AF_UNIX socket is indifferent to a new network namespace, and stage 1
keeps the old root — which is the same set of facts that lets phase two
open a fresh connection at all. Two reviewers caught it; it is corrected
here rather than quietly dropped, because the false version made an
ordering look load-bearing for a reason it is not.

**Phase two could not run at all between bf868804 and the commit that
fixes it.** The facts above -- that stage 1 keeps its pid, its uid and
its old root across the `unshare`, so it may open a fresh connection for
phase two -- are all true, and one more fact makes them insufficient:
after `unshare(CLONE_NEWPID)` the kernel refuses `CLONE_THREAD`
outright, because `copy_process` requires `pid_ns_for_children` to equal
the caller's active pid namespace and the `unshare` has just made them
differ. td-jail's connect deadline is a helper thread,
`std::thread::spawn` PANICS rather than returning an error when the
kernel refuses, and so every real launch aborted at `Complete` with
`failed to spawn thread`.

Measured rather than inferred, in both directions:
`unshare(NEWUSER|NEWNS)` leaves threads working and adding `NEWPID`
gives `EINVAL`; and a single-threaded binary that registers, unshares
for real, spawns a stage 2 and completes against a live broker aborts
before the fix and answers `OK` after it.

Why the host tests missed it is worth recording, because it applies to
anything else guarded this way: libtest runs each test on a spawned
thread even at `--test-threads=1`, so a cargo test process is already
multithreaded and `unshare(CLONE_NEWUSER)` refuses it with `EINVAL`. The
whole namespace transition is therefore unreachable from an in-process
test; only the real launcher, or a purpose-built single-threaded binary,
gets there. The confinement tests assert the ORDER of the calls in
`launch_application`, which was correct throughout, and the bus interop
test talks to a real broker without ever unsharing. The QEMU boot gate
would have caught it and is not reachable in this environment, so it has
not been rerun; the fix's evidence is the standalone reproduction rather
than a boot.

The deadline is what gives way. `within` attempts the helper with
`Builder::spawn`, which returns an error where `thread::spawn` panics,
and runs the work inline when the kernel refuses -- with the work parked
in a slot so a refusal hands it back rather than dropping it. A draft
PREDICTED the refusal with a throwaway probe thread instead; three
reviewers took that apart, and the version that discovers it has no
window between the question and the act.

**Nothing bounds a phase-two connect, and this document said otherwise
for one commit.** A draft claimed the jail unit's readiness deadline as
a backstop. It is not one: td-svc marks a unit whose `ready=` never
succeeds as failed, which settles ordering so dependents proceed, and
neither signals the process nor schedules a retry -- so `restart=always`
has nothing to restart -- while a compositor launch has no readiness
deadline at all. A stage 1 blocked in `connect` therefore stays, with
its stage 2 alive and blocked reading the proof pipe.

It is a cost of the fix rather than a regression: phase two's deadline
has never once worked, because the helper it needs cannot exist after
the unshare, so every phase-two connect that has ever run either aborted
the launcher or now runs inline. Closing it needs `socket(2)` and
`connect(2)` -- or `fcntl(2)` -- on `UNSAFE.md` §9's twelve, or a
connection opened before the unshare and held across the spawn, which §H
forbids for the reason §H gives: the channel that completes
registrations must be absent before stage 2 exists, not merely
close-on-exec. A liveness residual does not buy a weakening of that.Phase two after the proof write is the one that is genuinely a race, and
one that passes every test on an unloaded machine. The test asserts the
positions of `bus::register`, `sys::unshare_namespaces`,
`close_inherited_descriptors`, `command.spawn` and
`proof_writer.write_all` within `launch_application`, over a copy with
comments stripped, and that the refused-completion branch kills, reaps
and returns.

**The fixture's supervision edge does not change, and its reason does.**
`[jail-fixture]` is `after=busd` and `requires=wayland`; it is now a unit
that genuinely needs a WORKING broker rather than merely a bound socket,
and `requires=busd` is still wrong for the reason recorded in the recipe:
td-svc cannot distinguish a `restart=always` daemon inside its backoff
from a permanently failed one, so one busd crash in the boot window would
permanently kill the application tier of a machine whose broker recovers
a second later. The fixture's own `restart=always` is what recovers, and
it now covers the larger failure it always nominally covered. The gain is
that the boot gate became an end-to-end test of this protocol: the image's
one application cannot reach the screen without having registered.

**Routing is directed or matched broadcast.** A message naming a
`DESTINATION` that is a unique name on this bus is delivered to it; one
naming a name nobody owns is refused with `NameHasNoOwner`; one with no
`DESTINATION` splits by TYPE. A signal without one is offered once to each
permitted connection whose bounded match-rule set selects it; a method CALL
without one gets the opposite treatment. A draft of this section had both
dropped, which is exactly the failure the paragraph above argues against:
the caller is waiting on a serial that no route can answer, so it is told
`NotSupported` rather than left to time out.

**`SENDER` is rebuilt, not relayed, and the body is copied.** The broker
re-encodes the header with the sending connection's unique name in
`SENDER` — a client-supplied one still disconnects, per the rule above —
and copies the body byte for byte rather than re-marshalling it. A broker
has no business re-encoding a payload it does not read: re-marshalling
would round-trip correctly for everything this codec writes and would
silently change any encoding the specification permits but this writer
does not choose, and it would cost the whole body twice. The sender's
`serial` is preserved, because it is what the sender will match a reply
against.

**A message carrying descriptors is delivered only with them.** The
broker forwards the ordered descriptor array together with the restamped
bytes and preserves its `UNIX_FDS` header. Both sender and recipient must
have negotiated descriptor passing. If the recipient did not, a directed
method call gets `NotSupported` and a signal is omitted for that recipient;
the broker never delivers a body whose `h` values index a stripped array.
The pipelined-count regression and its frame-bounded receive fix are recorded
in the syscall subsection above.

**One writer per socket.** Each connection has an outgoing queue and a
thread that drains it, which is what makes the byte ceilings above a
queue rather than a blocking write: a sender appends and moves on, and a
peer that will not read fills its own queue instead of stalling whoever
wrote to it. The handshake's `OK` line goes through that same queue.
Writing it from the reading thread and routed messages from the writer
would be two writers on one socket, kept apart only by the order things
happen to occur in — a bug that would appear the first time a peer was
busy and never before.

The ceilings themselves are landed as this section specifies: a
per-connection byte ceiling of one whole maximum message, a bus-wide
ceiling of four of those, and a message COUNT kept as the secondary
guard against many tiny frames. Reaching the per-connection ceiling
tells the SENDER `LimitsExceeded`; reaching the bus ceiling disconnects
the largest consumer and logs it as the distinct broker-level
diagnostic.

**Reaching the per-connection ceiling does NOT disconnect the recipient,
and this section is amended to say so.** The rule as written above
attributed the fault to the recipient, and the sender chooses when it
fires: the ceiling is one whole maximum message, so two maximum messages
back to back exceed it BY CONSTRUCTION — the first is still in the
writer's hands when the second arrives, however promptly the recipient
reads. Under the old rule any peer could evict any other with two
frames, and `ListNames` hands every unique name to every caller, so one
application could walk the bus and evict every other application at no
cost to itself. That is a worse denial of service than the one the rule
existed to prevent, and it is the same argument the bus ceiling is
written from — *a ceiling an attacker aims at everyone else is not a
bound* — applied to the case that is aimed at someone else by
definition. The recipient's memory is still bounded by its own ceiling,
and a peer that genuinely never reads is removed when it becomes the
BUS's largest consumer, which is the right test because it compares
peers rather than trusting whoever wrote last.

Four things about those ceilings were wrong in drafts, and the first
three are the same mistake in different places: **a bound on a queue is
not a bound on memory.**

First, the empty-queue exception applies to the per-connection ceiling
ONLY. Applied to the bus ceiling too it stops being an exception and
becomes a multiplier — every queue is empty at some moment — and eight
connections took 134 MiB against a 67 MiB bound.

Second, **the charge lasts until the bytes are ON THE WIRE AND FREED,
not until the frame leaves the deque, and not until the socket is shut
down.** A draft uncharged a frame when the writer took
it, so a writer blocked in `sendmsg` against a peer that had stopped
reading held a whole maximum message that no counter knew about — and the
queue it came from now looked empty, so the exception admitted another.
That is the same multiplier rebuilt one frame lower down, and no test
could see it, because the tests that measured the ceilings never started
a writer. What a connection is holding, what the remedy weighs when it
picks the largest consumer, and the message COUNT backstop all count the
in-flight frame. A second draft then had `close` reclaim those bytes
itself, which is earlier still: `close` releases the SOCKET, and the
blocked `sendmsg` has to return before the allocation is freed. Budget
released in that window is real budget against memory that is still
live, so only the writer gives it back — and the bus's remedy waits,
briefly and boundedly, for the writer of the connection it just closed,
because otherwise it reports success having freed nothing yet.

Third, **the remedy runs wherever the condition is seen, and it names a
real consumer.** A draft ran it
only on the routing path and turned a bus overflow into a failure of
whoever was calling — so four peers that stop reading fill the budget and
the next INNOCENT peer to call a bus method, or the next connection to
reach its `Hello` reply or even its `OK` line, is the one disconnected
while the four sit there. A ceiling that an attacker aims at everyone
else is not a bound. Every append now applies the remedy and retries
once, and the frame is handed back rather than cloned, because cloning a
16 MiB message to retry it doubles the memory at the moment there is
least of it. The largest consumer is also re-read before it is acted on:
sampling each queue under its own lock cannot be one consistent snapshot
without holding every queue lock at once, which is a far worse
lock-ordering problem than the unfairness it would fix — but the one
outcome worth ruling out is disconnecting a peer that is holding
nothing, and a re-read rules it out. The victim is chosen under the
directory lock and dealt with without it, so the bounded wait above does
not stall every other route on the bus.

What the ceilings do NOT bound is the relay's transient cost: `restamp`
copies the body and `encode` allocates the whole outgoing frame, so a
16 MiB relay transiently costs about twice that in the sending
connection's own thread before any of it is charged. `encode` also
re-validates the body it was handed with a full walk, which is redundant
for one just decoded against the same signature and byte order. Both are
recorded rather than fixed: skipping the second walk needs a
trusted-body path through the codec, which is a footgun to add in a
review cycle.

**What a connection costs is two descriptors and two threads**, not one
of each: the writer needs its own handle on the socket, and the reader
blocks in `recvmsg` while the writer blocks in `sendmsg`. The
connection-ceiling arithmetic elsewhere in this section still describes
one apiece.

**The pending-reply table is landed.** The integrity half it was asked
for is closed. The availability half is closed for the case that motivated
it — a callee that DISCONNECTS — and open for the case of a callee that
stays connected and never answers; the residual paragraph below says so.

The availability half: a caller whose callee disconnects mid-call used to
wait for ever, because nothing noticed that the connection a call was
routed to had gone. A departing connection now leaves the directory and
sweeps its outstanding calls as ONE act under the one lock, and every
caller waiting on it is sent `org.freedesktop.DBus.Error.NoReply`
carrying the serial it was waiting on. That is best effort, because it
happens in `Drop` and there is nowhere left to report a failure to; a
caller that cannot be reached is one that was already leaving.

The integrity half, which is the more serious one. With nothing tracking
which call is outstanding to whom, any peer could send a `METHOD_RETURN`
or an `ERROR` carrying an arbitrary `REPLY_SERIAL` to any other peer —
and libdbus and GDBus both match a pending call by serial without
checking who the reply came from, so peer B could answer a call A made to
C and A's client library would hand the answer to A's caller. The
broker's stamped `SENDER` made that detectable and nothing detected it.
A reply is now carried only when the table says this connection is the
one that call was routed to. A serial is answered ONCE, since a second
reply bearing it is as forged as a first from the wrong peer.

A forged reply is DROPPED, not refused: there is no reply to a reply, so
there is nothing to carry an error on, and the sending connection stays
open because a forged message is a bad message rather than a bad peer.

A caller may not have two calls outstanding on one serial. The
specification asks for that anyway — a serial is unique among a
connection's undelivered messages — and the table needs it, because with a
duplicate allowed `(caller, serial)` stops naming ONE call: two answers
carry the same `reply_serial` to a client that cannot tell them apart, and
un-recording an undelivered call could remove the wrong entry and leave a
live one untracked. A caller that reuses an outstanding serial gets
`InvalidArgs`.

The bound is §D's `128 pending replies`, charged to the CALLER and
REFUSED rather than trimmed. Charged to the caller because a callee
cannot decline to be called, so charging the callee would let one peer
exhaust another's share by calling it. Refused rather than trimmed
because dropping an old entry to make room would un-track a call that is
still outstanding, sending its caller back to waiting for ever — the
exact failure the table exists to remove, reintroduced by the mechanism
meant to bound it. A caller at its bound gets `LimitsExceeded`.

**Well-known names are landed**, with the specification's three flags and
its four answers, the reservation, and a bound on both how many names one
connection may hold and how many connections may want one name.

`RequestName` answers `PRIMARY_OWNER`, `IN_QUEUE`, `EXISTS` or
`ALREADY_OWNER`. `ALLOW_REPLACEMENT` and `REPLACE_EXISTING` have to agree
before a name changes hands, and a displaced holder goes back to the FRONT
of the queue unless it asked not to be queued — so a name handed over and
handed back returns to whoever had it rather than to whoever asked longest
ago, and a holder that said it would not wait does not start waiting because
somebody took its name. A caller that cannot be given a place — because it
declined one, or because there is no room left — is told `EXISTS`, which is
the truthful answer in both cases: the name is taken and this caller does not
have it. Inventing a fifth code would tell a client library something it has
no branch for.

A REPLACEMENT is not refused for want of room, because a queue full of
bystanders must not be able to freeze a handover two peers have agreed on. It
does not grow the queue either: the displaced holder keeps its place only if
there is a place to keep, and is otherwise dropped from the queue while still
being told it lost the name. The bound holds; what gives way is the courtesy
of being re-queued.

Both bounds are charged on every path that ADDS this connection to a name,
including taking it from its holder. A draft charged them only on the
queueing path, so a peer at its limit could go on collecting names by
displacing consenting owners — reviewers demonstrated 52 names against a
limit of 32, and a queue of 24 against a limit of 16.

`ReleaseName` answers `RELEASED`, `NON_EXISTENT` or `NOT_OWNER`. Leaving a
QUEUE is a release and answers `RELEASED`: the specification's distinction is
between "not yours" and "nobody's", not between owning and waiting.

A well-known name ROUTES, which is the whole use of holding one, and every
lookup resolves through it: `GetNameOwner` answers the HOLDER's unique name,
`NameHasOwner` and `ListNames` account for held names, and
`GetConnectionUnixUser`/`GetConnectionUnixProcessID`/`GetConnectionCredentials`
answer about whoever is behind the name. The destination a caller wrote is
not rewritten on the way — a name is how the caller addressed it and the
`SENDER` is the broker's word about who answered.

This is also what makes the previous landing's forward guard observable at
last. A call is recorded against the connection the broker RESOLVED rather
than the name the caller wrote, so a reply from the holder still claims its
record and a holder's departure still sweeps the calls made to its name.
Until a name could be held, no test could tell the two apart.

`NameAcquired` and `NameLost` are DIRECTED signals, each to the one
connection it is about, so they need no subscription and land with the
machinery this broker already has. They are best effort, and the cost of
that is worth stating: they are state-machine signals rather than news, so a
peer that misses `NameLost` goes on believing it holds a name that now routes
elsewhere. Nothing better is available — the sweep runs from `Drop`, where
there is nowhere to report to, and disconnecting a recipient for being
behind is what the budget remedy above exists to avoid, since it did not
choose when this fired. A failure is therefore LOGGED as a broker-level
condition: reaching it means a peer's view of the bus and the bus's view of
it have parted. `NameOwnerChanged` is the subscribed broadcast form of the
same news. It is landed and filtered per recipient by the subject name,
which is the subscription-shaped version of the `see` question.

**A grant of a NAME is a grant of everything its holder serves**, and that
is worth stating because it is the shape of every destination-based bus
policy including `dbus-daemon`'s. The filter asks whether the caller may
address the name it wrote; the broker then routes to whoever holds it, and
the recipient dispatches on object path and interface without consulting the
destination. So a sandbox permitted to address `org.freedesktop.portal.*`
can reach ANY interface the portal's connection serves, not only the portal
ones §D enumerates. Three things bound it today. The holder of a portal name
will be the connection a supervisor registers as the portal, which is a
td-owned program rather than an arbitrary peer. Nobody can hold a portal
name without that capability. And a holder's UNIQUE name is an alias only
while it holds a well-known name the sandbox may address; losing the name
atomically loses that alias. Narrowing the grant to an interface set is a
real option and a separate decision: it has to keep `Peer.Ping` and
`Introspectable.Introspect` working, which every toolkit calls, so it
belongs with the portal landing rather than with the machinery that makes
names holdable.

**Seeing a name includes learning who holds it.** `GetNameOwner` answers
with the holder's unique name to any caller that may `see` the name asked
about. The same current-holder alias is visible and addressable when the
caller may see and talk to at least one well-known name that connection
currently holds. That is deliberate: a client must resolve a name to match
the `SENDER` of replies and signals, and portal Request and Session traffic
may be directed to the unique sender it learned. The authorization, ownership
check and route share the directory lock, so a connection that loses the
well-known name loses the alias before another message can route through it.

**The alias does not carry credentials.**
`GetConnectionUnixUser`, `GetConnectionUnixProcessID` and
`GetConnectionCredentials` do not follow a visible well-known name to its
holder and do not admit the holder's unique alias. A peer may ask about its
own unique name, and a holder asking about its own well-known name is answered
about itself; another application or the portal remains absent to those
queries. The host pid this design's identity story rests on therefore stays
withheld even though the routing identity is usable.

**A sandboxed application owns the names its permission file grants it, and
nobody at all may own a reserved one.** §D's default sandboxed policy owns no
name; the widening is the `[Session Bus Policy]` `own` entries, and those now
reach the broker. `td.Jail1`'s `Register` carries them as a fourth argument —
`Register(s instance, s app_id, as services, as owned) -> s token` — beside
the predeclared service list they are deliberately NOT merged with: `services`
names what an instance may activate on its own listener and `owned` names what
it may take on the session bus, and a registration that confused the two would
grant ownership of names an application only meant to answer for. They are
graded on arrival as well-known names, bounded at 32, and recorded on the
instance.

The grant travels with the IDENTITY rather than being looked up when a name is
asked for. Identity is resolved once at accept and never recomputed, and the
instance record behind it is swept as soon as its process ends — so a second
lookup at `RequestName` time could find the instance gone and silently drop a
grant the connection still holds, or find a different instance that registered
the same name in between. One walk answers about one instance and the grant is
part of that answer.

They are EXACT names. A grant of `org.mozilla.firefox` is not a grant of
`org.mozilla.firefox.Anything`, which is this file's own rule for session-bus
keys and is enforced by string equality with no prefix arm to argue through
later. The cost is stated rather than buried: an application whose names carry
a runtime-generated suffix cannot express its grant in this file at all, which
is exactly the MPRIS case §B.3.2 names: the instance suffix on
`org.mpris.MediaPlayer2.firefox.instance<N>` is chosen by the application at
startup. Admitting a suffix form
is an amendment to the key rule above, not a widening of the broker, and it is
owed before media keys and player integration work.

**An `own` entry carries the `see` and `talk` it implies.** `BusAccess` is an
ORDERED capability — `own` allows `talk` allows `see` — so a file that grants
a name has already granted the right to look that name up and to address
whoever holds it. A first version of this landing read `own` as ownership
alone, and the result was a broker that sent a peer `NameAcquired` for a name
and then answered `NameHasNoOwner` when that same peer asked about it, with
`ListNames` omitting it: the broker contradicting a fact it had just stated,
which is the failure the per-caller filter's own rules exist to avoid. The
grant does not depend on currently HOLDING the name, because it is the
permission file that confers it and a caller queued behind a holder has to be
able to reach it.

Standalone `see` and `talk` entries — for names the file does not also grant
`own` — parse and are refused by `td-jail` with a named reason. Widening what
a sandbox may address BEYOND its own names is a decision about the imported
services §B.3.2 lists rather than a mechanism this file is waiting on, and
admitting those entries before that decision would let a permission file claim
a grant nothing applies.

The count of `own` entries is charged where the file is graded, not only at
the wire. A permission file may carry 128 session-bus rows and a broker
records at most 32 names per instance; without a rule at the grader, a
33-entry file launched and failed with the broker's wire reader saying a list
"cannot be read", which names neither the file nor the ceiling.

The reservation covers `org.freedesktop.DBus`, `org.freedesktop.portal.*`,
`org.freedesktop.impl.portal.*` AND the two bare namespace roots
`org.freedesktop.portal` and `org.freedesktop.impl.portal`. The roots are
legal well-known names — a comment in the broker once claimed otherwise, and
that false premise was the whole argument for leaving them takeable — and the
permission parser has always refused an application that asks to own one, so
a broker that allowed them made the two graders state different rules with
the broker the weaker. Nothing is reachable through a bare root, since D-Bus
has no hierarchical routing; what was reachable was a namespace root held by
an arbitrary peer.

It holds against EVERYONE, including the
unconfined callers this design otherwise leaves unrestricted and including a
permission file that names one — which is the point, since an unsandboxed
same-uid process claiming `org.freedesktop.portal.Desktop` after a restart is
precisely the caller the reservation exists to refuse. It is refused twice
independently: once where a registration would record it, so a file that
claims it fails at launch rather than at first use, and once where the name
would be taken, which is the refusal that holds if the first is ever bypassed.
The crossing is explicit and narrow. A root, unconfined supervisor calls
`td.Portal1.Prepare(as names) -> s token` at `/td/Portal1`; the list is
bounded to the same 32-name ownership ceiling and contains distinct exact
members of the public or implementation portal namespaces. The broker proves
the supervisor from its socket and retains that process's pidfd with the
token. The intended portal child then calls
`td.Portal1.Activate(s token) -> ()` from its own connection. That connection
is pidfd-bracketed, and activation succeeds only while the retained
supervisor is still the same live process and the caller is its direct child.
No peer-supplied pid selects the portal process.

Successful activation atomically replaces the active process-and-name grant
and removes every owned or queued reserved-name claim the replacement no
longer authorizes. A hung predecessor therefore cannot keep the service, and
a queued predecessor cannot be promoted later. Only an unconfined connection
proved to be the activated `{pid, starttime}` may claim an exact granted name
through `RequestName`; that capability check and ownership change are one
directory operation. Pid reuse and an ordinary same-uid peer inherit neither
the capability nor a queued claim. The bare namespace roots never cross the
reservation. `td-svc` does not call these methods yet because `td-portal`
itself is not landed; the broker substrate no longer blocks that integration.

**A callee that never answers is still not bounded in TIME.** An entry
leaves the table when the call is answered, when the call could not be
sent, when the callee departs, or when the caller does. Nothing expires
it. A callee that stays connected and simply never replies therefore
leaves its caller waiting for ever — the failure this table was built to
remove, in the one shape the table does not see — and holds the caller's
128 places for the life of the connection, after which that caller can
call nobody. `dbus-daemon` has `reply_timeout` for exactly this. A clock
is a larger change than this landing: it needs a timer source the broker
does not have, a decision about whose clock, and an answer for a callee
that is slow rather than silent. It is recorded here rather than implied
by the paragraph above, which used to claim both halves were closed.

**An entry lives exactly as long as the call does**, and getting that
wrong reintroduces the failures the table exists to remove — reviewers
found two ways in and both are closed.

Resolving the destination and recording the call are ONE act under the
lock a departure also takes. Two acts leave a window: between a lookup
that found the callee and a record written after it, the callee can go,
its sweep runs over a table that does not mention this call yet, the
record lands behind the sweep, and the caller waits for ever on a
connection that has gone — with the entry holding its share of the bound
until the caller itself departs.

The record is written BEFORE the message is queued, because the callee's
reader is a different thread and may answer the moment the frame lands in
its outbox; so every path that fails AFTER the record has to undo it. A
relay can fail because the message cannot be re-encoded, because the
recipient is behind, because the bus is at its ceiling, or because the
recipient has gone. Each of those un-records the call. Left in place the
entry would be a call nobody will ever answer, and since a relay can fail
for reasons a THIRD peer caused, one peer could otherwise spend another's
whole allowance and leave it unable to call anyone.

The callee is recorded as the unique name the broker RESOLVED, never as
the name the caller wrote. Today the two are the same string, because
only a unique name routed; since `RequestName` landed they are not, and
recording the written name would test a reply from the owner against a
name the owner does not answer to — every genuine reply dropped, and
every one of those calls invisible to the departure sweep, which looks
for the departing peer's unique name. This was written as a forward guard
with no test, because nothing could observe it while only unique names
routed. It has one now, both halves: a call by name is answered by its
holder, and a caller waiting on a name is told when its holder departs.

**What may be SENT, beside who may be addressed.** The `see`/`talk`
filter decides who a peer may address; message type decides what it may
send them. A confined peer may CALL the portal, per §D's default. It may
not originate a directed SIGNAL: §D grants a sandbox the right to call
portal members and to RECEIVE the portal's replies and directed signals,
and a signal aimed at the portal is the reverse channel, which nothing
asked for and no toolkit uses. A directed signal from a confined peer is
dropped in silence, the same answer a broadcast gets. A reply is neither
— it is governed by the table above rather than by either rule.

Together these are what make the talk set safe to widen when
`RequestName` lands: the widening then admits calls to a name's owner
rather than arbitrary traffic aimed at it.

**A message can be legal to send and impossible to relay.** The broker
inserts a `SENDER`, and the cap on an incoming header-field array is the
same number as the cap on an outgoing one — object paths have no length
bound of their own — so a message whose fields sit within a couple of
dozen bytes of the ceiling cannot be re-encoded for delivery. It is
refused with `LimitsExceeded` and the sender stays connected; a draft
propagated it as a broker fault, which tore the sender down with no
reply at all for a message this broker had itself accepted. Giving the
incoming array explicit headroom for the field the broker will add would
close it properly, and is a change to what td-busd accepts rather than
to how it relays.

**A pid the kernel cannot report is not a name that has no owner.**
`SO_PEERCRED` answers 0 for a pid that does not exist in the reader's
namespace, which is the case td's own jails will produce.
`GetConnectionUnixProcessID` answers `UnixProcessIdUnknown` for it and
`GetConnectionCredentials` returns the entries it can fill — `UnixUserID`
without `ProcessID` — because an absent entry says "not known" where a
zero says "pid zero". Two drafts got this wrong in opposite directions:
the first reported `ProcessID: 0`, the second refused the whole call and
discarded a uid the kernel had reported.

**Foreign interop is checked against three independent implementations**,
because a broker that only talks to its own tests is a codec with extra
steps: sd-bus (`busctl list`, `status`, `call`), libdbus 1.12.16
(`dbus-send --print-reply`), and GDBus 2.86.0 (`gdbus call`) each
complete the handshake, say `Hello` and use the bus's own interface. The
one that exercises ROUTING is a `busctl` call to a parked `gdbus`
connection: sd-bus to GDBus, through td-busd, both `Peer.Ping` and an
`Introspect` that returns a real body — a full round trip in which the
broker is the only td code involved.

### What is landed of the bus on the image

**The broker is a boot job, and the boot proves it.** `[busd]` runs
`/bin/td-busd run --socket /run/user/1000/bus` through `td-login
exec-as tester`, and its `ready=` is a real `td-busd probe`: a client
that connects, completes `AUTH EXTERNAL` under the uid the kernel
reports for it, and reads back a well-formed `OK <guid>`. A broker that
bound the path and cannot serve it therefore never reaches ready, and
td-svc marks the unit failed rather than reporting it up.

It does not restart it, and the difference is worth knowing before
relying on `restart=always`: a readiness probe that never succeeds
changes the unit's PHASE and leaves the process running, while
`restart=` is evaluated when a process EXITS. So `restart=always` covers
a broker that dies, which is the common case, and not one that is up and
not serving. Nothing re-probes a unit once it has failed that way.

It `requires=seat`, and that is the load-bearing half rather than the
`after=` — but not because ordering is weak. A unit does not start until
every `after=` dependency has settled, so `after=seat` alone already
keeps the broker behind td-seatd; a draft of this section described a
race that td-svc does not have. What `requires=` adds is that a FAILED
seat settles too. With ordering alone the broker is released onto a
machine whose seat assignment did not happen: `bind` creates a missing
parent 0700 rather than refusing — deliberately, so a caller that made
its own directory is not turned away from a path it owns — so the broker
comes up, serves, and prints a healthy marker on a system with no seat,
no compositor and nothing that could use a bus. That is a marker saying
more than it knows, which is the failure this whole section is written
against. It also leaves `/run/user` itself created by an unprivileged
process rather than by the component whose job that is.

The same draft claimed clients would "look in the right place and find
nothing". They would not: td-seatd adopts an existing runtime directory,
`chown`s and `chmod`s it rather than replacing it, and the path is the
same literal string on both sides. The problem was never the path.

`/etc/bootsuccess` then probes the RUNNING broker in its health farm and
prints `TD-BUSD-RUN-OK`, which the image oracle requires. What that adds
over `ready=` is a failed BOOT, not a diagnostic: td-svc already logs
`readiness probe did not succeed in time` to the console, and the
broker — which declares no `log=` and so inherits td-svc's stdio — already
puts its own `cannot listen on …` there. Both land inside the tail the
oracle prints. What was missing is that a bus which never came up left
the boot GREEN, and a deployment marked successful is the thing that is
hard to walk back.

So the bus now stands beside uutils, ripgrep+fd, sshd, td-util, td-txt,
td-init and td-login in that farm — but its leg REPORTS and does not
VOTE. It prints the marker when the probe answers and never sets
`healthy=0` when it does not, and the difference is the whole argument of
this paragraph.

The first landing gave it the vote, on the `sshd` precedent: both are
shipped system services whose absence means the deployment is not what it
claims to be, and rolling back to a deployment that HAS them is the
fail-safe direction. Landing the jail's side of the bus on top of that
showed why this leg is not sshd's. `healthy` gates `td-boot success`,
which is what clears the attempt budget, so a leg with a vote is a lever
that rolls the machine back to `previous` — and unlike sshd's, an
application can pull this one. The broker admits 64 connections and 16
per peer PID, a jail is a PID namespace with no pids cap, and the farm's
probe is one more client asking for a slot. Sixteen connections from each
of four forks inside a single jailed application fill the table; the
broker then accepts the probe's connection and closes it, the leg fails,
and with a vote it would spend the attempt budget and roll the system
back. An unprivileged confined process reaching the deployment's rollback
decision is a worse outcome than a bus that is down.

What is kept is the half with no such lever. The image oracle still
REQUIRES `TD-BUSD-RUN-OK` on the console, so a build whose bus does not
come up fails at the gate — where there is no attacker, and where the
failure is a broken image rather than a running machine someone else can
push over. What brings the vote back is a per-INSTANCE admission key, not
the per-caller policy filter: filling the table is an admission problem,
and a filter on what a peer may SEND does not stop one opening
sixty-four sockets and saying nothing. Until then the marker is evidence
and not a verdict.

That the disclosure half of this landing is not live yet and the
starvation half is, is not an inconsistency — it is the reason the two
halves are treated differently. Filling the connection table takes
`connect(2)` sixty-four times and nothing else: no D-Bus library, no
message, not even a handshake, so any jailed process can do it the day
this lands. Every disclosure above needs a second peer, which does not
exist yet.

The residual is named rather than left implied: a broker that passes in
QEMU and fails only on a particular machine now leaves that deployment
marked SUCCESSFUL. Nothing recovers it either, because the failure mode
is a readiness probe that never succeeds, which sets the phase and leaves
the process running where `restart=` cannot see it. Applications on such
a machine do not start either, since `plan_launch` resolves the bus
socket before it unshares — but they retry rather than being skipped, so
a broker that comes back brings them with it. The console carries both
td-svc's `readiness probe did not succeed in time` and the broker's own
`cannot listen on …`. That is a diagnosable machine
and not a self-healing one, which is the trade accepted here: nothing on
the image needs the bus yet, and the alternative was leaving an
application able to roll the system back. Two things would change it — a
per-instance admission key restoring the vote safely, and a td-svc that
treats a never-ready daemon as a restartable failure. The second is a
td-svc change and belongs to `td-svc/DESIGN.md`, not here.

The marker itself keeps a bounded retry that does not depend on the vote.
`healthy=0` was what kept `/etc/bootsuccess`'s loop sweeping, so removing
it from this leg would have left the marker one attempt and reddened the
image on a broker `restart=always` happened to be restarting. The leg
counts its failures instead, and the loop's success gate waits for the
marker while that count is under `BUS_MARKER_GRACE_SWEEPS` — a delay of a
fixed few seconds, never a withheld `td-boot success`, which is exactly
the difference between a retry and a veto.

The probe is also the eighth `su` block in that farm and the only one
with a bounded wait of its own, which is why the guest's per-iteration
budget and the host's boot ceiling both moved with it.

**What the marker does NOT say.** It is the handshake and nothing more,
because `probe` is the handshake and nothing more. Read a green marker as
*the bus is reachable*, not as *the bus works*.

Since the jail registers, the image is no longer silent on the bus: each
launch says `Hello` twice, owns two unique names in turn, and has the
broker's replies routed to it. What that exercises is the handshake,
`Hello`, and the two `td.Jail1` calls. It does NOT exercise a message
routed between two peers, a signal, a match rule, a well-known name, or a
descriptor — and the fixture opens no connection from INSIDE the jail, so
lineage resolution and `td.AppId` are still not exercised by a boot.
Those remain held up host-side and against sd-bus, libdbus and GDBus,
and the portal is what will hold them up here.

It also checks a PATH and not a pid. The probe has no association with
the unit's process or generation, so what it establishes is that
something at `/run/user/1000/bus` completed the handshake as the login
user — not that the process td-svc supervises is the one that answered.
Nothing else on this image binds that path, which is what makes the
marker worth having; the day something else could, this is the
assumption to re-check rather than a claim to keep repeating.

**It shipped before anything consumed it, and now the jail does.** The
argument for booting a broker with no clients was that the parts most
likely to be wrong are exactly the parts only a boot can exercise — which
user the unit runs as, which directory exists at the moment it binds, and
whether the socket the unit names is the socket a client finds — and that
landing those before the portal rests on them is cheaper than diagnosing
them underneath it. That argument is now spent in the way it was meant to
be: the jail is the first consumer, and it found the socket the unit
named.

### What is landed of the bus inside the jail

**The socket is bound in, always.** §C's mount plan, step 12, has said
`bus <- bind, ALWAYS (the broker is the policy, not the mount)` since it
was written; the jail now does it. `/run/user/1000/bus` is bound
read-only into the jail's own `/run/user/1000` exactly the way
`wayland-0` is: a socket inode made by binding a listener and dropping
it, a private bind over that inode, then a `require_bind_source` at
preparation time and a `require_mount` the confined process checks
against its own `/proc/self/mountinfo` after `pivot_root`. The runtime
directory's name roster becomes exactly `td-app`, `bus` and `wayland-0`,
so a fourth entry there is a refusal rather than a surprise.

Read-only costs the app nothing, and it is worth writing down exactly
what it buys, because a draft of this paragraph named the wrong thing.
`connect(2)` is unaffected: read-only here is a vfsmount flag enforced by
`mnt_want_write()` on the write paths, not by `inode_permission`, and
`SCM_RIGHTS` is socket-layer and sees no mount flags at all. It does NOT
stop the app replacing the socket — unlink is governed by the parent
directory, which is the jail's own writable tmpfs; what refuses it is
that the path is a mountpoint (`EBUSY`) and the app has no
`CAP_SYS_ADMIN` to unmount it. What `MS_RDONLY` actually buys is `chmod`
and `chown`, which do take a write reference. The app owns that inode —
uid 1000, mode 0600, and the jail maps `1000 1000 1` — so without the
flag it could `chmod 0000` the HOST's real bus socket through its own
bind and deny `connect(2)` to the compositor, the portal and the
`/etc/bootsuccess` probe.

**`DBUS_SESSION_BUS_ADDRESS` was already compiled and never checked.**
The engine has put `unix:path=/run/user/1000/bus` into every application
spec since the tier existed, and td-jail's environment contract — which
already held `HOME`, `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` to exact
values — did not mention it. A spec could therefore name a bus the jail
does not mount, and nothing would say so until an application failed to
reach one. It is now the fourth entry in that contract and it is checked
by VALUE, not by presence: an address that is well formed and points
somewhere else is refused, because the address is not advice about where
a bus might be. It is the name of the one socket this jail binds.

Both halves are now held to one value by something that can notice them
drifting apart. The engine REFUSES a manifest that sets any variable the
jail pins, so a package cannot compile a spec whose only symptom is a
launch failure reported by the sandbox at the far end from the manifest
that caused it. And the td-jail recipe runs `validate_environment_list` —
the contract a real launch runs — over the text the engine compiles, with
a negative case that moves the bus one path over. A draft searched the
emitted text for a literal built from an ENGINE constant, which pins the
engine against itself and would say nothing if td-jail started expecting
a different path.

**There is no `sockets=` permission for the bus**, and step 12 gives the
reason in five words: the broker is the policy. A per-app switch would be
a second place to say no, in the component with no policy language, about
a question belonging to the component that will have one. What an app may
DO on the bus is the broker's business; that it HAS one is not a question
the mount namespace should answer differently per app.

**The broker draws that boundary now; the remaining work is substrate
exposure and service integration.** A draft of this section wrote that a per-app switch would
duplicate "a boundary td-busd already draws per connection", and when it
was written that was false: no well-known names, no match rules, no
per-caller filter, every peer able to list every unique name, read any
peer's uid and pid out of `GetConnectionCredentials`, and send a directed
call to any of them, with no call/reply pairing to stop a forged
`METHOD_RETURN`. Rungs 15a–15c and the match-rule landing closed those
broker-policy gaps. A confined peer now resolves to a jail identity taken from
`SO_PEERPIDFD` at accept; it sees and addresses the portal, its own name, the
names its permission file grants, and the current unique-name holders of
those well-known names. It may be told credentials only for the broker and
itself; well-known names and a bounded owner queue exist;
and a reply is delivered only against a call the broker actually routed;
matched broadcasts are delivered once and filtered by the same visibility
policy. Admission now resolves that identity before taking a place and keys
the share on the registered instance, so one application's children do not
each receive another quarter of the table.

Two more belong on that list. Descriptors cross between negotiated peers,
but the global open-descriptor budget is still shared rather than charged
to an application instance. A jailed peer can drive that shared budget to
its relief path and lose the connection holding the most attachments: safer
than evicting the unrelated peer that observes the pressure, but still the
same attribution gap as the connection table, through a different door. The
read-only bind does not reach it because `SCM_RIGHTS` is socket-layer. And
`GetConnectionCredentials` reports the pid `SO_PEERCRED` gave the broker,
which is a pid in the INIT namespace, so a jailed caller reads host
pids — its own included — through a channel its PID namespace otherwise
closes.

Every one of those is a problem between PEERS. What makes them
unreachable today is that nothing inside a jail opens the bus: the
fixture does not, and there is no second application. Half of that can be
machine-checked and is — the system recipe asserts
`SHIPPED_APPLICATIONS.len() == 1`, and a second entry breaks the build
with a diagnostic naming what has to land with it.

Be exact about what that tripwire does NOT cover, because a gate believed
to cover more than it does is worse than no gate. It counts
APPLICATIONS; the exposures are about PEERS.

- **`td-portal` will not trip it.** It is the next thing that will speak
  D-Bus and it is not a `ShippedApplication`. The broker now reserves and
  activates its exact names, hides its credentials, authenticates replies,
  and routes directed handles with the count still one. The service's own
  methods, handle lifecycle and compositor authority are the new surface the
  tripwire cannot grade.
- **One application is already two peers.** Nothing takes a
  single-instance lock, and the image has two launch routes for the
  fixture: the `jail-fixture` unit and the compositor's launcher menu.

The per-caller filter and per-instance admission key are now landed, so the
connection-table condition this paragraph used to defer is discharged. The
remaining shared descriptor budget is named above rather than hidden by that
statement, and portal service integration remains the next peer that makes
the routed policy observable.

---

## E. `td-portal` — the portals

All interfaces on `org.freedesktop.portal.Desktop`, object
`/org/freedesktop/portal/desktop`, with `RequestName(…, DO_NOT_QUEUE)`
and exit unless `PRIMARY_OWNER`. Every interface serves a `version`
property through `Properties.Get`, because toolkits read it before
calling — Properties is part of the first portal landing, not an add-on.

### Request and Session

A portal method returns a handle and the answer arrives as
`Request.Response(u response, a{sv} results)` on that object. The path is
**caller-derived** so a client can subscribe before calling: sender
`:1.42` plus `handle_token="t7"` gives
`/org/freedesktop/portal/desktop/request/1_42/t7`. The request is
exported *before* the method reply, closing the subscription race.
Sessions use `…/session/1_42/<token>` with `Close`/`Closed`. Responses:
0 success, 1 cancelled, 2 other. Getting this exactly right unblocks
every toolkit and getting it subtly wrong fails as timeouts, so its tests
are wire-level fixtures against the spec. The broker half is landed and has
wire-level coverage: a jailed call reaches the activated public portal name,
the authenticated method reply returns a Request or Session object path, and
the portal's directed `Response` or `Closed` signal reaches only that caller.
Export-before-reply and the handle objects themselves remain `td-portal`
service work.

### Order

| # | interface | needs | decision |
|---|---|---|---|
| 1 | `.Settings` | nothing | first, no UI. `org.freedesktop.appearance` `color-scheme`/`accent-color`/`contrast` plus the `org.gnome.desktop.interface` font/theme/cursor keys, from one td session config file — never inferred from absent GNOME services. Removes GTK's startup portal probe as an unknown. |
| 2 | `.Account` | a consent dialog | uid 1000's passwd entry, empty image URI. |
| 3 | `.FileChooser` | a surface | `OpenFile`/`SaveFile`/`SaveFiles` with filters, `current_filter`, `choices`, `current_folder`, `multiple`, `directory`. |
| 4 | `.OpenURI` | nothing | scheme→handler registry generated from installed exports; `http`/`https` start the configured browser via its `/bin` entry. The fd-taking `OpenFile` member returns NotSupported in v1. **`file` is REFUSED in v1, not merely restricted to "a path visible to the caller"** — which an earlier draft said and which does not follow: the handler runs in a *different* sandbox, so a path the caller can see is one the handler generally cannot, and launching it would open a file that is not there. Honouring `file:` needs the path to reach the handler's namespace, which is a Documents-portal job (deferred, no FUSE) or an explicit grant to the handler at launch. Refusing is the honest answer until one of those exists; "opened" and then blank is worse. |
| 5 | `.Inhibit` | compositor idle state | **idle flag only.** Suspend/logout/user-switch are refused, because td has no session manager and returning success without an observable inhibitor violates the readback principle. **The mechanism is the private `create_idle_inhibitor` and NOT public `zwp_idle_inhibit_v1`** — a review found both specified, and they cannot both hold: advertising the public global to every sandbox lets an app inhibit idle *directly*, with no portal record of who did it and no way for the user to see or revoke it, which is exactly the attribution the private call exists to provide. §F's `zwp_idle_inhibit_v1` row is therefore **not** required for `.Inhibit` to be honest, which is what it used to say; if it is ever implemented it is for unconfined clients, and it must not be advertised on a jailed connection. |
| 6 | `.Notification` | private protocol | bounded toasts with title/body/priority/actions and a bounded icon; markup, sound and arbitrary icon paths refused. |
| 7 | `.Screenshot` | private protocol | full output and, with `interactive=true`, a compositor-selected window — **not** a window named by a bearer `parent_token`, see below. PNG encoded std-only, CRC-32 and Adler-32, with a **fixed-Huffman** DEFLATE encoder (~1–2k lines, no roster change). Not *stored* blocks: 1920×1080 stored is ~8.3 MiB per screenshot, and the portal returns a file the user keeps. |
| — | `.Background` | — | `RequestBackground` returns denied; persistent background execution needs a td-svc user-service design. |
| — | `.Documents` | FUSE | **absent** (§0: no `CONFIG_FUSE_FS`). See below. |
| — | `.Print`, `.Camera`, `.ScreenCast`, `.RemoteDesktop` | spooler / PipeWire | **not exported.** A fake PipeWire descriptor would make successful setup indistinguishable from a broken stream. |
| — | `.Secret` | a keyring | deferred; apps fall back to plaintext inside the app dir. **That is a security DEGRADATION and the earlier claim that it is "the same trust boundary a keyring would be" is wrong** — a keyring adds encryption at rest, a lock state that survives the app, release mediated per secret, and the ability to share one secret between apps under control. Plaintext in the app directory has none of those: it is protected only by the directory's mode, so anything that reads the app's files reads its passwords. It is an acceptable v1 position for a single-user machine whose disk is not encrypted anyway, and it should be recorded as a gap rather than as parity. |

**The Documents consequence, stated honestly rather than buried.**
Without it, a file chooser can only grant what the sandbox can already
see — the app's own home and its declared `--filesystem` grants.
Selecting anything else returns response 2 with a named diagnostic.
Firefox with its default `xdg-download` grant downloads and uploads
fine; darktable needs an explicit `xdg-pictures` grant; several GNOME
workflows will look like they work until the user picks a file outside a
granted root. Landing it later needs `/dev/fuse`, the FUSE wire protocol,
host-to-app mount propagation, persistent document ids and revocation —
a separate normative design.

**Two things about that deferral, since it is easy to state wrongly.**
First, "no FUSE" is the reason it is absent *today*
and not the reason it is hard: serving `/dev/fuse` is ordinary read and
write, no new syscall, so the kernel pin is a prerequisite rather than
the obstacle. What makes it a separate design is its *size* — 15–25 kloc
of protocol plus a document-grant model. Second, this section used to
say it must not grow "a retained privileged process" — but propagating a
mount into a running sandbox's namespace is not something an
unprivileged process can do for itself, so a **narrow root mount broker**
is exactly what it would need. The resolution is narrower than the
refusal: what is refused is a *general* privileged helper and any
setuid `fusermount` substitute; what may be designed, on its own merits
and with its own review, is a broker whose entire vocabulary is "mount
this already-open `/dev/fuse` connection at this path in this registered
instance". Whether even that is worth it is the open question, and it is
open — this document should not pre-refuse it with a sentence that
contradicts its own audit section.

**ScreenCast's cost, for the record**: a minimal PipeWire-compatible
server — object registry, SPA pod codec, buffer negotiation, shm
transport, timing, nodes, portal sessions — is plausibly 30,000–60,000
lines. Firefox screen-sharing does not work on td, and that is a
deliberate refusal rather than a gap to be filled opportunistically.

### Who draws the dialog

**`td-portal` draws it as an ordinary Wayland client**: a keyboard-first
list navigator reusing the launcher's filter model, the multicall's PSF2
font, and the software renderer, appearing as a normal tiled window
titled `Open — <app name>`.

Not a compositor overlay — a file browser inside the compositor process
couples session survival to directory-listing code, which DESIGN.md's
whole trust-boundary argument runs the other way. Not a `td-term` picker
— summoning a terminal and a shell per portal request would make GUI
authorization depend on a shell. Being "just a client" is also what makes
it testable with the existing socket-pair harness.

### Caller authentication

The portal asks the broker who the caller is and **believes the answer**
— it performs no `/proc` check of its own. A draft had it re-verify
`/proc/<outer-pid>/root/.flatpak-info` beside the broker's reply, and
that was wrong twice over. It contradicts §D, which abandons that file
as an identity oracle for a specific reason — a nested Firefox child can
change its mount namespace and so change what the path resolves to, which
would make the portal deny the exact application this project exists to
run. And it asks for something the broker does not return: the reply is
one of three values (below), not a pid the portal could walk. **The file
is what an application reads to learn its OWN id**, which is why mount
step 13 generates it and why host tooling expects it there; it is not
evidence about anybody.

Same-uid *unsandboxed* processes get no app id and full portal access —
td's existing model, where ceilings between same-uid clients are
availability bounds rather than isolation. That sentence belongs in
`AGENTS.md` so nobody mistakes the portal for an intra-uid boundary.

**But that rule and §D's collide, and the collision is the security
question this section most needs to answer.** §D says "a process whose
lineage cannot be proven is denied"; the sentence above says a process
with no app id gets everything. Those are opposite resolutions of what is,
at the socket, *the same observation* — a peer the broker cannot tie to a
registered instance. Read naively, the portal's rule turns a failure to
prove containment into a **promotion**: an app that manages to defeat the
lineage check stops looking sandboxed and starts looking like a trusted
desktop process. That is fail-open in the one place this design cannot
afford it, and it arrives not from a weak check but from two components
each being locally reasonable.

**The fix is that "unknown" must be a third answer, not a synonym for
"unsandboxed".** The broker answers a caller-identity query with exactly
one of:

- **`Jailed { app_id, instance }`** — lineage proved, start times stable
  across the check. Policy applies.
- **`Unconfined`** — a *positive* result, not a default: the peer pid is a
  descendant of no live registered instance, and every registered
  instance's stage-2 pid is accounted for at query time. This is provable
  because the registry is complete by construction — nothing enters a
  jail except through stage 0, which registers before it unshares (§D's
  two-phase registration) — so "descends from none of them" is a real
  statement rather than an absence of evidence.
- **`Unknown`** — anything else: a registration in flight, a start time
  that moved, an instance whose stage-2 pid died between the two reads,
  `/proc` unreadable for the peer. **Denied by both the broker and the
  portal**, with a diagnostic naming the ambiguity.

So §D's rule is the general one and this section's is a statement about
`Unconfined` specifically. The distinction costs one enum and buys the
property that no failure of the identity mechanism can ever *increase* a
caller's authority — which is worth more than the portal access it
occasionally denies to a legitimate desktop process during a race.

The residual exposure is unchanged and already stated: a genuinely
unsandboxed uid-1000 process is uid 1000, and the portal is not a
boundary against it. What this closes is narrower and was open —
a *sandboxed* process reaching that status by breaking a check.

### The private portal ↔ compositor protocol

It is not a public Wayland global. The first draft justified that by
saying there is **no per-client credential** to key a privileged global
on, and that is false: a Wayland server accepts each client on its own
`AF_UNIX` socket, so it can read `SO_PEERCRED` per connection exactly as
`td-busd` does, and it may advertise a different registry to each one —
per-connection globals are ordinary Wayland practice, not a stretch.
After per-app uids (§L) the peers would differ by uid as well.

So the reason is a **policy choice, and a better one**, rather than a
protocol necessity. Keying on `SO_PEERCRED` would mean the privileged
global exists on the same socket every sandboxed app is already
connected to, one predicate away from being served to the wrong peer; a
bug in that predicate is a screen-capture global handed to Firefox.
Keying on **path visibility** means the compositor listens on a second
socket, `/run/user/1000/td-portal-wayland-0`, that no jail ever mounts —
so the privileged interface is not reachable to be mis-served. That is
td-seatd's argument in the same words: the boundary is what a process can
*name*, and an absent socket fails safe in a way a conditional does not.
Both mechanisms may be used together — the private socket SHOULD also
check `SO_PEERCRED`, since defence in depth costs one call here — but the
socket is the boundary and the credential is the belt.

**What that boundary does not survive is an escape, and it is worth
naming the consequence rather than leaving it implied by §L.** Path
visibility is a mount-namespace property. An application that breaks out
of the filesystem jail is still uid 1000 in v1, so it can open
`/run/user/1000/td-portal-wayland-0` directly and drive
`td_portal_manager_v1` with no portal UI in the way — which means
screenshot and screencast without the dialog that authorizes them, the
one capability the portal exists to gate. The jail is the boundary; the
socket path is an organizing convention behind it, not a second lock.

This is the same v1 same-uid exposure §L records, but it is worth
separating because its blast radius is worse than the general case: most
of what an escaped app gains at uid 1000 it could already ask for
through the portal *with* a prompt, whereas this specific bypass converts
a prompted capability into a silent one. Per-app uids close it, which is
a further argument for scheduling them; until then, the honest statement
is that the portal authorizes **confined** applications and stops meaning
anything the moment confinement fails.

```
td_portal_manager_v1
  get_dialog(wl_surface, parent_token, flags)   float, centre, modalize
  dismiss_dialog(wl_surface)
  capture_output(request, output, flags)        bounded screenshot bytes
  capture_toplevel(request, app_id, parent_token)
  create_idle_inhibitor(id, app_id, reason)
  create_notification(id, app_id, title, body, actions)
  close_notification(id)
  get_parent(token)                             resolve an xdg-foreign handle
```

The frame crosses as a descriptor into a plain unlinked temp file the
portal creates under `/run/user/1000` — no memfd syscall, no new surface,
since td-portal rides `conn.rs`.

**"No new surface" is right about SYSCALLS and wrong about the roster**,
which is a distinction `UNSAFE.md` §6 draws deliberately and this
document owed a row for. That section pins the transport's USERS in code
— `TRANSPORT_USERS = ["client.rs", "conn.rs", "term_client.rs"]` in
`td-compositor/src/main.rs` — precisely so a module can reach `sendmsg`
and `recvmsg` through a `Connection` without ever spelling `sys::`, which
is all the caller scan looks for. A portal personality holding one is a
FOURTH user, and `UNSAFE.md` says in terms that a module joining that
roster is an amendment. The confinement test will red the landing, which
is the mechanism working; what was missing is that §V.2's sequencing of
the three surface amendments has no row for this one, and it belongs
beside them.

**Two corrections to that, both from review, and both about the gap
between "the bytes arrive" and "the portal is honest".**

*A capture needs a URI the caller can open, and an unlinked file has no
name.* `Screenshot` answers with a `uri` in its `Response`, and the
caller then opens that path from inside its own jail — where the portal's
private runtime directory is not mounted, and where an unlinked file has
no path at all. So the descriptor is the *transport*, not the answer:
the portal writes the PNG into the requesting app's own granted output
location (its state directory by default, or a directory the user picked
in the same interaction), and returns THAT path. The unlinked temp file
survives only as the portal's own scratch buffer. Relatedly, the private
protocol signature above is incomplete for what it has to carry — a
capture needs a **completion event, a byte length, an error arm and the
descriptor itself**, none of which a one-way `capture_output(request,
output, flags)` expresses. Treat those four as part of the request rather
than as detail to be discovered during implementation.

*An xdg-foreign handle is a BEARER token, not an identity.* Using
`parent_token` to authorize `capture_toplevel` conflates two unrelated
things: the handle exists so a dialog can be *parented* to a window, it
is deliberately passable between clients, and any app that comes by
another's handle — which the protocol is designed to permit — would gain
the right to photograph that window's contents. Parent anchoring and
capture authority must therefore be **separate**: anchoring keeps the
handle, and capture requires either that the compositor confirm the
target toplevel belongs to the *requesting* instance, or an explicit user
selection in the compositor's own interactive picker (`interactive=true`,
which already exists above). No path may turn possession of a handle into
pixels.

**Which is why `capture_toplevel` carries an `app_id`** — the signature
above omitted it and the requirement was therefore unimplementable, since
every portal request reaches the compositor over the PORTAL's own Wayland
connection and the compositor sees only that one client. There is nothing
to compare the toplevel's owner against unless the portal says who asked.
`create_idle_inhibitor` and `create_notification` already pass it for the
same reason; capture is the request where omitting it costs the most. The
`app_id` is the broker's answer rather than anything the caller supplied
(§D), so passing it here forwards an authenticated fact rather than
introducing a claim.

These cannot be public globals because unrestricted screenshots are a
confidentiality failure, a portal dialog cannot be an `xdg_popup` child
of another client's surface, modal input capture across clients is
compositor policy, and an inhibitor must be attributed to the requesting
sandbox rather than to the portal process. **Parent-window handles come
from `zxdg_exporter_v2`/`importer_v2`** (§F) and only the compositor
resolves them; the compositor authenticates no token an app supplied.

---

## F. The Wayland protocol gap

Verified against the tree rather than assumed. Today the compositor
advertises exactly eight globals: `wl_compositor` v4,
`wl_subcompositor` v1, `wl_shm` v1, `wl_output` v4, `xdg_wm_base` **v1**,
`zxdg_decoration_manager_v1` v1, `wl_data_device_manager` v3,
`wl_seat` v7.

That set is no longer only a claim here. The decoration landing added the
sixth and introduced
`the_registry_advertises_exactly_the_globals_td_serves`; the subsurface
landing adds the seventh and updates the same test. It pins the name, order
and version of each against `advertise_globals`; the data-device landing adds
the eighth through that gate, so the next change to the list reds a test in
the crate rather than silently falsifying this paragraph.

Three corrections to the obvious assumptions, all checked in
`td-compositor/src/server.rs`:

- **`wl_shm` already advertises ARGB8888 *and* XRGB8888**, and
  premultiplied alpha is software-blended into the XRGB framebuffer — so
  GTK's client-side decoration shadows and rounded corners have a path.
  This is the single largest piece of good news in the gap analysis.
- **Pointer axes are implemented and the gate is CORRECT**: `axis`,
  `axis_source`, and `axis_discrete`, with the deprecation at v8
  reasoned about in a comment. An earlier draft of this bullet claimed
  the gate got one of the three wrong — that all three were gated at ≥5
  together, so a client binding `wl_pointer` at v1–v4 would get no
  scroll at all. **That claim was false and is withdrawn.** The gate is
  already per event: in `pointer_messages` the `version >=
  WL_POINTER_AXIS_EVENTS_SINCE` block wraps only `axis_source` and
  `axis_discrete`, and the bare `axis` is pushed outside it, so every
  version receives it; the caller gates only `wl_pointer.frame` at ≥5,
  which is right because `frame` did arrive in v5. It is covered:
  `pointer_worker_encodes_frames_versions_and_shared_serials` asserts a
  version-4 pointer receives the bare axis event while a v7 one gets the
  full triple plus `frame`. Do not "fix" this — the change the old
  bullet asked for is a no-op at best and a regression at worst. The
  correction came from the compositor workstream verifying the bullet
  against `server.rs` exactly as §V.4 asks, which is the process
  working rather than a defect in it.
- **Client cursors are DONE.** This row said they were not, measured
  against `origin/main`: `set_cursor` validated serial authority and
  assigned `SurfaceRole::Cursor`, then read and **discarded** both
  hotspot values and never rendered the surface. `ui: a client draws its
  own cursor, where its hotspot says` (`1c4b7f88`) has since landed on
  main — rendering at the hotspot, per-surface contents, a per-client
  1 MiB bound, focus-scoped — so the ~400-line estimate below is spent
  and the row is closed. That was not an erratum in the table; it is the
  ordinary consequence of writing a state snapshot against a repository
  several agents are moving, and it is why §V.3 requires B to re-read
  `server.rs` rather than trusting this section. Expect the same of any
  other row here that `ui-rolling` reaches first.

Those two hard errors are gone. `create_positioner` and `get_popup` returned
`"xdg_positioner is not supported"` and `"xdg_popup is not supported"`, so **a
GTK app opening its first menu was disconnected outright.** `ui: a menu is
placed where its client asked and floated over its window` implements both: the
positioner's rules are recorded and copied at `get_popup`, the popup is placed
by anchor, gravity and offset — by its window GEOMETRY's corner, so a toolkit's
shadow margin does not displace the menu — and it floats over its parent rather
than joining the layout: drawn above every window, hit-tested before the tiles
but not over td's own bar, held to the protocol's requirement that it abut its
parent, stacked by td's own order rather than by an object id libwayland
recycles, and dying with its parent along with any submenus hanging off it.
Hovering a menu counts as hovering the window that opened it, since
focus-follows-mouse deactivating that window is how a toolkit is told to close
the menu.

Constraint adjustment now flips, slides, and resizes a popup in protocol order
against the visible parent's usable output. The grab takes the KEYBOARD, and a
press outside the chain now closes it. What td DOES
signal now is its own dismissal: every popup a take-down cascades over is sent
`xdg_popup.popup_done`, deepest first, so a menu whose window went is no
longer open as far as its client knows — and is unmapped at the same time, as
the protocol makes those one act, so a client that misses the event and
repaints is refused rather than handed its menu back. A menu that can never be
placed is told sooner and by the same event: a popup whose parent is already
gone when it makes its initial commit is dismissed AT that commit rather than
configured, so td stops inviting a buffer it has already decided to decline,
taking the submenus the scene is holding down first, as any cascade does.
Answering an initial commit with a dismissal rather than a configure is a
deliberate divergence from xdg_surface's unconditional wording, recorded with
its reasoning in `td-compositor/DESIGN.md` §3. Where the parent is still
there, a re-map gets its serial back but not a second placement, which
`xdg_wm_base` version 1 has no event to revise. The grab itself is no longer
only a record: the topmost grabbing popup has keyboard focus, which is what
the protocol requires of one, bounded two ways that are td's own and recorded
as divergences: the menu must have a pixel on screen, and it must hang off the
FOCUSED window. The second is what makes a grab safe to honour — without it a
background client could take every key typed with no way for the operator to
get the keyboard back, which review demonstrated — and it leaves the two
gestures that move focus as the way to take the seat back. A menu can now be
driven with the arrow keys and closed with the Escape its own client sees.
Focus-follows-mouse is suspended
for the length of a grab, so the window under the menu is the one that has the
keyboard back when it closes. The pointer half is landed with it: a press with
none of the grabbing menus under it closes them, deepest first, and the press
is consumed rather than also delivered — except for the grab-owning client,
which the protocol gives its own pointer events as normal. What remains of
THIS row is a chain on a workspace SWITCH, which the dismissal path now exists
for and is held for its own landing; constraint solving has a row of its own
further down.
`td-compositor/DESIGN.md` §3 lists what is still open across all of them —
constraint adjustment and reposition, with the unenforced "parent must be
mapped first" rule and the dismissal cascade's placement-versus-object-graph
gap recorded beside them.
Several conformance gaps beside them are closed: `xdg_wm_base`'s error codes
are raised on the shell object rather than on the xdg_surface the request
arrived at, that shell object may not be destroyed before the surfaces it
made, a second role object on one xdg_surface is `already_constructed`, a
surface's role kind outlives the role object that carried it, and a menu may
not be destroyed while a submenu hangs off it. A popup's parent edge is also
broken rather than left holding a number when the surface it names is
destroyed, so a reissued id cannot re-parent a menu onto a window that never
opened it. The renderer's depth bound stays regardless: it is termination,
where the rules above are only a reason to expect no cycle.
`set_window_geometry` WAS the other of the two, parsed and discarded — a true
no-op, so CSD margins tiled as dead borders and clicks landed offset, though
the client survived. That row is closed: `ui: a window geometry is the part of
a surface td tiles` honours it as a crop on both the paint and the hit test,
with the two divergences it takes recorded in `td-compositor/DESIGN.md` §3.
`wl_surface.attach`'s x and y were a second no-op of the same kind, parsed and
dropped; `ui: an attach's offset is the cursor's to move` gives them the one
role that has a use for them, the cursor whose hotspot the protocol decrements
by them, and records why a tile ignores them and what an offset cannot reach.

| interface | td state | class | cost |
|---|---|---|---|
| `xdg_surface.set_window_geometry` honoured | **landed** | — | ~250 across scene and hit-test — spent. The geometry is the crop: a tile draws the client's own rectangle from its own origin and a pointer arrives in the client's coordinates. Two deliberate divergences, both in DESIGN.md §3 — the surface intersection is taken where it is used rather than frozen at the applying commit, and a geometry naming no part of the surface leaves the whole surface standing rather than cropping to nothing |
| `wl_shm` ARGB blending | **present** | — | verify with a golden |
| `wl_subcompositor` | **landed** | — | spent. Version 1 implements nested compound windows, parent-commit application of association/position/z-order, synchronized and desynchronized commits with recursive inherited synchronization through clean intermediate nodes, exact sibling validation, permanent role-kind and inert-object lifecycle, recursive rendering and child-local hit testing. A compound is clipped to its root's owned client rectangle, so signed children cannot cover compositor chrome or another tile. Parent plus synchronized descendants, and a parent-destruction cascade, mutate and settle under one runtime lock; their wire events wait until that lock is released. Cached callbacks are retired on teardown and cached input regions remain inside the aggregate quota |
| `wl_data_device_manager` v3, selection | **landed** | — | spent. Versions 1–3 bind one seat clipboard. Selection offers follow client keyboard focus, precede `wl_keyboard.enter`, survive focus changes between one client's surfaces, and become stale on cross-client leave or replacement. Server-created ids, MIME strings, retained offers, incoming descriptors and the cross-client seat queue all have explicit ceilings. `receive` preserves the supplied open-file description and rechecks current source identity and destination focus before forwarding exactly one descriptor to `wl_data_source.send`; a lost race or stalled source closes it as EOF. Source generations close id reuse. Replaced sources receive `cancelled`, source destruction/departure clears the selection, and version-3 DnD sources are immediately cancelled while older and NULL-source requests remain explicit no-op divergences because drag-and-drop is deferred |
| `xdg_positioner` + `get_popup` | **landed** | — | ~900 — spent. Rules recorded and copied at `get_popup`; anchor, gravity and offset resolved on independent axes; the popup floats over its parent, above the tiles and below td's own bar, hit-tested first (though never over the bar), placed by its window geometry, required to abut its parent, stacked by td's own order, and relative to the parent so a submenu hangs off the menu that opened it. A null parent is refused (td implements no protocol that could supply one later) and a zero-area anchor rectangle is accepted as a point |
| popup grabs + dismissal | **part** | **U** | 1,200–2,000, ~1,300 spent — `popup_done` is now sent for every popup a take-down cascades over, deepest first, which is the order the protocol makes a client destroy nested popups in; a menu whose window went is no longer left open to its client. The surface is UNMAPPED with it, which the protocol makes the same act: the configure tracker is reset, so a client that ignores the event and repaints gets `unconfigured_buffer` instead of its menu back, and the runtime refuses to place a menu it has recorded as over, so a commit already in flight when the press landed is dropped rather than painting the menu back over a grab that has gone — the dismissal is enforced rather than advised. A popup whose parent went before its own initial commit is dismissed AT that commit rather than configured and then refunded, taking the placed submenus down with it, deepest first, and one that maps again under a live parent gets the xdg_surface serial alone, since version 1 has no event that may revise a placement. `grab` is now recorded and checked. The chain is walked to a toplevel rather than read one level deep, since a menu whose window went keeps both its grab and its role object and would otherwise go on lending grabs to submenus opened under it. Only an ungrabbed popup parent is an error, `invalid_popup_parent` on the shell object; a chain that is dismissed, orphaned or gone gets the dismissal the protocol asks for instead of a closed connection. `invalid_grab` refuses a grab after the popup has ever been mapped, and the seat must name a `wl_seat`. The grab now takes the KEYBOARD: the topmost grabbing popup has focus, read off the same stacking order the paint uses, and bounded two ways td records as divergences from the protocol's "always" — the menu must have a pixel on screen, and it must hang off the focused window. The second is the operator's way back: without it a background client's popup was an inescapable keystroke sink, since the override outranks click-to-focus and `Super+arrow` and a grab suspends focus-follows-mouse. The seat's record is dropped when a mapping ends, and separately when the popup object or its surface is destroyed, so neither a menu painted back nor a reused id holds a grab it never asked for. Focus-follows-mouse is suspended for the grab's length, so the window under the menu is what focus falls back to. What is NOT enforced is that the parent is the topmost grabbing popup — the protocol names no error for it, so a branching client gets the keyboard on whichever branch it mapped last. A PRESS is routed through a grab too — motion and the wheel are not, and still go to whatever the pointer is over: a press with none of the grabbing menus under it closes them, deepest first, taking the pixels down and sending `popup_done`. "Outside" is a question about the chain — a grabbing menu survives a press on itself or on anything hanging off it, so a submenu's own press does not close the menu it hangs off — and the set asked is the seat's unfiltered, so a menu that holds no keyboard is still one a press ends. The press is CONSUMED rather than also delivered, since closing a menu must not click what it covered — except for the grab-owning client, which the protocol gives its pointer events for all of its surfaces as normal, so only somebody else's press is td's to take. Everything that decides a dismissal happens on the thread that read the press — grab, pixels, focus, the configure reset and the record that the menu is over — because a record left for another thread leaves a gap a client's own commit paints the menu back through. Only the wire event is the client's seat thread, since addressing it needs that client's registrations; the delivery carries the xdg_popup as well as the surface so that thread can prove the surface still wears the menu the press was about, and it holds the outbound lock across that lookup so `popup_done` cannot cross `delete_id` — never the registration lock, which the runtime takes with the runtime lock held, and which a blocking socket write must therefore not span. One cost is recorded: the byte ceiling is refunded on destroy rather than on dismissal. What is NOT dismissed yet is a chain on a WORKSPACE SWITCH, which the path now exists for and is held for its own landing. The input-event serial is read and not checked: td keeps no ledger of issued input serials to check one against |
| popup protocol conformance (error object, permanent role, `not_the_topmost_popup`, `defunct_surfaces`, `already_constructed`) | **landed** | — | ~350 — spent. `xdg_wm_base`'s errors name the shell object rather than the xdg_surface they arrived at, and it outlives the surfaces it made so that id stays meaningful; a surface's role kind outlives its role object, so a former menu cannot come back as a tiled window; and a menu may not be destroyed before the submenu hanging off it |
| shell edges across id reuse | **landed** | — | ~600 — spent. Neither shell edge keeps a number the client has back: a popup's parent edge is broken when the surface it names is destroyed, and a wl_surface's role edge is retired when its xdg_surface is. So a reissued id cannot re-parent a menu onto an unrelated window, commit a surface through a stranger's role object, close a popup cycle, or answer a `not_the_topmost_popup` scan for a submenu that does not exist. Per surface rather than a sweep, so one window closing leaves other windows' menus alone. The byte accounting it left open is the row below |
| popup byte accounting across a cascade | **landed** | — | ~120 — spent. A take-down reports the menus it dropped and the client's ledger gives back exactly those, rather than a second walk of the parent edges — which by then reads the edges that walk removed. An application that opens and closes menus is no longer charged for buffers td discarded, and so no longer approaches its own ceiling for holding nothing. All three refunds are reachable, including the one at `wl_surface.destroy`, which looked dead: `get_popup` refuses a parent with no role object, so no new menu can hang off a window whose toplevel has gone — but an existing one only has to be REPAINTED, since its popup object outlives the toplevel and puts the placement back in the scene. Dismissal now unmaps, so that repaint is a whole mapping again rather than a bare attach; it reaches the same refund by a longer road |
| popup constraint solving (slide/flip/resize) | **landed** | — | ~450 — spent. The six version-1 adjustment bits are applied in protocol order, independently per axis, against the usable output in the mapped parent's coordinates; unknown bits remain inert permissions |
| client cursor rendering | **done** (`1c4b7f88`) | — | spent |
| `zxdg_decoration_manager_v1` (answer `server_side`) | **landed** | **U** | ~500 — spent. Suppresses CSD, fits tiling, and removes most of the geometry problem for cooperating apps. One divergence, deliberate: `destroy` asks the compositor to stop decorating, and td keeps drawing the band because it is layout rather than a decoration td can withdraw |
| `zxdg_exporter_v2`/`importer_v2` | absent | **portal-blocking** | 1,200–2,000 |
| `zwp_primary_selection_v1` | absent | C | ~1,000 once data-device exists |
| `xdg_wm_base` v2→v6 | v1 | C | 1,000–1,800 after popups |
| `wp_viewporter` | absent | C at scale 1 | 1,500–2,500 |
| `wl_compositor` v4→v6 | v4 | C | 500–900 |
| `xdg_activation_v1` | absent | C | 1,500–2,500 |
| `zwp_idle_inhibit_v1` | absent | C | 800–1,200 — **not** needed for `.Inhibit`, which uses the private `create_idle_inhibitor` so the inhibitor is attributed to the app rather than to the portal (§E). Never advertise it on a jailed connection |
| `zxdg_output_manager_v1`, `wp_single_pixel_buffer_v1` | absent | C | ~800 each |
| `zwp_relative_pointer_v1` + `pointer_constraints_v1` | absent | app-specific | 2,500–4,000 |
| `zwp_text_input_v3` / `input_method_v2` | absent | **R** until an IME exists | 3,000–5,000 each |
| `wp_fractional_scale_v1`, `wp_presentation` | absent | **R** at scale 1 | — |
| `zwp_linux_dmabuf_v1`, explicit sync | absent | **R** | no GPU, nothing to export |
| touch, layer shell, output hotplug, Xwayland | absent | **R** | — |

**B** blocks a first window, **U** blocks a *usable* one, **C**
cosmetic, **R** refused.

**E2 result, 2026-08-25:** `wl_data_device_manager` is the first-window
blocker; `wl_subcompositor` is not. A registry-listener filter ran Guix's GTK
4.22.1 `gtk4-demo` against Weston 10.0.2's headless pixman backend. With only
`wl_subcompositor` hidden, the client acknowledged its XDG configure, created
a shm buffer, attached it, and remained alive until the four-second harness
timeout. With only `wl_data_device_manager` hidden, it exited 1 before making
a surface and reported that the compositor lacked a required interface.
Hiding both produced the same refusal.

That GTK binary is deliberately identified rather than called "the runtime's
`gtk4-demo`": the exact pinned `org.freedesktop.Platform` 25.08 deploy contains
GTK 3 (`libgtk-3.so.0`) and no GTK 4 library or demo. The experiment settles
current GTK's first-toplevel behavior, not every use of synchronized
subsurfaces and not an as-yet-unselected GTK 4 application runtime. Repeat the
matrix when that runtime is pinned. The result classified
`wl_subcompositor` as class U rather than cosmetic before it landed.

**dmabuf is never advertised.** Advertising it and rejecting every useful
format is worse than letting clients pick the shm fallback immediately.

### Software rendering

E2 proves the runtime's Mesa can present through `wl_shm` with no dmabuf. A
small Wayland-EGL client was compiled in exact Freedesktop SDK 25.08 commit
`b90ed309cc1d505dea48b6a2121c5dcfac22868120eee643b0596d31f96b9bb8` and
then executed with `flatpak build --runtime`, so `/usr` came from the pinned
Platform commit
`bd44a6230581917d04f89812a4c21090c304d390edb73995af1c2f9fd8abf4e8`.
Against a Weston registry with neither dmabuf nor `wl_drm` advertised and with
`LIBGL_ALWAYS_SOFTWARE=1`, it reported `GL_RENDERER=llvmpipe (LLVM 21.1.8,
256 bits)`, created a `wl_shm` buffer, attached it, and committed the surface.

The exact pinned Firefox deploy independently acknowledged an XDG configure
and attached two shm buffers against the same no-dmabuf compositor while
remaining alive for the ten-second harness. That launch intentionally lacked
the portal, D-Bus, accessibility, and input services and is a presentation
probe, not §H's usable-browser oracle.

**GTK4 with no GL at all is still the designed configuration, not a
fallback.** `GSK_RENDERER=cairo`
plus `GDK_DISABLE=gl,vulkan,dmabuf,offload` gives a pure-CPU renderer
submitting shm buffers; GTK3's default renderer is already cairo; and
Firefox's Software WebRender presents via shm natively. What still fails
is `GtkGLArea`, Vulkan, dmabuf offload, and hardware video — which is
darktable's OpenGL darkroom view, so darktable is a *later* and less
certain target than Firefox.

Do **not** rely on `MOZ_WEBRENDER=0`; current Firefox treats
`MOZ_WEBRENDER` as an enable input and has its own Software WebRender
fallback. Select it through a per-profile policy and prove the result in
`about:support`.

The forced-Cairo half was measured too: GTK 4.22.1 with that exact environment
created and attached a shm buffer both with all Weston globals and with
`wl_subcompositor` hidden. The env policy remains a small
**per-runtime-major table** reviewed when a new major is first installed, not
one global set, because each yearly runtime rebases Mesa and GTK.

### Budgets

The current 64 MiB shm-pool cap and 512-object cap are plausible for a
demo and unmeasured against a browser. Before the Firefox proof: bound a
client to 4 pools and 256 MiB aggregate declared shm, bound a committed
surface by output-scaled dimensions, bound aggregate copied pixels
separately from pool size, bound hidden synchronized subsurface commits
and the frame-callback and buffer-release queues, and raise the object
cap only from a captured trace. A malformed or greedy client is
disconnected without taking the compositor down.

And a performance caveat that belongs in the claim rather than in a
surprise: the renderer copies client pixels and software-composites the
whole scene into fbdev. A 60 fps 1080p Firefox is several hundred MiB/s
of memory bandwidth. **"Draws a window" does not imply watchable video.**

---

## G. Everything the app expects to find

- **`/etc/passwd`, `/etc/group`** — synthesized per instance: `root`, the
  real uid-1000 account with a home path that agrees with `$HOME`, and
  `nobody`. No shadow file.
- **`/etc/machine-id`** — GTK and GDBus both read it. **This already
  exists**, contrary to an earlier draft that proposed building it:
  `td-firstboot` mints it into `/var/lib/td/machine-id` once, mode 0444,
  preserved across updates like the sshd host key, and a `MUTABLE_ETC`
  entry symlinks `/etc/machine-id` at it — `td-firstboot/src/machineid.rs`
  and `provision_machine_id`, with a boot check already in the image. The
  work here is not to create THE MACHINE'S — the per-app value below is
  work, and a draft's flat "the work here is not to create it" read as
  denying that.
  **What each sandbox sees is a per-app minted value** (§O), not the
  machine's, so the jail mints one and binds it over `/etc/machine-id`
  during mount setup: the host id is a cross-application linkage identifier, and
  the draft's "one id per machine — differing per sandbox breaks nothing
  and helps nothing" was written before that decision and contradicts it.
  Minting is cheap, td has no legacy to preserve, and `GetMachineId`
  returns the app's own value so the two cannot disagree.
- **`/etc/resolv.conf`** — bind of `/run/resolv.conf` (td-netd's
  product), only with network. Absent with no network is correct: DNS
  should fail like the network it names. **`/etc/hosts`** carries
  loopback and the hostname.
- **`/etc/localtime`** — td carries no TZif and the bar is proudly UTC.
  Apps show UTC; a user who cares sets `TZ=` and glibc reads the
  *runtime's* zoneinfo. Recorded divergence, zero code.
- **CA bundle** — one consumer now, not two, and it survives the
  network stack's removal. Nothing here needs roots for itself
  (§B.3), but the *applications* still browse the web, so: **add a CA
  bundle as a reviewed fixed-output input** with provenance and SHA-256
  pinned, exposed as `/etc/ssl/certs/ca-certificates.crt` inside
  sandboxes. **Which artifact matters and is easy to get wrong.**
  Mozilla does not publish a PEM bundle — it publishes `certdata.txt`,
  the NSS trust database, whose format is NSS's own and which carries
  per-certificate trust bits that PEM cannot express. The PEM file
  everything actually pins is **curl's `cacert.pem`**, which curl
  generates from that `certdata.txt` on a published schedule and ships
  with a SHA-256 beside it. So the pin is curl's artifact, the
  provenance note says "Mozilla's trust store as rendered by curl", and
  the extraction step Mozilla's own file would need does not exist here.
  PEM blocks are hand-parsed strictly; no host OpenSSL. Firefox brings its own NSS
  store regardless, and runtime NSS databases stay runtime-owned.
- **Fonts and locale** — the runtime ships fonts, fontconfig caches,
  icons and MIME data; td exposes none of its own (its PSF2 face is not
  an app font). Cache misses rebuild into the app's private
  `XDG_CACHE_HOME`; runtime cache paths are read-only. `LANG=C.UTF-8`
  by default, and a real locale only after proving the `.Locale`
  extension supplies it. `LC_ALL` is never set, so app settings can
  still override a category.
- **`.Locale` subpaths** — without them the Locale extension is hundreds
  of MB; with `subpaths=/en` it is a few. Needed in v1, because runtimes
  declare Locale as autodownload.
- **`/dev/shm` + `memfd_create`** — load-bearing for both Firefox content
  processes and every shm pool. Real writable tmpfs, allowed syscall.
- **`ld.so`** — the runtime's, through the `/lib64` symlink.
  **`builder/src/elf.rs` is never involved.** Rewriting an interpreter or
  RPATH would invalidate the authenticated artifact, mix td's libc with
  the runtime's, make extension resolution diverge from upstream, and
  create an undeclared compatibility ABI. The bootstrap's interpreter
  rewriting exists for binaries that must run against *td's* glibc; a
  runtime is a sealed world that brings its own.
- **Audio: `td-audio`'s socket, and nothing else.** This entry said
  "Audio: none. Firefox starts and plays video silently" before §K
  existed, and §K supersedes it: `sockets=pulseaudio` binds td-audio's
  Pulse-protocol socket at the fixed path upstream uses, the kernel gains
  the `CONFIG_SND*` pins §0 lists, and `/dev/snd` is still **not** exposed
  — direct ALSA would bypass per-app mediation, which is the reason it
  was refused when there was no sound at all and remains the reason now.
- **Accessibility: the a11y bus, per §S.** `GTK_A11Y=none` is the v1
  setting and a stated gap rather than a permanent one; §S designs the
  bus and registry that lets it come off, and it is not silently
  redirected at a host bus in the meantime. Portals do not substitute for
  it.
- **Extensions** — `.Locale` yes; `.GL.default` installed when declared
  (llvmpipe is in the base runtime, and dmabuf stays absent regardless);
  `.openh264` and `.ffmpeg-full` on request, with the codec licence
  question made explicit rather than inherited. `merge-dirs`,
  `subdirectories`, `add-ld-path`, `versions`, `autodelete` and
  `no-autodownload` are parsed, and **unknown extension directives are
  refused rather than ignored**.

---

## H. Testing and proof

Layered as the compositor's tests are — pure models, adapter fixtures,
packaged selftest, boot oracle — with **network never in the gate**.

1. **Host tests**: manifest round-trips, refusals, and identity-name
   validation, including every rejection (path separators, leading dash,
   `.`, `..`, over-length, empty), have **LANDED** with the build-time manifest
   API. Alias validation is part of that manifest surface. Permission-file
   round-trips, canonicalization, typed construction, bounds and every closed-
   vocabulary refusal have **LANDED** with that format's typed schema.
   The D-Bus codec must cover both endian modes with malformed bodies, spoofed
   senders and serial-wrap boundaries; the auth state machine;
   name-queue transitions; match-rule evaluation; portal Request/Session
   lifetimes; the popup constraint solver, table-driven; and the BPF
   program through the test interpreter.
2. **Recipe-side seed tests.** The static seed path has **LANDED**. The
   repository fixture corpus is gone with the repository client; what replaces
   it is much smaller and sits where the work now happens. A seed recipe's
   checks assert that the pinned archive unpacks to the declared tree shape,
   that the entry point exists and is executable, that the declared runtime
   resolves, that no setuid bit or device node survives, and that the ELF is
   fully static or names the runtime's interpreter rather than the host's. The
   first seed takes the static branch; the dynamic interpreter/library branch
   arrives with the first toolkit seed. These are ordinary recipe checks and run
   in the ordinary recipe gate.
3. **Jail**: the three seccomp layers of §C, source invariants pinning the
   parent-death/containment ordering and a behavioral parent-death regression,
   plus in-QEMU assertions from
   *inside* a jail — `getuid()==1000`, `/proc/1/exe` is td-jail, host
   processes absent, `/app` and `/usr` reject writes, host `/usr` and
   `/td/store` absent, `/dev/fb0` and `/dev/input` absent, a large
   `/dev/shm` mapping works, `memfd_create` works, both sockets work.
   The transition-only probes deliberately begin after the application
   bootstrap; the installed QEMU fixture is what traverses both layers.
4. **Broker conformance**: committed byte streams with expected decode
   and routing; policy tests driving a fake sandboxed peer through an
   injected identity resolver (a trait, precisely so the test stays
   pure); descriptor counting; saturation refusals. Plus a **non-gate
   developer harness** pointing a host `busctl --user` or GLib client at
   td-busd — real foreign-client interop, not td talking to itself.
5. **Portal round-trips** over socket pairs: subscribe-before-reply
   without losing `Response`, cancellation and disconnect cleanup,
   spoofed identity rejection, pid-reuse and start-time rejection,
   nested-mount-namespace attribution, FileChooser flows through the pure
   dialog model, refusal of a file outside grants, screenshot pixel and
   PNG hashes, inhibitor acquire/readback/release/crash-cleanup.
6. **The offline QEMU oracle — the centrepiece.** Its first slice has
   **LANDED**. The image builds a **fixture package by recipe** from the
   static `td-compositor` artifact's small fixture personality
   with a generated manifest and an empty runtime — the cheapest proof at the
   recipe/runtime-dependency level that the jail needs nothing td-owned from a
   runtime. The fixture package deliberately carries a second copy of the
   compositor artifact, so this is not an image-size optimization.
   Because the package is a recipe output like any other, this needs no
   fixture repository and no install-time network — and since §B.1 the
   fixture is *in the image the test boots*, so the boot runs
   `/bin/<fixture>` directly with no install step at all. The guest
   currently asserts its `/proc` says `NoNewPrivs`/`Seccomp` and all five
   capability sets are zero, that
   `/app` and `/usr` reject writes, and that the host store, `/etc`,
   framebuffer and input nodes are absent; it completes a bounded datagram
   round-trip over the isolated loopback, connects the one granted
   `wayland-0`, maps a toplevel, commits a frame, then publishes readiness in
   its private volatile runtime bind. `td-svc` probes the host-side
   per-application end of that bind for the fixture service; an independent
   trusted evidence unit repeats the probe in its own bounded one-second loop,
   publishes the evidence file, emits `TD-JAIL-FIXTURE-BOOT-READY`, then
   publishes a completion record that lets the autotest greeter exit. QEMU
   requires that marker on every system boot.
   That unit is not a dependency of `bootsuccess`, so user-owned state cannot
   decrement the deployment attempt counter. The application has no terminal
   or console descriptor
   with which to forge that evidence or alter terminal state. The earlier
   plan's direct in-guest
   `mount(2) == EPERM` subprobe remains deferred: this slice directly reads
   back all five empty capability sets and the standard-filter interpreter
   pins `EPERM` for the complete mount syscall roster, but a target-side
   syscall attempt needs a separately reviewed safe probe surface. The later
   broker/portal slice calls
   `Settings.ReadAll` through
   the bus and reads its
   **direct reply** — `ReadAll` is a synchronous method returning
   `a{sa{sv}}`, NOT a Request-producing call, so an oracle that waited
   for a `Request.Response` would hang forever on a portal that was
   working perfectly; an earlier draft said exactly that, and it is the
   kind of error that presents as "the portal is broken" —
   and extends the end-to-end proof after `TD-BUSD-READY` and
   `TD-PORTAL-READY`. Today the exact fixture marker is jail + seccomp +
   immutable package/runtime + state + socket grant + compositor,
   offline, on the shipped kernel. The first *foreign*-toolkit oracle uses a
   pinned small GTK application — not Firefox.
7. **Recipe tests**: every new crate — `td-jail`, `td-busd`, `td-audio` —
   stays a one-package dependency-free crate and joins BOTH
   `DEPENDENCY_FREE_LOCKS` and `CARGO_TEST_CMDS` in
   `builder/src/affected.rs`, or its lints and tests never run; the
   kernel config contains every required option; the fixture launches
   offline; packages are read-only in the jail; host sentinel paths are
   absent; and no test-only probe enters the closure.
8. **The §B.8 marker's own tests, which are the ones this design most
   depends on** and which nothing else in this list covers:
   - a recipe naming a marked path in `inputs`/`native_inputs` rather
     than `payload_inputs` is a planning-time refusal — **landed**, in
     both plan builders for a marked recipe output, with a marked source
     pin refused earlier at the recipe-eval/catalog boundary before the
     private builder backend;
   - a `Step::Run` argv expanding a `payload_inputs` path is a refusal —
     **landed** as the pure argv/template audit, over every field the
     builder expands (including `Run` environment values), since this is
     the assertion whose violation is otherwise silent;
   - the interpreter assertion: the first seed is proved fully static, which is
     also a concrete demonstration that an interpreter cannot prevent direct
     execution outside a jail. The later dynamic payload's `PT_INTERP` must be
     absent from the built image tree and is asserted against the image, not
     assumed;
   - the closure query reports every marked path, and the set matches
     the reviewed pin list exactly — including the application→runtime
     edge, which exists only because the spec carries the runtime's full
     store path. The recipe-graph and registered-store queries have both
     **landed**. `ripgrep-seed`'s check binds their answers, proves the exact
     app→`empty-runtime` collector edge, and reports its reviewed archive as a
     build-only provenance input rather than a retained runtime path;
   - metadata normalization — **landed for the first seed**: no
     setuid/setgid bit, file capability, security xattr, device node or escaping
     symlink survives packaging;
   - a package's launcher name collides with no applet farm.

### Before anyone writes "Firefox runs"

A recorded proof naming exact hashes for the Firefox package, its
runtime package, the pinned seed archives behind both, and the td kernel
and image commits — showing:

1. a clean **first launch of the shipped Firefox package** from the built
   closure — an install verb until §B.1 put packages in the image, so
   what is proved is now that the deployment carrying it boots and the
   application starts — offline,
   with **no EXTERNAL network reachable from the guest at any point** —
   the earlier wording was "no network reachable at any point", which
   item 6 then contradicts outright, since a page cannot load over a
   network that does not exist. The two are reconciled by making the
   HTTPS origin part of the fixture rather than part of the internet
   (see item 6), so what is asserted is what td actually cares about:
   nothing is fetched from outside the test;
2. every package in the resulting closure reporting its provenance tier,
   and the set of marked entries matching the reviewed pin list
   exactly;
3. no host `/usr`, framebuffer, input node or raw disk in the app's
   mounts or process maps — and **neither `/td/store` NOR `~/.td`
   visible as a directory**, only the application's own package and
   runtime bound at `/app` and `/usr`, and its own state at `$HOME`. Both
   halves are needed and the earlier draft named only the store. Packages
   are in `/td/store` per §B.1 — so that assertion covers them and td's
   own binaries together — and `~/.td` is asserted separately because
   `~/.td/app` is every other application's STATE: cookies and sessions,
   which is the disclosure per-app uids exist to stop. (A draft written
   before §B.1 put packages under `~/.td/pkg` and argued the second half
   from that; the assertion is unchanged, its reason is not.);
4. main and content processes inside the jail, **and `about:support`'s
   sandbox section reporting the expected level for every process class**
   — content, GPU, socket and utility — because a Firefox whose own
   sandbox silently failed to install inside td's jail satisfies every
   other item on this list (§C's nested-sandbox note);
5. `about:support` reporting Wayland and Software WebRender;
6. one HTTPS page loading and rendering — served by a **declared
   in-guest TLS origin**, not from the public internet: a fixture server
   on the guest, a certificate minted for it at image-build time, and
   its issuing CA added to **the test profile's `cert9.db`** — never to
   the shipped bundle, and note that adding it to the bundle would not
   work either: Firefox trusts NSS's own roots and does not read
   `/etc/ssl/certs/ca-certificates.crt`, so a fixture that put the CA
   there would fail with `SEC_ERROR_UNKNOWN_ISSUER` and look like a
   confinement bug. "A valid public certificate" was the earlier
   wording and it is unreachable under item 1; it is also the wrong
   requirement, since what this item proves is that NSS initialises,
   the TLS stack runs inside the jail, and the renderer paints a real
   page — none of which needs the certificate to chain to a public root.
   The one thing the fixture must NOT do is disable certificate
   verification, because a Firefox that renders with verification off
   has proved nothing about the part that could plausibly be broken by
   confinement;
7. keyboard, pointer, cursor, wheel, a right-click menu opening and
   dismissing, and paste from td-term into the URL bar;
8. a download reaching the app-private or explicitly granted directory;
9. the Settings portal succeeding;
10. five minutes of navigation with no compositor or bus disconnect;
11. blocked syscall probes still blocked in the outer app process, and
    **zero seccomp denials for syscalls the application actually needs**
    — which is the only false-hit test a deny list admits. An earlier
    wording asked for "zero EPERMs for syscalls outside the roster",
    which is vacuous: in a deny list everything outside the roster is
    permitted, so it cannot be denied and the check could never fail.
    A deny list's false hits are syscalls wrongly *inside* it, and they
    surface as the application failing, so the observation that means
    something is the trace: every denial recorded during the run is
    matched against the roster and each one must be a syscall the roster
    intends to refuse. The roster's *misses* — dangerous syscalls left
    out — are invisible to this and are what §C's argued-row-by-row
    construction and td's kernel config are for;
12. `kill -KILL` of stage 1 reaping the whole instance;
13. **audio PLAYING**, not reported unavailable — this item said the
    opposite until the §K reversal, and it is the last place the old
    answer survived. Milestone 28 asks for sound out of Firefox and
    rungs 25–26 build it, so a proof run that accepted "unavailable"
    here would pass while contradicting the rung it is the acceptance
    test for;
14. *unsupported* portals — `ScreenCast`, `RemoteDesktop`, `Camera`,
    `Secret` — reported unavailable rather than succeeding, which is the
    half of the old item 13 that is still right and is a real check: a
    portal that answers a call it does not implement is worse than one
    that refuses.

A screenshot alone is insufficient.

---

## I. Milestone ladder

Each row is one landing or a small family, leaving the tree green.

| # | lands | visible result |
|---|---|---|
| 1 | **kernel namespace/seccomp/cgroup config pins + QEMU readback** (§0) — **LANDED**; the functional `unshare` and filter calls subsequently landed with surface #9 at rungs 8 and 11 | none — but this is the gate on everything |
| 2 | `td-login exec-as` with credential readback — **LANDED** | none |
| 3 | **the §B.8 marker — LANDED**: the recipe-level mark, `payload_inputs` as a declared channel, taint propagation from the source pin, and the planning-time refusals — including the argv/template audit at `td-recipe-eval`'s production planning boundary for the otherwise-silent assertion. The channel is `{payload:NAME}` resolution plus `ro,noexec` binds, replacing the un-implementable "never staged at all"; the mark is `foreign` on the source pin, the derived recipe flag, the tool-channel refusal in both plan builders, and the computed `contains_payloads` answer with the recipe-graph closure query that reads it | none |
| 4 | the canonical manifest, its recipe-side generator/parser, short-name validation, and the typed permission keyfile with every rejection — **LANDED** | none |
| 5 | **first seed recipe — LANDED**: upstream's pinned static ripgrep release becomes an unshipped store package with an empty runtime, generated manifest, payload-only edge, compiled source digest, and native §B.3/§B.8 checks | none |
| 6 | **spec compiler — LANDED**: runtime resolution by full store path, fixed effective environment, typed grants/defaults and entry point; the manifest and spec are builder-authenticated, and the registered-store closure query proves the app→runtime collector edge | none |
| 7 | **image application index — LANDED**: typed launcher metadata, the builder-authenticated export, immutable resolver/launcher tables, `/etc/td-app.conf`, and the empty `/bin/<name>` farm through `real_root_steps` + `bin_farms()`; no application is selected until the jail lands | none |
| 8 | **`td-jail` crate + surface #9 skeleton + stage-1/stage-2 transition — LANDED**: one value-pinned `unshare(2)` wrapper, exact identity-map and fresh-namespace readback, a token-synchronized safe-`Command` transition to PID 1, post-exec zero-capability readback for the nonroot app identity, a build-host policy smoke test, and an authoritative target-kernel QEMU probe from the packed static binary; ordinary application launch still refuses | none |
| 9 | **mount transition — LANDED**: inherited-FD closure, a private compiled tmpfs root with individually read-only allowlisted device binds and immutable metadata, fresh devpts/shm/tmp/var-tmp, capability-v3 set/get readbacks, an exact ambient/inheritable `CAP_SYS_ADMIN` exec bridge, an empty/read-back bounding set, stage-2 procfs for its own PID namespace, pivot + old-root detach, mountinfo/device/mode/writability readbacks, and host plus target-kernel probes; application launch still refuses | none |
| 10 | **capability drop/readback + PID-1 reaper — LANDED**: ambient is explicitly cleared before effective/permitted/inheritable become empty, all five sets are read back zero, and a copied static internal helper leaves a zero-capability grandchild for PID 1's bounded `wait4(-1)` oracle; ordinary application launch still refuses | none |
| 11 | **const BPF assembler, standard filter, interpreter tests, build-host and target probes — LANDED**: stage 2 sets and reads back no-new-privileges, validates and installs the constant policy, requires `Seccomp: 2`, and its filtered PID-1 descendants inherit the same restriction; the non-shipped td-GCC probe is injected only into the disposable QEMU volume | none |
| 12 | **fixture package shipped in the image and launched by `/bin/<fixture>` — LANDED**: the static `td-compositor` artifact's fixture personality is copied through an ordinary declared input into a generated application package with an empty payload-only runtime and a canonical application spec; the image selects it into the immutable registry and `/bin` farm, its supervised boot unit and the compositor launcher both enter through `/bin/td-jail-fixture`, and td-jail accepts only the canonical index/spec subset implemented by the landed rungs. Stage 1 canonicalizes, mounts and source-identity-checks immutable `/app` and `/usr`, the exact compositor socket, bounded private tmpfs trees, the five persistent state directories, and one private volatile runtime directory; stage 2 verifies the mount plan, clears capabilities, installs seccomp, holds a parent-death pipe, replaces application stdio with null descriptors, preserves the direct application's status, and gives survivors bounded TERM/KILL reap phases. The client publishes readiness through that volatile bind only after confinement readback and a presented frame; a separate readiness-gated evidence unit emits the exact QEMU marker without making mutable application state deployment-success authority | **first jailed pixels on the QEMU screen** |
| 12a | **explicit development-host launch — LANDED**: `td-jail --host CONFIG APPLICATION [ARG...]` resolves the ordinary authenticated manifest/spec through a separately materialized physical package root, maps the caller to the fixed uid/gid 1000 jail identity, binds caller-owned Wayland and local td-busd sockets, preserves the target `/app` and `/usr` layout and the full namespace/mount/capability/seccomp transition, and refuses missing user-namespace or seccomp support. The ordinary rung-12 fixture is the recipe acceptance test; it asserts exactly one named diagnostic each for the unavailable aggregate cgroup caps and direct host-Wayland global filtering. Mapped downstream bus authentication and host forwarding remain §D work | host mode works, and says exactly what it could not enforce |
| 12b | **immutable typed filesystem grants — LANDED**: canonical source resolution and identity pinning, reserved alias refusal, deny-wins/overlap merging, separate recursive bind targets, nested-mount hardening and stage-1/stage-2 readback. Mutable per-user overrides remain a later lifecycle landing | a jailed app can open only a builder-authenticated explicitly granted host path |
| 12c | **typed memory/task policy — LANDED**: cgroup2 is mounted by PID 1; PID 1 and system services remain at the hierarchy root under cgroup v2's root exception while td-svc enables `memory`/`pids` top-down and delegates an empty user subtree; td-jail creates one direct per-instance leaf, writes and exactly reads `memory.high`, `memory.max`, `memory.oom.group=1`, and `pids.max`, moves blocked stage 2 before release, verifies membership, sets and reads equal hard/soft `RLIMIT_DATA`, reports `memory.events`/`memory.peak`, and removes the empty leaf. Omitted policy gets the documented finite baseline; partial or page-rounded policy is refused. The 48 MiB/64 MiB/32-task fixture and active-cgroup probe gate the QEMU marker. `cpu-max` remains refused until the kernel bandwidth controller lands | application resource limits are effective rather than metadata |
| 12d | **application terminal/session containment — LANDED**: the argv0-selected launcher waits on a later-born stage 1; stage 1 binds its lifetime to that exact parent, then either preserves and reads back an existing no-terminal supervisor group or enters and reads back a new session with no controlling terminal. Only then may it resolve authority, register or create state. Parent death still tears down stage 1, stage 2 and the cgroup leaf. `devices=tty` remains refused until a fresh-terminal policy exists | the jail is not an ambient terminal member, and it does not escape a dedicated service stop scope |
| 12e | **typed CPU bandwidth policy — LANDED**: permission format 2 adds bounded `cpu-max=QUOTA PERIOD`; format 1 remains accepted and inherits a one-CPU baseline. The kernel pins fair-group scheduling and CFS bandwidth while keeping real-time group scheduling off; td-svc delegates `cpu` top-down; td-jail writes and exactly reads `cpu.max`, reports the bandwidth rows from `cpu.stat`, and the 50%-CPU fixture's active leaf gates the QEMU marker | aggregate CPU time is capped with the memory and task budgets |
| 12f | **typed shared-network policy — LANDED**: the authenticated `shared=network` permission selects the compiled namespace set without `CLONE_NEWNET`; stage 1 requires the network namespace inode to remain unchanged and skips the isolated-loopback ioctl. Omitted policy retains `CLONE_NEWNET`, exact changed-inode readback and the up-loopback oracle. The host fixture launches both its isolated defaults and a derived shared-network variant; the shipped QEMU fixture remains isolated. `/etc/resolv.conf` and mediated network policy remain later work | a declared application can use td's network stack without weakening the default isolated path |
| 13 | `td-busd` codec, auth, surface #10 | none |
| 14 | names, routing, match rules, descriptor passing | none |
| 15 | per-app policy, lineage identity, in-jail activation | none |
| 16 | `td-portal` personality: Request/Session core, Settings, Account | GTK settings call works |
| 17 | Wayland A: `set_window_geometry`, decoration manager, ARGB golden, single-pixel-buffer; **E2's GDK, llvmpipe, and Firefox presentation answers are recorded in §F and `td-compositor/DESIGN.md`** | none |
| 18 | Wayland B: `wl_subcompositor` — **LANDED** | a compound window renders and receives input in client-defined subsurface order, with synchronized frames applied on the parent commit |
| 19 | Wayland C: `xdg_positioner`/`xdg_popup`, click-outside dismissal and edge constraint solving LANDED | a menu appears where its client asked, takes the keyboard while it is up, closes when the operator presses outside it, and is flipped, slid or resized clear of an output edge as its positioner permits |
| 20 | **clipboard LANDED**: data-device v3 selection forwards a focus-scoped MIME offer and one bounded descriptor to its source; client cursors landed separately (`1c4b7f88`) | **paste, and the I-beam already arrived** |
| 21 | xdg-foreign + private portal socket + dialog placement | modal portal window |
| 22 | FileChooser, OpenURI, Screenshot, Notification | file dialog visible |
| 23 | **a pinned small GTK application as a seed package** | **first foreign-toolkit window** |
| 24 | runtime compatibility sweep; the launcher table is read from the image | none |
| 25 | **`td-audio` crate + surface #11**: the ALSA PCM back end alone, driven by a fixture that writes a tone — no protocol, no clients | **sound from the machine** |
| 26 | `td-audio`'s PulseAudio protocol: frames, tagstruct codec, `AUTH`/`SET_CLIENT_NAME`/sink info, one playback stream; `sockets=pulseaudio` binds the socket into a jail | a jailed fixture app plays audio |
| 27 | Firefox policy, shm budget tuning, trace fixes | Firefox attempts startup |
| 28 | the §H proof run to green; `AGENTS.md` trust-zone section; **all three** `UNSAFE.md` entries audited against shipped code | **Firefox window, an HTTPS page, and sound** |

**Two other reversals are still absent from this ladder, and that is a
scheduling gap rather than a decision.** §O made timezone support
("must be addressed", 17) and accessibility ("wanted", 13) required, and
§G still specifies UTC-only and `GTK_A11Y=none` — the *implemented*
positions, correctly, since nothing has changed them. But neither has a
rung here, an interface, or an acceptance test, which is how a required
thing quietly becomes a permanent one. Both belong **after** M28 and
before "daily usable": timezone is a TZif input plus `/etc/localtime`
in the mount plan and is small; accessibility is §S's second-bus
landing and is not. Naming them here is what stops the ladder reading as
the whole plan.

**Rungs 25 and 26 are the audio reversal arriving in the plan**, and
they were missing from an earlier version of this ladder — which is how
the document came to say audio was required in §K and unavailable in
§G and §H at the same time. They sit *before* Firefox rather than after
it deliberately: Firefox with no sound is a Firefox that half the
proof items cannot be written for, and the ALSA half (rung 25) is
testable with no browser, no jail and no protocol, which makes it the
cheapest place to find out that the pinned kernel's sound pins are
wrong. Splitting the crate's own landing from its protocol landing is
the same instinct as splitting `td-jail`'s skeleton from its mount
plan: the surface amendment gets reviewed on its own diff.

**The ladder has no GPU rung, and premise 6 says GPU is required.**
Stated plainly because the two read as a contradiction and are not quite
one. Every rung above delivers against td's **software** compositor: M12
is jailed pixels through `wl_shm`, M28 is a Firefox Software WebRender window
presented through runtime shm and blitted by td's CPU renderer. The runtime's
llvmpipe-over-shm path is available separately for EGL applications. Nothing
here does modeset, dmabuf import or direct scanout.

That is deliberate, and the reconciliation has three parts:

1. **GPU is a `ui-rolling` track, not an application-layer one.** DRM/KMS output,
   `zwp_linux_dmabuf_v1` and buffer leases are compositor work
   (§M), owned by whoever owns `td-compositor`. Putting them on this
   ladder would make the app tier wait on the display stack for
   something the app tier does not need in order to be correct — a
   sandboxed browser that renders in software is a *slow* success, not a
   failure, and it is the milestone that proves the whole model.
2. **What this ladder owes the GPU track is the seams, and they are
   cheap now and expensive later.** §M's table is the list, and three of
   its rows must land *within* the rungs above or M28 has to be
   partly rebuilt: the buffer abstraction (so `ShmSnapshot` is a variant
   rather than the only shape), the `OutputBackend` split with `paint`
   defined as *submit*, and per-buffer-type resource accounting — which
   §P's caps must be written against from the start, since counting CPU
   bytes is the wrong unit the moment a buffer lives on a card. Add them
   to rungs 17 and 27 rather than to a rung of their own.
3. **The premise is about the destination, not the order.** "Required,
   not eventual" was the maintainer's answer to a draft that proposed
   shipping a software-only desktop and treating GPU as a someday item;
   it rules out *designing so GPU cannot arrive*, which is exactly what
   §M exists to prevent. It does not oblige the app tier to deliver
   scanout before it delivers a jail.

If that reading is wrong and GPU is wanted *before* a foreign window,
the change is not to this ladder but to the ordering between two
workstreams — and it costs M23–M28 a delay measured in the display
stack's own schedule.

**Size, revised down.** The earlier estimate — 20,000–30,000 production
lines to first jailed pixels and 55,000–80,000 to a defensible Firefox
window — was for a design that included a repository client. §R put that
deletion at **15,000–25,000 lines**, and it comes off the front of the
ladder where the repository rows used to be. So: first jailed pixels
(M12) is on the order of **15,000–22,000 lines**, a defensible Firefox
window **40,000–60,000** plus fixtures and tests, and broader GNOME and
darktable support, Documents, audio and the optional protocols move it
toward **70,000–105,000**. What replaces the deleted code is recipe work
rather than production Rust, and it lands per application instead of all
at once — which is the shape difference that matters more than the
count.

This is still a large project. Stating that plainly at the design stage
is cheaper than discovering it at milestone 22.

---

## J. Risks, unknowns, refusals

Ranked by how likely they are to change the plan:

1. ~~**The kernel argument (§0) may simply be refused**~~ — **RETIRED,
   approved.** It was ranked first because a refusal killed the design;
   it is now a landing to write. What remains of the risk is narrower
   and belongs to that commit: the pins interact (`olddefconfig` takes
   the default of a symbol that a pinned dependency made visible, which
   is how `NET_NS` arrived unasked), so the landing is checked against
   the *resolved* config rather than the pin list, and the boot oracle
   is what keeps a later regression from surfacing as a broken app.
2. ~~**Toolkit hard requirements and Mesa-over-shm**~~ — **RETIRED by E2.**
   Current GTK 4 requires data-device but not subcompositor for its first
   toplevel, forced Cairo attaches shm, and the exact pinned Platform's
   llvmpipe attaches shm without a dmabuf global. A newly selected GTK 4
   runtime major still repeats the matrix (§F).
3. ~~**Firefox's nested sandbox**~~ — **RETIRED by E2.** Stock Firefox keeps
   its effective level-6 content seccomp sandbox when Flatpak denies the
   nested user namespace, so td selects one standard deny filter (§C). The
   full per-process §H oracle remains implementation work.
4. **Seed repackaging turns out to be lossy.** The seed path (§B.3)
   assumes a foreign deploy tree is a self-contained `files/` hierarchy
   that runs given its runtime. If some application needs
   install-time work that upstream's tooling performed — a post-install
   step, a generated cache, a path baked at deploy time — the recipe has
   to reproduce it, and each such case is bespoke. **E1 exists to find
   this out for the price of an afternoon**, and it is why E1 comes
   before the workstreams (§V).
5. **The deny list's misses**, whose failure mode is silent while its
   false hits are loud. Mitigated by matching upstream's decade-tested
   list, by td's kernel config independently removing io_uring/bpf/sysvipc,
   and by each divergence being individually argued so review can attack
   it row by row.
6. **Broker policy equivalence with `xdg-dbus-proxy`** — a subtle
   divergence is an app that works in testing and hangs on a filtered
   signal in life. Mitigated by a pure, vectored policy engine and the
   real-GLib-client harness.
7. **The curated set's maintenance cost is now td's**, which is the
   standing cost §R accepted rather than a surprise. A seed bump is a
   pin review plus a rebuild; the risk is that it is *nobody's job* by
   default, and the mitigation is that every package's pin is a
   reviewed line in one file rather than an update policy in code.
8. **Nested-namespace identity**, **PID-1 semantics** under Firefox's
   process tree, **performance** on a software compositor, and **storage
   capacity** for a store holding several runtimes.

**Never implemented:** app building or `flatpak-builder`; a target-side
repository client of any format (§B.6); a setuid `bwrap`, `fusermount`
or helper of any kind; `libflatpak`, libostree, GLib, GIO, libseccomp,
libwayland or GPGME as td dependencies; a seed admitted without a
reviewed pin, or a sandboxed-application package presented as source-provenanced;
`apply_extra` or any post-install script; runtime ELF rewriting; device
nodes, setuid bits, file capabilities or security xattrs from app
payloads; blanket host/home exposure; the system bus; X11 and Xwayland;
dmabuf on an fbdev backend; a Documents portal backed by a retained
privileged process; fake success from PipeWire, camera, screencast,
print, audio or inhibit; and "best effort" acceptance of an unknown
permission or trust directive.

---

## K. Audio

The first draft declared audio a non-goal. That was wrong, and the plan
below replaces it. Both design passes reached the same answer
independently, which is worth noting because the answer is not the
obvious one.

### K.1 Target the PulseAudio protocol, not PipeWire

Start from what the application dials, not from what the ecosystem
currently ships. Firefox's audio layer is **cubeb**, whose Unix backends
include `pulse` (Rust and legacy C), `alsa`, `jack`, `oss`, `sndio` and
`sun`. **On a PipeWire desktop today, Firefox reaches it through
`pipewire-pulse` — a PulseAudio-protocol server.** "Firefox on PipeWire"
is Firefox speaking Pulse to a socket with PipeWire behind it.

That framing is deliberately about what the shipped binary *selects*
rather than about what cubeb's tree contains, because the second is a
moving target and reviewers disagree about it — one reviewer here
asserted an upstream PipeWire backend exists. It does not matter to this
decision, and that is the point of stating it this way: a PulseAudio
socket serves Firefox whether or not such a backend exists, since it is
what the binaries in circulation select and what every PipeWire system
already presents. td serving the Pulse protocol is correct under both
readings; td serving PipeWire's native protocol is correct only under
one, and costs a second protocol either way. **E1 settles it for the
actual seed** by checking which backend the pinned build reports at
runtime — one line of `about:support`, not an argument.

A second fact narrows it to one door: **official Mozilla builds compile
only the Pulse backend** (ALSA support was removed and re-requests are
WONTFIX), and the seed td repackages is that official build. So no
amount of exposing `/dev/snd` into the jail can ever
give Firefox sound — it would be serving a backend the binary does not
contain. A Pulse-protocol server on a socket is not one option among
several; it is the only interface the target application speaks.

The costs are asymmetric in the same direction. A minimal Pulse server
is order 10⁴ lines. A PipeWire-native audio server is 40–70k, **and
would still need a `pipewire-pulse` equivalent on top to serve cubeb** —
PipeWire-first means implementing two protocols to get sound, Pulse-first
means one. PipeWire native remains genuinely required for the ScreenCast
and Camera portals, whose APIs hand the client a PipeWire remote
descriptor and node id; that is a separate, later design whose driver is
video, not audio.

Build `td-audio` around an internal `AudioSink`/mixer boundary a future
PipeWire service could reuse, and record that this is sequencing rather
than a vote against PipeWire.

### K.2 What the sandbox expects to find

Verified against upstream `common/flatpak-run-pulseaudio.c`: flatpak
discovers the host socket, bind-mounts it read-only at the fixed path
**`/run/flatpak/pulse/native`** — *not* at `$XDG_RUNTIME_DIR/pulse/native`
— and sets `PULSE_SERVER=unix:/run/flatpak/pulse/native` plus
`PULSE_CLIENTCONFIG=/run/flatpak/pulse/config`, whose generated content
includes **`enable-shm=no`**. td spells all of it byte-for-byte the same,
adds `autospawn=no`, and mounts its own socket there.

`enable-shm=no` is a gift: upstream already forces sandboxed clients onto
plain socket data, so a server that declines both the SHM and memfd
feature bits in the handshake matches what every flatpak app on every
distro does today. That removes SCM_RIGHTS, pool and block identifiers,
release/revoke synchronisation, seal policy, and recovery from a client
dying with blocks in flight — collectively the largest and most
lifetime-hazardous part of the protocol — from v1 entirely.

If an app requests `sockets=pulseaudio` and the daemon is unavailable,
the launch fails *before* starting it rather than producing an
apparently healthy but permanently silent session.

### K.3 The wire, and what "minimal but honest" means

Frames are a 20-byte descriptor of five big-endian `u32` — `length`,
`channel`, `offset_hi`, `offset_lo`, `flags`. `channel == 0xFFFFFFFF`
marks a control frame carrying a **tagstruct**; any other channel is
stream data for that playback stream, with `flags` as the seek mode. A
tagstruct is type-tagged values (`L` u32, `B` u8, **`1`/`0` boolean
true/false**, `t`/`N` string, `x`
arbitrary bytes, `R`/`r` u64/s64, `U` usec, `T` timeval, `a` sample
spec, `m` channel map, **`V` volume**, `v` cvolume, `P` proplist, `f`
format info). Two of those are corrections a review supplied and both
would have produced a decoder that fails only against real clients: a
boolean is its own pair of tag bytes and is NOT a `B`, which is an
arbitrary byte, so a schema expecting `B` where libpulse writes `1`
desynchronises the whole packet; and `V` (a single volume) is distinct
from `v` (a channel volume vector) and is required by the sink-info
schemas listed below, which the first draft's alphabet could not
express. It
is self-describing **at the value level and not at the message level**,
and that distinction is the one the decoder turns on. Every value carries
its type tag, so a parser can walk a tagstruct and skip values without
knowing what they mean — but nothing in the encoding says how many values
a given command carries or what each one *is*. That comes from the
command number and the negotiated version. So the decoder still needs
version-conditioned schemas, exact packet-exhaustion checks, and bounded
strings and proplists; what it does not need is to guess at framing.
(An earlier draft said "not self-describing" flatly, which is wrong about
the encoding and right about the consequence, so it is corrected rather
than dropped: the tags make a malformed packet *detectable*, and the
schemas are what make a well-formed but unexpected one an error.)

Advertise **version 35** and parse each command at
`min(client_version & PA_PROTOCOL_VERSION_MASK, 35)`. **The mask is not
decoration**, and an earlier draft's `min(client_version, 35)` on the raw
word is wrong in the dangerous direction: the `AUTH` version field
carries protocol FEATURE bits in its high half — the SHM and memfd
negotiation flags this design clears two sentences later — so an *older*
client that sets one of them presents a raw word far above 35 and would
be parsed at 35, against schemas it does not speak. Mask first, cap
second, and negotiate the feature bits separately, which is the same
order libpulse itself uses. A lower version would shrink the schemas but
steer clients into downgrade paths nothing in the ecosystem exercises —
`pipewire-pulse` answers 35, so 35 is what modern libpulse is tested
against. Authenticate by `SO_PEERCRED` uid rather than by the cookie
(which is still parsed at its exact 256-byte length and then ignored),
and clear both the SHM and memfd feature bits in the reply.

Playback needs: `AUTH`, `SET_CLIENT_NAME`, `GET_SERVER_INFO`,
`LOOKUP_SINK`, `GET_SINK_INFO`/`GET_SINK_INFO_LIST`,
`GET_SOURCE_INFO_LIST` (an **empty list**, not an error, so device
pickers see "no microphone" rather than a broken server), `SUBSCRIBE`,
`CREATE_PLAYBACK_STREAM`, stream-channel writes, server-initiated
`REQUEST` byte grants, `CORK`/`FLUSH`/`PREBUF`/`TRIGGER`/`DRAIN`,
`GET_PLAYBACK_LATENCY`, per-stream volume and mute,
`SET_PLAYBACK_STREAM_NAME`, `DELETE_PLAYBACK_STREAM`, and the
`STARTED`/`UNDERFLOW`/`OVERFLOW` events. `REQUEST` is what makes sound
happen at all — without byte grants the client writes one buffer and
stops forever.

**Latency is not polish.** The reported figure must be the client frames
already accepted, plus the mixer/conversion queue, plus resampler delay,
plus the frames still in the kernel and device (`SNDRV_PCM_IOCTL_DELAY`),
every count converted at the actually-negotiated rate. Absent, the audio
clock never advances and video stalls or free-runs; fabricated, the clock
is offset by exactly the hidden buffer, which is lip-sync error, jumpy
seeking, and cubeb sizing its callbacks against a phantom margin.
A constant 50 ms is not an implementation.

Three refinements a review supplied, all in the direction of "the sum is
harder than it looks". **Do not double-count**: bytes already accepted
that have been handed to the mixer are represented in the mixer queue,
so adding both the accept counter and the queue counts them twice, and
the resulting clock runs ahead of the sound by exactly the overlap.
Track one position per stream and derive the rest from it. **A Pulse
timing reply is not a scalar**: it carries timestamps plus read and write
indexes, and clients compute latency from those — so the server must
report a consistent *set*, not one number squeezed into a field. And
**per-stream `DRAIN` cannot map to ALSA `DRAIN`**, which drains and stops
the shared mixed PCM — draining one stream would silence every other
app. A stream is drained when its own output-frame position has been
consumed by the device, which is bookkeeping against the mixer rather
than an ioctl; the ALSA `DRAIN` in the roster exists for shutting the
device down, not for serving this command.

**Do not write the command table from memory.** Capture it: run the
pinned runtime's own libpulse and Firefox against a logging stub —
Web Audio, a local video, pause, seek, mute, tab close, and a
deliberately slow sink producing underruns — and commit the captures as
golden fixtures. This is the `.filez` rule again: the bytes in tree are
the oracle.

### K.4 Reaching the hardware

Direct ALSA PCM in **`SNDRV_PCM_ACCESS_RW_INTERLEAVED`** mode. The trap
to refuse is assuming ALSA-direct means the full mmap machinery — mapped
ring, status and control pages, `SYNC_PTR`, boundary arithmetic. None of
it is needed: RW mode drives the device with ordinary write ioctls and
`poll`, and the kernel paces the writer. At 48 kHz stereo `S16_LE` the
copy that mmap would avoid is under 200 KiB/s, so shared-control-page
correctness is not a dependency worth taking for v1.

The roster is nine to twelve requests — `PVERSION`, `HW_REFINE`,
`HW_PARAMS`, `SW_PARAMS`, `PREPARE`, `START`, `WRITEI_FRAMES`, `DELAY`,
`DROP`, `DRAIN`, with `STATUS` optional — each pinned by value.
`INFO` is **not** optional once discovery reads `/proc/asound/pcm`: it is
what confirms the node just opened is the playback device that file named,
and a daemon that skips it is trusting a path string on a real machine.
The struct discipline is td-compositor's `EVIOCGABS` argument arriving at
a bigger struct: `snd_pcm_hw_params` is **608 bytes** (verified against
the pinned `linux-7.1.4` `include/uapi/sound/asound.h`: 4 + 8×32-byte
masks + 21×12-byte intervals + 24 + an 8-byte `fifo_size` naturally
aligned at offset 536 + 16 + 48, no padding), and the kernel copies all
608 in *and* out with no length negotiation.

The pin has a property worth stating, and worth stating **more narrowly
than an earlier draft did**: the ioctl request number encodes the
argument size. `_IOWR('A', 0x11, struct snd_pcm_hw_params)` is
`0xC2604111`, in which `608 << 16` is `0x02600000`. Composing the request
constant *from* the pinned length means the two cannot drift apart in the
source: change the length, get a different request number, and the
kernel's dispatch refuses it with `ENOTTY`.

What that does **not** do is validate the buffer. The kernel copies the
number of bytes the request encodes, through whatever pointer it is
given; it has no way to know how large the caller's allocation is. So
passing the correct request number with an undersized buffer is still an
out-of-bounds write, and the encoding prevents only the *other* failure —
being tricked into a copy of a length nobody intended. The buffer's size
is a separate obligation, discharged the way td discharges it elsewhere:
the argument is one type whose length is a tested constant, and no call
site composes a request or sizes a buffer by hand. Both halves are needed
and the earlier draft claimed the first covered the second.

**Every constant in the two paragraphs above is an x86-64 fact, not
merely a Linux 7.1.4 fact, and premise 8 makes that worth flagging
here rather than discovering on the first port.** `snd_pcm_uframes_t` and
`snd_pcm_sframes_t` are pointer-width, so the 608-byte `snd_pcm_hw_params`
layout, the `0xC2604111` request number that encodes it, the
pointer-bearing `WRITEI_FRAMES` argument and `DELAY`'s size are all
different on a 32-bit target — and `_IOC`'s own bit layout differs on
some architectures again. So the rule for these is the one td already
applies to syscall numbers: **generate and test the layout per
architecture, never share a constant across two**, with the length
derived from the type on the target being built rather than written
down. A second architecture that inherited these numbers would issue
well-formed ioctls with the wrong size field, which is the
out-of-bounds-write case the paragraph above exists to prevent. Inside
the struct, the used
intervals are indexed from `FIRST_INTERVAL` and `RATE`/`PERIOD_SIZE`/
`BUFFER_SIZE` are adjacent same-typed fields, so an off-by-one is a
well-formed constraint on the *wrong* parameter — name them by an enum
with exactly the arms the daemon uses (td-sh's `Disposition` shape) and
let one tested function own the offset arithmetic. Then read back: after
`HW_PARAMS`, re-read the chosen rate, format and channel count out of the
returned struct and refuse to serve a stream the device did not take,
because nothing observable distinguishes a mask the kernel narrowed from
one it honoured until the pitch is wrong.

The control device (`/dev/snd/controlC0`) and its whole
`SNDRV_CTL_IOCTL_*` universe are **never opened**: volume is
multiplication in the mixer.

**Device discovery reads `/proc/asound/pcm`, not `/proc/asound/cards`.**
The earlier draft named the wrong file, and the difference is exactly
what discovery needs: `cards` lists *cards* — one line per adapter, with
no playback device or subdevice numbers — while the PCM node this daemon
must open is `/dev/snd/pcmC<card>D<device>p`, whose `<device>` appears
only in `pcm`. A card with no playback PCM at all (an HDMI-capture-only
adapter, a card whose only stream is capture) is indistinguishable from a
usable one in `cards`, so a daemon that guessed `D0p` would open the
wrong node or fail on a real machine while passing every test on a
single-device fixture. Both remain ordinary file reads — no new
syscall — and `pcm`'s format is stable and line-oriented. The fallback
if a line cannot be parsed is to refuse with a diagnostic naming the
device, never to guess a number.

**OSS emulation is refused, and the reason matters.** It survives in
`linux-7.1.4` (`CONFIG_SND_PCM_OSS`, `sound/core/oss/pcm_oss.c`,
including `SNDCTL_DSP_GETODELAY`) and would be genuinely smaller — plain
`write(2)` plus six int-argument ioctls. But it buys that size by
enabling a deprecated in-kernel emulation layer, including
`SND_PCM_OSS_PLUGINS`, as attack surface; by taking the one path here
with a real deprecation horizon across td's kernel bumps; and by getting
worse timing and format control in exchange. **Choosing a deprecated
compatibility shim to make an unsafe roster smaller is precisely the bad
trade the maintainer's premise forbids.** Nine pinned requests in a
pattern td has now built four times is not a cost worth that.

### K.5 The daemon, its uid, and the tests

`td-audio` is a new dependency-free crate and **unsafe surface #11**:
`ioctl(2)` with the pinned PCM roster, `poll(2)`, and `getsockopt(2)`
restricted to `SOL_SOCKET`/`SO_PEERCRED` with a pinned 12-byte
`[i32; 3]`. A multicall in the house style — `td-audio serve`, `status`,
`volume` — where the CLI personalities are ordinary Pulse *clients* of
the daemon's own socket, so policy tooling costs no second protocol.

**It runs as a dedicated `audio` uid, not as uid 1000 — CONFIRMED by the
maintainer**, which §O listed as the one privilege question the two
design passes disagreed on. This is the one place the maintainer's "if it
requires a nobody user, so be it" instinct pays, and it is a deliberate
departure from the ecosystem default. The
daemon parses an adversarial binary protocol from jailed clients all day
— the highest-exposure parser in the session after the compositor — and
unlike `td-busd` it needs nothing a separate uid withholds: its socket
lives at `/run/td-audio/native` (created and assigned by `td-seatd`
beside its existing `/run/user/1000` duty) rather than inside the 0700
runtime directory, and v1 needs no `/proc` lineage. Keeping `/dev/snd`
owned by `audio` rather than by the seat user also *enforces* mediation:
no uid-1000 process, jailed or not, can open the PCM behind the daemon's
back, which is what makes the daemon's policy the actual policy. A
compromise yields audio — playback disruption, and capture once capture
exists — and not the user's files, sockets or session.

**Three consequences of that separate uid were not specified, and
reviews found all three. They are the cost side of the decision, not
arguments against it.**

*The account does not exist, and the image refuses to invent one
quietly.* `system-x86-64.rs` declares exactly `root` and `tester`, and
its group builder materialises exactly `root`, `tester`, `wheel` (10)
and `tty` (5) — any other supplementary group is rejected at `cargo
test` until it is given a gid there, which is the check working rather
than an obstacle. So the audio uid needs: an account and gid in that
table, `td-seatd` chowning `/dev/snd/*` to it beside its existing
device duty, and a unit that starts the daemon under it — which today
means `exec=/bin/su -s /bin/sh audio -c …`, since `td-svc` has no
`User=`. None of that is on §I's ladder, where rungs 25 and 26 are the
crate and the protocol; it is a rung of its own and it comes first,
because a daemon with no account to run as cannot be started at all.
`system-x86-64.rs` is on §V.2's do-not-touch list, so this is also the
one place the audio work reaches into the image recipe and it should be
sequenced with whoever owns it.

*The socket's permissions have to be stated, because the obvious two
choices are each wrong.* If `/run/td-audio` is private to `audio` (0700),
then uid-1000 `td-jail` cannot even `stat` the socket it is supposed to
bind-mount into the sandbox, so the launch fails before the app runs. If
it is world-traversable, every local process reaches the daemon, which
for a single-user machine is *nearly* fine and is exactly the kind of
"nearly" this document refuses elsewhere. The specification: the
**directory is traversable (0755, owned by `audio`) and the socket is
0666**, with authorization done by the daemon on `SO_PEERCRED` — accept
uid 1000 and the audio uid, refuse everything else — rather than by mode
bits. That puts the decision in code that can say why it refused, which
is the same reason `td-busd` authenticates rather than relying on a 0700
directory. And `td-seatd` **creates the directory only**; it cannot
create a listening socket and exit, because a listening socket dies with
the process that holds it unless it is passed on, and nothing here passes
descriptors between units. The daemon binds its own socket.

*A dedicated uid cannot ask the portal for microphone consent.* The
portal authenticates its caller (§D), and the caller would be `td-audio`
— not the app whose capture request prompted it — so the prompt would
say "td-audio wants your microphone", which is both useless to the user
and unauthenticatable by the broker, since the audio uid has no access to
the uid-1000 bus in the first place. Capture therefore needs an
**authenticated relay**: the Pulse peer's identity is established at
connect time from `SO_PEERCRED` and the jail registry (the same lineage
answer §D gives), `td-audio` records it against the stream, and the
consent request is carried to the portal *on behalf of that app id*
through a channel the portal trusts. Designing that channel is the real
content of the microphone work, and it is why §K.5 stubs capture rather
than deferring it as a small remainder — the stub is honest and the
missing piece is an identity path, not an ALSA one.

(The competing view is that a session service should be uid 1000 like
`dbus-broker` and PipeWire are. That argument is sound for `td-busd`,
where the runtime directory and `AUTH EXTERNAL` both require it; it does
not transfer here, because nothing about `td-audio` needs the user's
identity. This is the one privilege question in the design where the two
passes disagreed, and it is a decision worth confirming.)

Mixing: a single-stream v1 is defensible but a daily driver wants
"browser plus notification", so mix from the start — per-stream queue and
clock, conversion to one internal rate and format, saturating summation,
fair wakeup and backpressure, per-stream volume, and inclusion in the
latency and drain accounting. Fix the device at 48 kHz stereo `S16_LE`
and mix internally at `f32`. Output volume and device selection are
session policy, not portal permissions; the portal becomes relevant only
for capture.

**No microphone in v1**: empty source list, `CREATE_RECORD_STREAM`
refused with a real Pulse error, `/dev/snd` never bound into a jail, and
the limitation reported explicitly when Firefox asks. This is a
deliberate divergence from upstream, where granting the Pulse socket
implicitly grants capture. When capture lands, `td-audio` mediates it
itself: map `SO_PEERCRED` to the registered jail lineage and app id, ask
`td-portal` for consent, delay the Pulse reply until consent or timeout,
record a session or persistent decision, show a compositor-owned
recording indicator, and support revocation by corking the stream.

Kernel and QEMU: `intel-hda` with `hda-duplex` as the first target
(mature, and it resembles real x86 hardware, where `virtio-sound` tests
only a virtual transport), with the `CONFIG_SND*` pins in §0. Real
machines need an explicit device matrix — USB audio, and SOF/SoundWire
with firmware on newer laptops — never inferred from QEMU success.

Testing, headlessly, at four levels: golden tagstruct and pstream
vectors plus a malformed-packet corpus; an in-memory `AudioSink`
replacing ALSA, asserting exact decoded PCM, drain timing, underflow and
mixing; `CONFIG_SND_ALOOP` in the guest, playing to the loopback
playback side and reading the capture side, which exercises the real
ALSA UAPI without speakers; and QEMU's `-audiodev wav` backend on the
host, playing a deterministic tone, terminating so the WAV header
finalises, then asserting rate, duration, non-silence and correlation
with the expected waveform. `CONFIG_SND_DUMMY` proves only that writes
were accepted and is an error-path tool, not the oracle.

**Size: 10,000–20,000 production lines**, plus a comparable test and
fixture corpus. The two passes estimated 5.5–8k and 17–28k; the gap is
almost entirely whether the server converts and mixes or fixes one
format and serves one stream. A Firefox-only prototype can sit at the
bottom of that range because cubeb resamples to whatever spec the sink
honestly reports — but it would be tuned to one client's trace and not a
sound server. Budget the middle.

---

## L. Privilege separation and the threat model

| component | uid | why |
|---|---|---|
| `td-svc` | root | the supervisor |
| `td-seatd` | root, oneshot | assigns `/dev/fb0`, `/dev/input/*`, `/dev/snd/*`; makes `/run/user/1000` and `/run/td-audio`; exits before any uid-1000 code |
| `td-compositor` | 1000 | owns the session |
| `td-busd` | 1000 | **required**, see below |
| `td-portal` | 1000 | reads the user's files in order to show them |
| `td-jail` (stage 0/1) | 1000 | fully unprivileged — resolve, register, unshare. It writes only under `~/.td/app` (§B.4); packages are read-only store paths (§B.1) |
| `td-authd` | root | §L.1 — elevation, and the ONLY component that grants it |
| `td-jail` | 1000 | it *is* the boundary; it holds nothing |
| `td-audio` | **`audio`** | §K.5 — the one dedicated uid |
| the app | 1000, identity-mapped | upstream's model; see below |

**`td-busd` must be uid 1000**, and the reasons are mechanical rather
than conventional. Its socket lives in a 0700 uid-1000 directory, so any
other uid requires opening that directory up — trading a theoretical
containment for a real exposure. And the baseline's lineage identity
(§D) reads
`/proc/<pid>/root/.flatpak-info` and `/proc/<pid>/stat`, which needs
ptrace-read permission — same uid or root — so a `nobody` broker could
not authenticate a sandboxed caller at all, and a root broker would be
strictly worse. It also buys nothing: every client is uid 1000, and a
compromised broker forges session messages whatever uid it holds.

**The app runs as uid 1000 in v1, and the consequence is stated rather
than buried.** `SO_PEERCRED` distinguishes a confined client only while
it stays confined; after an escape the process has the session's uid and
can connect to every uid-accessible socket. Same-uid is not a post-escape
boundary.

The Android-style answer — a distinct uid per app — is the largest
security gain available after the jail itself, converting "escape owns
the account" into "escape owns an empty uid". It is also much bigger than
it looks. Unprivileged processes may write only a single identity
mapping; anything else needs `CAP_SETUID` in the parent namespace, so it
needs a root `td-idmapd` broker (verify the requester, the pidfd, that
the target's maps are empty, that it is a registered child, and that the
app id owns the range; write `setgroups=deny` and the two maps; retain
nothing). That broker is 2–4k lines — but `~/.td/app/<name>` is then owned
by the wrong uid and needs a chown policy or idmapped mounts
(`open_tree`, `mount_setattr(MOUNT_ATTR_IDMAP)`, `move_mount`), the
Wayland/bus/audio socket permissions and every `SO_PEERCRED` check must
accept a *set* of uids, `AUTH EXTERNAL 1000` no longer equals the peer
uid, and per-app subuid allocation becomes persistent system state.
Realistically 8–15k lines beyond the broker, and it puts root on every
launch path.

**Recommendation: same-uid for v1, designed for the change.** The cheap
part is free now — every registration struct carries a uid *set* rather
than a scalar, and grants are expressed ACL-compatibly — so the later
project is additive rather than a rewrite. Schedule it as its own
hardening milestone after the first foreign-toolkit window, and make it
the default when it lands rather than opt-in.

**The question is not "accept a root component or not".** `td-authd`
(§L.1) exists for elevation regardless of identity, so the question is
whether the component td has already accepted should also do this. It
should: uid allocation is registry bookkeeping, and writing
`setgroups=deny` plus two maps for a process it can verify is a small
addition to something whose whole job is verify-then-act, and whose
enumerated-operation shape is exactly right for it. That does not shrink the 8–15k of downstream work — the chown
policy, the uid *sets* in every credential check, the socket permissions
— but it removes the largest structural objection, which was standing up
a second privileged path on every launch.

**Two consequences must be designed for now even though the work is
v2**, because both are silent breakages rather than missing features:

- **`AUTH EXTERNAL` stops being uid equality.** A sandboxed app inside a
  user namespace believes it is uid 1000 and sends `AUTH EXTERNAL` with
  1000 hex-encoded, while `SO_PEERCRED` — read by a broker *outside* that
  namespace — reports the mapped host uid. The claimed identity and the
  peer credential disagree by construction, so a broker that compares
  them for equality drops every sandboxed connection the moment per-app
  uids land. The rule must be that the claimed uid is checked against
  what the peer's credential *maps to*, and §D says so.
- **`~/.td/app/<name>` is owned by the wrong uid.** State directories are
  created before the identity exists in v1, so the v2 landing needs a
  chown pass or idmapped mounts, and `td-authd` (§L.1) is where that
  belongs — as an enumerated operation authorised at enrollment, not a
  prompt per launch.

**The claim.** Absent a kernel, namespace, seccomp, jail or
session-daemon vulnerability, a compromised flatpak app cannot read or
write outside its own `~/.td/app/<name>` and declared grants; cannot see host
processes, `/td/store`, the real `/etc`, the framebuffer, input nodes or
`/dev/snd`; cannot reach the compositor's private portal socket or other
clients' surfaces; cannot use the denied syscalls or gain privilege
through setuid or capabilities (`NO_NEW_PRIVS` plus empty caps, both read
back); cannot speak to non-portal bus names; and cannot record audio. td
does **not** claim that same-uid *unjailed* processes are isolated from
each other; that a kernel bug in the allowed syscall surface cannot void
the jail — in v1 an escape owns the user account; that network traffic or
network-namespace metadata is mediated when `shared=network` is granted;
that a malicious publisher is contained beyond the jail; or anything about
side channels, resource exhaustion, or the profile-data persistence in §B.

That shared namespace includes abstract AF_UNIX names and makes td's
interfaces, routes, socket rows and other network state visible through the
jail's fresh `/proc/net`. The td-busd and compositor-private-portal endpoints
use filesystem socket names, so their unmounted paths remain unreachable.
Other td services, including IP listeners, may be reachable through the
shared-network grant and continue to rely on their own authentication. Moving
session authority to an abstract listener would place it inside this grant and
must revisit that claim before landing.

**Nor that an app without `shared=network` cannot reach the network.**
`.OpenURI` starts the browser on an `http`/`https` URL with no dialog
(§E), so a URL is an egress channel and its query string is the payload.
Upstream has the same shape and this is recorded rather than fixed:
closing it means either prompting for every URI — which trains the
click-through §L.1's threat table exists to avoid — or refusing an
application the one thing users most expect it to do. What the claim
list must not do is imply a network boundary that `.OpenURI` walks
around.

---

### L.1 Elevation — consent without a secret

**Not part of the application layer**, and specified here because this is
where the question was decided. `td-authd` is a system component; whoever
owns `td-svc` and `td-compositor` builds it. Applications are among its
*non*-users deliberately — but the reason has to be stated precisely,
because a draft said "launching or updating one changes no machine state
that needs authority" while §W.2 said applying an application update
needs exactly the authority `td-authd` has. **LAUNCHING** one changes no
machine state, which is the claim that matters here: it is what keeps
the prompt rare, and rarity is most of what makes consent mean anything
(§L.1's own fatigue row). **INSTALLING OR UPDATING** one writes a system
location, and when the deferred tier of §W.2 arrives it arrives as an
enumerated operation in the table below, beside `deploy-publish`. Today
neither exists, which is why the tension went unnoticed: an application
update IS a deployment, and deployments are already in the table.

The requirement is a pair that looks contradictory and is not: **a user
must be able to elevate, and must never type a secret to do it**
(`AGENTS.md` principle 7). What resolves it is that a password and a
consent prompt answer different questions. A password asks *does this
person know the secret* — which malware holding the person's session can
also answer, having watched them type it. A consent prompt on a path
software cannot reach asks *is a human deliberately approving THIS
operation right now* — which malware cannot answer at all. UAC is the
shape; its secure desktop is the part that matters.

**Scope, decided by the maintainer and load-bearing for everything
below: physical access to a booted, unlocked machine is NOT in the threat
model.** An attacker at the keyboard can elevate, and that is accepted
rather than solved. What this defends against is **software in the
session** — a rogue or escaped application acting without the operator.
That is what makes the cheaper primary mechanism the right one rather
than a compromise.

#### The prerequisite, without which none of it works

§L's table runs `td-compositor`, `td-busd`, `td-portal` and the v1
application all at **uid 1000**. Under that layout `td-authd` receives
"the human approved" from a uid-1000 peer and cannot tell the compositor
from a rogue application — and with no LSM and therefore no Yama
restriction, a uid-1000 process can `ptrace` the compositor and steal
whatever channel it holds.

> **`td-compositor` must NOT share a uid with anything an application can
> become.** That means per-app uids (§L, v2) *and* the compositor at its
> own uid. A hard prerequisite, not a refinement: elevation must not ship
> before it.

Two mechanisms then carry the channel, and both are needed. The
compositor↔`td-authd` link is a **socketpair created by root `td-svc` at
startup** and handed to each side at spawn — authority by possession of a
descriptor no other process can obtain. And each side **pins its peer
with a pidfd** rather than trusting a reusable pid. A dedicated uid alone
would not survive the missing descriptor; the descriptor alone would not
survive same-uid `ptrace`.

**`td-svc` cannot pass a descriptor today, and that is a FOURTH
prerequisite** — this section listed three and missed the one its own
mechanism rests on. `td-svc`'s unit table is a closed key set (`type`,
`exec`, `ready`, `after`, `requires`, `restart`, `tty`, `log`, `console`,
and the three timeouts), any other key is a parse error, and there is no
socket activation, no fd passing and no `User=`; a unit that runs as
somebody else spells it `exec=/bin/su -s /bin/sh …` inside its argv. So
"a socketpair created by root `td-svc` at startup and handed to each side
at spawn" is a td-svc landing before it is a `td-authd` one, and it is
the piece with the widest blast radius, since a supervisor that can hand
out descriptors can hand out the wrong one.

#### Three things the secure path needs that the tree does not have

They share one shape: the compositor's ownership of the console is a
convention among cooperating processes, not a boundary.

1. **`td-seatd` does not hand the compositor exclusive devices; it
   CHOWNS them to the seat user.** `assign_path`
   (`td-seatd/src/main.rs:110-117`) `lchown`s `/dev/fb0` and every
   `/dev/input/event*` to the seat account at mode 0600, so **every
   uid-1000 process can open them** — read every keystroke including the
   approval one, and open `/dev/fb0` `O_RDWR` to paint over a
   compositor-drawn prompt and read back what it drew.

   **The fix is the uid split above, not `EVIOCGRAB`**, and getting that
   round the right way saves an `UNSAFE.md` amendment. The nodes are
   0600 and owned by the seat account; once the compositor has an
   account of its own, "the seat account" IS that account and an
   application at uid 1000 gets `EACCES` from the same `open` — for the
   framebuffer as well, which has no grab and so could not have been
   fixed the other way at all. A draft concluded that §L.1 carries an
   amendment to `UNSAFE.md` §6, whose stated ground for refusing
   `EVIOCGRAB` is that the compositor "owns the console outright, so
   there is nothing to take it from". That premise is false TODAY and
   the uid split is what makes it true, rather than the grab. What a
   grab would still add is exclusion against other processes at the
   compositor's OWN uid — a much smaller claim, and not one this section
   needs.
2. **`.Screenshot` captures full output** and `td_portal_manager_v1`
   exposes `capture_output`, with nothing excluding a prompt from a
   capture. The prompt must be excluded from every capture path by
   construction.
3. **`TIOCSTI`/`TIOCLINUX` are not the input-injection defence.** They
   are terminal injection, denied inside the *application* filter, and
   say nothing about `/dev/uinput`, virtual-keyboard or input-method
   protocols, emulated devices, or a compromise of any other uid-1000
   session component.

**Trusted input therefore needs its own roster** — an enumerated list of
every way an event can enter the compositor, everything not on it denied.

One entry is a genuine conflict inside this document. **§S deliberately
specifies synthetic input injection** on the private compositor socket so
an agent can drive the desktop; if that can reach a prompt, the prompt
authenticates a program. So **injected events are tagged at their source
and are never accepted as consent** — the prompt reads only events the
compositor took from evdev. Stated the useful way round: *the agent may
operate the machine, and may not authorise a change to it.*

#### Shape

```
requester ──request──▶ td-authd (root)
                          │  identity from SO_PEERCRED + lineage (§D),
                          │  never from anything the caller claims
                          ▼
                    td-compositor ── prompt on the secure path
                          │          exclusive input for its lifetime
                          ▼
                    human approves ─── reserved key combination, then a
                          │            randomized approval key
                          ▼
                 td-authd PERFORMS the named operation itself
```

#### Six properties, each load-bearing

1. **Enumerated operations, never a shell.** A fixed table of named
   operations with typed argument schemas — `deploy-publish`,
   `deploy-rollback`, `set-hostname` — *performed by `td-authd` itself*.
   It never returns a privileged handle, never spawns a process the
   requester controls, and there is no `elevate <argv>`. This is the
   biggest divergence from UAC and the one that matters most: UAC
   elevates a *process*, so every bug in it is a bug at high integrity,
   and its auto-elevating signed binaries have been the source of bypass
   after bypass. An operation table is reviewable the way `UNSAFE.md`'s
   rosters are.
2. **Identity comes from the kernel** — `SO_PEERCRED` plus §D's lineage
   walk, `Unknown` denied. A rogue application cannot present itself as
   another *process*, because the pid is the kernel's answer rather than
   its own. **It can still present itself under another NAME while v1
   runs every peer at uid 1000**, since §D's registration authenticates
   by uid and the app id is supplied by the registrant — so the prompt's
   "firefox is asking to publish a deployment" is only as good as
   per-app uids, which is the prerequisite this section already refuses
   to ship without.
3. **Consent is bound to the request, and binding is not just hashing.**
   One operation, one requester, one argument set, once. No remembered
   answers, no "don't ask again", no grace window. Hashing an argument
   does not bind the object it names — a requester can swap a symlink
   between approval and use — so security-relevant objects are **opened
   before the prompt and carried as pinned descriptors**, the prompt
   displays their canonical resolved meaning, and the operation acts on
   the descriptor rather than re-resolving the path. **A descriptor pins
   an INODE and not its contents**, which is the correction review made
   to that sentence: a regular file stays writable through any other
   descriptor, and a directory fd says nothing at all about what is
   under it, so a requester could still change what it got approved for
   between the prompt and the operation. What closes it is that
   `td-authd`'s arguments are **content-addressed store paths**, which
   are immutable by construction and are the thing the operator was shown
   — a hash, not a location. Any future operation whose argument is a
   mutable object must carry a digest computed over the same descriptor
   and re-verified at use, or it does not belong in the table. The nonce
   is **consumed atomically before the operation starts**, so a retry is
   unambiguously a new request.
4. **Authorization is separate from consent.** The table says what may be
   elevated; it does not say who may approve it. `SO_PEERCRED`
   authenticates the requester and a keystroke authenticates presence —
   neither says the approving human is entitled to publish a deployment.
   So `td-authd` carries an **operation-to-principal policy**, consulted
   *before* any prompt is shown, living with system configuration rather
   than in any user's home. **td is not single-user**: a family machine's
   accounts do not all get to publish a deployment, and that asymmetry is
   why elevation exists on such a machine at all. The single-operator
   case is that table with one row, not a different design.
5. **The prompt is the compositor's, exclusively.** Not a client surface.
   For its lifetime the compositor takes exclusive input, draws above
   everything, and delivers those events to no client.
6. **Fail closed.** No seat, no compositor, a remote or headless session,
   a `td-authd` that cannot reach the secure path — each is a refusal
   with a named diagnostic, never a fallback to a weaker check. A
   mechanism that degrades to a password under pressure is a password
   mechanism.

#### What proves a human is present

**The primary mechanism is a compositor-reserved key combination**, not a
security key — the classic secure attention key, chosen over hardware for
convenience rather than settled for: a token the operator must find and
touch for every system change is a token they will leave plugged in.

1. A requester asks `td-authd` for an operation.
2. The compositor posts an unobtrusive notice naming the requester and
   the operation. The notice grants nothing.
3. The operator presses the **reserved combination**, which the
   compositor never delivers to any client and no client may bind.
4. The compositor switches to the prompt, holding input exclusively.

That gives a **user-invoked path to a known-genuine prompt**, which
Windows' secure desktop lacks. A fake prompt can be drawn at any time but
cannot receive the reserved combination, so an imitation harvests
nothing.

**The approval key is randomized, and that is not a refinement.** If any
keystroke approves, a rogue application draws "Press Enter to continue",
waits for the primed user, and fires its request so the compositor reads
that Enter as consent. A minimum display time does not help — the user is
already reaching. So the prompt names a key chosen at random per request,
which a fake prompt cannot know and a primed reflex cannot supply.

**A security key remains optional and stronger**: a full assertion over a
domain-separated, versioned, length-prefixed canonical encoding of the
complete request. It is not *authentication* by itself unless UV is
enrolled — CTAP2 user presence proves possession, and the only ways to
make it two-factor are `clientPin` (a memorised secret principle 7
forbids) or an on-authenticator biometric. A PIN-less key means *whoever
holds the key may elevate*, consistent with physical access being out of
scope. If a token is used, **CTAP access must be exclusively mediated**:
a key is `/dev/hidraw`, not evdev, so the compositor's input path does
not cover it, and a rogue process holding hidraw can solicit an assertion
timed with the prompt and consume the operator's touch. Today no jail
binds hidraw, which closes it by accident and reopens the moment anything
wants WebAuthn.

**Neither mechanism runs on today's kernel**, and the key is further away
than it looks: the pin list contains **no `CONFIG_USB*` at all** — not
merely no `HIDRAW` but no host controller — so a token cannot be
enumerated. The key combination needs only what the compositor already
has, which is the other reason it is primary.

#### Threats a consent dialog invites

| threat | answer |
|---|---|
| **Prompt fatigue** — a rogue app requests repeatedly until the user approves to stop it | Rate-limit per requester with backoff; show a denial count; and above all keep the legitimate prompt RARE, which is why application launch never routes through it |
| **Confused timing** — a request fired as the user reaches to approve another | Requests QUEUE rather than stack, the prompt names the requester, and a new request cannot replace a displayed one. The randomized key is what actually answers this |
| **Fake prompt to train the habit** | An imitation cannot learn the randomized key and cannot receive the keystroke while the real prompt holds input. NOT answered by "a touch goes to the token regardless of what is on screen" — that same fact is the hidraw race above |
| **Input-focus theft** | Exclusive input for the prompt's lifetime; no client receives those events at all |
| **Elevate-a-shell** | Structurally impossible — no operation returns a process, and the table is enumerated |
| **Replay of a captured approval** | The assertion covers requester, operation, pinned arguments and a nonce, **length-prefixed rather than concatenated** (or `("a","bc")` and `("ab","c")` collide), and the nonce is consumed before the operation starts |
| **The requester lies about what it is** | It never says which PROCESS it is — that is `SO_PEERCRED` plus lineage. It does say which APPLICATION, since §D's registration is authenticated by uid and v1 runs every peer at uid 1000, so the name in the prompt is only as good as per-app uids. A draft wrote this row the other way round, claiming an escaped app is "promoted to `Unconfined`" where the prompt can only say "a process"; under §E's own definition that is wrong, because an escapee is still a descendant of a live registered stage-2 pid and resolves `Jailed` — the filter denies `unshare`, `setns` and `clone(CLONE_NEWUSER)`, and killing PID 1 of a pid namespace kills the namespace. The exposure is the id, not the lineage |
| **Walk-up attacker at an unlocked session** | **Out of scope by decision.** A password model would resist it and this one does not; that is the accepted trade. A screen lock is where to address it, and it belongs to the session rather than to elevation |
| **Prompt spam from an unidentifiable requester** | **Partly unanswerable as specified.** Rate-limiting assumes a stable requester identity, and `Unconfined` code can fork a fresh process per request. Rate-limit the jailed case per app id; for `Unconfined` the limit can only be global, which degrades into denying elevation to everyone while an attacker spams |

**Deliberately NOT in this design**: a `sudo`-equivalent running an
arbitrary command; remembered or timed authority; per-application
allow-lists that pre-approve anything; auto-elevation by signature or
path; any recovery path accepting a memorised secret (recovery is a
second enrolled token); and a policy language — the operation table is
code, reviewed as code.

**Cost, honestly.** A new root component and a new trust surface, needing
USB HID and CTAP2 in a kernel config that has neither, a compositor mode
that does not exist, and it is on no ladder here. What this section buys
today is that the question is decided and written down, so the workstream
that reaches it amends a plan instead of inventing one — and so no other
part of this design quietly assumes a password prompt is available.

## M. Hardware rendering — not painting into the corner

td will never need Mesa: the runtime brings the client GL stack. td's
side decomposes into (1) a DRM/KMS output backend — atomic modeset, dumb
buffers, page-flip vsync — still software-rendered but with real display
control; (2) `zwp_linux_dmabuf_v1` import for direct scanout of an
eligible fullscreen surface; and (3) GPU composition, which needs a
userspace driver stack td will not write and which steps 1 and 2 are
chosen so as never to require.

But "the seam is small" is only true for direct scanout. On a tiled
desktop, surfaces overlap, KMS has a bounded number of planes, and a
plane may not scale or crop a given format — so somebody composites.
And a client cannot be told through `zwp_linux_dmabuf_v1` that its buffer
is acceptable *only while fullscreen*: once a format and modifier are
advertised, they must be handled in ordinary placement. **Therefore do
not advertise dmabuf until a reliable linear-mapping CPU composition
fallback exists.**

**The honest toll is `mmap`.** The current stack's no-mmap property —
client pixels copied out of pools with `FileExt::read_exact_at`, the
device written with `seek`+`write_all` — does not survive KMS. A dumb
buffer has no `write(2)` path; pixels enter through `mmap` of the card
fd, and any dmabuf that must be CPU-read is `mmap` plus
`DMA_BUF_IOCTL_SYNC`. So the GPU path's first landing carries an
`UNSAFE.md` amendment of a genuinely **new class**: not another
syscall-instruction one-shot but a *lifetime-carrying mapping*, which the
roster's current phrasing does not cover. Write that anticipation into
`UNSAFE.md` **now**, so the future author amends a plan instead of
bending around a rule — budget a mapped-region abstraction with a pinned
length, a `Drop`-based unmap, and confinement tests in the `BorrowedFd`
guard style. Refusing `mmap` forever is the one way td could actually
paint itself into this corner.

What to change now, while it is cheap — and nothing more:

| today | change |
|---|---|
| a surface owns copied XRGB pixels | store a buffer abstraction; copied pixels become one variant (`ShmSnapshot`) beside a future `Dmabuf { planes, fourcc, modifier, fences }` |
| **every buffer is released immediately** | introduce buffer leases and completion. This is the decision most likely to force invasive change: a dmabuf may not be released at commit — only when GPU work finishes or its scanout is replaced and the page-flip event arrives |
| the renderer writes around `/dev/fb0` assumptions | put scanout behind an `OutputBackend` with `dimensions`/`supported_formats`/`begin_frame(damage)`/`present`/`poll_events`; define `paint` as *submit* (fbdev completes synchronously, KMS completes at flip) so no caller assumes pixels are on glass when it returns |
| resource caps counted in CPU bytes | account per buffer type and per outstanding lifetime |
| dmabuf "refused" | say "not advertised in the software phase" — refused until KMS, not refused as identity |
| one implicit output | give output an id, dimensions, scale and transform even with one at scale 1 |
| `/dev/dri` simply absent | an explicit `devices=dri` policy branch, parsed and refused (§C) |
| software env vars | keep them strictly per runtime-major and removable, never compiled in |

Three seams are already right and must stay: `Scene::render` is pure
bytes-in-bytes-out and needs nothing; `framebuffer.rs` is already the
only module that touches the device; and `server.rs::copy_buffer` is the
single buffer-ingestion point — do not let a second pixel path grow
elsewhere.

The seccomp filter needs **no change**, and that is an underappreciated
payoff of the deny-list decision: `ioctl` is allowed except the two
terminal requests, so DRM and dmabuf ioctls already flow. Record it as a
constraint — no future tightening may blanket-deny ioctl ranges without
checking DRM. Request-level DRM allowlisting in classic cBPF would be
brittle across driver versions; render-node visibility is the stronger
boundary. The jail will also need read-only `/sys/class/drm` and
`/sys/dev/char/<maj:min>` views for Mesa's device discovery — make that
list a data table now so adding them is a row.

Kernel: `DRM`, `DRM_VIRTIO_GPU`, `DRM_VIRTIO_GPU_KMS` and
`DRM_FBDEV_EMULATION` are already pinned, the 7.1.4 virtio-gpu driver
advertises `DRIVER_RENDER`/`MODESET`/`ATOMIC`, and there is no separate
`CONFIG_VIRGL` symbol — the capability is dormant only because QEMU runs
without VirGL. **Step 1 needs zero new kernel symbols.**

---

## O. Settled questions

Eighteen questions were put to the maintainer and answered; the ones
whose answers are not already a section of their own:

| question | answer |
|---|---|
| curated set or open Flathub | **curated, built by recipes** — §B.3.1 may seed one reviewed app/runtime pair from exact signed Flathub commits, but there is no open repository or target-side install and runtime EOL/update policy still follow the pin |
| performance gates | no metric targets yet; **QEMU is the only target until daily-usable** |
| filesystem-grant strictness | **per-package permission config**, Flatseal-shaped |
| microphone | **stub the interface now**, implement later |
| `.desktop` `Exec=` | **worth doing** — the bounded safe subset (§A) |
| accessibility | **wanted**, and for a reason that reshapes it: *"especially as it becomes important for ai agents to read the screen"* (§S) |
| `machine-id` | **mint per-app values** |
| timezone | **must be addressed** — a per-user `TZ=` reading the runtime's own zoneinfo is zero td code and the difference between a quirk and a daily irritant |
| architecture scope | **keep the path to other architectures open**; every UAPI layout here is x86-64-specific and marked so |

## P. Resource caps

Decision 15 is the one with a concrete, checkable answer, and half of it
already exists in td.

**Use `RLIMIT_DATA`, not `RLIMIT_AS`** — td already made this call and
wrote down why (`builder/src/sys.rs`). The reason needs stating more
carefully than that comment does, because Linux 4.7 changed what
`RLIMIT_DATA` covers and the loose version of the argument is no longer
true.

Since 4.7 `RLIMIT_DATA` bounds `brk` **and private writable mappings —
file-backed ones included**, not only anonymous, which a first draft of
this paragraph got half right. So it is not the brk-only limit the older
folklore describes and it does count a runtime's heap arenas. What it
still does not count is address space reserved **without write
permission** — the `PROT_NONE` reservation a runtime makes up front and
then `mprotect`s piecewise as it grows — which is exactly the pattern
that makes `RLIMIT_AS` unusable. It also does not count **shared**
mappings, which is the exclusion that matters most here: Firefox's
memfd-backed shm pools are shared, so the pixels a browser holds are
outside this limit entirely. That is another reason the cgroup is the
real bound and this is the backstop.
So the conclusion survives and the margin is narrower than "it ignores
virtual reservations" suggests: `RLIMIT_DATA` tracks memory a process has
actually made writable, `RLIMIT_AS` tracks every mapping including
reservations that will never be touched, and Firefox reserves liberally.
An `RLIMIT_AS` cap tight enough to matter kills it for reasons unrelated
to memory used. `td-jail` instead sets both halves of `RLIMIT_DATA` to the
authenticated `memory-max` value exactly. It is a per-process backstop to the
aggregate cgroup ceiling, not a separately tuned policy number.

Private writable thread stacks count at their reserved size, not at the pages
they have faulted in. A runtime using glibc's usual 8 MiB pthread stack can
therefore reach a 64 MiB fixture limit at roughly eight threads even while its
physical footprint is smaller. `pids-max` is an independent ceiling, not a
promise that every admitted task can reserve a default-sized stack. This is an
intentional cost of keeping the per-process backstop equal to `memory-max`;
applications that need a different thread/stack ratio need a larger reviewed
resource policy or smaller explicitly configured stacks.

(`builder/src/sys.rs`'s comment carries the loose version and should be
tightened when that crate is next touched — noted here rather than fixed,
since it belongs to the engine and not to this design.)

**But rlimits alone cannot answer the question that was asked.** They
are per-process and inherited across `fork`, so a limit is a bound on
*each* process, not on the application. Firefox is deliberately
multi-process — a main process plus content, GPU, RDD and socket
processes — so a 2 GiB `RLIMIT_DATA` with eight content processes bounds
the browser at sixteen gigabytes plus change, which is not a cap in any
sense the maintainer meant. The per-process limit is still worth having
as a runaway backstop: it turns one leaking process into one dead
process instead of a dead machine.

**Set the HARD limit, not just the soft one.** A process may raise its
own soft limit up to the hard limit without any privilege, so a backstop
written only to `rlim_cur` is a suggestion the application can decline —
and a runtime that catches an allocation failure and retries after
raising its limit is doing something entirely reasonable from its own
point of view. `td-jail` sets both to the same value before `exec`, and
that is one-way for a stronger reason than the draft gave: it is not
`NO_NEW_PRIVS` (which is about privilege gain through `exec` and has
nothing to say about limits) but that raising a HARD limit requires
`CAP_SYS_RESOURCE` in the **initial** user namespace — `capable()`
rather than `ns_capable()` — so it holds even for a process that owns
every capability in a namespace of its own, which is exactly what a
jailed app can arrange. Setting it needs no syscall of its own
(`CommandExt` cannot do it, but `setrlimit` is reachable through safe
`std`… it is not, in fact — `std` exposes no rlimit API at all, so this
is a **prlimit64(2) amendment** to surface #9 that the earlier draft did
not account for, and it should be landed with the caps rather than
discovered at implementation time).

**The aggregate bound needs cgroup v2**, which is why §0 pins the memory,
PID, fair-scheduler, and CFS-bandwidth controller symbols rather than
deferring them. The shape:

- `td-svc` (root) creates the session's cgroup subtree at boot and
  **delegates** it, which is the same thing systemd's `user@.service`
  does and the only way an unprivileged `td-jail` may create children in
  it. The delegation set is **three files, not one**: the directory,
  `cgroup.procs`, AND `cgroup.subtree_control`. The third is the one an
  earlier draft omitted and it is load-bearing — without ownership of
  `cgroup.subtree_control` the delegate cannot enable the `cpu`, `memory`,
  and `pids` controllers for the cgroups it creates, so their leaf controls
  never come into existence and every write below fails
  `ENOENT`. (`cgroup.threads` completes the conventional set and is
  harmless to include.) The failure mode is worth stating because it is
  not "permission denied" but "the file is not there", which reads like
  a kernel-config problem and is not one.
  Delegation is not an isolation boundary against a trusted, unconfined
  process with the same uid: such a process has the same authority to change
  child controls. The confined application cannot do that because cgroupfs is
  masked inside its mount namespace; td's other uid-1000 session components
  are therefore part of the trusted session boundary.
- **Two cgroup-v2 rules make that delegation fail if it is a bare
  `chown`, and both reviewers found it independently.** Controllers are
  enabled TOP-DOWN — every controller must already be in the parent's
  `cgroup.subtree_control` before the delegate can enable it in its own —
  so `td-svc` enables it down the chain at boot rather than assuming a
  distribution did. And a cgroup with member processes cannot enable
  controllers for its children (the "no internal processes" rule), so the
  delegated directory must stay EMPTY: session processes live in a leaf
  beside the per-instance ones, never in the delegation root. Get that
  wrong and the write to `cgroup.subtree_control` fails `EBUSY` — a
  different failure from the `ENOENT` above and, unlike it, one that
  points at the file rather than at the design.
- `td-login` moves a uid-1000 session into that fixed `session` leaf while it
  is still root and reads the unified membership back before dropping
  credentials. Stage 1 therefore starts in the session leaf; when it moves
  blocked stage 2 into a sibling per-instance leaf, the delegated root is the
  source/destination common ancestor and uid 1000 owns the required control files.
- `td-jail` creates one cgroup per instance, writes `memory.max`,
  `memory.high`, `pids.max`, and `cpu.max` from the resolved permission policy,
  then moves stage 2 into it *before* spawning the app, so every descendant is
  inside by construction. Permission format 2 carries the explicit CPU quota
  and period; format 1 inherits the reviewed `100000 100000` baseline.
  The bounds mirror the pinned kernel and there is no `max` spelling.
- Before creating the leaf, stage 1 starts a cleanup bootstrap with cwd `/` and
  retains a close-on-exec keepalive pipe. The bootstrap enters and proves a new
  session before spawning the watcher; the watcher enters and proves its own
  new session before sending readiness. A stale terminal-member snapshot may
  still kill stage 1 and the bootstrap, but cannot name the later-born watcher.
  Normal exit collects diagnostics, releases the leaf to that watcher, and
  closes the pipe; signal or abort closes it implicitly. All three paths
  therefore use the same bounded drain and removal without consuming the
  bounded active scan. `setsid(2)` is surface #9's fourteenth syscall;
  controller operations remain ordinary safe filesystem I/O.
  Failure to drain within ten seconds makes even a clean entry status a launch
  failure; td does not report success while charged descendants remain live.
- `memory.high` before `memory.max` matters: `high` throttles and
  reclaims, `max` invokes the OOM killer inside the cgroup. A browser
  that gets slow near its ceiling is better behaviour than one that
  loses a tab, so set both, `high` below `max`.
- `cpu.max` covers the fair scheduler only. td deliberately keeps
  `RT_GROUP_SCHED` off: applications get no real-time scheduling grant, and a
  real-time task in the populated hierarchy root cannot prevent td-svc from
  enabling the delegated CPU controller. The exact quota/period readback and
  required `nr_periods`, `nr_throttled`, and `throttled_usec` rows in
  `cpu.stat` prove the bandwidth interface rather than CPU accounting alone.
- **`memory.oom.group=1` is not optional, and leaving it out was the
  gap a review found.** `memory.max` on its own does not bound *the
  application*; it bounds the cgroup, and the memcg OOM killer's default
  is to pick a single process. For Firefox that is one content process —
  so the browser loses a tab, keeps running, allocates again, and the
  cycle repeats: exactly the "eating all my RAM" behaviour the premise
  named, now with tab loss added. `oom.group=1` makes the kill atomic
  over the whole cgroup, which for a jailed app is the whole app. The
  other half of the same point: an allocation can simply **fail** at the
  ceiling rather than triggering any kill at all, and a process that
  handles `ENOMEM` badly hangs instead of dying. So `memory.high` is
  what keeps the machine usable and `oom.group` is what makes the
  failure clean; neither substitutes for the other.
- Read `memory.events`, `memory.peak`, and the bandwidth rows from `cpu.stat`
  back for diagnostics: an app killed for memory or throttled for CPU should
  say so rather than looking like an unexplained failure or slowdown.

The landed hierarchy leaves PID 1 and system services at the hierarchy root,
which cgroup v2 explicitly exempts from the no-internal-process rule, beside
the empty delegated `td-user-1000` root. Application sessions and per-instance
leaves are the only descendants placed under the delegated subtree.
Per-app values live in the same per-package permission file as the filesystem
and device grants (decision 9). Omission means 1 GiB high, 1.25 GiB max, 1024
tasks, and one fair-scheduler CPU rather than unlimited — *unlimited* is the
setting that produced the complaint.

---

## R. What to build first, and the experiments that decide the rest

**The security platform is model-independent, and it is the majority of
the work**: the kernel pins, `td-jail` and the per-architecture seccomp
framework, subuid allocation and a stable identity registry, `td-busd`,
`td-portal` and scoped file grants, `td-audio` with the microphone stub,
cgroup resource caps, the Wayland protocol ladder, the
virtio-gpu/dmabuf path, the permission schema and spec compiler, per-app
machine-ids, timezone policy, the bounded `Exec=` parser, and §S's
accessibility work. Months of work with **zero decision risk**. Build
that. Do not build a general OSTree/OpenPGP stack or a 200-package desktop
graph; §B.3.1's bounded exact-commit importer is the deliberately smaller
exception.

**The sharpest formulation of the whole question, and the rule to
follow:** *the package provenance decision need not be irreversible; the
application identity, confinement and state contract must be.* Commit to
the second now; defer the first per application.

The experiments that settle what is left, none longer than a week:

| # | experiment | settles |
|---|---|---|
| **E1 — package half answered (§B.3.1)** | inspect the signed Firefox 154.0 and Freedesktop 25.08 deploy commits without executing the app; map their exact `files/` trees to `/app` and `/usr` | the deploy hierarchy is the right package/runtime split, has no special or setid files, and at 993.7 MB deployed is smaller than the roughly 1.4 GB uncompressed standalone Guix closure. Its separate compressed transfer is 382.7 MB. Execution waits on the dynamic-runtime/jail, bus and Wayland stop line rather than using host `bwrap` as a substitute for td-jail |
| **E1b — route selected, importer not landed** | fetch and materialize the same exact signed Flathub commits through a bounded control-plane importer | Flathub publishes no stable deploy tarball, so an ambient `flatpak` recipe and a locally hosted export are both refused. §B.3.1 is the importer contract |
| **E2 — COMPLETE** | filter globals from GTK 4.22.1; exercise forced Cairo; compile in exact Freedesktop SDK 25.08 and run against the exact pinned Platform; run pinned Firefox 154.0 on no-dmabuf Weston and test its nested user namespace and `about:support` sandbox report | data-device is the GTK first-window blocker while subcompositor is class U; forced Cairo, pinned llvmpipe, and pinned Firefox all attach shm without dmabuf; stock Firefox denies nested user namespaces yet retains effective content sandbox level 6, selecting one standard td filter. The pinned Freedesktop runtime contains GTK 3 rather than GTK 4, so a future GTK 4 runtime selection repeats that identified part of the matrix |
| **E3** | a Meson-world pilot — recipes for `pkgconf`, Ninja, Meson, a native CPython, then GLib and a Wayland-only `gtk3-demo` | td's *actual* per-package source cost, the number with the widest error bars. Near `cmake-x86-64`'s cost and the source track is real; a multi-week fight per package and the hybrid is permanent posture |
| **E4 — COMPLETE** | the §0 cgroup pins plus a fixture under `memory.high=48M`, `memory.max=64M`, `pids.max=32`, and `cpu.max=50000 100000`, with active membership and exact controller readback gating the QEMU oracle | §P's mechanism works on the target kernel |
| **E5** | `glxinfo` inside a jail on virtio-gpu QEMU with the runtime's GL extension mounted | §M's first step |
| **E6** | `RLIMIT_AS=2G` on Firefox; record the crash | closes the rlimits-versus-cgroups argument with data |

**Points of no return.** Landing a general GVariant/OSTree/OpenPGP tower is one
— sunk cost will argue for keeping it. E1 has now run and selects only the
bounded exact-commit importer in §B.3.1, not that tower. Landing
LLVM-with-Clang, Node and WASI toolchain recipes is the
other, and pays off only if source Firefox is genuinely pursued. Refusing a
target-side OSTree client remains the *reversible* commitment: if td later
wants target-side installs, the client can be built then, against a platform
that already runs the applications.

## S. Accessibility, and AI agents as a first-class consumer

Model-independent, and worth building early — decision 13 asked for it,
and source-building changes nothing about it.

1. **The a11y bus and registry.** Toolkits discover accessibility by
   calling `org.a11y.Bus.GetAddress` on the session bus, then speak
   AT-SPI on a second bus mediated by a registry daemon. This work sits
   **on top of `td-busd`, which does not exist yet either** — an earlier
   draft said "`td-busd` already exists", which is wrong in a document
   whose first line is that nothing here is built; it is milestone 13.
   And a **second bus is a second transport**, not a policy on the first:
   AT-SPI's address is handed out by `org.a11y.Bus.GetAddress` and the
   traffic goes somewhere else entirely, so this is another listener,
   another accept path, and another set of connections to authenticate.
   Serving that plus a bounded `org.a11y.atspi.Registry` — app
   registration, event routing, and the activation that starts the
   registry when the first client asks — is a `td-busd`-class landing of
   order 3–8 kloc, after which `GTK_A11Y=none` comes off and **Firefox's
   mature AT-SPI implementation lights up**, handing an agent roles,
   names, values, text runs and actions.
   Two limits on "every foreign app", since the earlier wording implied
   more than the mechanism delivers: the bus makes accessibility
   *possible*, and an actual consumer — the thing that reads the tree and
   presents it — is still td's to write (item 2 is the near-term
   substitute); and toolkit support is not universal, since Qt and Electron
   applications vary in how completely they implement AT-SPI, so what
   lights up is GTK and Firefox rather than everything. The client side
   ships inside the runtime in both models; only the bus side, and the
   consumer, are td's to build.
2. **A td automation protocol on the private compositor socket**, beside
   `td_portal_manager_v1` and privileged the same way — by path
   visibility. Enumerate toplevels (app id, title, geometry), capture
   (the Screenshot path already exists), inject synthetic keyboard and
   pointer events (the compositor owns input), and read td-native text
   directly. This is the 80% an agent needs *today*, since current
   agents work from screenshots plus input injection, and it is a few
   thousand lines on machinery that exists. It is also precisely what
   AT-SPI lacks on Wayland: input injection and trustworthy window
   identity.
3. **Track, do not invent, the ecosystem's next protocol.** GNOME's
   Newton experiment is the maintainer's instinct exactly — applications
   *push* accessibility trees to the compositor over a Wayland protocol
   — built on AccessKit, whose Unix adapter currently speaks AT-SPI.
   Newton is a prototype with an undefined protocol, not a foundation to
   bet on; but it is good evidence the compositor-level instinct is
   right for the future, and td's automation protocol should borrow
   AccessKit's vocabulary so convergence stays possible.

Guard rails for the agent interface, because it is an input-injection
capability by construction: capability-scoped semantic query and action,
explicit human authorization, redaction of password and private nodes,
visible-surface and focus constraints, rate limits and audit events, and
**separate, higher authority for raw pointer and keyboard injection**
than for reading.

---

## V. Splitting the work

### V.0 Two things land before anyone starts

**E1's package half has run and E1b has a selected contract** (§B.3.1 and
§R). The execution half stays behind the documented dynamic-runtime, bus and
Wayland stop line. If that later run finds something generated at install time,
an undocumented environment variable or a portal required before first paint,
that is the finding; the package inspection is not allowed to predeclare the
execution result.

**The new-crate de-collision.** Adding a crate touches three central
tables, and every one is a line four agents will edit at once:

1. `builder/src/affected.rs` — `DEPENDENCY_FREE_LOCKS` and
   `CARGO_TEST_CMDS` carry **hardcoded array lengths**. Every new crate
   adds a row and changes the length, so every pair of agents conflicts
   on both. Deriving the lengths removes the count line; it does not
   remove the row, so the real fix is **self-registration** — a crate
   declares itself and the tables are built from what is present.
2. `Cargo.toml`'s `exclude` is **one line** holding every target crate.
   Reformat it one entry per line.
3. `recipes/src/recipes/system-x86-64.rs` is over 7000 lines and everything
   touches it. Do not touch it during development: each component gets
   its own `td-<name>-test.rs` recipe and is tested without the image,
   and image wiring is one small serialized landing per component at the
   end.

### V.1 The four agents

| agent | branch | owns exclusively | never touches |
|---|---|---|---|
| **A** | `td-jail-rolling` | `recipes/src/recipes/linux-x86-64.rs`; `td-jail/**` and its recipes | td-compositor, td-audio |
| **B** | `td-busd-rolling`, and `ui-rolling` for compositor work | `td-busd/**` and its recipes; the portal personality and the Wayland protocol gap inside `td-compositor/**` | td-jail, td-audio |
| **C** | `td-audio-rolling`, `td-identity-rolling` | `td-audio/**` and its recipes; the uid-allocation registry; the `td-login exec-as` subcommand (landed) | td-compositor, td-jail, td-busd |
| **D** | `td-pkg-rolling` | every seed recipe, one file per application; the package format and spec compiler | everything above |

A's kernel commit is the only true blocking edge, and it is small.

### V.2 The shared files

| file | rule |
|---|---|
| `builder/src/affected.rs`, `Cargo.toml` | **serialize** until V.0 lands — they are the same landing anyway |
| `recipes/src/recipes/system-x86-64.rs` | do not touch during development; one serialized wiring landing per component at the end |
| the roster in `UNSAFE.md` | one at a time, in roster order (#9 A, #10 B, #11 C), each appending its own section. The roster entry is the conflict; the body is not |
| `recipes/src/ladder.rs` | append only |
| `recipes/src/source_pins.rs` | D only, per seed. Alphabetical keys *spread* insertions rather than preventing collisions — mitigation, not a mechanism |
| this document | **each agent amends its own section**, and only from rung 0 onward. While it is being written it is single-owned, which is what lets one increment change a conclusion and its premise together |

"Serialize" is a real cost rather than a solution: it is a process
instruction where the other rows get a mechanism, and it means three
agents wait. That is why V.0 is a prerequisite and not a nice-to-have.

### V.3 The compositor is already contended

`td-compositor` has a live `ui-rolling` workstream, and §E's portal
personality and §F's protocol gap both land inside it. B therefore
inherits a queue rather than a clear file. Host mode (§X) relieves this
for A and D — their work no longer waits on the portal — but it does so
by adding the forwarding backend to `td-busd/**`, which is B's. The
relief is real and it is bought from the bottleneck.

### V.4 What a workstream owes before it lands

Each of the four owes, on its own branch: a `td-<name>-test.rs` recipe
that exercises the component without the image; its `UNSAFE.md` section
with a value-pinned roster and confinement tests, where it has a surface;
the ladder markers it claims; and the three code reviews `AGENTS.md`
requires. Image wiring is separate and last.

## W. Updates

**The update model is source-distribution shaped**, and it is the one
td's architecture was already built for:

```
td update     git pull in the td checkout — recipes, pins, versions
td upgrade    run the machinery: build a new system image, and/or
              build the newest version of an application into it
```

**`td upgrade` requires elevation (§L.1); `td update` does not.** Pulling
a checkout changes what the machine *could* build and nothing about what
it runs, so it needs no authority. Building and deploying an image is the
archetypal machine-changing act, and on a multi-user machine it is
exactly the operation not every account may perform — so it is an
enumerated `td-authd` operation, `deploy-publish`, with the built
deployment pinned as a descriptor rather than a path.

This is Gentoo's `--sync`/`-u world` and Nix's channel/rebuild, not
Debian's fetch-a-package. It fits because **td ships no package format at
all** — the recipe graph *is* the distribution.

### W.1 What must be decided now, because it is expensive to change

**1. Packages are plain directories, not one image per package.** The
alternative — a verified EROFS per package, loop-mounted read-only the
way `root.erofs` already is — reuses a mechanism td has, but an image is
opaque: no file-level sharing between versions, and every update ships
the whole package. Directories keep sharing available, which is most of
what OSTree's efficiency actually was.

**2. Cut packages at stability boundaries.** The unit of transfer is the
package, so what is packaged together is re-shipped together. The
runtime/application split is the first cut and the most valuable; the
second is separating **large, rarely-changing data** — locales, ICU
tables, fonts, timezone data — from code that changes monthly. Shipping
an 800 MB runtime because a 200 MB application had a point release is the
failure this prevents, and re-cutting packages later means redoing every
recipe.

**3. The update unit is the package, never the image** — as a *design*
rule, even though today's delivery violates it (below). Nothing in the
launch path may assume that updating an application is a system
deployment.

### W.2 System updates today, and applications inside them

**What exists, and it is the hard half.** A deployment is a bundle —
`{bzImage, initramfs.cpio, root.erofs, manifest}` — whose id is
`sha256(manifest)`, with the manifest carrying a SHA-256 per payload and
signed host-side by `td-deploy` into a *detached* `manifest.sig`
(detached so a bundle can be re-signed under a rotated key without
becoming a different deployment, which would break rollback). On the
machine, `td-boot publish` verifies and materializes it into
`td/deployments/<id>/`; `td/boot/current` and `td/boot/previous` are
symlinks naming two of them. At boot, `td-boot` prefers `current`, falls
back to `previous`, and requires a valid ed25519 signature under a trust
root read from the rootfs it is running in — hashes prove integrity,
never authorship. A boot-attempt counter is decremented before trying,
`td-boot success` confirms a deployment that came up, and exhaustion
falls back. `/var` is a Btrfs subvolume minted once per machine by
`td-firstboot`, so it survives every deployment.

**What does not exist is the delivery.** Nothing in the tree moves a
bundle from the builder to the machine: `td-boot publish` takes a
directory that is already on the persistent volume. So today an update is
*build on the host, copy the bundle across by whatever means, publish,
reboot* — which is also, since §B.1, how applications arrive.

**The consequence, stated as a table because it is the specification of
the work that is deferred:**

| | system update | application update — **today** | application update — **the goal** |
|---|---|---|---|
| unit | a deployment bundle | *the same bundle* | one package |
| carries | kernel, initramfs, `root.erofs` | everything, including every other application | one application, or one runtime |
| applied by | `td-boot publish` + reboot | `td-boot publish` + **reboot** | a pointer flip, no reboot |
| verified by | signed manifest, ed25519 | the same signed manifest | a content hash against the recipe graph |
| rollback | `previous` selector, attempt counter | `previous` — so an application rollback is a **system** rollback, and can undo an unrelated security fix | repoint; state in `$HOME` untouched |
| covered by the attempt counter | yes | **not application execution** — `bootsuccess` requires the terminal but not the confined fixture. The fixture has a separate readiness-gated evidence unit for QEMU, so broken application code or mutable per-app state cannot drive the counter down. §A's immutable launcher table is emitted but not yet consumed; the current fixture card is compiled into the compositor | n/a |
| frequency | td's cadence | td's cadence | upstream's, which for a browser is far higher |

A browser is the fastest-moving thing on the machine and it now moves at
the slowest cadence on it. That is the cost of having no other delivery,
taken knowingly.

**What the deferred column needs** is a writable package root and a
pointer — every hard part (content hashes from the recipe graph,
generations, `rename`-into-place, extraction) is specified here and
exercised daily by host mode. What it does *not* have is the **authority**
to write a system location on a running machine, which is `td-authd`'s
(§L.1 says so too, now that both sections agree) and is the honest gap:
"no new trust machinery" would be too strong.

**And that writable root has to appear AT `/td/store`**, which bounds
what the mechanism can be. A package's `PT_INTERP`, `RPATH` and the
spec's runtime path are absolute store paths, so a tree unpacked
somewhere else — `/var`, a home directory, anywhere — is a tree whose
binaries look for libraries that are not where they are. The deferred
tier is therefore a writable store presented at that path (an overlay,
or a writable mount the image's read-only one is a lower layer of), and
never a second location with a second name. Recording it because
"packages move to `/var` where Btrfs can reflink them" is the natural
next thought after §W.3 and it does not work.

**Retention differs between the tiers, deliberately.** The root image
retains two and boots one — a machine cannot half-run two kernels, so a
deployment is atomic and `previous` exists for rollback rather than for
choice. Applications, once they have a tier of their own, keep several
versions with a pointer selecting one. **The system runs one thing at a
time; applications run many.**

### W.3 Deduplication, and what it does not solve

If packages are built locally rather than downloaded, the delta
machinery a repository needs is unnecessary. That is right, and the
reason is worth getting right too, because a draft said "the filesystem
deduplicates what they share" and Btrfs does no such thing on its own:
nothing merges the extents of a freshly compiled file, and inline
deduplication is not a feature Btrfs has. **Content addressing is what
deduplicates** — an unchanged recipe output has the same hash, so the
store already holds it and it is not rebuilt or re-stored at all, which
is one copy rather than two merged after the fact. `FICLONE` matters
where the builder COPIES a store path into a tree, which it must ask for
explicitly and which `/var` being Btrfs makes cheap; it is stronger than
a hardlink there, since reflinked files stay independent for writes.

Two things it does not merge. Deduplicating *storage* does nothing for
*build time*, which is the cost a source-built runtime actually has. And
it does nothing for the **reboot**, which is a property of where the
package lives rather than of how many copies of it exist.

## X. Host mode — development only

**The application layer runs on an ordinary Linux host, and that mode is
a development tool rather than a product.** It exists so this work can be
built and tested without booting td. It is not a flatpak competitor, is
not supported for anyone else's machine, and no feature owes it a story.

> **The two-configuration rule.** td is the product. Host mode is a test
> fixture that happens to be useful. A feature works on td; it works on a
> host if that is free or nearly so; and where it does not, host mode
> **degrades with a named diagnostic and the feature is not held back**.
> Where the two would diverge in *behaviour* rather than in availability,
> host mode is the one that gives way.

### X.1 What it is

`td-builder` builds an application with the ordinary recipe machinery and
materializes it into a prefix on the host — the same content-addressed
output, written somewhere a host can reach. `td-jail` then launches from
that prefix exactly as it launches from the store.

**The extraction is `td-builder`'s, never `td-jail`'s.** The control
plane already has `read_nar`; putting an archive parser and a filesystem
writer inside the crate that carries surface #9 would be precisely the
bookkeeping §A.0 keeps out of it.

**The two configurations differ in where a package sits**, which the rule
above says must not happen casually. It is a divergence in *availability*
— a host has no image, and no feature is bent to pretend otherwise. The
package root joins the state root (§B.4) as configuration and feeds the
same manifest/spec resolver and mount transition, so `td-jail` binds
`<pkgroot>/<name>/files` at `/app` and neither the jail nor the application
can tell which physical package root supplied it. Explicit host-mode
branches are confined to availability boundaries the product owns but a
foreign host does not: caller identity mapping, session-socket discovery,
aggregate cgroup enforcement and their diagnostics. A second application
layout or a weakened confinement transition would be behavioural
divergence and is still refused.

The landed invocation is:

```text
td-jail --host HOST-CONFIG APPLICATION [ARG...]
```

`--host` is recognized only in that position under the exact `td-jail`
argv[0] and is never inferred. It is refused whenever the compiled product
configuration is installed, so the development interface is absent from a
booted td image even though the same static artifact is exercised by the
host recipe test. The host configuration is an exact, ordered keyfile:

```text
format=1
package-root=/absolute/materialized/packages
state-root=/absolute/caller/home/.td/app
registry=/absolute/td-applications.tsv
launcher-table=/absolute/td-launcher.tsv
cgroup-root=none
```

The builder or test harness creates that tree; `td-jail` never extracts
it. Registry entries retain their logical `/td/store/<object>` identity,
while host resolution takes that object's basename directly beneath
`package-root` and refuses symlinks or canonical escapes. The state home,
the `0700` `XDG_RUNTIME_DIR`, the selected `$WAYLAND_DISPLAY` socket, and
the local td-busd socket at `$XDG_RUNTIME_DIR/bus` must be direct paths
owned by the invoking uid. Inside the jail that caller is mapped to the
product identity uid/gid 1000, so `/app`, `/usr`, `/home/td`, and
`/run/user/1000` do not change between configurations.

### X.2 What is missing on a host, and what answers it

None of td's session components exist on a foreign host, and the answer
in every case is flatpak's: **bind the host's socket into the jail.**

| td component | on a host |
|---|---|
| `td-compositor` | absent. Bind the host's `$WAYLAND_DISPLAY` socket. §F's protocol gap is irrelevant here — but see the grant below |
| `td-audio` | absent. Bind the host's **PulseAudio-protocol** socket — `pipewire-pulse`'s where the host runs PipeWire, which is what §K.2's layout and `PULSE_SERVER` already target. Not `pipewire-0`: §K.1 keeps those apart and they are different protocols |
| `td-portal` | **cannot run** — it is a personality of `td-compositor` and speaks a private Wayland protocol to it. `td-busd` **forwards** to the host session bus, where `xdg-desktop-portal` answers |
| `td-seatd` | absent and unneeded; the host owns its own devices |
| `td-authd` | absent, and nothing replaces it. There is no elevated operation here, so host mode has no privileged path at all — and it must not grow one by reaching for the host's `sudo`, which would be a password prompt from td's own code (principle 7) and a shell (directive 3) |

Rung 12a supplies the local td-busd endpoint needed by the existing
registration handshake, but it does not yet make that endpoint a usable
application bus for a caller mapped to uid 1000: the live broker transport
still treats the outside `SO_PEERCRED` uid as an unmapped EXTERNAL claim.
Mapped downstream authentication and the upstream host-bus proxy described
below both remain §D work. Applications that need the bus therefore remain
blocked there rather than bypassing td-busd or binding the host bus directly.

**Forwarding is the largest piece of work in this section, not the
smallest.** §D makes `td-busd` a *bus daemon* — it owns the socket,
assigns unique names on `Hello`, runs the `RequestName` owner queue, and
rejects a client-supplied `SENDER` so it can insert the authenticated
one. A bus daemon has no upstream. Forwarding makes it a proxy as well: a
second `EXTERNAL` authentication as somebody else's client, two disjoint
unique-name spaces to translate, serial and `REPLY_SERIAL` remapping,
`SENDER` rewriting inbound, `NEGOTIATE_UNIX_FD` mirrored, and no honest
answer for `GetConnectionUnixUser` about a host peer. That is
`xdg-dbus-proxy`, the process §D absorbs by not being one.

Four consequences, none an argument for building that proxy — they are
why host mode is not a confinement claim:

- **Attribution is lost.** `xdg-desktop-portal` identifies a caller from
  `SO_PEERCRED` and that pid's `/proc/<pid>/root/.flatpak-info`. A broker
  multiplexing every application onto one upstream connection presents
  one peer. It does not reject the calls — finding no sandbox metadata it
  treats them as an **unsandboxed host application** and applies no
  per-app permission at all, which is the permissive direction and the
  reason it is written down. **`td-busd`'s own filter is the entire
  policy in host mode.**
- **§D's default policy is safe only because `td-portal` is small.** That
  policy permits any member of `org.freedesktop.portal.*`, which bounds
  nothing once a real `xdg-desktop-portal` is behind it: `ScreenCast`,
  `RemoteDesktop`, `Camera`, `Secret` — every one marked *not exported*
  by §E, and one of them leaned on by name in §L.1's threat table. **Host
  mode must replace the namespace rule with an explicit member list.**
- **`Request`/`Session` object paths and their signals must route back.**
  The portal API's asynchrony is its whole shape: a call returns a handle
  and the answer arrives later as a signal on that path. A broker that
  forwards calls without tracking handles gives every application a
  dialog that never answers. Worse than a lost reply: a `Request` path is
  built as `…/request/<SENDER>/<TOKEN>`, and multiplexing every
  application onto ONE upstream connection makes `<SENDER>` the same for
  all of them — so two applications that pick the same token (a
  caller-chosen string, and "0" is a popular one) collide on a single
  object path, and one receives the other's answer. Serial and `SENDER`
  rewriting does not reach it, because the collision is in a path
  embedded in a body rather than in a header field. The fix is one
  upstream connection per downstream client, which is also what makes
  the attribution bullet above less bad — and it is a further argument
  that this proxy is the section's largest piece of work.
- **The host's audio and Wayland daemons see the app DIRECTLY, and
  `/.flatpak-info` tells them a story they cannot check.** Unlike the
  bus, those sockets are bound straight into the jail, so the peer the
  host sees is the application itself — and mount step 13 has written a
  file that identifies it as a Flatpak with an id the host has never
  heard of. PipeWire's access module reads exactly that file, classifies
  the client as sandboxed, and looks for a permission the host's store
  does not have. So audio can fail on a host for a reason that has
  nothing to do with audio, and the diagnostic will be about
  permissions. Host mode therefore owes a switch: either omit that file
  when the host would act on it, or accept that host-mode audio needs
  the host's own permission entry. Named here rather than solved because
  it is a development-mode question and §X's whole point is that these
  are allowed to be answered cheaply.
- **A FileChooser returns document-portal paths** under
  `/run/user/<uid>/doc/`, a FUSE mount that must be bound into the jail
  or the chooser succeeds and the application cannot open what was
  picked.

**Application-supplied bus policy is refused outright in host mode.** §D
admits `[Session Bus Policy]` entries from the permission file, which on
td widens onto a bus with almost nothing on it; on a host the same
entries reach `org.freedesktop.systemd1`, `org.freedesktop.Flatpak` and
`org.freedesktop.secrets`. That is divergence in **what a grant means**,
which the two-configuration rule does not cover, so the policy is td's
alone here.

### X.3 What host mode buys

It **decouples §A–§C from §E–§F**: packages, identity, the permission
model and `td-jail` can be built and exercised against a host session
while the portal and Wayland-gap work is still in flight. Under §I first
foreign pixels wait on rungs 16–22; under host mode a window is reachable
at **rung 12a** — not rung 8, since rung 8 is a jail skeleton and §X.4's
probe-and-refuse rule means nothing runs before the filter exists at 11.

**§D is not on the near side of that line.** Anything portal-mediated
still needs the broker, and the forwarding backend lands in `td-busd/**`
— agent B's exclusive file, and the crate §V.3 names as contended. Host
mode buys A and D their parallelism by adding work to the bottleneck.

### X.4 Limits

- **Seeds only, and the reason is the mount table rather than the
  extraction.** Unpacking a tree elsewhere does not relocate it:
  `PT_INTERP`, `RPATH` and shebangs are still absolute afterwards. What
  makes a seed work is that the jail **presents it at fixed paths** —
  `/usr` for the runtime, `/app` for the application — and a seed is
  already built for exactly those, because they are flatpak's. A
  source-built td package is prefixed into `/td/store`, which the mount
  table deliberately does not contain (§H asserts its absence inside a
  jail), so consuming one needs a jail-layout decision nobody has made —
  **on td as much as on a host**. The fix, when it is wanted, is a
  *constructed* `/td/store` inside the jail holding exactly that
  package's own closure, which keeps §B.3's structural-equivalence
  invariant intact.
- **Host kernels vary and are probed, never assumed.** User namespaces
  have been restricted or disabled on some distributions, and the seccomp
  numbers are per-architecture. `td-jail` probes and **refuses with a
  named diagnostic**; it never silently runs an application less confined
  than asked. That is §C's fail-closed discipline applied to a kernel td
  did not build.
- **A host socket is a bigger grant than the same socket on td, and the
  launcher must say so.** `sockets=wayland` is the specific place
  flatpak's confinement is weakest: it hands over every global that
  compositor advertises — `wlr-screencopy`, `zwlr_data_control`,
  `zwp_virtual_keyboard_v1`, `zwlr_export_dmabuf`, layer-shell — several
  of which td withholds deliberately (§M), and nothing short of proxying
  Wayland filters them. `sockets=pulseaudio` is coarse the same way: the
  protocol carries recording as well as playback, and its monitor sources
  are capture of the desktop's own audio. So the rule is operational:
  **`td-jail` enumerates, per launch, the restrictions it could not
  enforce on this host, and prints them.** Host mode may degrade; it may
  not degrade silently.
- **§Z still binds, and the line is not "no NAR ever leaves a machine".**
  Moving a file you built to a machine you own is distribution, not
  infrastructure — §W.2 already does exactly that with deployment
  bundles. A cache is an index, a URL and something serving them, and
  what principle 5 forbids is td requiring its maintainer to run one.
- **No claim of confinement parity.** A host's LSM, cgroup layout and
  `/dev` are not td's. Host mode is where the code is exercised, not
  where the security claim is proved; §H's proof runs on td.

### X.5 An interface, a rung and a test — or it becomes permanent

§I warns that a thing with no rung, no interface and no acceptance test
is how a required thing quietly becomes a permanent one. So host mode
gets all three:

- **`--host`, explicit and never inferred.** Not auto-detection, which is
  precisely how a fixture becomes a supported configuration. Absent the
  flag, `td-jail` targets td and fails on a host with a named diagnostic.
- **Rung 12a**, immediately after the in-image fixture launch —
  deliberately after, since a host that worked first would invert the
  two-configuration rule at the only moment it matters.
- **The landed acceptance test** runs the ordinary rung-12 fixture
  identity, manifest and binary under `--host` from a materialized host
  prefix. Its declaration, permission defaults and launcher come directly
  from the shipped fixture recipe rather than a copied policy. The host leg
  first launches those exact isolated defaults, then adds only
  `shared=network` and launches again to prove that policy preserves the
  caller's network namespace. The exact shipped spec remains the
  isolated-network QEMU oracle. It uses the real td-busd registration path
  (not an application-originated bus handshake) and caller-owned
  session-socket endpoints, and asserts the exact two-line degradation
  report: aggregate memory/task/CPU caps are unavailable without a delegated
  cgroup, and direct host Wayland cannot filter globals. User namespaces and
  the standard seccomp filter remain fatal prerequisites rather than
  degradation entries. A test that asserted only "it ran" would pass on a
  host enforcing nothing.

### X.6 A prerequisite in the control plane, since fixed

`read_nar` had an **arbitrary-file-write**, found while writing this
section and **fixed on main before it landed** (`nar: a restored entry
never lands on a path that already exists`). It is recorded rather than
deleted because host mode is what made it load-bearing, and because the
shape recurs: a reader that validates a NAME and then trusts the PATH.

It validated entry names — rejecting empty, `.`, `..`, and any name
containing `/` — but never required them unique or increasing, and
created regular files with `fs::File::create`, which follows a symlink
already at the path. A NAR carrying two entries of one name, a symlink
first and a regular file second, wrote the second through the first to
wherever it pointed and returned `Ok(())`, with nothing under the
destination to show for it. The symlink target was unvalidated, so the
write was fully attacker-chosen rather than confined below the
destination; and the `NarHash` check runs *after* `read_nar` returns, so
it detects a bad archive and does not prevent one.

Both fixes landed, and they close it at different levels: entry names
must be strictly increasing (which also restores canonicality — an
out-of-order archive re-serializes to different bytes than it arrived
as, so it could not match the hash that admitted it), and the create is
`O_CREAT|O_EXCL|O_NOFOLLOW`, which does not care how a path came to
exist and so covers one planted between the check and the create.

What remains true is the reason it mattered: the live caller is the
`nar-restore` CLI, and what it could corrupt is a developer's own
prefix. A development tool that can be made to write outside its prefix
compromises the machine building the distribution.

Two properties of NAR are worth stating while it is in view. It encodes
**one permission bit** (`mode & 0o100`) and no ownership, mtime, hardlink
or xattr, so extraction produces 0644/0755 — half a security property, in
that a setuid bit and a device node are *unrepresentable* rather than
merely rejected, and half a limitation. And `nar.rs`'s three `unsafe`
blocks are all `OsStr::from_encoded_bytes_unchecked`, for which Unix has
the safe stable `OsStrExt::from_bytes`; converting them would let the
file drop its `#![allow(unsafe_code)]`, which is a reduction in td's
unsafe surface available for free.

## Z. No server infrastructure

A constraint rather than a design, stated because several decisions above
turn on it and one earlier draft violated it without noticing.

**td must not require its maintainer to run any service.** Not a package
repository, not a binary cache, not an update server, not a mirror.

What this permits, and it is enough:

| need | how it is met | who runs it |
|---|---|---|
| recipes, pins, versions | a git checkout of `td`, pulled (§W) | a git forge — GitHub and sr.ht already mirror `origin` |
| sources and seeds | pinned URL + `sha256` at **upstream's own** location | upstream |
| local download reuse | `td-feed`, a shared host-side cache across worktrees | nobody — it is a dev-host daemon, not a public service |
| built artifacts | built locally from the recipe graph | nobody |

`td-subst` is the exception that proves it: a signed binary cache exists
in the tree, and **nothing populates it** —
`builder/src/check_loop.rs` records that its publisher retired. That is
not an oversight to correct. A binary cache is precisely the piece that
would demand hosting, and td has done without it by building locally.

Three consequences that are easy to violate by accident:

1. **Never pin an artifact td produced.** A pin is a URL plus a hash, so
   pinning a locally-assembled tree means publishing it somewhere. This
   is exactly what §B.3's earlier draft did by archiving a flatpak deploy
   tree, and it is why that step was rewritten.
2. **Prefer upstreams that publish plain files.** Where a project ships only
   through a repository protocol of its own — a Flatpak runtime through
   OSTree, say — the choices are a distro's plain package files, building from
   source, or §B.3.1's bounded control-plane import of exact upstream-hosted
   objects. Never host a repackaged copy.
3. **Delta and chunking machinery is not needed** and should not be
   built speculatively. It optimizes repeated pulls from a server that
   does not exist.

The scale at which this stops being sufficient is worth naming so the
decision is revisited deliberately rather than suffered: **a second
machine that does not build its own software.** At that point either it
becomes a build host too, or something must serve it artifacts, and §Z is
what has to be reopened rather than worked around.

---
