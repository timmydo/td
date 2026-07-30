# td UI stack

This is the normative design for td's graphical stack. The first increment is
a deliberately narrow, software-rendered Wayland environment for the QEMU
system profile. It proves the kernel, seat, compositor, protocol, image, and
boot-service seams without importing a second native build ecosystem.

## 1. Stack and trust boundary

The target stack is:

```
Linux devtmpfs + sysfs + fbdev + evdev
  -> td-seatd
  -> td-compositor
  -> td-ui-demo and later wl_shm Wayland clients
```

`td-seatd` is not compatible with seatd or libseat. It is a root oneshot for
one permanently configured local seat. It creates `/run/user/1000`, validates
`/dev/fb0` and every `/dev/input/eventN` as real character devices rather than
symlinks, assigns them to the graphical user with mode 0600, verifies the
result, and exits. This gives the active user the seat capability directly.
It deliberately provides no multi-user arbitration, descriptor revocation,
hotplug, suspend/resume, or VT switching.

The assignment is path-based because safe `std` exposes path ownership and
permission operations, not `fchown(2)`. There is consequently a check/use
window between rejecting a symlink and changing the node. The fixed `/dev` and
`/run/user` parents remain root-owned, and `td-seatd` runs before any uid 1000
process, so the assigned user cannot replace a checked name in that window.
Supporting hot seat reassignment after login would require an fd-based,
separately reviewed syscall surface.

`td-compositor` runs as uid 1000. It opens only the assigned framebuffer and
evdev nodes. It renders XRGB8888 pixels in software, reads Linux input events,
and owns the Wayland socket below the user's mode-0700 runtime directory. It
does not run as root and has no device-broker protocol. Readiness is not
announced until the framebuffer has accepted an initial paint and every
enumerated input node has been opened.

All target-side UI code is dependency-free Rust built by td's source-built
stage2 toolchain. The target closure contains no Mesa, libdrm userspace,
libinput, libudev, libseat, libwayland, Cairo, Pango, fontconfig, Meson,
Ninja, or pkg-config.

`td-ui-demo` is the second argv[0] entry point of the same dependency-free
multicall executable as `td-compositor`; the package installs it as a relative
symlink rather than storing or compiling the artifact twice. The invoked name
selects the client protocol loop, but the artifact contains both sides and is
not a privilege-separation boundary. Keeping the pair in one package avoids a
second unsafe exception for the client half of wl_shm.

## 2. Hardware profile

The first supported output is QEMU's virtio-gpu framebuffer, exposed as
`/dev/fb0` by the kernel's DRM fbdev emulation. Width, height, and stride come
from `/sys/class/graphics/fb0`; the compositor refuses any format other than
32 bits per pixel and treats it as little-endian XRGB8888. Renderer tests pin
that interpretation against a file-backed framebuffer.

Input is QEMU's PS/2 keyboard and pointer through evdev. The compositor
supports EV_KEY, EV_REL, and EV_SYN. It has a fixed US key map. The
compositor bindings deliberately follow Emacs navigation:

- `Super+b`, `Super+f`, `Super+p`, and `Super+n` focus left, right, up, and
  down;
- adding Shift to a focus binding moves the focused tile in that direction;
- `Super+1` through `Super+9` switch workspaces, and adding Shift moves the
  focused tile to that workspace;
- `Super+x 2` selects a vertical split for the next toplevel, `Super+x 3`
  selects a horizontal split, and `Super+x 1` toggles fullscreen.

The `Super+x` prefix survives key and modifier release, as an Emacs prefix
does, and is consumed by the next non-modifier key press. Left and right
modifier keys are tracked independently. A compositor chord consumes both
the press and release of its command key. Its modifier transitions still
reach the focused client, as do ordinary keys and their releases.
Arbitrary keymaps, touch, calibration, gestures, and real GPUs are later
increments.

Compositor commands act only on key presses. Evdev autorepeat records are
ignored for both compositor and client delivery. A held `Super+x 2` therefore
cannot fall through into repeated workspace switches after consuming the
prefix. Ordinary keys omit XKB's `repeat=no` property and libxkbcommon 1.11
treats symbol keys as repeatable by default. Clients combine that per-key
property with `wl_keyboard.repeat_info`.

The framebuffer is single-buffered from userspace's perspective. The renderer
allocates its frame storage once, composes a full frame after scene changes,
then writes one stride-complete image. There is no page flip, vblank,
acceleration, DMA-BUF, or tear-free claim.

