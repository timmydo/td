# td `unsafe` surfaces

This file is the normative record of every `unsafe` in td. It exists
because the roster is the point: the value of writing these down is being
able to count them and see each one's justification beside the others,
which is exactly what stops a thirteenth being added quietly. Where this file
and the code disagree, one of them is a bug.

## The rule

In the control-plane engine `unsafe` is confined to the raw-syscall layer
in `builder/src/sys.rs` and the low-level conversions in `nar.rs` and
`sandbox.rs`. Those three files carry `#![allow(unsafe_code)]` so builder
can stay `libc`-free. `ostree.rs` calls one safe syscall wrapper and carries
no unsafe allowance. Every other
engine crate (the shared `engine` lib and
`recipes`/`fetch`/`feed`/`subst`) `forbid`s `unsafe_code`. There are THIRTEEN
target-side exceptions, each a standalone crate OUTSIDE the
`builder`/`recipes`/`engine` workspace with a scoped `#[allow]` around its
recorded raw Linux boundary (the crate itself `#![deny(unsafe_code)]`s).
The first nine confine that boundary to their syscall-instruction layer except
for `td-compositor`, whose client-side clipboard source consumes one exact
received descriptor through a second scoped allow. The tenth, `td-busd`,
carries the same different shape for general descriptor forwarding. Sections
6 and 10 argue the two separately. The eleventh, `td-profiler`, also owns the
pointer accesses into the perf ring mapping whose lifetime and bounds that
same module controls. The twelfth, `td-portal`, confines descriptor-carrying
Wayland I/O to one raw-syscall module and immediately reopens received regular
files through `/proc/self/fd` so no raw descriptor ownership escapes it. The
thirteenth, `td-audio`, is back to the plain shape: one syscall-instruction
layer, no descriptor adoption and no mapping, because the ALSA transfer mode
it uses is `SNDRV_PCM_ACCESS_RW_INTERLEAVED` and that mode has none. It is
also the only surface whose scoped `#[allow]` sits on the entry point ALONE
and not on the module — see §13 for the escape a module-level one permits.

Do not add `unsafe` anywhere else; a new `unsafe` surface is a reviewed
amendment recorded HERE. A new syscall in an existing surface, a new
value-pinned request, or a second scoped `#[allow]` is likewise an
amendment to this file, and to the crate's own normative doc where it has
one.

Standalone crates that carry NO `unsafe` are not on the roster and do not
need to be, but two of them are worth naming because they look like they
would need one and do not: `td-boot` verifies a signature and kexecs
through a helper, and `td-install` writes GPT and FAT32 onto a block
device. Partition tables and filesystems are bytes at offsets, a device's
size is a `seek`, and its sector size is a file under `/sys` — so
`td-install/DESIGN.md`'s D8 asks for that to stay true, and where a later
increment cannot keep it (rereading a partition table needs `BLKRRPART`,
an ioctl) the amendment is made here first rather than found in a diff.

## Roster

