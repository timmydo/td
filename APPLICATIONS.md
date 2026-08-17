# Applications on td

**Status: proposal.** Nothing here is built. This is the normative design
for running third-party graphical applications (Firefox, darktable, GNOME
apps) inside td's own Wayland compositor: the package format, identity,
confinement, permission model and state contract, and the components that
implement them — `td-jail`, `td-busd`, `td-portal` and `td-audio`.

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

That removes the network stack, the signature trust root and the
repository format from this design entirely: there is no OSTree client,
no GVariant reader, no OpenPGP verification, no HTTP and no TLS on the
target. The signature that matters is checked once by a human when the
pin is reviewed — the same way td already trusts the kernel tarball and
the Rust bootstrap snapshot.

The cost is that the curated set is td's to maintain: a seed bump is a
pin review plus a rebuild, and no application td has not packaged can be
installed at all.

## Premises

Settled by the maintainer; these are premises, not conclusions.

1. **Packages ship in `/td/store` with the image**; only writable state
   lives in `~/.td/app`. There is no install step and no privileged
   installer — choosing which shipped application to run needs no
   authority beyond a user's own launcher table.
2. **`td-jail` carries seccomp from the first landing**, as a new
   `UNSAFE.md` surface — not namespaces now and a filter later.
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
# IPC_NS deferred: needs SYSVIPC, which nothing else on the image wants
# FUSE_FS deferred: lands with the Documents portal, not before
```

An earlier draft deferred `CGROUPS` on the grounds that "rlimits cover
the first need". They do not, and §P explains why: rlimits are
per-process and inherited, so a browser's content processes multiply the
cap rather than share it. Since the kernel landing is one reviewed
commit either way, the cgroup symbols belong in it.

The boot oracle asserts, as uid 1000, that `unshare(CLONE_NEWUSER|
CLONE_NEWNS)` succeeds and that a trivial allow-all filter installs — so
a kernel regression reds the image rather than the first app. Failure
disables application launch with a named diagnostic; it never silently
selects a weaker sandbox.

**LANDED, in two halves, and the split is worth reading before relying on
it.** The pins above (minus audio, which waits for td-audio) are in
`linux-x86-64.rs`, each guarded against the RESOLVED `.config` rather
than against the pin list; and the greeter carries a kernel-capability
farm that prints `TD-SANDBOX-KERNEL-OK` once the RUNNING kernel has been
observed to have every one that can be witnessed from `/proc` — all of
them but `MEMCG`, for the reason below. What did NOT land is the wording
above taken literally: the oracle READS `/proc` and does not ISSUE
`unshare(2)` or `seccomp(2)`. Those two calls are surface #9's, which
arrives with `td-jail`, and nothing on the image today may issue them —
so a prober written for this rung would have meant an `unsafe` surface
added outside the crate that owns it, which §V.4 and `UNSAFE.md` both
forbid. The functional assertions therefore land with the rungs that
own the calls: the `unshare` at rung 8, where `td-jail`'s skeleton and
surface #9 arrive, and the filter install at rung 11, which is where a
filter exists to install at all.

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

**`MEMCG` has no runtime witness at this rung, and the reason is a trap
worth carrying into §P.** `/proc/cgroups` is the obvious place to look
for a controller, and it does not list `memory` on this kernel even
though `CONFIG_MEMCG=y`: `proc_cgroupstats_show` skips any subsystem
where `cgroup1_subsys_absent()` holds — no v1 interface but a v2 one —
and memcg registers its `legacy_cftypes` under `#ifdef CONFIG_MEMCG_V1`,
which resolves to `n` here. The kernel says as much itself, once, on the
console: *"/proc/cgroups lists only v1 controllers, use cgroup.controllers
of root cgroup for v2 info"*. `pids` is listed because it registers v1
files unconditionally, so the greeter asserts that one — anchored on the
`enabled` column, since `cgroup_disable=pids` leaves the row and clears
it, which is the one failure no config guard can see.

The consequence for §P is that **`cgroup.controllers` on a mounted
cgroup2 is the only interface that answers "is the memory controller
available"**, so td-svc's delegation landing owns that assertion. A first
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
there is no install, no uid allocation in v1, and no launch plan to
generate at run time — the recipe emits the spec at build time — so the
work that would have justified one is either gone or happens earlier.

### A.0 The launcher

**`/bin/firefox` is a symlink to `td-jail`, which reads its own `argv[0]`
and looks the name up in a build-time table in the store.** `td-init`,
`td-util`, `td-login` and `td-sh` are all multicalls keyed on `argv[0]`;
this is a fifth.

**`argv[0]` selects, it does not authenticate.** A caller controls
`argv[0]`, so the only thing it can do is name a *different shipped
application* — which that caller could have launched directly anyway. The
security property comes from the table being in the read-only store and
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

**The `/bin/<name>` symlinks are a seventh applet farm and must be
registered as one.** `bin_farms()` enumerates six, and
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
re-exec is internal, reached by a nonce stage 1 generates and a
descriptor it passes, and is not a documented entry point. No
per-instance copy of the spec is written and then re-read — that would
put a writable file back on the path this rule exists to keep immutable.

That spec names the runtime by its **full store path**, not by short
name, because td discovers closure edges by scanning output bytes for
store hashes (§B.8): a short name leaves the application→runtime edge
invisible to the closure query and to any future collector.

**Registration is the crate's own obligation, not a separate program's,
and it is split across the two stages the way §D describes.** Stage 0
opens phase one — `{instance, app-id, permitted service names}`, for an
opaque one-shot token — before it unshares anything, because the pid the
record needs does not exist until after; stage 1 completes it with the
stage-2 pid it gets back from `Command::spawn`. §E rests `Unconfined` —
which grants full portal access — on the registry being complete by
construction, because nothing entered a jail without a registrar running
first. The registrar and the jail are now one binary and `/bin/td-jail`
is a documented entry point, so that premise has to be restated as an
invariant of the crate itself: **stage 1 refuses to proceed without the
token stage 0 obtained**, and entering stage 1 without it is a refusal
rather than an unregistered jail. (An earlier draft called registration
"stage 1's obligation" flatly while §D and §E named stage 0, which are
the two halves of one protocol described as though they were rival
answers.) The
exposure if it were not is bounded — an unregistered launcher is a
uid-1000 process, which is `Unconfined` anyway — but the argument that
makes `Unconfined` a positive result rather than a default is not.

**Not a shell script.** Directive 3 keeps shell out of these crates,
`/bin/sh` is td-sh, and a launcher composes the argv of the process the
whole sandbox is about — a quoting bug there is a confinement bug. The
symlink form is less code than a script, not more.

