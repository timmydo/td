# td `unsafe` surfaces

This file is the normative record of every `unsafe` in td. It exists
because the roster is the point: the value of writing these down is being
able to count them and see each one's justification beside the others,
which is exactly what stops a ninth being added quietly. Where this file
and the code disagree, one of them is a bug.

## The rule

In the control-plane engine the only `unsafe` is the raw-syscall layer
(`builder/src/sys.rs` and its callers `nar.rs`/`sandbox.rs`), which carry
`#![allow(unsafe_code)]` so `builder` can be `libc`-free. Every other
engine crate (the shared `engine` lib and
`recipes`/`fetch`/`feed`/`subst`) `forbid`s `unsafe_code`. There are EIGHT
target-side exceptions, each a standalone crate OUTSIDE the
`builder`/`recipes`/`engine` workspace whose only `unsafe` is that same
`syscall`-instruction layer under a scoped `#[allow]` (the crate itself
`#![deny(unsafe_code)]`s).

Do not add `unsafe` anywhere else; a new `unsafe` surface is a reviewed
amendment recorded HERE. A new syscall in an existing surface, a new
value-pinned request, or a second scoped `#[allow]` is likewise an
amendment to this file, and to the crate's own normative doc where it has
one.

## Roster

| # | crate | syscalls |
|---|-------|----------|
| 1 | `td-kexec` | `kexec_file_load(2)`, `reboot(2)` |
| 2 | `td-netd` | `ioctl(2)` |
| 3 | `td-init` | ten — see [§3](#3-td-init--the-boot-glue-multicall) |
| 4 | `td-login` | `setgroups(2)`, `setgid(2)`, `setuid(2)` |
| 5 | `td-svc` | `kill(2)` |
| 6 | `td-compositor` | `recvmsg(2)`, `close(2)`, `sendmsg(2)` |
| 7 | `td-util` | `ioctl(2)`, three pinned requests |
| 8 | `td-sh` | `umask(2)`, `rt_sigaction(2)` (disposition-only), `ioctl(2)` (three pinned requests) |

The control-plane exception (`builder/src/sys.rs`) is described under The
rule above and is not part of this numbering: it is host-side, and no
target artifact contains it.

## 1. `td-kexec` — the guest kexec helper

The `td-kexec` guest helper is confined to exactly two syscalls
(`kexec_file_load(2)` + `reboot(2)` with `LINUX_REBOOT_CMD_KEXEC`) copied
from `sys.rs`.

## 2. `td-netd` — the network bring-up daemon

The `td-netd` network bring-up daemon is confined to a single `ioctl(2)`
wrapper (`syscall3`) through which its interface-config ioctls
(SIOCSIFFLAGS/ADDR/NETMASK, SIOCGIFHWADDR, SIOCADDRT) go — all its socket
I/O rides `std`, so DHCP needs no AF_PACKET raw socket.

## 3. `td-init` — the boot-glue multicall

The `td-init` boot-glue multicall, whose one `syscall5` body in
`td-init/src/sys.rs` carries EXACTLY these ten syscalls, one per applet
that safe `std` cannot reach: `reboot(2)` + `sync(2)`
(reboot/poweroff/halt), `mount(2)` + `umount2(2)` (mount/umount, and
switch_root's `MS_MOVE`), `chroot(2)` (switch_root), `setsid(2)` +
`ioctl(2)` (cttyhack and losetup), `sethostname(2)` (`hostname -F`, the
flag uutils lacks), `wait4(2)` (init, which as PID 1 must reap the orphans
a targeted `Child::wait` cannot see), and `mknod(2)` — the tenth, and the
last privileged busybox job on the boot path. The deployment initramfs
mounts devtmpfs on `/dev` first thing, so `/dev/loop0` normally comes from
the kernel; this is the fallback for when the loop driver registered none,
and it cannot come from a cpio `nod` line because the devtmpfs mount
shadows whatever was there. Unlike `losetup` and `kill`, the busybox
spelling here WAS build-time checked — `INITRAMFS_APPLETS` names are swept
against `busybox --list` — so this is not that argument. What it buys is
narrower and real, and it is all about the ARGUMENT rather than the call:
`dev` is one integer with major and minor packed into disjoint bit ranges,
and a wrong packing is a well-formed node pointing at a DIFFERENT driver,
which `mknod(2)` creates and reports success for. So the crate now has
exactly one `dev_t` packing (`td-init/src/devt.rs`, replacing the private
copy `losetup` carried), an encode that REFUSES an unencodable pair rather
than truncating it, and a readback that stats the created node and unlinks
it — reporting BOTH failures if the unlink also fails — unless the kernel
agrees about both numbers. Same "nothing observable distinguishes the
wrong outcome" argument that made `losetup` re-read its read-only flag out
of sysfs. `mknod.rs` is the only permitted caller of `sys::mknod`,
asserted like `mount`'s and `attach_loop`'s, because `mode`'s top bits are
the node type and so choose the driver class. Only BLOCK nodes are served;
`c`/`u`/`p` are refused, since nothing on td's boot path creates one and
the type is the part of `mode` that picks the driver. An ELEVENTH syscall
is an amendment here. `ioctl(2)` is the one with TWO permitted requests —
`TIOCSCTTY` for cttyhack and `LOOP_SET_FD` for the `losetup` applet — both
pinned by value, so widening that roster is as reviewable as adding a
syscall to it. `losetup` is why `td-boot` no longer runs any third-party
program: attaching the verified root loop had rested on busybox existing
at an absolute path and parsing `-r <device> <file>` as expected, with
nothing tying the two together, so dropping that applet would have stopped
every boot with no build-time complaint — the same argument that moved
`kill(2)` into td-svc. `LOOP_SET_FD` rather than `LOOP_CONFIGURE`
deliberately: its argument is the backing descriptor, an integer, where
`LOOP_CONFIGURE` would need a `struct loop_config` laid out by hand, and a
field at the wrong offset is a kernel operation invisible at the call
site. Read-only is not passed as a flag but follows from the descriptor
td-boot verified and handed over, which is why it cannot be forgotten the
way `-r` can; `losetup.rs` then reads it back out of
`/sys/dev/block/<major>:<minor>/ro` — by the number off the OPENED device,
not by the path string, so the answer cannot be about a different device
than the one the ioctl went to — and refuses unless the kernel agrees,
because nothing observable distinguishes a read-only loop from a writable
one at attach time and a writable loop over the verified root is a root
whose contents no longer match the hash that admitted it. `mount(2)` was
restricted to `MS_MOVE` until the `mount`/`umount` applets landed; they
need the real flag word, so the restriction moved from the syscall to
`td-init/src/mount.rs`, the only module that composes one. Because the
flags are a runtime parameter now rather than a frozen constant, FOUR
assertions replace the old call-site pin, and between them they are the
confinement: `sys.rs` declares exactly thirteen `MS_*`/`MNT_*` constants
and no more; each is value-pinned; the option table's BITS must each be
one of them (so a bare `0x4000` cannot reach the kernel); and no module
but `mount.rs` — plus `switch_root`'s two `MS_MOVE` moves — may name one
or call the two wrappers. That amendment is what lets both initramfses and
`/etc/inittab` mount without busybox, and the `LOOP_SET_FD` request above
is what lets `td-boot` attach the verified root loop without it — so
nothing td-boot runs is a third-party program any more. Busybox is still
the `/init` interpreter and still serves the shell utilities those scripts
call; what left is the privileged work. Deliberately NOT in that surface:
`pivot_root(2)` (it fails on the initramfs rootfs, so switch_root moves
the mount as util-linux and busybox do), `fork`/`execve` (`Command` plus
the safe `CommandExt::exec` cover both), `dup2` (`Stdio::from(File)` wires
the console), and any signal handler — which is why td's init supports no
`ctrlaltdel`, `shutdown` or `restart` inittab action. An ELEVENTH syscall,
a new `MS_*` bit, or a second scoped `#[allow]` is an amendment here;
`td-init/src/main.rs`'s confinement tests assert all three against the
crate's own source, since the compiler alone cannot.

## 4. `td-login` — the credential multicall

The `td-login` credential multicall (`login`/`su`), whose one `syscall2`
body in `td-login/src/sys.rs` carries EXACTLY three syscalls —
`setgroups(2)`, `setgid(2)`, `setuid(2)` — issued once each, in that
order, from the single `creds::apply`, which then re-reads
`/proc/self/status` and refuses to `exec` unless the kernel agrees with
what was asked for. This one is NOT reachable through safe `std`:
`CommandExt::groups` is unstable (`feature(setgroups)`), so the only
stable behaviour drops every supplementary group, and `std` applies
credentials in a forked child where nothing can read back what took — and
the readback is the defence, because a `setuid(2)` issued before
`setgroups(2)` starts a working session that silently keeps the previous
holder's groups. Deliberately NOT in that surface:
`getuid`/`getgid`/`getgroups` (`/proc/self/status` answers all three and
has to be read anyway), `setresuid`/`setreuid` (a second way to set the
same thing is a second way to get it wrong), `execve` (safe
`CommandExt::exec`), and `umask`. `td-login/THREAT-MODEL.md` is the
normative specification for that crate and its confinement tests assert
what it says — including the ORDER of the three calls, which no compiler
checks; a fourth syscall, or relaxing the fail-closed authentication
policy, is an amendment there AND here.

## 5. `td-svc` — the service supervisor

The `td-svc` service supervisor, whose one `syscall2` body in
`td-svc/src/sys.rs` carries EXACTLY ONE syscall — `kill(2)` — reached only
from the single `send_signal` in `supervise.rs`. td-svc
`#![forbid(unsafe_code)]`d until it took this, and shelled out to the
uutils `/bin/kill` instead; that traded an `unsafe` block for something
worse. The supervisor's ability to stop ANYTHING became a runtime
dependency on a third-party multicall existing at an absolute path and
reading `-<pgid>` as a process group rather than as a flag, with nothing
tying the two together — dropping `kill` from the image's applet list
would have left every `stop`, every `restart` and the whole ordered
teardown silently unable to signal, with no build-time complaint. It also
cost a `fork`+`exec` per signal on the shutdown path and made seven of
td-svc's own stop-path tests skip on any host lacking `/bin/kill`, so the
code most needing coverage was the code least often run. Deliberately NOT
in that surface: `killpg(2)` (it is `kill(2)` with a negated argument),
the `rt_sig*` family (td-svc installs no handlers, and DESIGN.md §5 turns
on there being none), `getpid`/`getpgid`/`getsid` (`/proc` answers those,
and I3 requires reading it anyway), and `waitpid`
(`Child::wait`/`try_wait` cover it). A SECOND syscall is an amendment
here; `td-svc/src/main.rs`'s confinement tests assert the roster and its
one value-pinned number, the whole `asm!` block including which register
each argument lands in, that the crate names the unsafe lint exactly
twice, that the entry point is private to `sys.rs` and named nowhere else,
that `send_signal` is the wrapper's only caller, and that NOTHING imports
out of `sys` — an alias would give the one audited call a name none of
those scans looks for. `sys.rs`'s own tests then issue the syscall,
because every assertion above is about source TEXT and a `kill` that
returned `Ok(())` without issuing anything satisfies all of them.

Everything else td-svc needs is still reachable through safe `std`, which
is what keeps that surface at one. `td-svc/DESIGN.md` is its normative
specification, recording both that and the invariants no compiler checks
(no `pre_exec`, liveness read from `/proc` rather than inferred from an
exit status, and a console that is neither skippable nor indefinitely
delayed).

## 6. `td-compositor` — the software Wayland server

The `td-compositor` software Wayland server, whose one `syscall3` body in
`td-compositor/src/sys.rs` carries `recvmsg(2)` for wl_shm and demo-client
keymap SCM_RIGHTS reception, `close(2)` for the received descriptor after
safe duplication through `/proc/self/fd/N`, and `sendmsg(2)` for the
td-native demo client's wl_shm pool descriptor, the server's wl_keyboard
keymap descriptor, and the transport selftest. Stable Rust exposes no
stable ancillary-data API. It also carries a FOURTH, `ioctl(2)`, for
td-term's PTY, with FOUR value-pinned requests reached only from `pty.rs`:
`TIOCSPTLCK` (0x40045431) to unlock the slave, `TIOCGPTPEER` (0x5441) to
obtain it as a descriptor rather than by `/dev/pts/N` name, and
`TIOCSWINSZ`/`TIOCGWINSZ` (0x5414/0x5413) to publish a grid and read it
back. The readback is the point, as it is for `losetup`'s read-only flag:
nothing observable distinguishes a `TIOCSWINSZ` the kernel applied from
one it clamped or ignored, and a child that lays out its screen for a size
the terminal does not have is a terminal that looks broken with every test
green. The request roster is enforced in code, not only in a test — one
`ioctl` entry point refuses anything outside the four before issuing the
syscall — and the winsize argument is an `[u16; 4]` rather than a
`#[repr(C)]` struct so its field ORDER is a tested function; a swapped
rows/columns pair is a well-formed resize to a different size.
`TIOCGPTPEER`'s returned number is adopted through the SAME
`/proc/self/fd/N` reopen the received-descriptor path uses, deliberately
NOT `OwnedFd::from_raw_fd`: that would be a second scoped allow of a
different shape — a descriptor adoption rather than the
syscall-instruction layer — and the crate can reopen by descriptor
identity instead. Deliberately NOT in that surface: framebuffer and evdev
I/O (ordinary files), Unix socket setup and byte I/O (`std`), mmap (wl_shm
pixels are copied with `FileExt`), device ownership (safe `td-seatd`), or
anything else the PTY needs — no termios call (the slave's kernel defaults
ARE the canonical-input policy), no `setsid(2)` or `TIOCSCTTY` (the child
gets its session from the declared `td-init` input's `cttyhack --stdin`),
and no `fork`/`execve`/`dup2` (`Command` plus `Stdio::from(File)` cover
all three). `td-compositor/DESIGN.md` is the normative UI-stack
specification. Its confinement tests pin the allow count, assembly body,
syscall numbers, callers, and absence of unsafe from every other module;
adding another syscall or scoped allow is an amendment there AND here. The
two surfaces behind the one body are pinned to disjoint modules —
transport to `client.rs`/`server.rs`, terminal control to `pty.rs`, and no
other module names `sys` at all.

## 7. `td-util` — the diagnostics multicall

The `td-util` diagnostics multicall, whose one `syscall3` body in
`td-util/src/sys.rs` carries EXACTLY ONE syscall — `ioctl(2)` — with THREE
value-pinned requests, reached only from `term.rs`: `TCGETS`/`TCSETS`
(0x5401/0x5402) to take a keystroke without waiting for Enter, and
`TIOCGWINSZ` (0x5413) to ask how many rows a page is. td-util
`#![forbid(unsafe_code)]`d until it grew `less`, the pager that let
busybox's `more` (and with it `vi` and `awk`) leave the image. Neither
operation is reachable through safe `std`: there is no stable API for
terminal modes at all, and `IsTerminal` answers only whether a descriptor
is a terminal, never how big. Because `ioctl(2)` is one syscall onto an
unbounded space of operations, the number in `rax` is not the surface —
the request in `rsi` is, so all three are pinned by VALUE and each call
site is pinned whole, the same argument that pinned `LOOP_SET_FD` rather
than trusting the name. Deliberately NOT in that surface: `TIOCSWINSZ`
(the setter; nothing td ships has a reason to resize an operator's
terminal), `TCSETSW`/`TCSETSF` (they drain or discard pending terminal I/O
another process may own — a pager has no business throwing away what
someone else wrote), `TIOCSTI` (it injects input into a terminal, the
classic escape from a restricted session), `TIOCSCTTY` (that is td-init's,
for cttyhack), and `isatty`, which `std::io::IsTerminal` already answers
safely. The termios buffer is OPAQUE BYTES in `sys.rs`; `term.rs` is the
only module that knows what a field means, it never CONSTRUCTS a termios
(the kernel's own bytes are read, two `c_lflag` bits and two `c_cc` slots
are patched, and the untouched original is what `Drop` writes back), and
it re-reads the whole struct and refuses to page unless the kernel agrees:
the two flag bits cleared, `VMIN`/`VTIME` took, and NO OTHER BYTE moved.
All three are load-bearing. `TCSETS` can succeed having applied only part
of what was asked, and a terminal still in canonical mode is
indistinguishable from one whose reader has not typed yet, so that failure
presents as a hung pager rather than as anything about terminal modes;
control bytes that did not take make a command read wait for several
keystrokes or time out and read EOF, which this pager treats as `q`; and
the third is what makes "never constructs a termios" a property rather
than a claim, since a ZEROED buffer passes the other two — a zeroed
`c_lflag` has ICANON and ECHO clear — while `c_cflag = 0` is B0, a hang-up
on a serial console, and `c_oflag = 0` drops ONLCR so every line
staircases. The guard holds a `BorrowedFd`, not a bare `RawFd`: it issues
a syscall from `Drop` on a descriptor it does not own, so the borrow
checker is what stops the terminal being closed — or closed and RECYCLED —
before the restore reaches it. `ISIG` is deliberately left ON, so Ctrl-C
still ends a pager stuck on a huge file. An eighth syscall, a fourth
request, or a second scoped `#[allow]` is an amendment here;
`td-util/src/main.rs`'s confinement tests assert the roster and its one
value-pinned number, the three request values and that each is named
exactly twice, the whole `asm!` block including which register each
argument lands in (and that `options(nomem)` stays absent, since two of
the three requests have the kernel write through a pointer), that the
crate names the unsafe lint exactly twice, that `term.rs` is the wrappers'
only caller, and that no module names the syscall module any way but plain
`use crate::sys;` — an alias would give the audited calls a name none of
those scans looks for. Two of those assertions are about an ARGUMENT
rather than a call, which is where this surface's real risk lives:
`WINSIZE_LEN` is pinned because TIOCGWINSZ copies `sizeof(struct winsize)`
through the pointer with no length negotiation, so a shorter buffer is an
out-of-bounds kernel write from code the compiler reads as safe; and
`raw()` patching the kernel's own bytes is pinned because the runtime
check that would catch a constructed termios can only fire against a real
terminal, which the gate has none of.

Every other applet td-util serves reads `/proc` or `/dev/kmsg` as an
ordinary file, which is what keeps that surface at one syscall.

## 8. `td-sh` — the shell

The `td-sh` shell, whose one `syscall4` body in `td-sh/src/sys.rs` carries
EXACTLY THREE syscalls — `umask(2)`, a DISPOSITION-ONLY `rt_sigaction(2)`,
and `ioctl(2)` with three value-pinned requests — reached from exactly
three modules: `builtin.rs` for the `umask` and `trap` builtins,
`process.rs` for the guards that hand a subshell back the process state a
fork would have kept for it, and `term.rs` for the terminal mode and width
the line editor needs. `std` exposes an API for none of them, and in the
umask case that is not a gap that can be worked around: it is why the
shipped `/init` still spells one line `busybox sh -c 'umask 077; …'`, and
why the shell that is supposed to replace busybox could not serve it.

`umask(2)` cannot fail, so there is no `check()` on that path; what it
does is RETURN THE PREVIOUS MASK, and both wrappers are built out of that.
Reading is a set-and-restore (`umask(0)` then put it back), because there
is no reading syscall; and `umask_set` proves its own effect by asking for
the same mask a second time, which changes nothing and returns what the
first call left. That readback is not ceremony: nothing observable
distinguishes a mask that did not take, since the wrong bits surface later
as a file created with permissions nobody asked for — the same argument
that made `losetup` re-read its read-only flag out of sysfs. It earned its
keep immediately, catching `MODE_BITS` declared as `0o7777` when Linux's
`sys_umask` keeps only the nine rwx bits.

`rt_sigaction(2)` is the amendment the paragraph above used to defer, and
it is deliberately the SMALL half of it. A shell wants two things from
`trap`: to ignore a signal and to catch one. Catching needs a handler, and
a handler on x86-64 needs a hand-laid `SA_RESTORER` trampoline to return
through — so only the ignore is taken here, and the type system is what
holds the line: `Disposition` has exactly two arms, `SIG_DFL` and
`SIG_IGN`, neither of which runs any code, and no other handler word is
constructible outside `sys.rs`. What that buys is not shell bookkeeping.
POSIX makes `trap '' SIG` a real kernel disposition, INHERITED ACROSS
`execve`, so it is the half of `trap` that has to reach the children a
script starts; recording it in a table would leave `trap '' INT; long_job`
interruptible and every other shell's scripts wrong under this one. The
catching half stays deferred, and `trap 'action' SIG` therefore asks for
`SIG_DFL` — honest about the shell dying on the signal rather than
pretending an action is waiting to run.

Three things confine the new syscall beyond its two-armed argument.
FIRST, it is disposition-only in the STRUCT as well as the type: `install`
writes `[handler, 0, 0, 0]` — `sa_flags` 0 (so no `SA_RESTORER`, no
`SA_SIGINFO`), `sa_restorer` 0, empty `sa_mask` — and the word count is
pinned because `sys_rt_sigaction` copies `sizeof(struct sigaction)`
through the pointer with no length negotiation, so a short buffer is an
out-of-bounds kernel write from code the compiler reads as safe. The
struct is a plain `[usize; 4]` rather than a `#[repr(C)]` type so its
field ORDER is a tested function, as td-compositor's winsize is: a handler
written at the wrong offset is a well-formed `sa_flags` and a disposition
left alone. SECOND, `signal_set` re-queries and REFUSES unless the kernel
agrees, the same argument as the mask above — a signal the kernel still
defaults on kills the shell at the moment the script was written to
survive it. THIRD, td-sh only ever changes a signal it found at `SIG_DFL`,
asked ONCE per signal and cached (dash's `sigmode`, and for dash's reason:
after the first change the process can no longer be asked what it started
with). `execve` resets every caught signal to default, so a non-default
answer on first touch was installed by someone else — a parent that
ignored it, which is POSIX's rule that a signal ignored on entry cannot be
trapped or reset and is what makes `nohup` stick, or Rust's own runtime,
which ignores SIGPIPE and handles SEGV/BUS before `main`. The SIGPIPE case
is not academic: un-ignoring it would turn every `yes | head` into a dead
shell. SIGKILL and SIGSTOP are refused before the call rather than after
it, so every refusal that does surface is the kernel disagreeing about
something it could have done — and SIGCHLD is refused as well, though the
kernel would take it, because `SIG_IGN` there is POSIX's request that
children be AUTO-REAPED and would leave every external command reporting
`ECHILD` instead of its status.

Two limits of the ignore are worth writing down beside it. `trap ''`
cannot carry SIGPIPE into a child, because `std::process::Command` undoes
the runtime's ignore in every child it spawns and neither end of that is
reachable from safe code; undoing it in the shell instead would make td-sh
die on a closed stdout as dash does, which is a different failure mode for
every write in the crate and so a separate landing. And a non-empty action
asks for `SIG_DFL` rather than leaving an inherited ignore standing, so the
table and the process never describe different shells.

This surface now PASSES POINTERS, which the umask-only one did not, and
that is what `options(nomem)`'s absence was being kept for: the option is
a promise about the FUNCTION, not about today's calls, and the kernel both
writes the previous action through one address and reads the new one
through another — addresses that escape only as integers, so a `nomem`
promise could let the compiler keep a stale buffer across the call or drop
the one it is about to read.

Deliberately NOT in that surface: reading the mask out of
`/proc/self/status`'s `Umask:` field (real and safe, but it answers only
half the builtin, so it would buy a `/proc` dependency without removing
the syscall); `chmod`/`fchmod` (`std::fs::set_permissions` covers them);
`kill(2)`, `rt_sigprocmask(2)` and `sigaltstack(2)` — this shell sends no
signals, blocks none, and runs on no alternate stack; a handler-bearing
`rt_sigaction`, which is the `SA_RESTORER` question above and lands as its
own amendment when `trap INT` is served for real.

A FOURTH syscall, or a fourth `ioctl` request, is an amendment here;
`td-sh/src/main.rs`'s confinement tests assert the roster and its three
value-pinned numbers, that the three ioctl REQUESTS are value-pinned too
and named three times each — the declaration, the roster the gate checks
against, and the one wrapper that issues them — with the five refused
neighbours (`TCSETSW`, `TCSETSF`, `TIOCSWINSZ`, `TIOCSTI`, `TIOCSCTTY`)
absent from `sys.rs` entirely, since `TCSETS` mistyped as `0x5404` is
`TCSETSF` and the in-code roster cannot tell them apart, that the two
handler words are pinned by value and named in `sys.rs` alone, that the
installed action is `handler` followed by three zeros, the whole `asm!`
block including which register each argument lands in and that
`options(nomem)` stays absent, that the crate names the unsafe lint
exactly twice outside comments, that those three modules are the
wrappers' only callers, that no module aliases OR IMPORTS OUT OF the
syscall module, that `syscall4` has exactly one call site per syscall
and that each passes the NAMED number rather than a bare literal (the
number reaches the kernel as an argument, so pinning the declarations
alone does not pin what is issued), that `sys.rs` carries no block
comment — which is what makes the line-based comment strip complete for
the one file that may hold `unsafe`, since a `/* */` between two tokens
changes nothing the compiler sees — and that the scan COVERS every
module `main.rs` declares, whatever its visibility, since a module
missing from that list is one no other assertion can see. `sys.rs`'s own
tests then issue two of the three and check the answer against
`/proc/self/status` — `Umask:` for one, the `SigIgn:` mask for the other
— while `term.rs`'s issues the third against a descriptor that is not a
terminal and requires `ENOTTY` back, since the gate has no terminal to
ask. Every assertion above is about source TEXT, and a wrapper that
returned a plausible value without issuing anything would satisfy all of
them.

`ioctl(2)` is the third, and it is td-util's surface arriving in the shell:
the same `TCGETS`/`TCSETS`/`TIOCGWINSZ` roster, taken for the same reason
that no stable `std` API exposes a terminal's mode or its width —
`IsTerminal` answers only whether a descriptor IS one. Because `ioctl` is
one syscall onto an unbounded space of operations, the number in `rax` is
not the surface: the request in `rsi` is. All three are pinned by VALUE,
and unlike td-util's per-wrapper form the roster is ENFORCED IN CODE — one
`ioctl` entry point refuses anything outside the list before issuing, so a
fourth request is a compile-time edit to a named array rather than a new
call site somebody has to notice. Deliberately NOT in it: `TIOCSWINSZ`
(the setter; nothing td ships has a reason to resize an operator's
terminal), `TCSETSW`/`TCSETSF` (they drain or discard pending terminal I/O
another process may own), and `TIOCSTI` (it injects input into a terminal,
the classic escape from a restricted session).

`term.rs` is the only module that knows what a `termios` byte means, and
it is a copy of `td-util/src/term.rs` down to the argument for each check:
a termios is never CONSTRUCTED — the kernel's own bytes are read, known
offsets are patched, and the untouched original is what `Drop` writes back
— and raw mode is read back and REFUSED unless the kernel agrees, because
a terminal still in canonical mode is indistinguishable from one whose
reader has not typed yet. Three things are compared on that readback: the
flag bits cleared, `VMIN`/`VTIME` took, and NO OTHER BYTE moved, the last
being what makes "never constructs a termios" a property rather than a
claim, since a ZEROED buffer passes the first two while `c_cflag = 0` is
B0, a hang-up on a serial console. The guard holds a `BorrowedFd`, not a
bare `RawFd`: it issues a syscall from `Drop` on a descriptor it does not
own, so the borrow checker is what stops the terminal being closed — or
closed and RECYCLED — before the restore reaches it.

Where td-sh DIFFERS from the pager is `ISIG`, and it is worth writing down
because it looks like a relaxation and is the opposite. td-util keeps
signal generation on so Ctrl-C can kill a pager stuck on a huge file. A
shell wants the reverse for a reason particular to this one: td-sh
installs no signal handler, so `SIG_DFL` for SIGINT would END THE SHELL at
its own prompt — which is what it did before the editor existed. With
`ISIG` cleared, Ctrl-C arrives as a byte the editor acts on and the line
is abandoned instead. The trade is bounded by the guard's lifetime: raw
mode is taken PER LINE and dropped before any command runs, so a child
still gets all three of Ctrl-C, Ctrl-\ and Ctrl-Z. Ctrl-C while a command
is running therefore still ends the shell; that needs a real handler or
job control, and both remain deferred.

Clearing `ISIG` also removes the operator's last in-band escape if the
restore never runs, which is worth stating because `Drop` is not a
guarantee: this crate builds with `panic = "abort"`, so a panic inside
the editor would leave the terminal with no `ECHO`, no `ICANON` and —
unlike the pager, which keeps `ISIG` for just this — no Ctrl-C either,
so `stty sane` would have to be typed blind from elsewhere. What holds
that shut is the crate's own no-panic rule rather than the guard: every
path through the editor returns `Option`/`Result`, `pos` moves only
through the two boundary walkers, and the escape parameter saturates
rather than overflowing a debug build. It is tested the only way an arm
that needs a terminal to type can be — the dispatch is driven headlessly
over a generated keystroke stream, in `line.rs`'s
`no_keystroke_sequence_panics_the_editor`.

Every other primitive td-sh needs — pipes, process spawn, `exec`, the
virtual fd table that stands in for `dup2` — is reachable through safe
`std`, which is what keeps that surface at three.