| # | crate | syscalls |
|---|-------|----------|
| 1 | `td-kexec` | `kexec_file_load(2)`, `reboot(2)` |
| 2 | `td-netd` | `ioctl(2)` |
| 3 | `td-init` | ten — see [§3](#3-td-init--the-boot-glue-multicall); `ioctl` has four pinned requests |
| 4 | `td-login` | `setgroups(2)`, `setgid(2)`, `setuid(2)` |
| 5 | `td-svc` | `kill(2)` |
| 6 | `td-compositor` | `recvmsg(2)`, `close(2)`, `sendmsg(2)`, `getsockopt(2)` with fixed `SO_PEERCRED`, `fcntl(2)` with two value-pinned commands, `ioctl(2)`; plus one scoped client-side clipboard descriptor adoption |
| 7 | `td-util` | `ioctl(2)`, three pinned requests |
| 8 | `td-sh` | `umask(2)`, `rt_sigaction(2)` (disposition-only), `ioctl(2)` (three pinned requests), `poll(2)` |
| 9 | `td-jail` | `close(2)`, `ioctl(2)` with two value-pinned requests, `wait4(2)`, `kill(2)` with two fixed signals, `setsid(2)`, `capget(2)`, `capset(2)`, `pivot_root(2)`, `prctl(2)`, `mount(2)`, `umount2(2)`, `unshare(2)` with two value-pinned namespace sets, `prlimit64(2)` with one value-pinned resource, `seccomp(2)` with one value-pinned operation |
| 10 | `td-busd` | `recvmsg(2)`, `sendmsg(2)`, `getsockopt(2)` with two value-pinned options; plus a SECOND scoped allow for descriptor adoption — see [§10](#10-td-busd--the-session-bus-broker) |
| 11 | `td-profiler` | `close(2)`, `mmap(2)`, `munmap(2)`, `ioctl(2)` with four pinned requests, `setgroups(2)`, `setgid(2)`, `setuid(2)`, `clock_gettime(2)`, `perf_event_open(2)` |
| 12 | `td-portal` | `recvmsg(2)`, `sendmsg(2)`, `close(2)` for the private Wayland client's bounded descriptor transfer |
| 13 | `td-audio` | `ioctl(2)` with eleven value-pinned PCM requests, `poll(2)`, `getsockopt(2)` pinned to `SOL_SOCKET`/`SO_PEERCRED` |

The control-plane exception (`builder/src/sys.rs`) is described under The
rule above and is not part of this numbering. This is a program-role boundary,
not a build-provenance boundary: both the host-seeded builder and the later
target-built builder contain it. The latter is a distribution artifact used to
build td on td, but the current boot image does not ship builder and no
application runtime reaches this surface.

That host-side layer includes `renameat2(2)` with both directory descriptors
fixed to `AT_FDCWD` and flags fixed to `RENAME_NOREPLACE`. Its only caller is
the authenticated OSTree deploy materializer. Stable `std::fs::rename`
replaces an empty destination directory, so checking absence before a large
deploy is decoded leaves a writer race in which one complete result can
silently replace another. The raw wrapper makes absence part of the atomic
publication operation. It accepts two NUL-free paths but no caller-selected
directory descriptor or flags word. The materializer constructs both paths
below `/proc/self/fd/N` while it holds `N` as the opened destination parent, so
staging, publication, cleanup and parent synchronization retain one directory
identity even if its caller-visible path is retargeted. A second caller,
another flag, or another `*at` operation is an amendment here.

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

Before those credential calls, uid 1000 attempts to join its fixed delegated
session cgroup through safe filesystem writes and exact `/proc/self/cgroup`
readback. Failure is diagnosed without withholding the console; td-jail still
fails closed unless the placement succeeded. No caller selects the path, and
this adds no syscall to the unsafe roster.

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
is what keeps that surface at one. That includes cgroup-v2 delegation:
PID 1 mounts the hierarchy through the existing audited mount applet, while
td-svc creates cgroups, enables controllers at the root exception, and
changes ownership through safe filesystem APIs. This includes the `cpu`,
`memory`, and `pids` controllers; no controller operation adds an unsafe call.
`td-svc/DESIGN.md` is its normative
specification, recording both that and the invariants no compiler checks
(no `pre_exec`, liveness read from `/proc` rather than inferred from an
exit status, and a console that is neither skippable nor indefinitely
delayed).

## 6. `td-compositor` — the software Wayland server

The `td-compositor` software Wayland server, whose one `syscall5` body in
`td-compositor/src/sys.rs` carries `recvmsg(2)` for wl_shm, clipboard and
demo-client keymap SCM_RIGHTS reception, `close(2)` for a received descriptor
after safe duplication through `/proc/self/fd/N` or after its lifetime as an
exact clipboard endpoint, and `sendmsg(2)` for the
td-native demo client's wl_shm pool descriptor, the server's wl_keyboard
keymap descriptor, and the transport selftest. Stable Rust exposes no
stable ancillary-data API. The bounded parser records the first content or
policy refusal, including an unsupported kind, invalid rights-payload width,
or negative descriptor, but continues over valid framing. Any recognizable
SCM_RIGHTS descriptors that the kernel already installed are collected and
closed before the receive is refused. A structural framing error instead
closes everything collected through the last trusted boundary and returns
immediately because later records cannot be identified. The `syscall5` body
also carries `getsockopt(2)` once per
accepted private-portal connection, with level fixed to `SOL_SOCKET=1`, option
fixed to x86-64 `SO_PEERCRED=17`, and an exact 12-byte `[u32; 3]` result. The
wrapper refuses a different returned length and exposes only the uid word;
`server.rs` has one pinned caller and accepts only uid 1000 before allocating a
private client slot. Stable `std` exposes neither Unix peer credentials nor a
safe wrapper for this option. It also carries `fcntl(2)` with only `F_GETFL=3`
and `F_SETFL=4`: `conn.rs` temporarily adds x86-64 `O_NONBLOCK=0o4000` while
the bounded clipboard writer drains one destination, then restores the exact
prior status word. This closes the indefinite-write denial of service without
holding a registry lock across I/O; another command, caller, or flag is an
amendment here. The surface also carries `ioctl(2)` for
td-term's PTY, with FOUR value-pinned requests reached only from `pty.rs`:
`TIOCSPTLCK` (0x40045431) to unlock the slave, `TIOCGPTPEER` (0x5441) to
obtain it as a descriptor rather than by `/dev/pts/N` name, and
`TIOCSWINSZ`/`TIOCGWINSZ` (0x5414/0x5413) to publish a grid and read it
back. The readback is the point, as it is for `losetup`'s read-only flag:
nothing observable distinguishes a `TIOCSWINSZ` the kernel applied from
one it clamped or ignored, and a child that lays out its screen for a size
the terminal does not have is a terminal that looks broken with every test
green. Two more requests joined that roster for the ABSOLUTE pointer:
`EVIOCGABS(ABS_X)`/`EVIOCGABS(ABS_Y)` (0x80184540/0x80184541), reached only
from `input.rs`, which is a THIRD disjoint module on this surface rather
than a widening of either existing one. A tablet reports a position in its
own units, so mapping one to a screen needs the device's declared range, and
nothing but this ioctl reports it — `/sys` carries which axes exist but not
their bounds, and guessing would put the pointer somewhere other than where
the operator is pointing. It is asked at open, and again only where an
answer can have gone stale without a report saying so — a recovery, which a
`SYN_DROPPED` and a button overflow both reach, since each discards a report
this crate never sees the axes of; the SPAN is a property of the device rather
than of any report, so nothing asks per frame. A device that
refuses it is relative, which is the ordinary case and not an error. It asks
for three of the six words: the two bounds, and `value` — where the axis IS
at the moment it is asked, which is the only account of a device's position
before it has reported anything, and which the kernel needs because it omits
an axis whose value has not changed. The argument is pinned for the winsize
buffer's reason, arriving at it differently. `EVIOCGABS` copies
`sizeof(struct input_absinfo)` — 24 bytes, six `__s32` — through the pointer,
and unlike the winsize and termios calls it takes that length from the MINIMUM
of the REQUEST NUMBER's own size field and its own `sizeof`. Half of that is
protective, and the half that is not is the reason the two must be pinned
TOGETHER: an oversized number cannot make the copy longer, but a buffer
shortened without the number is 24 bytes written into less — an out-of-bounds
kernel write from code the compiler reads as safe. Both numbers encode that
same 24 and a test checks it against `ABSINFO_WORDS`, so the two cannot drift
apart. The axis is named by an ENUM
rather
than by a number at the call site, td-sh's `Disposition` shape: the two
requests differ in one nibble, and a caller free to compose one could
compose a third. Its buffer is an `[i32; 6]` for the winsize reason and
more sharply — `value`, `minimum` and `maximum` are three ADJACENT words of
the same type, so an index off by one is a well-formed position and range
that maps every report to the wrong part of the screen, with nothing
observable to say so.
The request roster is enforced in code, not only in a test — one
`ioctl` entry point refuses anything outside the six before issuing the
syscall — and the winsize argument is an `[u16; 4]` rather than a
`#[repr(C)]` struct so its field ORDER is a tested function; a swapped
rows/columns pair is a well-formed resize to a different size.
`TIOCGPTPEER`'s returned number is adopted through the SAME
`/proc/self/fd/N` reopen the file-backed received-descriptor path uses. That
route remains safe because the crate can reopen this file-backed descriptor by
identity. A clipboard transfer endpoint is the narrower exception to the
REOPEN: it may be a pipe or socket and SCM_RIGHTS requires its original
open-file description. `sys.rs` stores that raw number in `ReceivedFd`, whose
`Drop` calls the existing close wrapper. One source-pinned server adoption site
wraps it in an opaque `TransferEndpoint` before the runtime can route it and
does not use unsafe conversion. The separately pinned client adoption site
consumes a data-source endpoint through `ReceivedFd::into_file`; its second
scoped allow calls `File::from_raw_fd`, while `ManuallyDrop` prevents the raw
owner from closing after `File` assumes sole ownership. This conversion is
necessary because `/proc/self/fd/N` cannot reopen a socket and would create a
different open-file description where reopening succeeds. The one conversion
site is in `conn.rs`; no registry lock spans its eventual write. Deliberately
NOT in that surface:
framebuffer and evdev
READING (ordinary files — every input REPORT td acts on arrives as bytes off
a `File`; the one thing that does not is the POSITION a resync reads, since
`EVIOCGABS` answers `value` beside the bounds and the recovery frame
publishes it, which is a cursor move that came through this surface rather
than through a file), Unix socket setup and
byte I/O (`std`), mmap (wl_shm
pixels are copied with `FileExt`; the mapping hardware rendering will need is
anticipated below rather than present), device ownership (safe `td-seatd`), or
anything else the PTY needs — no termios call (the slave's kernel defaults
ARE the canonical-input policy), no `setsid(2)` or `TIOCSCTTY` (the child
gets its session from the declared `td-init` input's `cttyhack --stdin`),
and no `fork`/`execve`/`dup2` (`Command` plus `Stdio::from(File)` cover
all three). Nor, on the evdev side: `EVIOCGABS` for any axis but X and Y
(a pressure or tilt axis is not a place on a screen, and the request number
is composed from the axis, so serving one would mean composing them);
`EVIOCGBIT`/`EVIOCGNAME` (which axes a device HAS is answered by whether
`EVIOCGABS` reports a SPAN — the call itself succeeds for every axis on a
device that has an absinfo table at all, zeroed where it has none — and its
name by `/sys`); and `EVIOCGRAB`, which would
take a device away from everything else on the machine — td's compositor
owns the console outright, so there is nothing to take it from, and a grab
that outlived a crash would leave a keyboard nothing can type on.
`td-compositor/DESIGN.md` is the normative UI-stack
specification. Its confinement tests pin the allow count, assembly body,
syscall numbers, callers, and absence of unsafe from every other module;
adding another syscall or scoped allow is an amendment there AND here. The
four surfaces behind the one body are pinned to their modules —
transport to `client.rs`/`conn.rs`/`server.rs`, terminal control to
`pty.rs`, the absolute-axis range to `input.rs`, and private peer
authentication only to `server.rs`; no other module names `sys` at all.
`conn.rs` is the client
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
`wl_keyboard`, and RECEIVES its keymap descriptor, which §6 keeps inside
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

### The anticipated mapping class

Nothing in this crate maps memory, and the surface above is complete as
written. This subsection is an ANTICIPATION, recorded before the code exists
because `APPLICATIONS.md` §M asks for it: the DRM/KMS output backend that
replaces `/dev/fb0` cannot keep the no-mmap property — a dumb buffer has no
`write(2)` path, so pixels enter through a mapping of the card descriptor —
and the roster's current phrasing does not describe what that needs. Writing
the shape down now is what lets the author of that landing amend a plan
instead of bending around a rule.

It is a new CLASS rather than a thirteenth syscall on an existing one. Every
surface in this file is a syscall-instruction one-shot: the unsafe begins at
the instruction and ends when it returns, and what returns is a number that
safe code then owns. §11's perf ring is the one mapping already rostered and
it is not this shape either — `Ring` copies bytes out before they leave the
module, so no pointer and no borrowed region escapes `sys.rs`, which is
exactly why "owns the pointer accesses into the perf ring mapping" was
enough to describe it. A scanout target inverts that. The mapping IS the
destination: the renderer writes pixels INTO it, so a slice over
kernel-owned memory must cross a module boundary and stay valid while the
frame is drawn. Neither "one instruction" nor "nothing escapes the module"
covers a lifetime-carrying mapping, and a class nobody named is a class
somebody adds quietly.

What the amendment must budget when it lands:

- `mmap(2)` and `munmap(2)` join surface #6. `mmap` is pinned to a shared
  read/write mapping of one owned card descriptor at the kernel-reported
  offset and length of a dumb buffer this crate created; `munmap` receives
  only the owned pair. That is §11's pinning, for §11's reason.
- One region type owns the pair. Its length is the length the mapping was
  created with, held in the type rather than recomputed at each use. A length
  that can drift from its mapping is an out-of-bounds write the compiler
  reads as safe — the `EVIOCGABS` buffer's failure mode, one layer up, and
  the reason that request number and `ABSINFO_WORDS` are pinned together.
- `Drop` unmaps, nothing else does, and the type is not clonable: the
  region's lifetime is the value's, which is the property its name claims.
- The borrowed slice is the only way out. No raw pointer reaches the
  renderer, and the borrow cannot outlive the guard — the `ReceivedFd` and
  `BorrowedFd` guard style this section already uses for descriptors,
  applied to bytes instead.
- Confinement tests pin the allow count, both syscall numbers, the single
  owning module, the single construction site, and the length, exactly as
  the descriptor guards are pinned today.
- A dmabuf that must be CPU-read needs `DMA_BUF_IOCTL_SYNC` bracketing the
  access. That is a seventh value-pinned `ioctl` request, not a widening of
  the entry point to caller-supplied requests, and it is named here so the
  count is right when it arrives rather than discovered in a diff.

None of this is authorization, and none of it is in the code: the
confinement tests still pin the absence of `unsafe` from every module but
`sys.rs`, `sys.rs` carries neither mapping syscall, and no source in the
crate names `mmap` at all. What the landing gains is that its shape was reviewed before it was written; what it
still owes is the ordinary amendment to this file, to the roster table, and
to `td-compositor/DESIGN.md` §4 in the same commit. What this subsection
settles is only that the answer is not "refuse `mmap` forever", which is the
one way td could paint itself out of hardware rendering.

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
virtual fd table that stands in for `dup2`, and process substitution —
is reachable through safe `std`, which is what keeps that surface at
four.

Process substitution is the one of those that had to be argued rather
than observed, so the argument is written down here. `<(cmd)` expands to
a PATH an arbitrary program opens for itself, and in ash that path is
`/dev/fd/64`: a descriptor the shell duplicated with `F_DUPFD` and the
exec'd command INHERITED. Every descriptor safe `std` opens is
close-on-exec — `io::pipe`, `File::open`, and `try_clone_to_owned`,
which is `F_DUPFD_CLOEXEC` — and nothing in safe `std` clears that flag,
so the inheriting spelling needs `fcntl(2)` as a FIFTH syscall. It was
built that way, with `F_SETFD` pinned by value and its argument pinned
to zero, and then removed — not for taste. The flag belongs to the
DESCRIPTOR and the descriptor table belongs to the PROCESS, while this
shell's subshells are threads, so clearing it hands the end to every
`execve` the shell makes next, the body's own included, where a forking
shell's body cannot see a descriptor its table predates. Two hangs came
of it, both measured: `seq 3 > >(tac)` gave `tac` a copy of the write
end of its own input, and `head -n 1 <(seq 100000)` gave `seq` a copy of
the read end of its own output. td-sh names the descriptor by PID
instead — `/proc/<pid>/fd/N`, which the command OPENS rather than
inherits. That needs no flag, leaks no descriptor into any child, reads
the same in both directions, and costs only the text of the word.

## 9. `td-jail` — the application sandbox

The landed `td-jail` surface carries exactly FOURTEEN syscalls on x86-64
through one `syscall5` body: `unshare(2)`, `close(2)`, `ioctl(2)`,
`wait4(2)`, `kill(2)`, `setsid(2)`, `mount(2)`, `umount2(2)`, `pivot_root(2)`,
`capset(2)`, `capget(2)`, `prctl(2)`, and `prlimit64(2)`, plus `seccomp(2)` with exactly
one operation:
`SECCOMP_SET_MODE_FILTER`=1 and flags zero.
The unshare wrapper accepts only the two compiled namespace sets the
application design permits:
`CLONE_NEWUSER|CLONE_NEWNS|CLONE_NEWPID|CLONE_NEWUTS`, with
`CLONE_NEWNET` either absent for a declared shared network or present for
an isolated one. There is no caller-provided flags word.
The ioctl wrapper accepts no caller-provided request or interface. It uses
only `SIOCGIFFLAGS`=0x8913 and `SIOCSIFFLAGS`=0x8914 over a pinned 40-byte
`ifreq` naming `lo`, preserves the kernel-returned flags while adding `IFF_UP`,
and reads `IFF_UP` back. A safe `std` UDP socket is only the ioctl carrier and
is dropped before inherited descriptors are swept.

The argv0-selected launch parent first spawns and waits on a later-born stage
1, because that child cannot already be a process-group leader. Stage 1 sets
`PR_SET_PDEATHSIG=SIGKILL` and reads its exact parent back from procfs to close
the death-before-set race. A child whose controlling-terminal field is zero
and whose process group is that exact parent's pid preserves and reads back
the group, keeping a supervisor's stop containment intact. Every other child
issues `setsid(2)` and requires its process group and session to equal the
returned id while the controlling-terminal field is zero. This happens before
authority resolution, registration, cgroup creation or namespace work. Parent
death therefore still kills stage 1, whose existing proof-pipe and cleanup
protocols tear down stage 2 and its cgroup. The new `setsid(2)` caller and use
of the existing parent-death operation are the application-session amendment;
no syscall or operation was added.

Stage 1 is single-threaded when it issues the unshare call, writes `setgroups`
deny before the identity gid map, reads both maps back, and checks that
the user, mount and UTS namespaces changed. Application launch derives the
network choice from the authenticated permission policy: isolation additionally
requires that the network namespace changed and brings up loopback, while
`shared=network` requires that the network namespace stayed identical and skips
the loopback ioctl. The probe always selects and reads back isolation. It
enumerates `/proc/self/fd` and closes every inherited descriptor above 2 before
opening the proof pipe. The
`close(2)` entry is a reviewed amendment: safe `std` cannot take
ownership of an arbitrary inherited descriptor, while leaving one open
would preserve an old-root handle across `pivot_root`. An iterator's
already-closed directory descriptor is the sole tolerated `EBADF`.
The build-host recipe leaks a real caller fd 9. Target `td-sh` cannot
forward virtual descriptors above 2, so the target oracle instead asks
the probe to open `/proc/self/status`, transfer its live descriptor with
safe `IntoRawFd`, verify that it is above stderr, and feed it through the
same sweep before stage 2 proves that only stdio survived.

The mount wrapper is reached only with the compiled probe plan or the closed
application plan resolved from a builder-authenticated spec. Stage 1 makes the
tree private, replaces its scratch `/tmp` with a fresh tmpfs, builds another
tmpfs as the root, binds only null/zero/full/random/urandom as individually
read-only mounts into a fresh nosuid/noexec `/dev`, mounts fresh devpts and
`/dev/shm`, mounts fresh `/tmp` and `/var/tmp`, and remounts `/dev` read-only.
The application plan also mounts an 8 MiB nosuid/nodev/noexec `/etc` tmpfs.
It binds only the closed runtime-configuration allowlist and applies the
filesystem-grant hardening loop recursively to every selected directory row;
synthesizes the fixed account, group, host, hostname, NSS and validated
per-application machine-id files; and binds the resolved pinned CA file
read-only. A direct
bounded resolver file is bound read-only only when authenticated policy shares
the network and the file exists. The `/etc` tmpfs is then remounted read-only.
Probe mode overlays the current executable as one read-only, nosuid, nodev,
executable file bind inside the otherwise-noexec `/tmp`, and source-identity
checks that bind before detaching the old root; application mode never creates
it. Application filesystem sources are canonicalized outside the namespace;
reserved aliases and overlapping allowed grants are refused, deny
intersections remove the containing grant before creation, and source
type/device/inode are checked before and after the bind. Regular files require
exactly one hardlink throughout the transition. Every source, target, and
visible nested mount is preflighted before creation. Mountinfo device/root
identity comparison covers every visible mount below reserved trees, home
trees, denied sources, and other allowed sources, closing bind-mount aliases
that path canonicalization cannot see. Grant targets that overlap fixed
application state, `/oldroot`, `/root-write-probe`, or compiled jail mounts are
refused rather than replacing their readbacks. Directories use recursive
binds. Every mountinfo row below each
target is remounted `nosuid,nodev,noexec`, deepest first, and every row is also
read-only for a read-only grant; both stages read the complete policy back.
Only the jail target and typed mode cross into stage-2 argv. An absolute target
can have the same spelling as its host source, but stage 2 never interprets it
as mount authority. Stage 2 mounts a
fresh procfs for the PID namespace it inhabits while the old procfs is
still visible (the kernel's `mount_too_revealing` rule requires that),
pivots, changes directory to `/`, detaches the old root, removes its
mount point, and remounts the new root read-only. It reads mountinfo back,
requires only PID 1 in procfs, checks the exact root and device entries,
exhaustively checks every grant-created scaffold outside the dynamic private
home, checks device numbers, modes and every device-bind flag, and proves the
base root, `/dev`, and device binds are read-only while procfs, devpts,
bounded `/dev/shm`, `/tmp`, and `/var/tmp` carry their compiled writable,
no-exec flags and size ceilings. The application action additionally reads
back its bounded private `/run` and exact selective `/etc`, including every
runtime nested mount, synthesized contents, CA PEM shape, conditional resolver
mount, size ceiling and `EROFS` write refusal. Read-only bind metadata does not
prevent the compiled character devices from performing their device
operations; the probe writes to `/dev/null` after the remounts. The mountinfo
`ro` row is the evidence that
the bind remount itself succeeded; the `EROFS` write probes separately prove
there is no writable route at `/app` or `/usr`, even when their source
superblock is already read-only. Writable probes use an unpredictable
create-new path and unlink it before writing through the open descriptor, so
ordinary completion and every failure after unlink leave no state litter. They
do not sweep prefix-matching residue because an application may own such a
lookalike. Interruption in the single create/unlink syscall window can leave the
empty `.td-jail-write-probe-*` name behind in a writable application-state or
granted host directory. A same-uid process can also rename that name during the
window, but it already has direct authority over each writable probe target.

`capset(2)` and `capget(2)` use capability ABI v3 and a compiled
two-word structure. `prctl(2)` has exactly EIGHT operations:
`PR_SET_PDEATHSIG`=1, `PR_GET_DUMPABLE`=3, `PR_SET_DUMPABLE`=4,
`PR_CAPBSET_DROP`=24, `PR_CAPBSET_READ`=23,
`PR_SET_NO_NEW_PRIVS`=38, `PR_GET_NO_NEW_PRIVS`=39, and
`PR_CAP_AMBIENT`=47;
the last has exactly THREE sub-operations, `IS_SET`=1, `RAISE`=2 and
`CLEAR_ALL`=4. Stage 1 preserves its current effective and permitted
sets while making only `CAP_SYS_ADMIN` inheritable, clears then raises
that one ambient bit, drops every capability named by the kernel's
bounded `cap_last_cap`, and reads all of it back. A safe `Command` spawn
creates stage 2;
before releasing it, stage 1 verifies `/proc/<child>/ns/pid` differs
from its own original PID namespace. Stage 2 refuses unless its
namespace PID is 1 and a fresh 32-byte proof arrives on the stdin
descriptor stage 1 explicitly installed. Stage 2 installs a `SIGKILL`
parent-death signal before mounting. On the application-launch path, after the
filter readback, a watcher retries an interrupted read and treats proof-pipe
EOF, unexpected data, or any other read error as fatal, closing the
set-before-check race without relying on a parent PID invisible in the new
namespace. Before
mounting it requires
effective, permitted, inheritable and ambient to equal exactly
`CAP_SYS_ADMIN`, the bounding set to be empty, and the syscall and
`/proc/self/status` readbacks to agree. After the mount readback, stage 2
clears ambient first, empties effective, permitted and inheritable, and
requires all five capability rows to be zero.

`prlimit64(2)` is reached only through `set_and_require_data_limit`. Both
calls fix pid to self (`0`) and resource to `RLIMIT_DATA`=2. The first sets
the soft and hard limits to the same authenticated `memory-max` byte count;
the second supplies no new limit and reads the exact two-u64 structure back.
Stage 2 performs both before it creates its watcher thread or application
child. No caller selects a pid, resource, unequal soft/hard pair, or old-limit
destination.

After capability removal, stage 2 validates the compiled constant cBPF
program, sets and reads back no-new-privileges, installs that exact program,
and requires `/proc/self/status` to report `NoNewPrivs: 1` and `Seccomp: 2`.
It remains single-threaded through this flags-zero installation. Only after the
readback does it create the liveness watcher, which therefore inherits the
filter; the embedded-source test pins that ordering.
The filter's instruction count is derived from its array, and a safe validator
refuses unknown opcodes, offsets, actions, out-of-range jumps, wrong lengths,
more than the kernel's 4096 instructions, and programs without a final return
before the raw syscall sees them. The program is policy, never caller data.
Its test interpreter executes the exact
array over every rostered syscall and the argument-sensitive rules. A separate
td-GCC-built, non-shipped C probe recipe consumes a bounded serialized copy and
checks the real kernel's errno and kill behavior on an unconstrained build host
and in the QEMU target fixture. QEMU builds that helper directly, so host-policy
smoke tests cannot prevent the target oracle from booting. A host with no
filter but inherited no-new-privileges
may skip the impossible pre-install negative leg. One with an inherited filter
validates the artifact but skips behavior because filters cannot be removed and
the outer policy could alter results. The target invocation allows neither case
and requires both initial states to be zero.
QEMU copies both inputs into a root-owned, non-writable `/run` tree before
dropping identity, gates guest health on the exact result, and recognizes only
an exact marker line. Reaper descendants require the installed restriction
and filter readbacks too.

`wait4(2)` is pinned to pid -1, a null rusage pointer, and either zero or
`WNOHANG`. `kill(2)` is likewise closed: its pid is always -1 and its signal
is exactly `SIGTERM` or `SIGKILL`; callers choose only between two
argument-free wrappers. After the direct application exits, PID 1 sends
`SIGTERM`, polls and reaps for two seconds, then repeatedly sends `SIGKILL`
while polling and reaping for at most two more seconds. If PID 1 has not yet
observed `ECHILD`, it exits with an error and kernel PID-namespace teardown
supplies the final hard stop. Reap accounting is fixed-size rather than
proportional to a hostile child table.

Through the probe-only single-file bind, the transition probe launches a
zero-capability child that creates a grandchild and
exits without waiting, then requires PID 1 to reap both the direct child
and the reparented orphan with successful raw statuses under one bounded
deadline. Only after `ECHILD` makes the report pipe nonblocking does it
read the orphan PID and verify the exact collected set. It then creates two
long-lived reparented orphans: the production survivor cleanup must reap the
first exact PID with `SIGTERM`, and its shared hard-phase implementation must
reap the second exact PID with `SIGKILL`, both under the installed filter.
Namespace exit removes the probe-only mount.
The same transition now has one closed application action. A `/bin` symlink
selects a name by argv[0]; safe Rust reads the immutable image configuration,
sorted registry and canonical builder-owned spec, then accepts only the
builder-authenticated spec with optional shared network, the Wayland socket and
the closed typed filesystem subset. The runtime parser closes the spec grammar
and uses the shared permission type's exhaustive
network-plus-Wayland-plus-filesystem-and-resources predicate; adding another
policy field cannot silently widen this rung. The image compiler
selects the fixture's empty runtime. Stage 1 binds
authenticated package/runtime trees read-only, the exact compositor socket
read-only, application-owned persistent state read-write, one application-owned
volatile runtime directory read-write, and only the resolved filesystem
grants. Package and runtime binds are builder-authenticated. State sources are
canonicalized and checked for the application uid, private mode, source device
and subtree root before stage 2 can enter them. Grant sources are canonicalized
to regular files or directories and refused when they alias a reserved tree.
The only exception is a source physically and strictly below the exact real
home overlapping a reserved identity that strictly contains that home; the
source cannot carry the reservation's siblings. Exact state and whole-home
aliases remain refused in the shipped topology; the backing-volume
reservation refuses every whole-home alias there. Sources
are pinned by type/device/inode across the bind. Thus the shipped
`/home -> /var/home` alias and
mountinfo's escaped path fields resolve to the same identity the kernel
records. This is not isolation from an unsandboxed same-uid process, which can
replace its own state path during launch; that process already has direct
authority over all application state. The
volatile runtime bind carries the fixture's readiness socket across the
otherwise-private `/run`; safe Rust on the host probes the per-application
end before trusted QEMU evidence. Stage 2
performs the same mount/capability/filter readbacks before spawning the entry.
Before stage 2 is released, stage 1 creates one direct child of the delegated
`/sys/fs/cgroup/td-user-1000` root, writes and exactly reads back
`memory.high`, `memory.max`, `memory.oom.group=1`, and `pids.max`, and moves
the blocked stage-2 pid through `cgroup.procs`. The same safe filesystem path
writes and exactly reads `cpu.max`; diagnostics read the required CFS-bandwidth
rows from `cpu.stat`. Stage 2 re-reads its exact
unified membership from procfs before setting `RLIMIT_DATA`. The cgroup
filesystem remains masked by the mount plan, so the application cannot move
itself into a sibling. Before the leaf exists, stage 1 starts a cleanup
bootstrap with cwd `/` and retains a close-on-exec pipe. The bootstrap issues
`setsid(2)`, proves its new session and zero controlling-terminal field, and
only then spawns the watcher. The watcher issues the same proved detachment and
sends one readiness byte. It was therefore never present in a terminal-member
snapshot that could still signal the bootstrap and stage 1 after readiness.
Stage 1 does not create the leaf until it receives that byte.
After PID 1 and its descendants are reaped, stage 1 reads `memory.events`,
`memory.peak`, and `cpu.stat`, releases the leaf to the watcher, and closes the pipe. Normal
exit, signal, and abort therefore use the same bounded drain and removal. The
watcher's detached session survives both process-group and controlling-terminal
shutdown. Controller operations remain safe filesystem I/O.
It preserves the direct entry's status while bounded survivor cleanup drains
the namespace to `ECHILD`; the transition probe exercises both natural orphan
reaping and forced survivor cleanup. No caller supplies a mount, filter,
namespace or raw syscall argument. The application entry, environment and
argv cross the authenticated stage boundary as inert argv data and are
grammar-revalidated there; binding them to the authenticated spec remains
stage 1's job. Stage 2
itself execs with an empty environment; only the final ordinary application
child receives the compiled environment after the capability and filter
readbacks. Stage 2's argv includes the one-use proof token, entry, environment,
filesystem jail targets and typed modes, and application arguments. Before
launch it becomes non-dumpable, so procfs
denies the unprivileged child PID 1's `fd`, `exe`, and `environ` entries but
continues to expose `/proc/1/cmdline`. None of the stage-2 argv values may be a
secret; the child's own arguments and environment are likewise
application-visible. The child cannot
reopen PID 1's executable or proof descriptor, no td-jail binary exists in its
mount plan, and the installed filter refuses the namespace transition. In
launch mode, stage 2 receives the
proof pipe on stdin, null
stdout and one bounded diagnostic pipe on stderr. It sets and reads back PID 1
as non-dumpable, pre-opens the application's null stdin/stdout/stderr, and
then spawns it. PID 1 retains the proof reader for its filtered liveness
watcher and the diagnostic writer through the application's final status.
Spawn and post-spawn failures therefore still reach stage 1, while procfs
denies the same-UID application access to PID 1's descriptors. A separate
trusted unit probes the post-frame readiness socket before emitting the QEMU
marker; it is not deployment-success authority. The application inherits no
descriptor through which it can emit the marker, flood the console, or alter
terminal state. The readiness socket is same-UID evidence rather than an
authenticated peer: another uid-1000 process can satisfy the probe, so this is
an image-test oracle, not hostile-payload attestation.

There is likewise no `fork`, `pre_exec`, `clone`, `setns`, or caller-
supplied namespace, mount set, or BPF program. A fifteenth syscall, a ninth
prctl operation, a second seccomp operation or nonzero seccomp flag, a fourth
ambient sub-operation, or a third unshare flag set
is an amendment here.

## 10. `td-busd` — the session bus broker

`td-busd` carries THREE syscalls through one `syscall5` body in
`td-busd/src/sys.rs`: `recvmsg(2)` and `sendmsg(2)` for the SCM_RIGHTS
descriptor passing D-Bus requires of a broker, and `getsockopt(2)` for
exactly TWO value-pinned options at `SOL_SOCKET`, `SO_PEERCRED` and
`SO_PEERPIDFD`. The body is
`syscall5`, like td-compositor now that its private listener also uses
`getsockopt`; the two message calls use three of the five arguments and pass
zero for the rest.

`SO_PEERCRED` is read because the EXTERNAL mechanism in `auth.rs` has
nothing else to check a client's asserted uid against, and the kernel's
answer is the only account of it that the client cannot write. Stable Rust
does not expose it: `UnixStream::peer_cred` is gated behind
`peer_credentials_unix_socket` (rust#42839) and has been since 2017, so
this is an unstable-API gap rather than a preference.

`SO_PEERPIDFD` (value 77, Linux 6.5 and later; td pins a 7.x kernel) is
read because `SO_PEERCRED`'s pid is a NUMBER and the broker's identity
model needs a PROCESS. The kernel samples peer credentials at `connect(2)`
and holds a `struct pid` reference; that keeps the struct alive and does
not reserve the number, which `free_pid` returns to the allocator once the
connecting process is reaped. A peer that connects, hands its socket to a
sibling through SCM_RIGHTS and exits can therefore have its pid recycled
before the broker reads it, and any `/proc` walk built on that number then
describes somebody else — the dangerous direction being a confined peer
resolving unconfined. `SO_PEERPIDFD` returns a descriptor naming the
process itself, and `td-busd/src/lineage.rs` reads
`/proc/self/fdinfo/<pidfd>` before and after its walk: for as long as the
kernel still reports a pid there, the process has not been reaped, so its
number was never free, so every `/proc` read taken in between belongs to
it. Stable `std` has no spelling of this option at all, so unlike
`SO_PEERCRED` it is not even an unstable-API gap.

The pidfd is a liveness ORACLE and not a reservation. The distinction is
worth stating because an earlier draft of `APPLICATIONS.md` §D got it
wrong: holding a pidfd across a reap does not stop the pid NUMBER being
handed out again, which is measured rather than reasoned about — a held
pidfd saw its number reused after some thirty thousand forks. What the
descriptor buys is the ability to ask, and the answer is exact: the pid
while the process is alive, the pid while it is a zombie, and `-1` once it
has been reaped.

Neither wrapper takes a caller-supplied level or option — there is no
general `getsockopt` here, only `peer_credential` and `peer_pidfd` — and
each checks the length the kernel writes back before believing what was
written. A short `ucred` would leave a zeroed uid that reads exactly like
`root`; a short `SO_PEERPIDFD` write is a partly written `i32`, and
adopting an arbitrary descriptor number this process does not own would
hand the broker somebody else's socket to call the peer's identity.

`MSG_CMSG_CLOEXEC` is requested on every receive so a forwarded descriptor
cannot leak through a concurrent `exec`, and `MSG_NOSIGNAL` on every send
so a peer that left mid-write is an `EPIPE` return rather than a signal.
Rust's runtime already ignores SIGPIPE at start-up, so the second flag is
belt-and-braces — but it makes the disposition a property of the call
rather than of process-wide state, which is worth the zero it costs.

### The second scoped allow

This surface has two `#[allow(unsafe_code)]` of different shapes: the `syscall`
instruction, and `OwnedFd::from_raw_fd` for adopting a descriptor the kernel
has already installed. Section 6 now uses the same adoption shape only when a
native clipboard source must consume one exact endpoint; it still re-derives
file-backed descriptors through `/proc/self/fd/N` and keeps server-routed
clipboard endpoints in a small safe owner whose `Drop` reaches its rostered
close syscall. Taking the allow here still needs the reason that narrower
client conversion does not cover a general broker.

It is this: the reopen trick works on the compositor's wl_shm pool files and
keymap memfds, but not on its clipboard pipes and sockets; those stay in the
bespoke owner until the compositor forwards or drops them. A broker forwards
whatever an application chooses to send, and
for a socket `open("/proc/self/fd/N")` fails with ENXIO — a socket inode
has no open method — while an `eventfd` or other `anon_inode` fails with
EACCES. Both are ordinary D-Bus payloads. A broker built on the reopen
would therefore refuse to forward exactly the descriptors applications
most want forwarded, and would do it with an errno that looks like a
permissions bug. Reopening also produces a NEW open file description, so
even where it succeeds the receiver gets independent offset and status
flags rather than the shared description SCM_RIGHTS defines; for a pipe
handed between two applications that is a semantic change, not a
different route to the same place.

The adoption appears ONCE, in one private function, and a confinement test
pins that count. Two callers reach it — `receive` and `peer_pidfd` — and
the count that matters is the SITE, because a second adoption site is a
second place to get the ordering rule wrong.

That discipline is an ordering rule rather than a condition, and **the two
callers order it OPPOSITELY**. This is the most confusable thing about this
surface, so both directions are stated here and each is pinned by a test.

`receive` adopts BEFORE it refuses. EVERY descriptor a `recvmsg` returns is
adopted into an `OwnedFd` before `MSG_CTRUNC` is examined, before the count
is compared against the message's UNIX_FDS field, and before any parsing.
The kernel installed those descriptors whether or not the message that
carried them is one this broker will accept, so a check that returns early
ahead of the adoption leaks a descriptor per malformed message — a remote
fd-table exhaustion reachable by a client that only has to be malformed,
not authenticated. There, refusal happens after ownership, never instead of
it.

`peer_pidfd` refuses BEFORE it adopts, and **what that buys is narrower
than a first version of this paragraph claimed.** It keeps `adopt` from
ever seeing a negative number: `OwnedFd` has a validity niche and
constructing one from `-1` is unsound on its own terms, independently of
which descriptor would be closed. That is the whole of it.

It does NOT prevent adopting a descriptor this process never received, and
the first version said it did. A short write cannot produce a foreign
descriptor here: `number` starts at `-1`, this surface is x86-64 by
construction, and a partial write fills from the low byte up — so any
partial answer stays negative and is refused whichever way the two
statements are ordered. What CAN produce one is a wrong option number,
since a different option answers a whole `i32` of something else; a
mutation to `SO_PASSPIDFD` yields `0`, and adopting stdin aborts the process
on the double close. Neither the ordering nor the length check catches that.
The value pin at the constant and its confinement test are what catch it,
and the first version of this paragraph narrated that abort as the thing
the reordering fixed. It fixed nothing about it.

The length is checked because `getsockopt` clamps to whatever the caller
asks for, and a short ask is answered short. Measured against the kernel
rather than inferred: ask for two bytes and it writes two, reports two, and
installs the pidfd regardless — a descriptor whose number was never
delivered and which therefore cannot be closed. So asking for exactly
`sizeof(int)` is the load-bearing part, the check is what notices a kernel
that answers otherwise, and "installed alongside a short length" is a real
kernel behaviour rather than the self-contradiction an earlier draft called
it. Such a leak would also not be bounded by the connection ceiling:
connections come and go, so one leak per accept exhausts the table over
time rather than plateauing.

### Deliberately not here

`close(2)` is NOT on this roster, and its absence is the point of taking the
adoption: `OwnedFd` means `std` performs every close, so the crate has no close
of its own. Section 6 needs one because its server retains exact endpoints in a
safe raw owner and it must also dispose the raw number behind every reopened
file-backed descriptor. Its client conversion narrows that owner only after a
source `send`. This broker instead adopts every admitted descriptor immediately
and pays for its second allow by giving back a syscall.

There is no `poll(2)` or `epoll_*`. Stable `std` exposes neither, and
readiness multiplexing would be a FOURTH rostered syscall bought for a
scalability the session bus does not need; `td-busd` serves one connection
per thread and blocks in `recvmsg`, which is the design decision recorded
in `td-busd/src/transport.rs` rather than an oversight. There is no
`socket(2)`, `bind(2)`, `listen(2)`, `accept(2)`, or `connect(2)` — safe
`std` `UnixListener`/`UnixStream` do all of those, and the raw layer only
ever borrows a descriptor `std` already owns. There is no `mmap`, no
`fcntl`, and no credential-changing call of any kind: this broker reads a
uid and never assumes one. A fourth syscall, a THIRD socket option, or a
third scoped allow is an amendment here and to `APPLICATIONS.md` §D in the
same landing.

## 11. `td-profiler` — continuous system observation

The profiler carries exactly NINE syscalls through one x86-64 `syscall6`
instruction: `perf_event_open(2)`, `mmap(2)`, `munmap(2)`, `ioctl(2)`,
`close(2)`, `clock_gettime(2)`, `setgroups(2)`, `setgid(2)`, and `setuid(2)`.
One module-level allowance covers that instruction, atomic acquire/release
accesses to the kernel-owned perf ring head and tail, bounded metadata-header
reads used to validate the data offset and size, and the bounded copy from the
data mapping after the acquired head. `Ring` owns the mapping, checks the
kernel-returned data offset and power-of-two size against the exact mapped
extent, refuses an overrun or malformed record, copies bytes before they leave
the module, advances `data_tail` with the acquire/release ordering the ABI
requires, and unmaps on drop. No raw pointer reaches collection or reporting
code.

`perf_event_open` receives one of two compiled 128-byte attribute layouts. The
metadata event is `PERF_COUNT_SW_DUMMY` and pins mmap, mmap2, comm+exec, task,
`sample_id_all`, `use_clockid`, and the identifier/TID/time/CPU trailer. The raw
schema accepts context-switch records for forward compatibility, but this
layout does not request them until off-CPU attribution is modeled. The sampling
event is `PERF_COUNT_SW_CPU_CLOCK`, frequency mode, the same clock and identity
fields plus IP and callchain, and excludes kernel and hypervisor execution.
Both are system-wide for one compiled CPU number, with pid and group fd -1 and
only `PERF_FLAG_FD_CLOEXEC`; the caller supplies the rate but no event type,
sample layout, flags word, pid, or group.

The `ioctl` entry point is private to this module and is reached with exactly
four pinned requests: `PERF_EVENT_IOC_ENABLE`, `PERF_EVENT_IOC_DISABLE`,
`PERF_EVENT_IOC_SET_OUTPUT`, and `PERF_EVENT_IOC_ID`. SET_OUTPUT redirects the
CPU-clock event into that CPU's metadata ring; ID writes one live `u64`; enable
and disable take argument zero. `mmap` is exactly shared read/write over the
metadata descriptor at offset zero for one metadata page plus a power-of-two
data page count. `munmap` receives only the owned pair. `close` receives only
the two event descriptors created by this module.

`clock_gettime` is pinned to `CLOCK_MONOTONIC`, the clock selected in both event
attributes, so startup fences, ring records, and capture coverage share one
domain. The credential calls occur exactly once in the security order
`setgroups(0, NULL)`, `setgid(profiler)`, `setuid(profiler)`, after event setup
and startup inventory and before sampling is enabled. Collection reads
`/proc/self/status` through safe `std` and refuses unless all four uid/gid slots
and the empty supplementary group list agree.

Deliberately absent are ptrace, BPF, sockets, `openat`, `statx`, caller-supplied
ioctl requests, another perf event type, kernel samples, and a signal handler.
Ordinary capture files, `/proc` and `/sys` reads, fsync, and atomic rename use
safe `std`. A tenth syscall, a fifth request, another event layout, a second
scoped unsafe allowance, or any pointer escape from `sys.rs` is an amendment
here and in `td-profiler/DESIGN.md`.

## 12. `td-portal` — the private Wayland dialog client

The FileChooser client carries exactly THREE syscalls through one x86-64
`syscall5` instruction in `td-portal/src/sys.rs`: `recvmsg(2)`, `sendmsg(2)`,
and `close(2)`. Safe `UnixStream` carries descriptor-free Wayland messages.
The raw layer exists only because `wl_keyboard.keymap` and
`wl_shm.create_pool` carry one SCM_RIGHTS descriptor in opposite directions.
It borrows the stream and the descriptor it sends; no socket creation,
connection, path lookup, or caller-selected ancillary type enters the surface.

Every receive requests `MSG_CMSG_CLOEXEC`, parses at most 128 ancillary bytes,
accepts only `SOL_SOCKET`/`SCM_RIGHTS`, and records a content or policy refusal
while valid framing remains walkable. It closes every recognizable installed
descriptor before returning that refusal. A structural framing error closes
everything collected through the last trusted boundary and returns because
later records cannot be identified; Linux closes descriptors that do not fit
the supplied control buffer. The dialog connection admits at most
eight queued descriptors. Its only consumer removes one exact descriptor for
`wl_keyboard.keymap`, reopens `/proc/self/fd/N` as a safe `File`, and then calls
the rostered close on the received number on both success and reopen failure.
The pinned private compositor sends a regular keymap backing file, so this
narrow reopen preserves the required readable bytes without the second scoped
raw-descriptor adoption that the general D-Bus broker needs. The exact keymap
size and contents are checked before input is accepted.

The send path emits exactly one descriptor with a 24-byte control buffer and
fixed `SOL_SOCKET`/`SCM_RIGHTS`; its only production caller sends the
FileChooser's unlinked 0600 regular backing file in `wl_shm.create_pool`.
`sendmsg` pins `MSG_NOSIGNAL` so a compositor departure is an error rather than
a process-wide signal, and carries the first bytes and descriptor atomically.
A short body write continues through safe `UnixStream`; the descriptor is not
sent a second time.
`close_raw` is private to the raw module. Its crate-visible disposal helper
still takes descriptor numbers because safe Rust cannot own a descriptor
installed by `recvmsg` without another scoped adoption allowance. The four
production call sites pass only numbers returned in that connection's
`Received` value or moved into its bounded pending queue; confinement tests pin
those call sites. That provenance is a source-level contract, not something
the helper's raw-fd type can express.

Confinement tests inventory every portal source file, pin the three syscall
numbers, the single instruction and scoped allowance, both ancillary constants,
`MSG_CMSG_CLOEXEC`/`MSG_NOSIGNAL`, the one send, receive, and reopen call site,
and all four refusal/drop routes.
There is no general descriptor-forwarding API, raw descriptor owner, mmap,
fcntl, ioctl, credential call, or network socket. A fourth syscall, a second
ancillary kind, a second scoped allowance, another production caller, or raw
descriptor adoption is an amendment here and in `APPLICATIONS.md` in the same
landing.

## 13. `td-audio` — the ALSA playback back end

`td-audio` is the audio daemon `APPLICATIONS.md` §K designs: the PCM back
end, the mixer above it, a tone fixture, and the PulseAudio-protocol server
that serves clients over a Unix socket. The dedicated `audio` account §K.5
specifies is a rung of its own and lands separately; nothing here creates an
account or a directory.

§I's rung 25 and §K.5 both call this "surface #11". That was already
`td-profiler` when the audio reversal reached the ladder, and `td-portal`
has since taken #12, so both are corrected to #13 in the landing that adds
this section. A surface number that names a different crate is the one
error this roster cannot absorb, because the roster is the number.

**Three syscalls.** `ioctl(2)`, carrying exactly the eleven PCM requests
§K.4 pins; `poll(2)`, which is how a writer waits for the device to make
room and how the daemon waits on its clients and its device together; and
`getsockopt(2)`, pinned to `SOL_SOCKET`/`SO_PEERCRED` with a 12-byte
`struct ucred`, which is how §K.5 authorizes a peer. Everything else rides
`std`: the PCM node is an ordinary `std::fs::File` opened
`O_WRONLY|O_NONBLOCK`, `/proc/asound/pcm` is an ordinary read, and the
socket is a `std::os::unix::net::UnixListener`. There is no descriptor
adoption and no mapping — this is the plain shape §§1–5 and 7–9 have.

**`getsockopt(2)` is one syscall onto a wide space of operations, exactly as
`ioctl(2)` is,** so the surface is the (level, option) PAIR rather than the
number in `rax`: `SOL_SOCKET` (1) and `SO_PEERCRED` (17), both pinned by
value, with the 12-byte length pinned too and the length the kernel writes
back checked rather than assumed. `SO_PASSCRED` is 18 and `setsockopt(2)` is
syscall 54 — the neighbours a slip reaches — and the confinement tests refuse
both by name. A second option, or `setsockopt`, is an amendment here.

**One scoped `#[allow]`, on the raw entry point and nowhere else.** Not on
`mod sys;`: a module-level allowance exempts every line of the module, and a
review demonstrated the consequence by appending a second, arbitrary
`unsafe` block to `sys.rs` and watching every confinement test still pass.
The function-level allowance is sufficient on its own, so the module-level
one does not exist.

**The bound is a count of the keyword, not a match on its shape.** Matching
shapes was tried and does not hold. A second review broke every shape-based
assertion three ways, compiling and running each: a block comment between the
keyword and its brace, which is not whitespace and so survived the squeeze; a
`cfg_attr` wrapping the allowance, which is not the literal the attribute
counter looked for; and `//` inside an ordinary string literal, which blinded
the line-comment strip for the rest of that line. Stripping comments cannot
be made exact either — Rust block comments nest, and a string may contain a
comment opener — so the tests count occurrences of the keyword itself, per
file, against a pinned number. Rust has one spelling of it and no way to
introduce a region without the text being present.

**The pins carry no slack, and that is what makes them a bound.** A
confirmation pass broke the first version of this rule, which pinned counts
that were mostly prose. A count with room in it is a budget: delete a sentence
that names the keyword, add a region that uses it, and the total does not
move. That escape compiled, read arbitrary memory, and passed every test and
the staged-source scan. So the budget is spent to nothing. Each pin equals
exactly the tokens that must be there — the crate-level denial in the root,
the scoped allowance and its block in `sys.rs`, three across the scanned set —
and no comment in either file may name the keyword; the prose says "the
keyword" instead. The staged-source scan in the recipe pins the same numbers
over the bytes that ship, and the conditional-attribute form is refused
outright, since it can spell any attribute the other assertions name.

**And the scanned set is derived, not written down.** Three passes broke this
confinement and the third one named the reason the first two kept working:
both scans read a list somebody typed, and neither list was checked against
the files the compiler actually reads or the recipe actually stages. A file
in `src/` that no list mentions is reached by an include-by-path, which needs
no module declaration, and it was staged, compiled and shipped with its own
allowance while every assertion passed. So the crate's list is now checked
against the directory, subdirectories are refused, include-by-path is
refused, module declarations are read from whole file text rather than line by
line, and the recipe's scan is built from the `WriteFile` steps the recipe
emits rather than from the module table those steps happen to be generated
from. The counts and the shapes bound what is scanned; these are what make
"what is scanned" mean the crate.

**One inline-assembly block, with five argument registers.** `getsockopt(2)`
takes five arguments; the ioctls and `poll` reach the same block through a
three-argument forwarder that supplies zeros. That is deliberately one
register mapping rather than two: `r10`, not `rcx`, is the fourth syscall
argument, and a second block would be a second place to get that wrong.

**The eleven requests, each pinned by value:** `PVERSION` (`0x80044100`),
`INFO` (`0x81204101`), `HW_REFINE` (`0xC2604110`), `HW_PARAMS`
(`0xC2604111`), `SW_PARAMS` (`0xC0884113`), `DELAY` (`0x80084121`),
`PREPARE` (`0x00004140`), `START` (`0x00004142`), `DROP` (`0x00004143`),
`DRAIN` (`0x00004144`), `WRITEI_FRAMES` (`0x40184150`). A twelfth request
is an amendment here. `ioctl(2)` is one syscall onto an unbounded space of
operations, so the number in `rax` is not the surface — the request in
`rsi` is.

**No `mmap(2)`, and that is the design rather than an omission.** §K.4
refuses the mapped-ring machinery outright: `SNDRV_PCM_ACCESS_RW_INTERLEAVED`
drives the device through write ioctls and `poll`, and the kernel paces the
writer. Taking the mapped ring would add a status page, a control page,
`SYNC_PTR` and shared-memory boundary arithmetic to the surface in order to
save a copy that is under 200 KiB/s at 48 kHz stereo `S16_LE`. The
confinement tests refuse `SYNC_PTR` by name.

**No control device.** `/dev/snd`'s `controlC*` nodes and the whole
`SNDRV_CTL_IOCTL_*` universe are never opened: output volume is
multiplication in the mixer, not a mixer element on a card. The tests refuse
the `controlC` path literal anywhere in the crate and refuse a constant
DECLARED under any of these names; that is a narrower claim than "refuse the
path crate-wide" and it is the accurate one, because a request number the
code composed inline would not be caught by a name-based scan. What closes
that gap is a different test: every call site is pinned WHOLE, and the
composer is used only where a request constant is declared, so an operation
outside the roster has nowhere to be written.

**No capture.** `READI_FRAMES` and `READN_FRAMES` are outside the surface,
which is what makes §K.5's "no microphone in v1" a property of the code
rather than a policy that could be forgotten.

**The request numbers are composed from the pinned struct lengths.** The
size field of an `_IOC` request encodes how many bytes the kernel copies, so
`0xC2604111` and the 608-byte `snd_pcm_hw_params` are the same fact written
twice. `sys.rs` writes it once: `ioc(dir, nr, LEN)` derives the request from
the length, and changing the length changes the request, which the kernel's
dispatch answers with `ENOTTY` rather than with a copy of a size nobody
intended. A test pins the resulting values against a compile of the UAPI
header, so a WRONG length cannot hold still either.

That prevents one of the two failures, not both. The kernel copies the
encoded number of bytes through whatever pointer it is handed and cannot
know how large the caller's allocation is, so a correct request with an
undersized buffer is still an out-of-bounds write. The second half is
discharged by type: `PcmInfo`, `HwParams` and `SwParams` are newtypes over
fixed-size arrays, one per request, and no call site sizes a buffer or
composes a request by hand. `WRITEI_FRAMES` is the one request whose payload
length is not fixed by its type — it names a frame count the kernel reads
through a caller pointer — and its wrapper refuses a count the slice cannot
back before any pointer is passed.

**Every constant here is an x86-64 fact.** `snd_pcm_uframes_t` and
`snd_pcm_sframes_t` are pointer-width, so the 608-byte layout, the request
that encodes it, `DELAY`'s argument and `snd_xferi` all differ on a 32-bit
target, and `_IOC`'s own bit layout differs on some architectures again. A
`compile_error!` refuses any other target rather than letting a second
architecture inherit these numbers and issue well-formed ioctls with the
wrong size field — which is exactly the out-of-bounds case above.

**The socket is not the gate; the peer's uid is.** §K.5 puts the socket at
`/run/td-audio/native` with mode 0666 in a 0755 directory, deliberately, so
that a jailed application can reach it — and authorizes on `SO_PEERCRED`
instead, "in code that can say why it refused". The credentials are set by
the kernel at `connect(2)` and cannot be forged by the peer, which is why
§K.3 authenticates this way rather than by the cookie; the cookie is still
parsed at its exact 256-byte length and then ignored.

Confinement tests inventory every source file, pin all three syscall numbers
by value, pin the socket level and option by value, pin all eleven request
compositions, pin the single inline-assembly block whole and every call site
whole, hold the raw entry point private and unnamed outside its definition
and calls, pin the per-file count of the keyword itself, count unsafe regions
in every syntactic form, check the scanned list against the files on disk and
the recipe's scan against the steps it stages, refuse the inner attribute form
of the allowance, refuse a conditionally written attribute, a block comment, an
include-by-path, a subdirectory, and a module declared outside the crate root,
and refuse the mmap family, the capture transfers, the control-device path
literal and constants declared under the
`SNDRV_CTL` names. A fourth syscall, a twelfth request, a second socket
option, a second scoped allowance, or any descriptor adoption is an
amendment here and in `APPLICATIONS.md` §K in the same landing.