Pointer motion is a scene change: each evdev `SYN_REPORT` currently performs
that full repaint while holding the runtime lock. This is bounded enough for
the supported QEMU PS/2 profile but is not a high-rate input design. Damage
tracking or a throttled render loop is required before adding such hardware.
The shared logical-seat boundary spans a key decision through its runtime
delivery, so complete input deliveries serialize behind that lock; partial
pointer records return before acquiring either lock.

## 3. Wayland surface

The server accepts local Unix-stream clients at
`/run/user/1000/wayland-0`. The socket and parent directory are owned by uid
1000 and are not group/world accessible.

The first protocol surface is:

- wl_display and wl_registry
- wl_compositor, wl_surface, and wl_region
- wl_shm, wl_shm_pool, and wl_buffer
- wl_output
- wl_seat and wl_keyboard
- xdg_wm_base, xdg_surface, and xdg_toplevel
- wl_callback completion and wl_buffer release

Only wl_shm ARGB8888 and XRGB8888 buffers are accepted. Pool, offset, size,
stride, and pixel-count arithmetic are checked before allocation or I/O. A
commit copies the declared pixels out of the pool with safe `FileExt`
positioned reads, so client mutation after commit cannot race the renderer.
The compositor never maps client memory. ARGB8888 follows Wayland's
premultiplied-alpha rule.

An XDG toplevel becomes eligible to map only after the client performs the
required empty initial wl_surface commit and acknowledges the resulting
xdg_surface configure serial. A buffer attached before that handshake is a
client protocol failure.

The boot profile starts one `td-ui-demo` toplevel. It discovers and binds
globals rather than depending on registry names, completes the initial XDG
configure/ack handshake, and uses 512x320 only for the first client-selected
buffer. Once mapped, it acknowledges the compositor's nonzero tile configure,
regenerates its dependency-free software pattern at that exact size, and
replaces its XRGB8888 wl_shm buffer. Later layout configures do the same.
Dynamic wl_shm pool, buffer, and frame-callback ids are reused only after
wl_display.delete_id. A zero width or height in a third-party compositor's
configure independently keeps the demo's current or default dimension, as the
XDG protocol requires. A bare xdg_surface.configure reuses the last applied
toplevel size and states.

The demo exposes its mode-0600 readiness socket and prints
`TD-UI-CLIENT-READY` only after the tile-sized replacement receives both
wl_buffer release and its frame callback. The client has no toolkit, fonts,
input handling, animation, or application model; it is the live resize proof
and a visible boot fixture. Its presentation handshake has a 20-second
absolute deadline, shorter than the supervisor's 30-second readiness deadline,
so a stalled compositor makes the client exit and permits `restart=always` to
retry.

The one supported output owns workspaces 1 through 9. Each workspace owns an
n-ary split tree whose leaves are mapped XDG toplevels. A new toplevel is
inserted after the focused leaf using the selected split axis and becomes
focused. Directional focus chooses the closest cross-axis-aligned tile with a
stable surface-key tie break. Directional move swaps two leaves while focus
stays with the moved toplevel. Unmapping a leaf collapses one-child
containers. A transient buffer detach remembers the toplevel's workspace and
reinserts it there on remap; destroying its wl_surface or disconnecting the
client forgets that assignment. A new mapping exits fullscreen so focus never
points behind a fullscreen tile.

Geometry uses a fixed outer and inner gap and divides remainders from the
first child onward. Undersized splits reserve a pixel for as many children as
the axis can show before budgeting gaps, and zero-area tiles draw neither
decoration nor client pixels. Borders are composed before all client buffers
so overlapping decorations cannot overwrite a neighboring client. These
rules make every result stable for odd and undersized output dimensions.
The runtime publishes changed view snapshots through a capacity-one wakeup
per client. The runtime computes and indexes one immutable snapshot per layout
change; workers clone its shared handle rather than rebuilding it. A pending
wakeup represents the latest snapshot rather than a queue of intermediate
layouts. The client's configure worker converts that snapshot into nonzero
xdg_toplevel sizes and the fullscreen and activated states, followed by an
xdg_surface serial. Every mapped leaf carries the tile size from its home
workspace, including when hidden, so the first coalesced post-map snapshot
cannot lose its configure. Hidden views lose active states. An observed unmap
clears the last layout state without emitting an event. Repeated states are
deduplicated.