**What it launches is generated at build time.** The recipe that produces
a package emits its jail spec — mounts, permission defaults, the runtime
it names, the entry point — into the store beside the payload. The spec
`td-jail` parses is therefore trusted, immutable input, which is what
keeps the confinement crate small: what moved in is a resolver over a
file the store guarantees.

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
└─ /bin/firefox -> td-jail                 argv[0] resolves the store
   │                                       spec (§A.0); writes the
   │                                       instance dir + /.flatpak-info,
   │                                       registers with td-busd, re-execs
   └─ td-jail [stage 1]                    single-threaded;
      │                                    unshare(NEWUSER|NEWNS|NEWPID|
      │                                    NEWUTS[|NEWIPC][|NEWNET]);
      │                                    setgroups=deny, then uid_map
      │                                    and gid_map "1000 1000 1"
      └─ td-jail [stage 2] <nonce>         PID 1 of the new pid ns.
         │                                 mount plan, pivot_root, drop
         │                                 caps, NO_NEW_PRIVS, seccomp,
         │                                 readback, then spawn and reap
         └─ firefox                        PID 2; interpreter is the
            └─ content processes           RUNTIME's ld.so
```

**The two-stage re-exec is the answer to `pre_exec`.** A target crate has
one scoped `unsafe` allow, in `sys.rs`, and `CommandExt::pre_exec` is
itself an `unsafe fn` — calling it from the module that spawns would need
a *second* allow, which every `UNSAFE.md` confinement test counts and
refuses. So the process boundary is the fork instead: stage 1 unshares in
its own still-single-threaded `main` (`CLONE_NEWUSER` requires a
single-threaded process, so stage 1 spawns no thread before it), and the
first child it spawns through safe `Command` lands as PID 1 of the new
pid namespace with the mount namespace inherited. Stage 2 then does
everything a bwrap child does as ordinary safe code.

**Stage 2 does not exec the app over itself.** A PID 1 that is Firefox
reaps only its own children, and orphaned grandchildren pile up as
zombies — so stage 2 stays resident as an init: spawn, `wait4(-1)` until
the app exits, propagate the status, terminate survivors. `wait4(2)` is
on surface #9 for it.

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
blocks. The descriptor is inherited across the ordinary `Command` spawn.

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
launcher table. It is constrained like a `td-login` account name — a
bounded character set, a length limit, no leading dash, no path separator
— and validated once, at build.

**Foreign applications still announce reverse DNS on wires td does not
own**, so the manifest records those strings as **aliases**, used only
where a foreign protocol demands one: matching a toplevel to its launcher
entry, scoping bus-name policy, resolving a `.desktop` reference. The
alias never becomes the identity, and an application with no alias is
ordinary rather than special.

`/.flatpak-info` and `FLATPAK_ID` keep their upstream spelling for one
reason: **the application reads them.** GIO decides whether to route
through portals by looking for that exact path, so a repackaged binary
that does not find it will try to open files directly and fail
confusingly. It is a compatibility surface td writes, not a name td uses.

### Launcher integration

The build emits a bounded `td-launcher.tsv` of
`name<TAB>display-name<TAB>search-terms`, merged from every shipped
package's `exports/` and read by the compositor's launcher. Activation is
always the literal argv `/bin/<name>`.

Launcher names are checked at build against reserved names and against
the `/bin` applet farm — `applet_farms_are_disjoint_and_boot_names_stay_static`
already refuses a second provider of a name, and a foreign package must
not be able to claim `sh`.

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
- the boot-attempt counter does **not** cover applications — boot success
  is recorded when the system comes up, before any application runs — so
  a broken application is not automatically rolled back;
- every account on the machine gets the same set of applications, and
  two accounts can no longer disagree about a browser version;
- the image carries every packaged application and runtime.

**Three size constants in the tree say this does not fit yet, and each
is a landing this decision owes**:

| constant | today | why it blocks |
|---|---|---|
| `td-boot/src/protocol.rs`'s `MIN_VOLUME_BYTES` | 2 GiB | its own comment is *"two deployments plus the attempt bookkeeping, with room for the update that installs the third before retiring the first"* — so a deployment is retained **three times transiently**, not twice, and `td-install` enforces the constant. A Firefox-plus-runtime deployment will not fit at 3×, and the failure surfaces mid-publish |
| the QEMU oracle's `PERSISTENT_VOLUME_BYTES` / headroom | 1 GiB / 256 MiB, `copies = 3` | caps a whole deployment at roughly 256 MiB for the boot oracle, which hard-errors above it. §H's fixture fits; a real GTK application is doubtful and Firefox is impossible — and §H's proof *is* an oracle run, so this blocks the design's own acceptance criterion |
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
  spec                the jail spec, recipe-generated (§A.0)
  files/              becomes /app inside the jail
  exports/            launcher entry, icons, mime associations
```

**`manifest` and `spec` are two files with two jobs**, and keeping them
apart matters because they have different readers. The manifest is the
*declaration* — what this package is, written for a human and for the
recipe checks. The spec is the *compiled jail plan* derived from it at
build time — mounts, grants, the entry point, and the runtime's full
store path — and is the only one `td-jail` parses.

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

Permissions are a **separate file**, deliberately not part of the
manifest, because they have a different lifecycle: the manifest is
content and changes only when the package does, while a permission grant
is a decision an operator revisits without rebuilding anything.

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
2. **A recipe transforms it** into a package: unpack, lay out `files/`,
   write the manifest, emit the spec. Deterministic, and living in the
   recipe, so the artifact is a build output rather than a thing needing
   distribution.
3. **The recipe's checks are ordinary recipe checks**: the entry point
   exists and is executable, the declared runtime resolves, the metadata
   normalization of §B.8 holds, and the interpreter assertions there are
   made.

**Which upstream artifact, per application, is a real question**, and it
is E1b's (§V.0) rather than this document's to answer:

- **Applications are usually easy.** Mozilla publishes official Firefox
  binaries as plain tarballs at stable release URLs — upstream's own
  artifact, needing no flatpak on the dev host. Most applications that
  ship Linux binaries at all ship them this way.
- **The runtime is the hard half.** A flatpak runtime such as Freedesktop
  SDK is distributed *only* through OSTree, so it cannot be pinned as a
  file. Three alternatives, none requiring hosting: pin a set of **distro
  packages** (Debian `.deb` files are plain files at stable URLs with
  published hashes, and collectively provide a complete glib/GTK/ICU
  stack); **build the runtime from source**; or write a
  **control-plane-only OSTree or OCI importer**.

  The third is refused on **cost, not principle** — §Z forbids td
  *serving* bytes, and a control-plane fetcher pulling upstream-owned
  digest-pinned objects from upstream's own repository hosts nothing. But
  an OSTree importer is GVariant plus repo modes plus
  `.filez`/`.dirtree`/`.dirmeta` handling, and a `.deb` is `ar` plus
  `tar` plus a hash. A later reader reaching for it should have to argue
  past the price, not past a claim of impossibility.

That asymmetry relocates the project's difficulty: **the application is
not the hard part, the runtime is.**

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


### B.5 Activation and state — there is no install

There is no install step and no installer binary. The packages are in the
image; nothing is fetched, materialized or verified at install time
because there is no install time. What the earlier per-user tier needed
an installer for now happens in two other places:

