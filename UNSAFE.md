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
| 3 | `td-init` | ten — see [§3](#3-td-init--the-boot-glue-multicall); `ioctl` has four pinned requests |
| 4 | `td-login` | `setgroups(2)`, `setgid(2)`, `setuid(2)` |
| 5 | `td-svc` | `kill(2)` |
| 6 | `td-compositor` | `recvmsg(2)`, `close(2)`, `sendmsg(2)`, `ioctl(2)` |
| 7 | `td-util` | `ioctl(2)`, three pinned requests |
| 8 | `td-sh` | `umask(2)`, `rt_sigaction(2)` (disposition-only), `ioctl(2)` (three pinned requests), `poll(2)` |

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
spelling here WAS build-time checked — `INITRAMFS_APPLETS` names were
swept against `busybox --list`, and that sweep is emitted only while the
list has something in it, which since the td-sh flip it does not — so
this is not that argument. What it buys is narrower and real, and it is
all about the ARGUMENT rather than the call:
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
is an amendment here. `ioctl(2)` is the one with FOUR permitted requests —
`TIOCSCTTY` for cttyhack and getty, `LOOP_SET_FD` for the `losetup` applet,
and `TCGETS`/`TCSETS` for the line settings getty applies — each pinned by
value, so widening that roster is as reviewable as adding a syscall to it.
Unlike the two-request form this replaced, the roster is now ENFORCED IN
CODE (td-sh's shape, not td-util's per-wrapper one): a single `ioctl` entry
point in `sys.rs` refuses anything outside `IOCTL_REQUESTS` before issuing,
so a fifth request is an edit to a named array rather than a new call site
somebody has to notice. The four wrappers are pinned whole in turn, because
with one entry point the syscall-argument pin no longer sees a request
number: `TIOCSCTTY` reads its third register as the steal flag,
`LOOP_SET_FD` as a descriptor, and the two termios calls as a pointer the
kernel copies 36 bytes through. That length is the kernel's `struct
termios` (four flag words, `c_line`, NCCS=19 slots) and NOT glibc's 60-byte
one, pinned for td-compositor's `WINSIZE_LEN` reason: the copy has no length
negotiation, so a buffer sized from the wrong header is an out-of-bounds
kernel write from code the compiler reads as safe.

`TCGETS`/`TCSETS` arrived with the `getty` applet, which is what took the
LAST busybox name off the image — the tty setup half of the login chain,
where `login` and `su` had already moved to td-login and `sh` to td-sh. What
that applet needs beyond a session is a line a person can type on, and
`term.rs` is the only module that knows what a `termios` byte means: it
never CONSTRUCTS one, patches named bits into the kernel's own bytes, and
compares the whole 36-byte readback against exactly what it computed, so a
byte the patch never named moving is a failure too. The bits are the ones
whose absence makes a line unusable rather than a full `sane`: canonical
input, echo, signal generation and visible erase; `OPOST`/`ONLCR` (without
which every line staircases); `ICRNL` with `IGNCR` clear (without which
Enter never terminates a line); 8N1 with `CREAD`; `CLOCAL` for `-L`; and the
control bytes a canonical line needs, `VMIN`/`VTIME` among them because a
leftover raw configuration carries `VMIN = 0`, under which `login` reads
end-of-file at once. The failure this exists for is a shell that took raw
mode and died before its restore ran: the next session on that terminal
echoes nothing and submits no line, and nothing in the boot reports it.

The line SPEED is the one field a refusal would be wrong about, and it gets
a three-way answer rather than a check. A serial line programs its divisor
from `CBAUD`; a virtual console has no speed and ignores the field. Refusing
the second case would make the applet unusable on `tty1`, which a graphical
image wants, and accepting it silently would hide a serial console left at
the wrong speed — a console that prints garbage. So the readback tells "took
it" from "left it exactly as found" and reports the latter on stderr, while
a THIRD speed, neither asked for nor kept, is an error naming the field.

`TIOCSCTTY` is where getty deliberately DIVERGES from the applet it
replaces. busybox getty re-acquires the terminal with `TIOCSCTTY(1)`, a
steal; td-init passes the same pinned `NO_STEAL` 0 cttyhack does, so a
terminal a LIVE session still holds is EPERM. Where the two applets then
differ is what that means: cttyhack degrades and execs anyway, because a
rescue shell without job control beats no shell, and getty REFUSES —
the caller asked for a login session on this terminal, and one with no
controlling terminal is a session where Ctrl-C reaches nothing and
`login`'s child cannot be signalled. Refusing is also what the shipped
`/etc/tty-session` is written around: its `getty … && td-svc reboot`
short-circuits, so the supervisor restarts the greeter rather than powering
the machine off as though the operator had logged out.

`losetup` is why `td-boot` no longer runs any third-party program:
attaching the verified root loop had rested on busybox existing at an
absolute path and parsing `-r <device> <file>` as expected, with
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
nothing td-boot runs is a third-party program any more. Neither
initramfs packs the multicall, and since `getty` became an applet here
neither does the real root: that used to be a claim about the ARCHIVES
alone, because the greeter unit respawned busybox's `getty` every boot,
and it now holds for the whole image. Nothing td runs is a third-party
program bar the Rust userland it builds from source.

Deliberately NOT in that surface:
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
transport to `client.rs`/`conn.rs`/`server.rs`, terminal control to
`pty.rs`, and no other module names `sys` at all. `conn.rs` is the client
transport itself, extracted from `client.rs` so the terminal is a second
USER of one connection rather than a second copy of it; the descriptor
queue is intrinsic to that connection, so it moved with it. That widens the
transport's caller list by one module and narrows nothing. A `Connection` is
crate-visible, though, so a module could reach `sendmsg`/`recvmsg` through
one without ever spelling `sys::` — which is all the caller scan looks for.
The transport's USERS are therefore pinned by that same test, which is what
makes "the terminal modules — parser, model, renderer, keyboard, PTY policy
— reach none of it" a checked property rather than a claim. A module joining
that roster is an amendment here, as a new caller is, and `term_client.rs`
is the first: the terminal's own Wayland client is a transport USER by
construction. It is NOT a new caller — it names no `sys::` at all, and the
descriptors it receives arrive through `Connection` — so the syscall
roster above is unchanged by it.

"Eventually" is now: the terminal binds a `wl_seat`, creates a
`wl_keyboard`, and RECEIVES its keymap descriptor, which §12 keeps inside
the same transport boundary wl_shm submission is in. That is one more user
of
`recvmsg(2)`'s product and no new caller of it — the fd is claimed with
`Connection::take_fd` and validated by `conn::verify_keymap`, both in
the module the roster already names. Its TEST needed the other
direction, a peer sending a descriptor, and that is why
`conn::send_event_with_fd` exists rather than the test spelling
`sys::send_with_fd` in `term_client.rs`: a test that named it would be a
roster change, and the confinement scan refuses one — which is how this
was caught rather than decided.

A connection's READING half detaches: `Connection::detach_reader` hands a
`Reader` to a thread, so a client whose main loop has a second source to
serve is not blocked in a socket read. `recvmsg(2)`, and the `close(2)` that
retires a descriptor nobody claimed, therefore issue from that thread rather
than from the main one. That is not an amendment to the roster above — same
wrappers, same one calling module, and `term_client.rs` still names no
`sys::` — but it is a change to what "intrinsic to that connection" means
above, so it is recorded rather than left to be inferred. The one-reader
rule is the TYPE's: a `Reader` is not clonable, exposes no descriptor, and a
`Connection` refuses both to read and to detach again once it has given one
up. Detaching also requires the handshake to be over, because the socket
read timeout a deadline sets outlives the deadline itself.

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
EXACTLY FOUR syscalls — `umask(2)`, a DISPOSITION-ONLY `rt_sigaction(2)`,
`ioctl(2)` with three value-pinned requests, and `poll(2)` — reached from
exactly three modules: `builtin.rs` for the `umask` and `trap` builtins,
`process.rs` for the guards that hand a subshell back the process state a
fork would have kept for it, for the one that stops the shell listening
to the terminal while a foreground child runs, and for the one question
`read -t` asks, and `term.rs` for the terminal mode and width the line
editor needs. `std` exposes an API for
none of them, and in the umask case that is not a gap that can be worked
around: it is why the shipped `/init` spelled one line `busybox sh -c
'umask 077; …'` until this surface existed, and why the shell that is
supposed to replace busybox could not serve it. That line is `/bin/sh -c`
now, and it was the last call the image made into the multiplexer.

`umask(2)` cannot fail, so there is no `check()` on that path; what it
does is RETURN THE PREVIOUS MASK, and both wrappers are built out of that.
Reading is a set-and-restore (`umask(0)` then put it back), because there
is no reading syscall; and `umask_set` proves its own effect by asking for
the same mask a second time, which changes nothing and returns what the
first call left. That dance now happens EXACTLY ONCE, from `main` before
the shell runs anything, and every later READ answers from the shell's own
record. The window it opens is the reason: for the two instructions
between clearing the mask and putting it back the process has none at all,
which was unobservable while the shell was the only thing running and
stopped being so when pipeline stages began to stream concurrently — a
sibling stage creating a file in that window gets it world-writable, and
nothing about the file says why. `umask_set` holds that record across its
own install-and-readback for the same reason, or two stages setting a mask
interleave and each reads back the OTHER's.

What that does NOT close, and must not be read as closing, is a stage that
SETS a mask. `umask(2)` is per-PROCESS and a pipeline's stages are threads
of one, so `{ umask 000; … } | { …; : > f; }` really does create that file
world-writable — a window the length of a stage rather than of two
instructions, in the same direction and with the same consequence. It is
bounded to the pipeline (the mask is restored after) and it is a
divergence from every forking shell, which gives each stage its own. Real
isolation needs the mask applied per stage at each file creation, or
stages in processes of their own; neither is here, and the record above
is only sound for the read.

A BACKGROUND JOB is in that same paragraph and reaches further, because
it outlives the statement that started it rather than a pipeline.
`umask 077; { umask 022; sleep .05; } & umask 027; wait; umask` prints
0027 under bash and 0077 here: the job restores the mask it captured
when it was cloned, over the one the parent chose while it ran. Same
mechanism, same absent fix — a mask is per-PROCESS and these are threads
of one — and the same answer, which is a job in a process of its own.

A stage that sets NONE is a different matter and is closed. Every clone
captured the mask and put it back when it ended, which is invisible while
subshells run one at a time and is a stage reaching into a SIBLING's
lifetime once they do not: `umask 022; { umask 077; sleep .2; : >a; } |
sleep .1` created `a` as 0644, because the right-hand stage — which asked
for nothing — restored 022 between the left one's `umask` and its file.
So a clone now restores only a mask it actually CHANGED. The direction is
why it is not merely untidy: the file comes out more permissive than the
stage that created it asked for, and nothing about the file says so.

That readback is not ceremony: nothing observable
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

Inside a PIPELINE that install is deferred to the spawn rather than made
when `trap` runs, because a disposition is one cell the concurrent stages
share: a stage that installed its ignore would hand it to whatever a
SIBLING spawned at that instant, and — since each stage restores what it
SAW — two stages touching the same signal leave the parent holding one
forever, outliving the pipeline that caused it. So a stage records the
ignore and `spawn_uninherited` installs the SPAWNING stage's own intent
for as long as it takes to create the child, which is the only moment a
disposition is read. Nothing about `trap ''` reaching a script's children
changes; what a stage gives up is protecting ITSELF from the signal,
which a pipeline already gave up to the guard exemption below, for the
same reason. The set installed is the stage's whole ignore roster and not
just the two the guard moves, so `trap '' TERM` in a stage reaches its
children as bash's does; it also covers every signal the shell has
INSTALLED an ignore for, which the trap table alone cannot name — a stage
that CLEARS its parent's `trap '' TERM` no longer mentions TERM, while
the process is still holding it, and the child would inherit an ignore
the stage asked to be rid of. SIGCHLD is refused there as it is
everywhere, since ignoring it is POSIX's request to AUTO-REAP and would
cost the very status the spawn is about to wait for.

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
with). Every signal is asked at STARTUP rather than on first use, because
lazily is too late once stages run at once: a sibling's guard is an
ignore, and so is the one a sibling's spawn installs for the length of a
`fork`, and a stage resolving a signal inside either window would cache
POSIX's never-touch-this for a signal that started at the default — and a
cache is answered once and kept. `execve` resets every caught signal to
default, so a non-default
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

A FIFTH syscall, or a fourth `ioctl` request, is an amendment here;
`td-sh/src/main.rs`'s confinement tests assert the roster and its four
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
missing from that list is one no other assertion can see, and that the
pollfd length is pinned in the SHIPPED build for the reason the termios
and winsize lengths are. `sys.rs`'s own tests then issue three of the
four: two are checked against `/proc/self/status` — `Umask:` for one, the
`SigIgn:` mask for the other — and `poll` is asked about a real pipe,
whose readiness the test controls from both ends. `term.rs`'s test
issues the fourth against a
descriptor that is not a terminal and requires `ENOTTY` back, since the
gate has no terminal to ask. Every assertion above is about source TEXT,
and a wrapper that returned a plausible value without issuing anything
would satisfy all of them.

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
still gets all three of Ctrl-C, Ctrl-\ and Ctrl-Z. Ctrl-Z is the one of
the three nothing here answers: with no job control there is no `fg` and
no parent to continue the process, so a stopped shell stays stopped —
which is what dash does without job control too, and what job control is
the fix for. Ctrl-C while a command is RUNNING is the other half, and
`rt_sigaction` serves it too: the shell ignores SIGINT and SIGQUIT for
exactly as long as it waits, so the driver's signal ends the command and
leaves the shell. NOT inside a pipeline, though, and that exception is
load-bearing: stages are threads of one process, so a guard taken by the
stage waiting on `cat` covers the stage running `while :; do :; done` as
well — and that one can only ever be stopped by a signal. The pipeline
became unkillable with no in-band way out, which is strictly worse than
the death the guard replaces. So a pipeline is not covered, which is also
what `bash --posix` does with one — for a signal delivered to the process
GROUP, which is what the terminal driver sends and what the Ctrl-C above
means. For one aimed at the SHELL ALONE bash does not agree, and that
half is a real cost of the exemption rather than a tie: `sleep 1 | cat;
echo AFTER` under `kill -INT <shell>` prints `AFTER` under bash and
exits 130 here, where td-sh survived it before stages streamed. Any
supervisor that signals a shell by pid sees it, so it is not only the
interactive case. Closing it needs the same thing the rest of this
paragraph needs — somewhere to RECORD a signal, or stages in processes
of their own — since what the exemption gives up is the shell's own
protection for exactly as long as a pipeline runs.

A BACKGROUND JOB is exempt for the same reason and by the same flag,
which is named `concurrent` rather than `in_pipeline` because a job and
a stage are the same kind of thing to every mechanism that cares: both
are threads of one process, so neither may take a guard that covers its
siblings or install a disposition they share. Exempting the job is what
keeps `while :; do cmd & done` killable — while `&` was synchronous the
job took the guard and covered the LOOP, which is why the `&` arm used
to carry a job's interrupt back out to end the shell. That propagation
is gone with the reason for it: a background job dying of a signal is
not the shell dying of one, which is what bash reports too.

The exemption is measured rather than argued, on the shape that isolates
it: `sleep 30 & while :; do :; done`, where the job holds a guard for
thirty seconds while a loop only a signal can stop runs inside it. Under
a group SIGINT the shell died 6 times out of 6 with the job marked
`concurrent` and survived 6 of 6 with that one line removed —
`a_shell_with_a_background_job_can_still_be_interrupted` is the
assertion, the background half of the pipeline's.

Both of them, because Ctrl-\ reaches the shell exactly
as Ctrl-C does and a shell that survived one and died on the other would
be a coin toss from the keyboard. The ignore is taken AFTER the child
exists, which is what keeps the child interruptible — a disposition is
copied when a process is created, so one installed later cannot reach
it. The guard therefore covers the WAIT and nothing else: between
commands — PATH resolution, expansion, parsing, the gap between `spawn`
returning and the ignore, and any builtin that blocks (`read` most of
all) — the shell is back at `SIG_DFL` and a signal there still ends it.
That is not a narrow race but a real exposure, and what divides the
covered case from the uncovered one is not how LONG a command runs but
whether an EXTERNAL command runs at all. Measured against the built
binary with a randomly-timed group SIGINT: `while :; do sleep 0.5; done`
and `while :; do sleep 0.02; done` each left the shell alive 30 times out
of 30, `sleep 0.002` 27 of 30, and `while :; do true; done` — `true` is a
builtin, so no guard is ever taken — died every time. A signal that lands
INSIDE the guard with nothing left to die of it is lost outright rather
than deferred, for the same reason: with no handler there is nowhere to
record that it arrived. That is not only a timing tail. `kill -INT $$` is
DETERMINISTICALLY swallowed, and it is the POSIX idiom for a script to end
itself as if by the terminal: this shell has no `kill` builtin — sending a
signal would need `kill(2)`, which is not on this surface — so `kill` is
an external command, always inside the guard, and the signal it sends to
the shell arrives on an ignored disposition and is discarded. `sh -c 'kill
-INT $$; echo alive'` prints `alive` here where dash and bash report 130
— OUTSIDE a pipeline, that is. Inside one no guard is held, so
`{ kill -INT $$; echo alive; } | cat` is the opposite failure and the
shell dies of the signal where bash prints `alive`. The two are the same
missing handler seen from either side.
`kill -QUIT $$` matches them, since SIGQUIT infers nothing either way.
Both are inherent to installing none, since a
shell that ignored the signal while doing its own work would lose the
keystroke instead of surviving it. Closing either needs somewhere to
RECORD the signal — a handler — or the child in a process group of its
own. Real job control, with the child in that group and the terminal
handed to it, closes it properly and is the better answer, and is still
deferred: it needs `setpgid(2)` and a `TIOCSPGRP` ioctl, which is an
amendment here rather than a use of what the surface already has.

A BACKGROUND JOB now runs while that guard is held, which nothing did
before `&` became asynchronous, and it is recorded rather than measured:
an interactive `cmd &` puts an external command on a terminal whose
`ICANON`, `ECHO` and `ISIG` the editor has cleared for the line being
typed, and the editor's restore-what-was-there writes back over whatever
mode that command set. Neither half is reachable from a script, and the
guard's lifetime is one line, so this is smaller than it sounds; the fix
is the same one the umask and the ids want, a job in a process of its
own.

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

`poll(2)` is the fourth, and the narrowest: it exists for ONE builtin,
`read -t`, whose whole question is the one poll answers — would a read
return without waiting, and if not, does that become true within a
deadline. `std` exposes no readiness primitive at all, and the two safe
ways round it are each worse than a syscall. A blocking read is the thing
the timeout exists to avoid. A read on a thread abandoned when the
deadline passes has already CONSUMED what it read — for a shell that is a
line of the script's input lost rather than returned late, on a
descriptor a parent process usually shares.

Before this, the table answered from its own SHAPE, and got it wrong in
two different directions. An inherited descriptor or an internal pipe
stage answered "cannot tell", and the builtin REFUSED — `cannot time this
descriptor without poll(2)`, status 2. Everything else answered "ready",
including the things that are a `File` to the table and not a regular
file to the kernel: a FIFO, a socket, an opened terminal, where the read
then BLOCKS with the deadline already spent. Measured on the parent
commit, `exec 3<>fifo; read -t 1 x <&3` never returned. So the option
served only the descriptors that never needed it, and on the rest it
either refused or hung. A dead variant went with the refusal: `Fd::Pipe`
existed to make the table say "cannot tell", and a pipe is a file in
every other respect, so the question is answered by asking the kernel
rather than by the shape of the table.

`poll` has a single meaning, so unlike `ioctl` there is no request roster
to gate; what is pinned instead is the ARGUMENT, and in three parts. The
buffer LENGTH is asserted in the SHIPPED build rather than only in a
test, for the reason the termios and winsize lengths are: `poll` reads
`nfds * sizeof(struct pollfd)` through the pointer and writes `revents`
back through it with no length negotiation, so a short buffer is an
out-of-bounds kernel write from code the compiler reads as
`deny(unsafe_code)` clean. `nfds` is pinned WITH it and by name, because
a length alone is only half the bound — the kernel writes `nfds` structs
whatever the buffer is, so a bare `2` at the call site reaches past an
eight-byte one exactly as a short buffer would. And the EVENT is pinned:
`POLLIN` by value and named three times and no more, the three
output-only bits by value beside it, and `POLLOUT`/`POLLPRI` absent —
each of those over the SHIPPED half of `sys.rs`, since the crate's own
tests name what they assert about, as the `ioctl` roster's scan does.

The struct is a plain `[u32; 2]` rather than a `#[repr(C)]` type so its
field ORDER is a tested function, as td-compositor's winsize is: the two
`short`s share the second word, and a swapped pair is a well-formed
request for a DIFFERENT event, which the kernel accepts and answers.
Composing and reading that word are two named functions for a reason no
runtime observation covers — `events` is `POLLIN`, which is itself a
member of the ready set, so a wrapper that read the REQUEST back as
though it were the answer would report ready whenever poll returned at
all and agree with every other test in the file.

Two arguments are refused before the call rather than passed on, and both
are about an ANSWER rather than a failure — `losetup`'s argument again.
`poll` IGNORES a negative descriptor, reporting it neither ready nor in
error, so `read -t 5` on one would wait the full five seconds and report
a timeout that never happened; and a negative timeout is poll's spelling
of "wait forever", which is the one thing this option exists to prevent.
Neither is reachable from today's callers, which is why they are guards
rather than error handling.

Three output-only bits count as ready beside `POLLIN`: `POLLERR`,
`POLLHUP` and `POLLNVAL`. The kernel reports them whether or not they
were asked for, and each means a read returns AT ONCE rather than blocks
— end of file on a pipe whose writer is gone, an error condition, or a
descriptor that is not open. That is `read -t`'s question, and it is
already what this shell answers for a here-document at EOF and for a
closed descriptor, so the bits agree with the table rather than adding a
rule to it.

What poll can answer bounds how the shell may READ, which is the one way
this syscall reaches past its own call site. `read -t` carries ONE
ABSOLUTE DEADLINE and polls with what is left of it before EVERY byte, as
ash does: a single poll before the loop bounds only the FIRST byte, so a
writer that sends a partial line and keeps the pipe open would leave
`read -t 1` blocked forever, past the deadline it was given. And the
shell must not read AHEAD, because bytes in its own buffer are invisible
to poll, which sees only what is still in the kernel — reading stdin
through `std::io::Stdin`, a `BufReader`, took up to 8 KiB off a shared
descriptor and then reported a timeout for a line already in hand. So the
`read` BUILTIN takes stdin ONE BYTE at a time through an unbuffered
handle, as this shell already did for every other descriptor and as its
line editor already did in raw mode. Nothing in the shell is LEFT
reading ahead on stdin: the editor's cooked fallback and the reader for
a script arriving on stdin end every line with the descriptor exactly
past it, so the script and the commands in it cannot disagree about
where in that descriptor they are.

HOW that position is reached depends on whether the descriptor can be
given back to, and only the `read` builtin needs it a byte at a time —
poll is the reason there, and a pipe has no way to return an over-read.
A script on stdin that is a REGULAR FILE does not need either: `sh <
script` reads a block and REWINDS to the byte after the newline, which
is what bash does and what costs two syscalls a line rather than one a
byte. How much that is worth depends on how long the lines are, since
the old cost was per BYTE and the new one is per LINE: on a 60k-line
script of ordinary length it is about a third of the time, and on one
of two-byte lines it is barely anything. Regular rather than merely
seekable, because a descriptor can answer a position query and still
refuse the negative seek this ends with, and by then the block is read.
That is not a silent loss — the failing seek becomes a
`ScriptLine::Failed` and the shell stops — but a script that stops is
still not a script that ran, so the narrower test is the one taken.

The rewind is not tidiness but the whole of that path's correctness:
without it the block takes lines the script's own `read` is owed, which
`a_file_stdin_script_agrees_with_a_piped_one` pins by running both
paths over one script and holding each to the same expected output
rather than to the other. It is issued before `read_line_block`
returns, so the descriptor is ahead only between that read and that
seek. NOTHING td-sh itself starts can observe that window: a pipeline's
stages are threads joined before the list returns, and a BACKGROUND JOB
— which since `&` became asynchronous really does run while the parser
reads — is given `/dev/null` for stdin, as POSIX 2.9.3 requires of an
asynchronous list wherever job control is disabled. What could is a
process OUTSIDE the shell holding the same file description: a
daemonising child, or a sibling of whoever set up the redirect. For one
of those the cost is worse than seeing a transient offset: the rewind is
RELATIVE, so a sharer that moves the position inside the window sends it
to the wrong absolute place and the reader does not recover. That is
inherent to reading more than a byte and giving the rest back, bash
included.

The job's `/dev/null` is worth recording as the FIX rather than as a
detail, because the alternative was nearly written down here as
impossible. Before it, a 40-line `sh < script` opening with `{ read a;
read b; } &` lost three lines and left the parser resuming MID-LINE,
running `line8` as a command. Neither candidate repair to this reader
helps: a byte at a time while a job is live still interleaves, the job's
own `read` being bytewise too, and seeking absolutely rather than
relatively races the same way, the position query being a second
syscall. Both were tried and measured still broken. The descriptor was
the bug, not the read.

Deliberately NOT done: bash's further trick of deferring the rewind
until it is about to run something that reads stdin. That buys the rest
of the distance to bash and replaces an invariant that holds
unconditionally with one every future command path has to remember.

Nor may a descriptor be LOCKED across that read. The shell's table holds
an open file behind a bare `Arc` rather than an `Arc<Mutex<…>>`, because
a mutex held across a blocking one-byte read is a sibling stage's
`read -t 5` waiting on a lock without ever reaching `poll` — the deadline
missed by the very mechanism meant to keep it. `&File` is both `Read` and
`Write`, so the kernel serialises two stages sharing a descriptor exactly
as it does two processes. What remains is a RACE rather than a wait: a
sibling can take the byte between the poll and the read, and that one is
inherent to sharing a descriptor at all.

Deliberately NOT in that surface: `select(2)`/`pselect6(2)`, `ppoll(2)`
and the `epoll` family — the same question with a larger argument, a
descriptor-indexed bitmask or a kernel object with a lifetime of its own,
where poll's is eight bytes on the stack; `read(2)` itself, since every
read this shell does goes through `std`; `fcntl(2)` with `O_NONBLOCK`,
which would answer the same question by MUTATING a descriptor the shell
usually does not own — an inherited stdin handed back to a parent
nonblocking is a different failure for every program that shares it; and
a deadline built from `alarm(2)` or `setitimer(2)`, which arrives as
SIGALRM and so needs the handler this surface deliberately cannot
install.

Every other primitive td-sh needs — pipes, process spawn, `exec`, the
virtual fd table that stands in for `dup2` — is reachable through safe
`std`, which is what keeps that surface at four.