Clients may acknowledge any known outstanding serial; that acknowledgement
also supersedes every older serial. Attaching the first buffer remains blocked
until the initial serial is acknowledged, while later unacknowledged layout
configures leave the previous copied buffer visible and clipped. This avoids
freezing a client during resize without accepting an unknown ACK. Once 32
serials are outstanding, newer states remain implicit in the latest runtime
snapshot. An ACK wakes the worker to send that latest state after space opens.
A null buffer that unmaps a mapped surface resets the initial handshake, so a
remap needs another null-buffer or empty commit, initial configure, and ACK. A
null attach on an already-unmapped surface is itself a valid initial commit.
Configures still in flight at the mapped-to-unmapped transition remain
acknowledgeable until the new initial serial is acknowledged; they cannot
authorize a buffer in the new mapping generation.
Decoration, clipboard, drag-and-drop, subsurfaces, popups, output
reconfiguration, fractional scale, screen capture, data devices, and client
pointer input are not yet advertised. Unknown objects, malformed sizes,
invalid object reuse, missing file descriptors, and unsupported requests
disconnect only that client.

The server advertises `wl_seat` version 7 as `td-seat0` with only the keyboard
capability. Every `wl_keyboard` receives a dependency-free, self-contained US
XKB v1 keymap through a descriptor for one process-wide backing file. The
file is created beside the Wayland socket in its private runtime directory,
created owner-only and normalized to mode 0600 so an unusual umask cannot
remove owner read access, reopened read-only, and unlinked before the server
accepts clients. Repeated `get_keyboard` requests send descriptors for that
same inode rather than creating unbounded keymap files. Repeat information is
25 keys per second after a 600 millisecond delay. The map covers the PC
alphanumeric block, modifiers, function keys, navigation keys, keypad, and
the supported media keys. It uses the standard XKB masks: Shift, Control,
Mod1 for Alt, Mod2 for Num Lock, and Mod4 for Super. Caps Lock and Num Lock
are sent as locked rather than depressed modifiers. Modifier and lock keys
are explicitly non-repeating. The first profile's keypad is digits-only:
Num Lock state is reported but does not select navigation symbols.

Only the visible activated XDG toplevel has keyboard focus. Focus changes
produce an ordered leave, enter, and modifier snapshot; enter carries the
forwarded keys that remain physically held. A keyboard bound after focus
already exists receives the same enter/modifier snapshot after its keymap.
One routed seat event receives one serial shared by every existing keyboard
resource to which that event is delivered. A newly bound keyboard's initial
enter and modifier snapshot are separate events with fresh serials.
The per-client runtime queue is active only while that client owns at least
one keyboard resource, so keyboard-agnostic clients receive no deliveries.
Evdev timestamps cross the adapter as explicit milliseconds modulo 2^32.
They retain evdev's default realtime clock and can step when wall time changes;
selecting a monotonic clock would require a separately reviewed ioctl surface.
All event nodes contribute to one logical seat state. Duplicate presses and
unmatched releases are suppressed, a key held on two devices remains down
until both release it, and an event node that closes releases its contribution.
That synthetic release burst holds the same seat-ordering boundary as normal
events, so another device cannot interleave halfway through it.
For a modifier key, the forwarded key transition precedes the resulting
modifier snapshot, matching the ordering established by wlroots.
After `SYN_DROPPED`, the adapter releases that node's state, cancels a partial
prefix, discards records through the next `SYN_REPORT`, and resumes without
guessing the lost state.
Before a surface id is released, its `wl_display.delete_id` is placed in the
bounded keyboard queue after the focus update. The worker therefore writes
the queued leave first without making request dispatch wait for socket
backpressure. The numeric object id has an independent reservation until that
ordered `delete_id` write completes, so its premature reuse cannot stall
unrelated ids. Queue saturation closes the subscription and makes deletion
fail closed rather than allowing a stale surface reference onto the wire.
Keyboard objects may be released at their negotiated protocol version.
`get_pointer` and `get_touch` fail with `wl_seat.missing_capability` because
neither capability is advertised.

Resource ceilings are part of the protocol boundary: at most 32 clients run
at once, each has at most 512 objects, 64 queued descriptors, one pending
layout wakeup, 64 pending keyboard deliveries, and 32 MiB of committed pixels.
Each XDG surface has at most 32 current-generation configures. During remap it
may additionally retain the previous generation's bounded set while the one
new initial serial is pending, for at most 33 serials total; the tracker
enforces the retained-generation bound itself. Output pixels, the bundled
demo's generated buffer, and one client's aggregate committed pixels share
the same 32 MiB ceiling, so every accepted output has a representable
single-client tile. A framebuffer's stride-padded shadow allocation has a
separate 64 MiB ceiling. Framebuffer and buffer dimensions share a
16,384-pixel ceiling. At four bytes per pixel the area ceiling is 8,388,608
pixels: tight 3840x2160 is accepted, while 4096x2160 is rejected. The complete
scene retains at most 128 MiB. Rendering clips rows and columns to the output
before visiting pixels. These are availability bounds against a same-user
client, not isolation between mutually distrusting users.