| job | where it happens now |
|---|---|
| identity allocation | build time, in the recipe |
| the permission defaults | build time — part of the jail spec (§A.0) |
| the launcher table | build time, merged from every shipped package's `exports/` |
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
| **OSTree client** (repo modes, `.filez`/`.dirtree`/`.dirmeta`, GVariant) | Against a **curated dozen pins** a general repository client is a format tower serving a handful of URLs, and it is the most expensive kind of code here — a trust-path parser of attacker-controlled bytes — where a recipe is the cheapest. A control-plane importer is a different question and §B.3 prices it |
| **Summary, index, refs, static deltas** | All are incremental-update machinery for a repository td does not talk to. A pin bump plus a rebuild is td's update mechanism. |
| **OpenPGP verification** | The signature is checked by a human at pin review, as it is for every other fixed-output input. An implementation on the target would be a parser for attacker-supplied input serving a trust decision already made elsewhere. |
| **HTTP/TLS on the target** | Nothing on the target fetches. The control plane's `td-net` already does this, under the existing dependency exception. |
| **a target-side `fsck`/`verify` verb** | Refused. A package is in a read-only image admitted by a signed manifest, so a userspace hash sweep would re-check it with a weaker mechanism (§B.5). This refusal has flipped three times as the layout moved, which is the entry's real content: **a refusal argued from a layout is only as settled as the layout.** |
| **OCI / container registries** | A second foreign format with the same objection as the first, and no application td wants is distributed only that way. |

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

**The taint starts at the source pin, not at the output.** Marking only
the finished package would leave the packaging recipe free to execute the
pinned foreign binary while building it, and would let a second recipe
consume the same archive and emit an *unmarked* output. So `foreign` is a
property of the fixed-output source pin, it propagates to every output
derived from it, and an unmarked descendant of a marked source is a
planning-time refusal in the shape `seed_digests.rs` already uses for
`provenance rejected`.

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
staged into a `Step::Run` sandbox at all.** `payload_inputs` resolve only
for the typed data operations — `CopyTree`, `StageRuntimeClosure`, and
whatever spells a runtime path into a spec — which are performed by the
builder itself rather than by a program the recipe chose. A step that
runs a command simply does not have the path in its filesystem, so
"never executed, never linked against" stops being a property to check
and becomes one the sandbox cannot express. That is the same move
`AssertStatic` makes: not "did anyone link against the host libc" but
"the host libc is not in there to link against".

The argv scan is still worth having, downgraded to what it is worth: a
cheap pure pass in `classify_graph_inputs` that catches the honest
mistake — a recipe naming a payload where it meant a tool — and reports
it at planning time with a name and a line, rather than as a step that
fails inside a sandbox for no visible reason.

**The channel has LANDED (rung 3), and the paragraph above needed one
correction to become code.** "Never staged into a `Step::Run` sandbox at
all" is not implementable as written: every step of a build runs inside
ONE sandbox invocation, and `CopyTree`/`StageRuntimeClosure` are
performed by `td-builder` *in that same sandbox* — so a payload the data
operations can read is necessarily mounted while a `Step::Run` also
runs. Per-step mount manipulation would close that, and it would mean a
new `unsafe` call site in `build.rs` outside the one the control plane
records. So the enforcement is split in two, and neither half is a scan:

- **Resolution.** A payload is withheld from `TD_INPUT_MAP` at assembly
  and placed in `TD_PAYLOAD_MAP`, reached by a template token of its
  own, `{payload:NAME}`, which resolves ONLY in `CopyTree`'s `from` and
  `StageRuntimeClosure`'s `roots`. A command-bearing step has no name
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
than resolved by precedence, and so is one shared with `sourceInput`,
which reaches the build as `TD_SRC` — the same channel through another
door; a `payloadInputs` that is not an array, or holds a non-string, is
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
channel is `mesboot`-only, since no other build system has the typed
data steps that read one.

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

Two things about the shape it was going to borrow, because they are the
measure of how far a scanner falls short. `ladder.rs`'s `command_texts`
covers **four of eighteen `Step` variants** and returns nothing for the
rest — so `Step::Require { exec: true }`, `PatchShebangs`, `Unpack`,
`CopyTree` and ten others name paths it never sees. And it does not read
`Step::Run`'s `env`, which is exactly how the image recipe supplies a
search path today (`.env("PATH", &post_bootstrap_path())`), so
`PATH={in:payload}/bin` puts every bare command in the step inside the
payload with the scan clean. It is also `#[cfg(test)]`, so the precedent
is a test rather than a planning gate. A scanner that fixed all three
would still be syntax; the point of the paragraph above is that it does
not have to be the mechanism.

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

#### Metadata, exports, and what the marker does not do

The payload's metadata is normalized at build: no setuid or setgid bits,
no file capabilities, no security xattrs, no device nodes, no symlink
escaping the tree. `exports/` is not trusted either — launcher names come
from the recipe's own metadata and are checked against reserved names and
the `/bin` applet farms (§A.0), so a foreign package cannot claim `sh`.

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

One `unshare(2)` in single-threaded stage 1:

```
CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUTS [| CLONE_NEWIPC] [| CLONE_NEWNET]
```