A full keyboard queue closes that client's runtime subscription instead of
blocking the evdev reader. The keyboard worker drains the bounded queue and
disconnects the client when it observes the closed subscription. If that
worker is already blocked writing to a client that does not read, the
disconnect waits for the same accepted per-client availability gap as the
configure worker; other clients and the input reader continue.
A request that illegally reuses the exact id whose ordered `delete_id` is
blocked on that socket also waits for the write, preserving ordering instead
of letting request dispatch overtake it.
A client that stalls during the initial keymap and focus burst holds its
keyboard-registration ordering guard; its worker queues behind that guard
until saturation closes the subscription. The worker cannot observe that
closure or disconnect the client until the blocked initial write and guard
unblock, so this is part of the accepted per-client availability gap below.

These ceilings do not yet bound time. A connected client that stops reading
events can block its configure worker on a socket write and retain one client
slot. It does not hold the runtime or framebuffer lock while writing, so it
cannot block input or other clients. The write deliberately retains that
client's configure-registration and tracker locks to keep event pairs ordered,
so the same client's ACK, role mutation, and request dispatch can also wait for
the write. This can park both of that client's threads and retain its slot
until the peer reads events or closes; there is no self-heal. If dispatch exits
for another reason, it shuts down its socket clone before joining, which
interrupts a blocked write. This first profile accepts the remaining
per-client availability gap. A future write deadline must be an explicit,
injectable connection policy with deterministic tests that do not wait for
elapsed wall time.

## 4. Unsafe confinement

Wayland carries wl_shm and wl_keyboard keymap descriptors as SCM_RIGHTS
ancillary data on its Unix stream. Stable Rust 1.96 exposes no stable
ancillary-data API. The user approved one new target-side exception for this
transport.

`td-compositor/src/sys.rs` contains the sole scoped `unsafe` block. One raw
`syscall3` body carries exactly:

- sendmsg(2), to send the demo client's wl_shm descriptor, the server's XKB
  keymap descriptor, or a test request;
- recvmsg(2), to receive wl_shm pool descriptors; and
- close(2), to release a received descriptor after it has been safely
  duplicated through `/proc/self/fd/N`.

No framebuffer, input, socket, allocation, process, or filesystem operation
passes through that surface. The crate denies unsafe globally; confinement
tests pin the allow count, assembly body, syscall numbers, callers, and the
absence of unsafe from every other source file. Adding a syscall or another
scoped allow amends this document and the repository-wide unsafe inventory.

## 5. Boot and recovery

PID 1 still mounts devtmpfs, procfs, sysfs, tmpfs, and the immutable root.
`td-svc` starts `td-seatd` after root checking, then starts
`td-compositor` and `td-ui-demo`, in that order, through td-login's credential
switch. Both long-running processes are restartable with backoff. Client
readiness is probed through its private socket, so deployment health cannot
race ahead of the first committed frame. Graphical failure never suppresses
or owns the serial `ttyS0` greeter; that remains the recovery console.

The serial greeter remains independent of graphical readiness and can recover
when the graphical daemon fails. The automated deployment-success transaction
strictly requires td-svc to declare the graphical service ready, however, so a
broken UI cannot mark an update healthy or let QEMU power off before testing
the new boot seam. The
graphical service prints `TD-WAYLAND-READY` only after the framebuffer has
been painted and the Wayland socket is listening. The QEMU system oracle
requires that marker and the client's later `TD-UI-CLIENT-READY` marker.

## 6. Required proof

The landing must prove:

- the kernel pins fbdev, virtio-gpu, PS/2, and evdev built in;
- interactive QEMU attaches virtio-vga and preserves ttyS0 on stdio;
- the seat and compositor multicall artifacts are static ELF64 ET_EXEC files,
  and the demo entry point is a relative symlink to that static multicall;
- the seat assigner rejects symlinks/non-devices and verifies ownership/mode;
- wire parsing rejects truncation, overflow, invalid object use, and a
  descriptor-less wl_shm request;
- an SCM_RIGHTS-backed wl_shm buffer commits and is copied into the scene;
- the boot client discovers globals, completes both initial and tile-sized XDG
  configure/ack cycles, replaces its buffer at the requested size, receives
  wl_buffer release and a frame callback, and remains mapped;