All in one call so the pid namespace is owned by the new user namespace —
the kernel applies `NEWUSER` first, which is what grants the capability
for the rest. `NEWNET` only when metadata lacks `shared=network` (Firefox
has it, so Firefox keeps td's stack); loopback in a fresh net namespace
is **brought up** with a pinned `SIOCGIFFLAGS`/`SIOCSIFFLAGS` pair on the
name `lo` — leaving it down turns "no internet" into "no sockets" for
every app with a localhost helper. `NEWIPC` only
once `CONFIG_IPC_NS` is on (§0). uid/gid maps are **identity** — `1000
1000 1` — because an app that sees uid 0 mis-chowns its own files, and
`setgroups` is denied *before* the gid_map write (CVE-2014-8989); that
ordering is ported verbatim from `map_userns_id`.

### Mount plan

Stage 2, in order; every bind read-only unless marked **rw**, and a
failed read-only remount on a load-bearing bind is fatal, never degraded:

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
 8  /proc  fresh procfs (stage 2 is PID 1 of the new ns), nosuid/nodev/noexec
        then mask sys, sysrq-trigger, irq, bus, acpi, scsi, kcore, keys,
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
10  /dev   tmpfs 0755, containing ONLY bind-mounted host nodes:
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
11  /tmp, /var/tmp   tmpfs rw 1777;  /var/lib, /var/cache  empty tmpfs
12  /run   tmpfs;  /run/user/1000 mode 0700
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
        plus one bind per granted --filesystem
15  mkdir <newroot>/oldroot  (pivot_root requires put_old to EXIST and to
    be under new_root; newroot is a fresh tmpfs, so nothing else makes it)
    pivot_root(newroot, newroot/oldroot); chdir /; umount2(/oldroot,
    MNT_DETACH); rmdir /oldroot
16  PR_CAP_AMBIENT_CLEAR_ALL; PR_CAPBSET_DROP per bit; THEN capset the
    permitted/effective/inheritable sets empty -- that order, because
    PR_CAPBSET_DROP needs CAP_SETPCAP still EFFECTIVE and a capset
    first takes it away; then three readbacks, one per set that has
    one: capget (effective/permitted/inheritable), PR_CAPBSET_READ per
    bit, and PR_CAP_AMBIENT_GET -- capget does NOT return the ambient
    set, so it is the one that needs its own question
17  PR_SET_NO_NEW_PRIVS, then PR_GET_NO_NEW_PRIVS readback
18  seccomp(SECCOMP_SET_MODE_FILTER, 0, &prog)
19  READBACK: /proc/self/status says NoNewPrivs: 1 and Seccomp: 2,
    or stage 2 refuses to spawn anything
20  spawn argv; wait4(-1) as the namespace's reaper
```

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
`CLOEXEC`, because stage 2 does not `exec` — it is a `Command` child that
inherits an open table. Leaking it would hand the confined side the
channel that completes registrations, which is the one channel in this
design whose whole authority is that only stage 1 has it. So: sweep,
create the pipe, register, close the broker connection, spawn. Stage 2
performs no sweep of its own — the pipe is the only descriptor it is
given above stdio, and a second blind loop would have to exempt it.

*The "no controlling terminal" claim was false.* Surface #9's exclusion
list said an app gets no controlling terminal and therefore needs no
`setsid(2)`/`TIOCSCTTY` — but a controlling terminal is a property of the
**session**, and nothing here creates a new one. Stage 2 inherits
the launcher's session and its controlling terminal, the old mount plan bound
`/dev/tty` into the jail, and the filter denies only `TIOCSTI` and
`TIOCLINUX`. An app could therefore reach the operator's terminal through
`/dev/tty` and through any inherited terminal descriptor. Two thirds of
that is closed by the two steps above — no `/dev/tty` node, no inherited
terminal fd, and `/dev/tty` opens `ENXIO` for a process whose session has
no terminal, which is the state step 0's stdio replacement leaves it in
for practical purposes.

The remaining third is honest to state rather than to paper over: the
process is still *in* the caller's session, so the operator's Ctrl-C
reaches it, and a future policy that binds `/dev/tty` (a terminal
application is a real thing to want) would reopen the hole in full.
**Closing it properly needs `setsid(2)`, which is a THIRTEENTH syscall and
therefore an `UNSAFE.md` amendment** — deferred, because nothing on the
first ladder wants a terminal, and recorded here so it is a planned
amendment rather than something discovered later. `CommandExt`'s stable
`process_group` is `setpgid` and does not create a session, so it is not
the way round it. Until then, **`devices=tty` does not exist as a policy
key**: it is refused by the parser like `devices=dri`, so the first
person to want it lands the amendment.

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

**Capabilities are dropped and read back** (step 16). Mount setup needs
namespace-local `CAP_SYS_ADMIN`; the app must not inherit it. Each
`capset` is followed by `capget` and each `PR_CAPBSET_DROP` by
`PR_CAPBSET_READ`, with any surviving capability fatal. The design does
not rely on `exec` incidentally clearing anything.

**The ORDER inside that step is not arrangement, it is the difference
between working and `EPERM`**, and the step list spells it out because a
plausible reading of "drop the capability sets" fails: `PR_CAPBSET_DROP`
requires `CAP_SETPCAP` in the caller's EFFECTIVE set, so a `capset` that
empties permitted/effective first has thrown away the privilege the
bounding drops need, and every one of them then fails. Bounding set
first, `capset` last. The ambient set is cleared before either, because
it is the one set whose bits survive an `exec`, and it is cleared
EXPLICITLY rather than left to the side effect that clearing the
permitted set has: the side effect is real, but the readback that would
confirm it is not — `capget(2)` returns effective, permitted and
inheritable and never ambient — so relying on it would leave the only
set that outlives an `exec` as the only one nothing observes.

Steps 16–19 are four readbacks in a row and that is the point: nothing
observable distinguishes a jail whose filter did not load from one whose
did — until an attack, which is the wrong place to learn it. Same
argument as `losetup` re-reading its read-only flag out of sysfs.

### Filesystem grants

Implemented: `xdg-download`, `xdg-documents`, `xdg-pictures`,
`xdg-music`, `xdg-videos`, `xdg-desktop`, subpaths of the app's home, and
explicit absolute paths, each with `:ro`/`:rw`, and `:create` only below
the user's home. Deny entries override grants. td creates a granted xdg
directory on first use, since a fresh td image has no `~/Downloads`.

**Refused, deliberately stricter than upstream:** `filesystem=host`,
`filesystem=home`, `/`, `/usr`, `/app`, `/run`, `/proc`, `/sys`, the
flatpak repo itself, any `/td/store` path, socket paths, and device
trees. An app that genuinely needs blanket home access gets a reviewed
per-app override rather than a default. Sources are canonicalized before
the app starts and targets checked component by component; no mount
target may be an ancestor or descendant of `/app`, `/usr`,
`/.flatpak-info`, the bus, or the private portal socket.

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

**A deny list, matching upstream's, not an allow list.** This diverges
from td's instincts and is argued rather than slipped in: an allow list
over the runtime glibc's whole syscall surface breaks every time that
glibc updates — a new `*_time64` or `statx` variant appears and every app
dies — and td does not control that glibc, Freedesktop SDK does. The deny
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
| `socket` with `arg0` outside `{AF_UNIX, AF_INET, AF_INET6, AF_NETLINK}` → EAFNOSUPPORT | **upstream filters address families and an earlier draft of this table did not**, which made "matching upstream's" false in the permissive direction: without it a jail reaches `AF_VSOCK`, `AF_XDP`, `AF_PACKET` and the rest of a large and unevenly audited surface. `EAFNOSUPPORT` rather than `EPERM` because it is the answer a kernel without the family gives, and every library already handles it. `AF_CAN`/`AF_BLUETOOTH` are upstream's permission-gated pair and neither has a td permission to gate them, so they are simply out |
| `ptrace`, `perf_event_open` → EPERM (dropped under `allow-devel`) | cross-process inspection; kernel attack surface |
| `userfaultfd` → EPERM | pause-kernel-paths primitive used by exploit chains |
| `personality` with arg0 outside `{0, 0xFFFFFFFF}` → EPERM | `READ_IMPLIES_EXEC` and historical bypasses. **`0xFFFFFFFF` must be allowed**: it is not a personality to set but the standard *query* form, which returns the current value and changes nothing, and glibc and several runtimes use it during startup. An earlier draft denied every nonzero argument and would have `EPERM`ed a read-only question |
| `ioctl` with `(arg1 & 0xFFFFFFFF) == TIOCSTI (0x5412)` → EPERM | terminal input injection. **The mask is load-bearing** — the kernel truncates the request to 32 bits, so an unmasked compare is bypassed by `TIOCSTI \| 1<<32`, the exact historical filter bypass |
| `ioctl` with `(arg1 & 0xFFFFFFFF) == TIOCLINUX (0x541C)` → EPERM | VT injection, same family |
| `clone` with `arg0 & CLONE_NEWUSER` → EPERM | see the open question below |
| `clone3` → ENOSYS | as above |
| `unshare`, `setns`, `chroot` → EPERM | namespace creation and joining |
| `mount`, `umount`, `umount2`, `pivot_root`, `move_mount`, `open_tree`, `fsopen`, `fsconfig`, `fsmount`, `fspick`, `mount_setattr` → EPERM | the whole mount surface, old and new API — filtering only the old one is a hole |
| `open_by_handle_at` → EPERM | the Shocker escape primitive |
| `add_key`, `request_key`, `keyctl` → EPERM | shared kernel keyring |
| `move_pages`, `mbind`, `get_mempolicy`, `set_mempolicy`, `migrate_pages` → EPERM | NUMA policy on other processes' pages |
| `kexec_load`, `kexec_file_load`, `swapon`, `swapoff`, `reboot`, `sethostname`, `setdomainname`, `init_module`, `finit_module`, `delete_module`, `acct`, `quotactl`, `syslog`, `uselib`, `vhangup`, `modify_ldt`, and the obsolete set → EPERM | privileged or obsolete; denied so the answer never depends on capability arithmetic |
| `bpf`, `io_uring_setup`, `io_uring_enter`, `io_uring_register`, `pidfd_getfd`, `process_vm_readv`, `process_vm_writev` → EPERM | **td additions beyond upstream**, each recorded as such: bpf and io_uring are the two largest post-2015 kernel attack surfaces, `pidfd_getfd` steals descriptors, `process_vm_*` is ptrace by another number |

Everything else is allowed, including `seccomp(2)` and
`prctl(PR_SET_SECCOMP)` — Firefox installs its own filters in every
content process, and filter stacking under `NO_NEW_PRIVS` is exactly what
the kernel provides. A nested filter can only narrow.

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
2. A **declared, non-shipped target probe** — built by td's GCC, never in
   the closure — installs the production filter and issues the real
   syscalls in children, checking exact errno and termination status.
   This is where `ptrace` and `TIOCSTI` get proved, since safe td code
   cannot issue them; it is why the probe is a C-side test binary rather
   than a `cfg`-gated widening of `sys.rs`.
3. The **QEMU boot oracle** runs the same probe on the booted image,
   because layer 1 proves the program and layer 2 proves *a* kernel
   loaded it — only the target kernel proves the pinned config supports
   every piece.

Plus negative tests: omitting `NO_NEW_PRIVS` must make installation fail,
and a corrupted-jump or wrong-length program must be refused before
`seccomp(2)`.

### The open question: Firefox's nested sandbox

**The two source designs disagreed here, and it is not resolvable from a
document.** One holds that upstream flatpak denies `clone(CLONE_NEWUSER)`
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

**Resolution:** build the standard (deny) filter first, and structure the
filter as `static STANDARD_FILTER: [SockFilter; N]` from the outset so a
second profile is a data change rather than a redesign. Settle it by
experiment before Firefox milestones are sequenced — on a dev host with
real flatpak, run `flatpak run --command=sh org.mozilla.firefox -c
'unshare -Ur true'` and read `about:support`'s sandbox section on a
stock flatpak Firefox. That is a ten-minute check and it decides whether
td ships one filter or two. Record the answer here when it is known.

### `UNSAFE.md` surface #9 (draft)

> ## 9. `td-jail` — the application sandbox
>
> The `td-jail` application sandbox, whose one `syscall5` body in
> `td-jail/src/sys.rs` carries EXACTLY TWELVE syscalls — `unshare(2)` with
> a value-pinned namespace set, `mount(2)`, `umount2(2)` and
> `pivot_root(2)` for the validated mount plan, `capset(2)` with
> `capget(2)` for the capability drop and its readback, `prctl(2)` with
> SIX value-pinned operations (`PR_SET_NO_NEW_PRIVS`=38,
> `PR_GET_NO_NEW_PRIVS`=39, `PR_SET_PDEATHSIG`=1, `PR_CAPBSET_DROP`=24,
> `PR_CAPBSET_READ`=23 — the readback mount-plan step 16 requires,
> which an earlier draft mandated in the plan while forbidding it in the
> roster — and `PR_CAP_AMBIENT`=47 with its own two pinned
> sub-operations, `PR_CAP_AMBIENT_CLEAR_ALL` and `PR_CAP_AMBIENT_GET`.
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
> and read back through the same call — and `ioctl(2)` with exactly TWO
> value-pinned requests
> (`SIOCGIFFLAGS`=0x8913, `SIOCSIFFLAGS`=0x8914, argument a pinned 40-byte
> `ifreq`, interface name pinned to `lo`) — reached only from `jail.rs`,
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
> unshared and the name inherited); `setsid(2)`/`TIOCSCTTY` — **excluded
> for a corrected reason**: the earlier draft said an app gets no
> controlling terminal, which was simply false, since a session is
> inherited and nothing here creates a new one. They are excluded because
> the jail closes the terminal off by *removing what reaches it* (no
> `/dev/tty` node, no inherited descriptor — see the mount plan's step 0
> and the note below it) rather than by detaching the session, and
> because a real terminal policy would need `setsid(2)` as a documented
> THIRTEENTH-syscall amendment rather than as an assumption; `statfs(2)`
> (nothing in this design issues it — with packages in the image,
> nothing writes gigabytes at launch); `mmap(2)` (nothing here maps anything —
> but see §M, which anticipates that the GPU path WILL need a
> mapping-shaped amendment, of a different class from this
> syscall-instruction layer); and any `ioctl` request beyond the two
> `ifreq` ones — in particular no terminal or device control, the
> jail's other relationship to `ioctl` being that its FILTER denies two
> of them. A THIRTEENTH syscall — `setsid(2)` is the one already
> anticipated, if a terminal policy is ever served — a third ioctl
> request, a seventh prctl
> operation, a third `PR_CAP_AMBIENT` sub-operation, a second seccomp
> flag, an arch
> beyond x86-64, or a caller-supplied BPF program is an amendment here;
> `td-jail/src/main.rs`'s confinement tests assert the roster and its
> TWELVE value-pinned numbers, the five prctl and one seccomp operation
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
descriptor, per spec. Refused: `ANONYMOUS`, `DBUS_COOKIE_SHA1`,
non-numeric identities, a second `BEGIN`, lines over 4 KiB, more than 16
auth commands. Unknown commands get `ERROR` without changing state.

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
16 MiB body, nesting 32, signature 255, 64
descriptors per message, 256 match rules and 64 KiB of rule text per
connection, 128 pending replies, 64 connections — the last **per
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
the per-connection ceiling disconnects that connection; reaching the
global one is a broker-level condition that disconnects the largest
consumer rather than refusing service to everyone, and is logged as a
distinct diagnostic because it means a policy elsewhere is wrong. The
test is a client that subscribes broadly and never reads.

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
`path_namespace`, `destination`, `arg0`…`arg63`, `arg0path`,
`arg0namespace`, with exact D-Bus escaping. `eavesdrop=true` and
`BecomeMonitor` are refused. A signal is delivered at most once per
connection however many rules match.

### Sandbox policy

At accept: `SO_PEERCRED` → pid → the registered jail instance. The
default sandboxed policy may own no name; may call the
`org.freedesktop.DBus` subset above; may call any
`org.freedesktop.portal.*` member and receive its replies and **directed**
signals (a `Request.Response` arrives as a directed signal, which is what
makes portals work with no other grant); receives broadcasts only from
portal-owned names; and gets `AccessDenied` for everything else, with
broadcasts silently undelivered rather than errored — answering a message
that was not addressed to you is worse than dropping it.
`[Session Bus Policy]` `see`/`talk`/`own` entries from authenticated
metadata widen that.

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
namespace and so change what `/proc/<pid>/root/.flatpak-info` resolves
to. So the broker holds `{instance, app-id, stage-2 pid, start time,
permitted service names}`, and a connection is authenticated by **descent
from that registered PID 1**. `SO_PEERCRED.pid` is expressed in
the *broker's* pid namespace, so it stays meaningful even though the app
sees itself in a nested one. A process whose lineage cannot be proven is
denied. Unsandboxed same-uid connections are unrestricted — td's existing
trust model, stated explicitly in §E — but **"unsandboxed" is a proved
answer here and not a fallback**, because the two sentences you just read
would otherwise resolve the same ambiguous peer in opposite directions.
The broker's three-valued reply, and why the middle answer is provable,
are in §E.

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
process is created, which cannot be reused by definition. §P already
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
   service names}` and receives an opaque one-shot token. The instance
   exists; it has no pid and accepts no connections yet.
2. **Stage 1 completes the registration** with `{stage-2 pid, start
   time}` under that token, on its own broker connection — the pid is
   what `Command::spawn` returned, already in the broker's namespace for
   §A's reason. Only then does the broker accept connections for the
   instance. (§A's parent-death pipe is not this channel: it runs stage 1
   → stage 2, and the broker is on neither end.)

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

### `UNSAFE.md` surface #10 (draft roster)

```
| 10 | `td-busd` | `recvmsg(2)`, `sendmsg(2)`, `close(2)`, `getsockopt(2)` |
```

`getsockopt` accepts only `SOL_SOCKET`/`SO_PEERCRED` with a fixed
`[i32; 3]` buffer whose length is pinned in the shipped build, since the
kernel writes exactly `sizeof(struct ucred)` through the pointer.
`recvmsg` pins `MSG_CMSG_CLOEXEC` and a control buffer sized for exactly
64 descriptors. Sole callers are `transport.rs` and `auth.rs`.

**How a forwarded descriptor is owned.** td-compositor reopens received
descriptors through `/proc/self/fd/N` rather than `from_raw_fd`, and that
trick is *unavailable* to a broker — opening a `/proc/self/fd` entry
naming a **socket** fails, and the compositor's descriptors are memfds
and files. A forwarded descriptor here is freight: it is recounted
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

Deliberately absent: `socket`/`bind`/`listen`/`accept` and byte I/O (all
`std`), arbitrary socket options, `SCM_CREDENTIALS`, and any syscall a
D-Bus *service* would need.

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
are wire-level fixtures against the spec.

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
advertises exactly six globals: `wl_compositor` v4, `wl_shm` v1,
`wl_output` v4, `xdg_wm_base` **v1**,
`zxdg_decoration_manager_v1` v1, `wl_seat` v7.

That set is no longer only a claim here. `ui: a window is told the
compositor draws its titlebar` added the sixth and, with it,
`the_registry_advertises_exactly_the_globals_td_serves`, which pins the
name, order and version of each against `advertise_globals` — so the next
change to that list reds a test in the crate rather than silently
falsifying this paragraph, as that commit itself would have.

Three corrections to the obvious assumptions, all checked in
`td-compositor/src/server.rs`:

- **`wl_shm` already advertises ARGB8888 *and* XRGB8888**, and
  premultiplied alpha is software-blended into the XRGB framebuffer — so
  GTK's client-side decoration shadows and rounded corners have a path.
  This is the single largest piece of good news in the gap analysis.
- **Pointer axes are implemented**: `axis`, `axis_source`, and
  `axis_discrete`, version-gated to 5..=7 with the deprecation at v8
  reasoned about in a comment. **The gate is wrong for one of the three
  and a review caught it**: `wl_pointer.axis` has existed since
  *version 1*; only `axis_source` and `axis_discrete` need 5. Gating all
  three together means a client that binds `wl_pointer` at v1–v4 gets no
  scroll events at all — a silent loss, since the pointer otherwise
  works and nothing reports a version mismatch. The gate must be per
  event: `axis` unconditionally, the other two at ≥5. Verify against
  `server.rs` before fixing, per §V.4 — this is a §F claim about code,
  not a specification.
- **Client cursors are *not*** — against `origin/main`, which is what
  every state claim in this section is measured against. `set_cursor`
  validates serial authority and assigns `SurfaceRole::Cursor`, then
  reads and **discards** both hotspot values and never renders the
  surface. The protocol plumbing exists; the cursor does not.
  **Already superseded off-main**: `ui: a client draws its own cursor,
  where its hotspot says` implements it, so by the time workstream B
  starts this row is likely to be closed and its ~400-line estimate
  spent. That is not an erratum in this table — it is the ordinary
  consequence of writing a state snapshot against a repository that four
  agents are moving, and it is why §V.3 requires B to re-read `server.rs`
  rather than trusting this section. Expect the same of any other row
  here that `ui-rolling` reaches first.

And two hard errors that disconnect a client outright: `create_positioner`
returns `"xdg_positioner is not supported"` and `get_popup` returns
`"xdg_popup is not supported"`. **A GTK app opening its first menu is
disconnected today.** By contrast `set_window_geometry` is parsed and
discarded — a true no-op, so CSD margins tile as dead borders and clicks
land offset, but the client survives.

| interface | td state | class | cost |
|---|---|---|---|
| `xdg_surface.set_window_geometry` honoured | parsed, discarded | **B** | ~250 across scene and hit-test |
| `wl_shm` ARGB blending | **present** | — | verify with a golden |
| `wl_subcompositor` | absent | **B** | 2,500–4,000 |
| `wl_data_device_manager` v3, selection | absent | **B** | 4,000–6,000 (DnD later, +~900) |
| `xdg_positioner` + `get_popup` + grabs + constraint solving | **hard error** | **B/U** | 4,500–7,000 |
| client cursor rendering | role tracked, image ignored | **U** | ~400 |
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

`wl_subcompositor` and `wl_data_device_manager` are classed blocking on
purpose, even though a particular GTK build might tolerate their absence:
the gate is compatibility across the pinned runtime, not accidental
startup. Confirm cheaply anyway — run `gtk4-demo` from the actual runtime
against weston or sway with a ~20-line proxy that filters the global out,
and watch. One afternoon settles it.

**dmabuf is never advertised.** Advertising it and rejecting every useful
format is worse than letting clients pick the shm fallback immediately.

### Software rendering

The claim the whole plan leans on is that the runtime's Mesa presents
through `wl_shm` with no dmabuf. `LIBGL_ALWAYS_SOFTWARE` selects llvmpipe,
but llvmpipe being a software *renderer* is not proof that Mesa's Wayland
winsys can *present* without dmabuf in the pinned runtime build. **Treat
that as an explicit unknown and prove it against the runtime**, on the
same dev-host afternoon as the subcompositor check.

The answer that makes it not blocking either way: **GTK4 with no GL at
all is the designed configuration, not a fallback.** `GSK_RENDERER=cairo`
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

The env policy is a small **per-runtime-major table** reviewed when a new
major is first installed, not one global set, because each yearly runtime
rebases Mesa and GTK.

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

1. **Host tests**: manifest and permission-file round-trips and
   refusals; identity-name validation, including every rejection
   (path separators, leading dash, over-length, empty); alias mapping;
   the D-Bus codec in both endian modes with malformed bodies, spoofed
   senders and serial-wrap boundaries; the auth state machine;
   name-queue transitions; match-rule evaluation; portal Request/Session
   lifetimes; the popup constraint solver, table-driven; and the BPF
   program through the test interpreter.
2. **Recipe-side seed tests.** The repository fixture corpus is gone
   with the repository client; what replaces it is much smaller and
   sits where the work now happens. A seed recipe's checks assert that
   the pinned archive unpacks to the declared tree shape, that the entry
   point exists and is executable, that the declared runtime resolves,
   that no setuid bit or device node survives, and that the ELF
   interpreter named is the runtime's rather than the host's. These are
   ordinary recipe checks and run in the ordinary recipe gate.
3. **Jail**: the three seccomp layers of §C, plus in-QEMU assertions from
   *inside* a jail — `getuid()==1000`, `/proc/1/exe` is td-jail, host
   processes absent, `/app` and `/usr` reject writes, host `/usr` and
   `/td/store` absent, `/dev/fb0` and `/dev/input` absent, a large
   `/dev/shm` mapping works, `memfd_create` works, both sockets work.
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
6. **The offline QEMU oracle — the centrepiece.** The image test builds a
   **fixture package by recipe**: a small td-built static Rust binary
   with a generated manifest and an empty runtime — which is also the
   cheapest proof that the jail needs nothing td-owned from a runtime.
   Because the package is a recipe output like any other, this needs no
   fixture repository and no install-time network — and since §B.1 the
   fixture is *in the image the test boots*, so the boot runs
   `/bin/<fixture>` directly with no install step at all. The guest
   inside the jail
   asserts its `/proc` says `NoNewPrivs`/`Seccomp`, asserts `mount(2)`
   fails `EPERM`, connects `wayland-0`, maps a toplevel and commits a
   frame, calls `Settings.ReadAll` through the bus and reads its
   **direct reply** — `ReadAll` is a synchronous method returning
   `a{sa{sv}}`, NOT a Request-producing call, so an oracle that waited
   for a `Request.Response` would hang forever on a portal that was
   working perfectly; an earlier draft said exactly that, and it is the
   kind of error that presents as "the portal is broken" —
   then prints one marker the oracle latches after
   `TD-BUSD-READY` and `TD-PORTAL-READY`. That single marker is jail +
   seccomp + broker + policy + portal + compositor, end to end, offline,
   on the shipped kernel. The first *foreign*-toolkit oracle uses a
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
     than `payload_inputs` is a planning-time refusal;
   - a `Step::Run` argv expanding a `payload_inputs` path is a refusal —
     the argv/template scanner, since this is the assertion whose
     violation is otherwise silent;
   - an unmarked output derived from a marked source pin is a refusal;
   - the interpreter assertion: the payload's `PT_INTERP` is absent from
     the built image tree, so a direct `execve` outside a jail fails
     `ENOENT`. Asserted against the image, not assumed;
   - the closure query reports every marked path, and the set matches
     the reviewed pin list exactly — including the application→runtime
     edge, which exists only because the spec carries the runtime's full
     store path;
   - metadata normalization: no setuid/setgid bit, file capability,
     security xattr, device node or escaping symlink survives packaging;
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
| 1 | **kernel namespace/seccomp/cgroup config pins + QEMU readback** (§0) — **LANDED**, except the functional calls, which need surface #9: the `unshare` at rung 8, the filter install at rung 11 | none — but this is the gate on everything |
| 2 | `td-login exec-as` with credential readback — **LANDED** | none |
| 3 | **the §B.8 marker**: the recipe-level mark, `payload_inputs` as a declared channel, taint propagation from the source pin, and the planning-time refusals — plus the argv/template scanner, since that assertion is the one that is otherwise silent. The **channel has LANDED** (see §B.8: `{payload:NAME}` resolution plus `ro,noexec` binds, replacing the un-implementable "never staged at all"); the MARK — `foreign` on the source pin, the derived recipe flag, taint propagation and `contains_payloads` — is the remaining half | none |
| 4 | the manifest and permission keyfile, name validation and every rejection; the closure query reporting marked paths | none |
| 5 | first seed recipe — a pinned upstream artifact becomes a store package with §B.3's checks and §B.8's assertions | none |
| 6 | the spec compiler: runtime resolution by full store path, mounts, grants, entry point | none |
| 7 | the `/bin/<name>` farm through `real_root_steps` + `bin_farms()`, the launcher table, the state directory | none |
| 8 | `td-jail` crate + surface #9 skeleton + stage-1/stage-2 transition | none |
| 9 | mount plan, pivot, fresh proc/dev/tmp | none |
| 10 | capability drop/readback + PID-1 reaper | none |
| 11 | const BPF assembler, standard filter, interpreter tests, target probe | none |
| 12 | **fixture package shipped in the image and launched by `/bin/<fixture>`** | **first jailed pixels on the QEMU screen** |
| 12a | the same fixture under `--host`, asserting the degradation report (§X.5) | host mode works, and says what it could not enforce |
| 13 | `td-busd` codec, auth, surface #10 | none |
| 14 | names, routing, match rules, descriptor passing | none |
| 15 | per-app policy, lineage identity, in-jail activation | none |
| 16 | `td-portal` personality: Request/Session core, Settings, Account | GTK settings call works |
| 17 | Wayland A: `set_window_geometry`, decoration manager, ARGB golden, single-pixel-buffer; **the GDK and llvmpipe experiments run and their answers land in DESIGN.md** | none |
| 18 | Wayland B: `wl_subcompositor` | none |
| 19 | Wayland C: `xdg_positioner`/`xdg_popup`/grabs | menus work |
| 20 | clipboard (data-device v3), then client cursors | paste and a real I-beam |
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
is jailed pixels through `wl_shm`, M28 is a Firefox window painted by
llvmpipe inside the runtime and blitted by td's CPU renderer. Nothing
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
2. **Toolkit hard requirements and Mesa-over-shm** — both settled by one
   dev-host afternoon (§F). If GTK4's cairo path has rotted in the pinned
   runtime, the GL-less story weakens and dmabuf pressure rises, which is
   a dead end with no GPU. This is the most expensive possible surprise.
3. **Firefox's nested sandbox** (§C) — one filter or two, decided by a
   ten-minute experiment, affecting the security posture of the most
   exposed program on the image.
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
the jail — in v1 an escape owns the user account; that network traffic is
mediated when `shared=network` is granted; that a malicious publisher is
contained beyond the jail; or anything about side channels, resource
exhaustion, or the profile-data persistence in §B.

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
| curated set or open Flathub | **curated, built by recipes** — runtime EOL and update policy then fall out of the pin, and a Flathub signature never enters the picture |
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
to memory used; an `RLIMIT_DATA` cap does not, but it is a real bound
rather than a generous one and wants measuring before a number is picked.

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

**The aggregate bound needs cgroup v2 `memory.max`**, which is why §0
now pins `CONFIG_CGROUPS`, `CONFIG_MEMCG` and `CONFIG_CGROUP_PIDS`
rather than deferring them. The shape:

- `td-svc` (root) creates the session's cgroup subtree at boot and
  **delegates** it, which is the same thing systemd's `user@.service`
  does and the only way an unprivileged `td-jail` may create children in
  it. The delegation set is **three files, not one**: the directory,
  `cgroup.procs`, AND `cgroup.subtree_control`. The third is the one an
  earlier draft omitted and it is load-bearing — without ownership of
  `cgroup.subtree_control` the delegate cannot enable the `memory`
  controller for the cgroups it creates, so `memory.max` and
  `memory.high` never come into existence and every write below fails
  `ENOENT`. (`cgroup.threads` completes the conventional set and is
  harmless to include.) The failure mode is worth stating because it is
  not "permission denied" but "the file is not there", which reads like
  a kernel-config problem and is not one.
- **Two cgroup-v2 rules make that delegation fail if it is a bare
  `chown`, and both reviewers found it independently.** Controllers are
  enabled TOP-DOWN — `memory` must already be in the parent's
  `cgroup.subtree_control` before the delegate can enable it in its own —
  so `td-svc` enables it down the chain at boot rather than assuming a
  distribution did. And a cgroup with member processes cannot enable
  controllers for its children (the "no internal processes" rule), so the
  delegated directory must stay EMPTY: session processes live in a leaf
  beside the per-instance ones, never in the delegation root. Get that
  wrong and the write to `cgroup.subtree_control` fails `EBUSY` — a
  different failure from the `ENOENT` above and, unlike it, one that
  points at the file rather than at the design.
- `td-jail` creates one cgroup per instance, writes `memory.max`,
  `memory.high`, `pids.max` and optionally `cpu.max` from the app's
  permission file, then moves stage 2 into it *before* spawning the app,
  so every descendant is inside by construction.
- Writes are ordinary file writes to `cgroup.procs` and the limit
  attributes — **no new syscall, no roster amendment**. This is the
  rare case where the capable mechanism is also the safe one.
- `memory.high` before `memory.max` matters: `high` throttles and
  reclaims, `max` invokes the OOM killer inside the cgroup. A browser
  that gets slow near its ceiling is better behaviour than one that
  loses a tab, so set both, `high` below `max`.
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
- Read `memory.events` and `memory.peak` back for the diagnostic: an app
  killed for memory should say so, since an OOM kill inside a cgroup is
  otherwise indistinguishable from a crash.

Per-app values live in the same per-package permission file as the
filesystem and device grants (decision 9), with a documented default
rather than unlimited — *unlimited* is the setting that produced the
complaint.

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
that. Do not build an OSTree/OpenPGP stack or a 200-package desktop
graph.

**The sharpest formulation of the whole question, and the rule to
follow:** *the package provenance decision need not be irreversible; the
application identity, confinement and state contract must be.* Commit to
the second now; defer the first per application.

The experiments that settle what is left, none longer than a week:

| # | experiment | settles |
|---|---|---|
| **E1** | copy a GTK4 application's and its runtime's `files/` trees out of flatpak; run under plain `bwrap` with §C's mount plan against stock sway, with no flatpak, ostree or portal daemon present | whether a deploy tree is a self-contained hierarchy — the assumption the whole model rests on. One afternoon, and the decisive one |
| **E1b** | for the same application, identify what upstream publishes at a stable URL with a hash, and run §B.3's unpack-and-lay-out against it | seed or source-build, per application. Untouched, and the harder half — Flathub publishes no deploy tarball |
| **E2** | `gtk4-demo` behind a global-filtering proxy; llvmpipe-over-shm in the pinned runtime; the ten-minute Firefox nested-userns check | §F's toolkit requirements and §C's open question. The most expensive possible surprise is here: if GTK4's cairo path has rotted, the GL-less story weakens with no GPU to fall back on |
| **E3** | a Meson-world pilot — recipes for `pkgconf`, Ninja, Meson, a native CPython, then GLib and a Wayland-only `gtk3-demo` | td's *actual* per-package source cost, the number with the widest error bars. Near `cmake-x86-64`'s cost and the source track is real; a multi-week fight per package and the hybrid is permanent posture |
| **E4** | the §0 cgroup pins plus a fixture under `memory.max=64M` with a `memory.oom.group` readback in the QEMU oracle | §P's mechanism |
| **E5** | `glxinfo` inside a jail on virtio-gpu QEMU with the runtime's GL extension mounted | §M's first step |
| **E6** | `RLIMIT_AS=2G` on Firefox; record the crash | closes the rlimits-versus-cgroups argument with data |

**Points of no return.** Landing a GVariant/OSTree/OpenPGP tower is one —
sunk cost will argue for keeping it, so do not start it before E1 has
run. Landing LLVM-with-Clang, Node and WASI toolchain recipes is the
other, and pays off only if source Firefox is genuinely pursued. Dropping
the OSTree client is the *reversible* commitment: if td later wants
target-side installs, the client can be built then, against a platform
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

**E1 and E1b** (§R). One afternoon each, and they test the assumption the
model rests on. If a deploy tree needs something generated at install
time, an undocumented environment variable, or a portal before it will
draw, that is the finding — and it is worth far more before four agents
start than after.

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
| `UNSAFE.md` + the roster table in `AGENTS.md` | one at a time, in roster order (#9 A, #10 B, #11 C), each appending its own section. The table row is the conflict; the body is not |
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
| covered by the attempt counter | yes | **almost never, and not quite never** — the `bootsuccess` unit is `after` the compositor and terminal and `requires` the terminal, so it records after the graphical session is ready and before any application is launched. The exception is §A's launcher table: the compositor reads a package-generated `td-launcher.tsv` at startup, so a malformed one can keep readiness from being reached and drive the counter down. A rollback for a bad application is therefore possible by that one path and by no other | n/a |
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
— a host has no image, and no feature is bent to pretend otherwise — and
it is **one configuration value, not a code path**: the package root
joins the state root (§B.4) as configuration, so `td-jail` binds
`<pkgroot>/<name>/files` at `/app` and neither the jail nor the
application can tell which configuration produced the path. If it ever
becomes two code paths, the divergence has stopped being availability.

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
at **rung 12** — not rung 8, since rung 8 is a jail skeleton and §X.4's
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
- **An acceptance test**: the rung-12 fixture run under `--host` in CI,
  asserting the launch **and the degradation report** against what that
  host's kernel and compositor actually offer. A test that asserted only
  "it ran" would pass on a host enforcing nothing.

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
2. **Prefer upstreams that publish plain files.** Where a project ships
   only through a repository protocol of its own — a flatpak runtime
   through OSTree, say — the choices are a distro's plain package files,
   or building from source (§B.3). Not hosting a repackaged copy.
3. **Delta and chunking machinery is not needed** and should not be
   built speculatively. It optimizes repeated pulls from a server that
   does not exist.

The scale at which this stops being sufficient is worth naming so the
decision is revisited deliberately rather than suffered: **a second
machine that does not build its own software.** At that point either it
becomes a build host too, or something must serve it artifacts, and §Z is
what has to be reopened rather than worked around.

---