- every tiling command, split geometry edge case, workspace transition, tree
  collapse, fullscreen transition, and Emacs binding is a deterministic host
  test;
- parsed key chords and complete pointer frames cross the evdev adapter into
  a recording target, while the runtime integration test proves a layout
  command repaints a file-backed framebuffer;
- configure model tests cover initial gating, stale and superseding ACKs,
  deduplication, the remap handshake, hidden state, fullscreen, focus, invalid
  sizes, and backpressure at the outstanding-serial ceiling;
- a worker/dispatch socket test proves an ACK at that ceiling wakes and emits
  the latest deferred layout rather than stalling;
- a real server/client socket test resizes the demo after a second map, changes
  activation without resizing, enters fullscreen, and crosses workspaces;
- wl_seat advertises only keyboard, sends a structurally validated
  self-contained keymap descriptor, repeat information, held keys, and all
  four modifier fields;
- the exact serialized keymap parses with libxkbcommon 1.11, where an ordinary
  key reports repeatable and an excluded modifier reports non-repeating;
  in-tree tests also pin its delimiter, type, explicit repeat exclusions,
  ordinary-key omissions, symbol, and evdev-code invariants without adding
  libxkbcommon to the target graph;
- focus changes and ordinary evdev keys cross the bounded keyboard worker in
  order, while pre-registration events and intercepted Emacs chords do not;
- a threaded server/client socket test binds the seat and keyboard, receives
  focus and key events, and tears down a live keyboard registration;
- keyboard model tests cover held-key snapshots, cross-client focus, modifier
  locks, key releases, registration cutoffs, and queue saturation;
- software composition clips surfaces to tiles and never indexes outside a
  frame;
- the image contains all three binaries, the service order is checkable, and
  the compositor and client run as uid 1000;
- existing serial boot checks remain green.

## 7. Testability contract for tiling

The keyboard-driven tiling shell is a pure policy layer. Workspaces own
split-container trees, and leaf containers own mapped XDG toplevels. Layout,
focus, move, split, fullscreen, and workspace operations are deterministic
state transitions over ordinary Rust data. They do not read devices, sockets,
clocks, or global process state. Geometry calculation consumes explicit
output dimensions and returns placements that tests inspect without a
framebuffer.

Linux evdev readers and Wayland connections will remain adapters around that
state machine. Parsed input events, generated serials, and elapsed time must
cross those adapters as explicit values. Tests use file-backed framebuffers,
byte-backed evdev records, Unix socket pairs, and injected serials or
deadlines. A concurrency test synchronizes the state it needs with messages or
socket events; elapsed sleeps and scheduler luck are not correctness
conditions.

Connection teardown is the existing example: the read side preserves a peer
reset as a distinct result while an orderly close remains a zero-length read.
Event writes record disconnected client state, and the dispatch loop consumes
all three outcomes without depending on which side of a socket race wins.

Layout notification follows the same rule. Runtime mutations publish only
when mapped-view geometry or state changes. The capacity-one channel
coalesces bursts without losing the latest snapshot, and a stop flag plus the
same wake channel terminates the configure worker without polling or sleeps.
The configure tracker is a pure state machine whose tests inject serials and
view states; socket tests cover only encoding and cross-thread delivery. The
keyboard state machine similarly consumes explicit focus, key, and modifier
values and returns routed events. It has no evdev, socket, file, clock, or
global serial access. Runtime tests cover focus routing and bounded queues;
protocol tests cover XKB descriptor and event encoding, while one narrow
threaded socket test proves registration, delivery, and teardown compose.

Every layout operation has table-driven model tests covering focus, geometry,
tree collapse, and conservation of mapped views. Each tested command sequence
checks the global invariants: one occurrence per mapped surface, live focus
and fullscreen references, and no degenerate split containers. Input parsing,
chord state, pointer coalescing, scene composition, and runtime repaint each
have a separate adapter test, so failures identify the seam they cross.
Protocol tests then prove that a Wayland commit maps pixels and that teardown
removes them. Target selftests cover the packaged binary and QEMU covers only
the final device, service, and boot seams. This keeps policy failures
reproducible as fast host tests while retaining an end-to-end proof of the
shipped image.

## 8. Deferred UI stack

The next increment is focused `wl_pointer` delivery from the existing evdev
motion path. A bitmap font and launcher, clipboard, terminal, hotplug, and
real DRM/KMS profiles follow. General Wayland toolkit compatibility is not
claimed until the missing core protocols have explicit tests. Hardware
acceleration, niri, portals, PipeWire, Xwayland, and a C desktop stack remain
optional consumers rather than foundations of td's UI.
