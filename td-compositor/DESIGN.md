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
  selects a horizontal split, and `Super+x 1` toggles fullscreen;
- `Super+x l` opens the launcher. `Control+n` and `Control+p`, or Down and Up,
  move its selection; Enter activates it; Escape and `Control+g` close it.
  ASCII letters, digits, space, and hyphen filter its registry, and Backspace
  edits that filter.

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
allocates its frame storage once and composes a full frame after scene changes.
It then writes only the rows that changed, keeping a second allocation holding
what the device is believed to contain. An unchanged scene writes nothing at
all. Rows rather than rectangles: a band is one `seek`+`write` pair, where
per-row column spans would be one pair each and the syscalls would cost more
than the bytes they saved. The shadow copy is marked untrustworthy for the
duration of every write and trusted again only once that write has returned, so
a failed or partial write resends the whole image rather than leaving the device
holding bytes no shadow describes.

The compositor is not the only writer of that device. It deliberately does not
take the VT, and the boot profile keeps fbcon there on purpose so a recovery
console stays reachable; owning the `/dev/fb0` node through td-seatd does not
stop a writer inside the kernel. Every paint used to rewrite the whole image and
so healed foreign pixels for free, where a shadow copy that is never distrusted
would keep them until a scene change happened to touch those exact rows. Two
things bound that: one paint in every 240 is an unconditional full write, and
a tiling command distrusts the shadow outright, so the repair is both automatic
and reachable by a user who can see the artifact. That bound is counted in
paints, not seconds, and the batching above is what lowers the paint rate --
the two halves of this mechanism pull against each other, so the interval is a
count of repairs deferred rather than a wall-clock age. A screen nothing changes
writes nothing and so never spends the interval, which is the same reach the
renderer had before damage tracking. There is still no page flip, vblank,
acceleration, DMA-BUF, or tear-free claim.

Pointer motion is a scene change, and a moving pointer is the highest-rate
source of them. A reader drains up to 64 evdev records per read and takes one
paint for the batch rather than one per `SYN_REPORT`: every record still crosses
the seat in order, so clients see the whole motion path, but the framebuffer
only ever shows the newest state. This is the throttle, and it is
self-regulating — it costs a lone report nothing, because a read returns as soon
as one record is available, and it coalesces exactly as hard as the compositor
is behind. Motion therefore owes a paint instead of taking one, and any repaint
before the flush settles that debt; the flush also runs after device teardown,
so a batch that ended in an error still leaves the truth on screen. The debt is
pessimistic across the paint exactly as the shadow copy is across its write —
a paint that failed leaves the screen owed, never settled — so the owing flag
means "the output may not match the scene" whatever raised it, and a failure
cannot be swallowed by having already cleared it. Together
with damage tracking, a one-pixel pointer step at 1920x1080 writes 14 rows
instead of an 8 MiB image. Both were prerequisites for real GPU hardware, and
neither is a substitute for it: acceleration would make a redundant full-screen
frame cheaper without making it unnecessary.
The shared logical-seat boundary spans a key decision through its runtime
adapter delivery, so evdev inputs remain ordered behind that lock; partial
pointer records return before acquiring either lock. Each adapter operation
reacquires the scene runtime lock, so a Wayland commit may interleave between
command, launcher, key, modifier, and pointer operations from one report. This
keeps process launch outside the scene lock without changing evdev ordering.
An input parse, runtime-delivery, or framebuffer-repaint error closes that
device's reader after releasing its seat contribution. The first fixed-device
QEMU profile deliberately fails stopped instead of retrying a potentially
persistent error in a hot loop; the serial recovery console remains available.
Launch and reap failures are contained inside the process adapter and leave
the reader active so the launcher can be closed or retried.

## 3. Wayland surface

The server accepts local Unix-stream clients at
`/run/user/1000/wayland-0`. The socket and parent directory are owned by uid
1000 and are not group/world accessible.

The first protocol surface is:

- wl_display and wl_registry
- wl_compositor, wl_surface, and wl_region
- wl_shm, wl_shm_pool, and wl_buffer
- wl_output
- wl_seat, wl_keyboard, and wl_pointer
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
globals rather than depending on registry names, binds the version-5-or-newer
seat, and completes the initial XDG configure/ack handshake. It accepts only a
seat with both keyboard and pointer capabilities, requests those devices only
after receiving that capability event, and verifies the keymap descriptor
byte-for-byte against td's pinned map before becoming ready. It uses 512x320
only for the first client-selected buffer. Once mapped, it acknowledges the
compositor's nonzero tile configure, regenerates its dependency-free software
pattern at that exact size, and replaces its XRGB8888 wl_shm buffer. Later
layout configures do the same. Dynamic wl_shm pool, buffer, and frame-callback
ids are reused only after
wl_display.delete_id. A zero width or height in a third-party compositor's
configure independently keeps the demo's current or default dimension, as the
XDG protocol requires. A bare xdg_surface.configure reuses the last applied
toplevel size and states.

The demo exposes its mode-0600 readiness socket and prints
`TD-UI-CLIENT-READY` only after the tile-sized replacement receives both
wl_buffer release and its frame callback and its seat and keymap are ready.
The client has no toolkit. Its pure UI model tracks keyboard focus, held and
last keys, modifier masks, pointer focus and 24.8 coordinates, and held
buttons. A built-in 5x7 bitmap font paints that state without a font library.
Keyboard events update the model individually. Pointer events are bounded and
applied transactionally only at `wl_pointer.frame`, so an incomplete frame is
never rendered. An evdev report retains at most 64 button transitions.
Pointer routing can add at most an initial motion plus one leave and enter
when an implicit grab ends, so the client accepts the composed maximum of 67
events per frame. At most one input-driven replacement is in flight; further
updates coalesce into the latest model revision until both buffer release and
frame completion arrive. Configure-driven replacement retains the existing
XDG behavior. The presentation handshake has a 20-second absolute deadline,
shorter than the supervisor's 30-second readiness deadline, so a stalled
compositor makes the client exit and permits `restart=always` to retry.

The launcher is a compositor-owned overlay, so opening it never depends on an
already-running client. Its registry currently has an input-monitor entry
that starts another `td-ui-demo` and an explicit close entry. Each entry owns
a label, lowercase search terms, and a typed launch request. The pure launcher
model stores a bounded 64-byte ASCII filter, requires every whitespace-
separated term to occur in an entry's search text, and resets selection to the
first match after an edit. An empty result is explicit and Enter leaves it
open; Backspace can recover it. Opening clears the previous filter. While the
overlay is open, all non-modifier keys are consumed by the compositor, and
modified keys that are not launcher commands do not become text. Activation
with a match closes the overlay before process creation; activation with no
matches keeps both the overlay and input capture active. The input adapter
updates its capture state from the model's post-action visibility instead of
guessing which action opened or closed it. It never enables capture before a
successful open. An overlay action is transactional with its framebuffer
paint: a failed paint restores the complete prior launcher model, so another
input device cannot observe model state with stale capture.
The compositor receives the client executable as an explicit
`--launcher-client` argument, requires both it and the Wayland socket to be
absolute, derives a unique readiness-socket name beside the socket, and passes
both paths as literal argv values without a shell. It reaps exited children
before each launch and retains at most 16 live launched clients. A launch or
reap failure is reported without terminating the evdev reader, so the user
can close or retry the launcher. Reaping also removes a dead child's private
readiness socket and reports any path that cannot be safely removed. Opening
the modal overlay immediately withdraws ungrabbed pointer focus; closing it
immediately restores focus under the stationary cursor. An existing implicit
grab remains routed through the overlay until release, but unmapping its
surface clears the grab and sends leave before object deletion. If the
compositor exits while a launched child is still live, its readiness socket
may remain until logout clears the runtime-directory tmpfs. Names carry the
compositor pid, so that residual path cannot block a later compositor.

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
reconfiguration, fractional scale, screen capture, data devices, pointer
axes, and touch are not yet advertised. Unknown objects, malformed sizes,
invalid object reuse, missing file descriptors, and unsupported requests
disconnect only that client.

The server advertises `wl_seat` version 7 as `td-seat0` with keyboard and
pointer capabilities. Every `wl_keyboard` receives a dependency-free,
self-contained US XKB v1 keymap through a descriptor for one process-wide
backing file. The file is created beside the Wayland socket in its private
runtime directory, created owner-only and normalized to mode 0600 so an
unusual umask cannot remove owner read access, reopened read-only, and
unlinked before the server accepts clients. Repeated `get_keyboard` requests
send descriptors for that same inode rather than creating unbounded keymap
files. Repeat information is 25 keys per second after a 600 millisecond
delay. The map covers the PC alphanumeric block, modifiers, function keys,
navigation keys, keypad, and the supported media keys. It uses the standard
XKB masks: Shift, Control, Mod1 for Alt, Mod2 for Num Lock, and Mod4 for
Super. Caps Lock and Num Lock are sent as locked rather than depressed
modifiers. Modifier and lock keys are explicitly non-repeating. The first
profile's keypad is digits-only: Num Lock state is reported but does not
select navigation symbols.

Only the visible activated XDG toplevel has keyboard focus. Focus changes
produce an ordered leave, enter, and modifier snapshot; enter carries the
forwarded keys that remain physically held. A keyboard bound after focus
already exists receives the same enter/modifier snapshot after its keymap.
One routed seat event receives one serial shared by every existing keyboard
resource to which that event is delivered. A newly bound keyboard's initial
enter and modifier snapshot are separate events with fresh serials.
The keyboard side of the per-client seat queue is active only while that
client owns at least one keyboard resource, so keyboard-agnostic clients
receive no keyboard deliveries.
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
bounded seat queue after the focus update. The worker therefore writes
the queued leave first without making request dispatch wait for socket
backpressure. The numeric object id has an independent reservation until that
ordered `delete_id` write completes, so its premature reuse cannot stall
unrelated ids. Queue saturation closes the subscription and makes deletion
fail closed rather than allowing a stale surface reference onto the wire.
Keyboard objects may be released at their negotiated protocol version.
`get_touch` fails with `wl_seat.missing_capability` because touch is not
advertised.

Pointer focus is the visible surface pixel under the software cursor. Gaps,
borders, clipped-away pixels, empty tile space, and pixels excluded by the
surface's committed input region have no pointer focus. A region retains at
most 256 nondegenerate add/subtract operations and one client can retain at
most 4,096 operations across surface snapshots. Overflow and degenerate
requests are bounded no-ops. `set_input_region` takes an immutable
copy-on-write snapshot immediately, and the pending snapshot takes effect
atomically on the next surface commit; later mutation or destruction of the
region object cannot change it. A null region restores the default infinite
input region. Enter and motion coordinates are surface-local integers encoded
as checked 24.8 `wl_fixed` values. Motion is coalesced through the evdev
`SYN_REPORT` boundary, preserving its explicit timestamp. Pointer objects at
version 5 or newer receive one `wl_pointer.frame` after each logical delivery;
a leave and enter caused by one focus transition form one delivery. Objects
before version 5 receive the same events without a frame, and `release` is
accepted only from version 3 onward.
When a retile changes local coordinates under a stationary cursor, the
synthetic motion reuses the most recent evdev timestamp, or zero before the
first evdev frame; it is a coordinate refresh rather than a gesture clock.

Mouse buttons from `BTN_MOUSE` through `BTN_TASK` retain their Linux evdev
codes on the wire. Duplicate physical presses and unmatched releases are
suppressed across the logical seat. Logical edges are derived from global
physical and delivered state only when a device reaches `SYN_REPORT`, so
reports from two devices cannot reorder a replacement press behind the last
release. A press starts Wayland's implicit grab: motion and further buttons
continue to the pressed surface until every button is released. If another
press follows the last release in the same frame, focus first reconciles with
the surface under the cursor and the new grab starts there. Removing or hiding
the grabbed surface cancels the grab and reconciles focus without leaving a
stale surface reference. Partial button or motion records are discarded on
`SYN_DROPPED`; delivered button state is tracked separately so recovery
releases only buttons the client had actually seen. A report retains at most
64 button transitions; crossing that limit performs the same fail-closed
release and resynchronization through the next `SYN_REPORT`. Hiding or
destroying a grabbed surface instead cancels its delivered state and sends
leave, because an unmapped surface is no longer a valid button target.

`set_cursor` accepts a null surface or assigns the cursor role to a
`wl_surface`, preventing later XDG-role reuse, only when its serial matches the
latest enter sent to that client for the seat and the runtime still focuses
that entered surface. Late pointer resources reuse the client-wide serial.
Stale, pre-enter, and logically post-leave requests are ignored without
consuming a role even when socket delivery of the leave is delayed; a valid
incompatible role uses `wl_pointer.error.role`. Cursor buffers are immediately
released and never enter the tiling scene. The first renderer continues to
draw its fixed software cursor and deliberately ignores the requested image
and hotspot; themed client cursors are a later rendering increment. There are
no axis events in the PS/2 profile.

Keyboard and pointer deliveries share one bounded per-client seat queue.
Each event serial is shared by all matching resources, and a resource bound
after an event receives an exact current snapshot rather than queued history.
A client with neither resource receives no seat deliveries. Surface leave,
pointer frame, and `wl_display.delete_id` therefore retain one order, and the
object ID reservation remains held until every earlier keyboard or pointer
event naming that surface is serialized.

Resource ceilings are part of the protocol boundary: at most 32 clients run
at once, each has at most 512 objects, 64 queued descriptors, one pending
layout wakeup, 64 pending seat deliveries, and 32 MiB of retained toplevel
pixels.
Each XDG surface has at most 32 current-generation configures. During remap it
may additionally retain the previous generation's bounded set while the one
new initial serial is pending, for at most 33 serials total; the tracker
enforces the retained-generation bound itself. Output pixels, the bundled
demo's generated buffer, and one client's aggregate retained toplevel pixels
share the same 32 MiB ceiling, so every accepted output has a representable
single-client tile. Cursor-role buffers are released without retaining their
pixels. A framebuffer's stride-padded shadow allocation has a
separate 64 MiB ceiling. Framebuffer and buffer dimensions share a
16,384-pixel ceiling. At four bytes per pixel the area ceiling is 8,388,608
pixels: tight 3840x2160 is accepted, while 4096x2160 is rejected. The complete
scene retains at most 128 MiB. Rendering clips rows and columns to the output
before visiting pixels. These are availability bounds against a same-user
client, not isolation between mutually distrusting users.

A full seat queue closes that client's runtime subscription instead of
blocking the evdev reader. The seat worker drains the bounded queue and
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

The demo client reads only enough stream bytes to complete its next Wayland
event. SCM_RIGHTS descriptors can arrive with bytes that begin an earlier
non-descriptor event, so descriptors remain in one bounded stream-order queue
until the next event whose signature consumes one. The completed presentation
handshake rejects any descriptor left over after all expected events. The
descriptor queue and keymap read are bounded, and every overflow or abandoned
connection closes all descriptors it owns.

`td-compositor/src/sys.rs` contains the sole scoped `unsafe` block. One raw
`syscall3` body carries exactly:

- sendmsg(2), to send the demo client's wl_shm descriptor, the server's XKB
  keymap descriptor, or a test request;
- recvmsg(2), to receive wl_shm pool descriptors or the demo client's XKB
  keymap descriptor;
- close(2), to release a received descriptor after it has been safely
  duplicated through `/proc/self/fd/N`; and
- ioctl(2), for the four pinned terminal-control requests in section 12.

No framebuffer, input, socket, allocation, process, or filesystem operation
passes through that surface. The crate denies unsafe globally; confinement
tests pin the allow count, assembly body, syscall numbers, callers, and the
absence of unsafe from every other target source file. Each developer tool is
a separate crate root that also denies unsafe. Adding a syscall or another
scoped allow amends this document and the repository-wide unsafe inventory.

The two surfaces behind that one body are disjoint and are pinned to disjoint
modules: descriptor transport is reachable only from `client.rs` and
`server.rs`, terminal control only from `pty.rs`, and no other module names
`sys` at all. `ioctl(2)` is the request-carrying one, so its roster is the
confinement: a request outside the four is refused before the syscall, and a
test pins each value, the single guard, the single entry point, and each
wrapper's operand shape.

## 5. Boot and recovery

This section records the current demo-client boot profile. When the td-term
cutover specified in sections 12 and 14 lands, it replaces the demo service,
marker, and final-image symlink and adds the specified devpts setup to the
early mount sequence. The compositor ordering, readiness, restart, and
serial-recovery guarantees remain in force.

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

These are the current compositor and demo proofs. When the td-term proof in
section 14 lands, it supersedes the demo-specific entry-point, image-roster,
and `TD-UI-CLIENT-READY` requirements without waiving the remaining checks.

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
  wl_buffer release and a frame callback, verifies its exact keymap
  descriptor, consumes focused keyboard and framed pointer input, redraws the
  resulting model, and remains mapped;
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
- wl_seat advertises keyboard and pointer, sends a structurally validated
  self-contained keymap descriptor, repeat information, held keys, and all
  four modifier fields;
- the exact serialized keymap parses with libxkbcommon 1.11, where an ordinary
  key reports repeatable and an excluded modifier reports non-repeating;
  in-tree tests also pin its delimiter, type, explicit repeat exclusions,
  ordinary-key omissions, symbol, and evdev-code invariants without adding
  libxkbcommon to the target graph;
- focus changes and ordinary evdev keys cross the bounded seat worker in
  order, while pre-registration events and intercepted Emacs chords do not;
- keyboard model tests cover held-key snapshots, cross-client focus, modifier
  locks, key releases, registration cutoffs, and queue saturation;
- pointer model tests cover enter, leave, motion, cross-client routing,
  duplicate buttons, mid-frame re-grabs, implicit grabs, surface removal,
  workspace cancellation, snapshots, and revision exhaustion;
- scene tests prove pointer hit testing excludes gaps and clipped-away
  pixels while retaining local coordinates for an implicit grab, and server
  tests pin per-region and aggregate retained-operation ceilings;
- evdev tests prove relative motion and logical multi-device buttons flush
  only at `SYN_REPORT`, while EOF, `SYN_DROPPED`, and an oversized partial
  report cannot strand a delivered button;
- a batch of 32 pointer reports delivers 32 frames and takes one paint, a lone
  report is painted without waiting for a second, a record split across two
  reads is carried rather than parsed short, an interrupted read resumes, and a
  flush failure closes the device only after releasing its pressed buttons;
- a failed flush and a failed repaint both leave the paint owed, so the next
  flush retries it rather than closing the device on a stale screen;
- a run of banded paints resends the whole image on the 240th paint and no
  sooner,
  and a tiling command resends it immediately, so foreign pixels fbcon left on
  the device cannot outlive either bound;
- framebuffer tests pin the damaged band exactly: an unchanged scene writes
  nothing, a one-pixel pointer step writes the cursor's 13 rows, a diagonal step
  writes 14, a full-size output writes under 2% of its image, a banded sequence
  leaves the device byte-identical to one full write of the same scene, and a
  failed write resends everything even though the scene did not change;
- pointer wire tests cover checked fixed-point coordinates, event payloads,
  shared serials, version-gated frame and release, cursor roles, queue
  saturation, and leave/frame/delete ordering;
- a threaded server/client socket test binds keyboard and pointer, receives
  focus, motion and button delivery, releases the resources, and tears down
  the live registrations;
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

The pointer state machine consumes explicit time, hover target, grabbed
target, and button values and returns per-client event frames. It has no
evdev, layout, socket, serial, file, or clock access. Scene hit testing maps
the current software-cursor position to those explicit targets. Runtime tests
compose both models with bounded subscriptions; socket tests cover only
encoding, registration, ordering, cursor roles, and teardown.

The demo UI follows the same split. Its model consumes typed keyboard updates
and complete pointer-frame slices, has no socket, descriptor, filesystem, or
clock access, and increments one checked revision per visible transition.
Bitmap painting consumes an explicit XRGB slice and dimensions and clips every
glyph. Model tests cover focus, held-state bounds, transactional rollback, and
revision exhaustion; raster tests pin deterministic pixels, clipping, glyph
coverage, the XRGB byte, and state-dependent pixels. Client adapter tests
separately cover exact and growth-raced keymap descriptors, split and
coalesced event framing, descriptor association and cleanup, event validation,
frame boundaries, and one-in-flight render coalescing. One real server/client
socket test composes keyboard and pointer delivery, a changed framebuffer,
wl_shm replacement, release, and callback completion.

The launcher follows the same boundary. Its pure model consumes typed actions,
owns the query and matching indices, and returns an optional launch request;
its renderer consumes an explicit frame slice, dimensions, and stride. Input
tests route parsed Emacs chords and the complete accepted character set through
a recording target, while runtime tests prove that overlay repaints do not
mutate the tiling tree. Process policy turns a launch request and explicit
paths into literal argv, with its active-child ceiling tested independently.
Every overlay pixel is clipped to the computed card rectangle, including on
an output too short for its normal layout. No launcher model or renderer reads
input devices, sockets, clocks, the filesystem, or ambient environment.

## 8. Deferred UI stack

Focused keyboard and pointer delivery now connect the demo client to the
existing evdev input path, and the Emacs-style launcher has a filterable
application registry. A terminal launcher entry follows after the native
terminal client is packaged. Clipboard, pointer axes, client cursor rendering,
hotplug, and real DRM/KMS profiles follow. The terminal stack has the separate
contract below.

Of that contract these are built: the parser and terminal model, the native
corpus including its `key` operations, the keyboard adapter of section 11
(translation, autorepeat, the bounded input queue, and the scrollback
viewport it selects), and from section 12 the PTY open/unlock/peer/winsize
operations, the account and environment policy, the child argv through
`cttyhack --stdin`, and the PTY reader thread.

Section 11's pinned font is landed: the committed Unifont face, its licenses
and provenance record, the importer that derives it reproducibly, and the PSF2
reader that validates every header field, table entry, and pixel offset before
the renderer can index a glyph.

Section 11's renderer is landed as a pure function, with section 14's exact
P6 goldens as its oracle: the palette, the six renditions, the cursor, the
visual-bell ring, the clipping, and the scrollback viewport, whose selecting
keys landed after it.
What it still lacks is a caller. Nothing submits its output to a surface, so
the frame-callback coalescing, the persistent-buffer reuse-after-release, and
the buffer replacement on resize that section 11 also specifies land with the
Wayland client, which is where a frame's lifecycle exists at all.

Section 12's writer thread, child waiter, readiness socket, `TD-TERM-READY`
marker, and `probe` subcommand are not built. The Wayland client, packaging,
and boot cutover of sections 12 and 14 follow them; the terminfo entry is
landed. Until that client exists the PTY adapter has no production caller;
its host tests drive every operation against a real PTY, and the packaged
binary's selftest covers the policy layer, which is what runs where devpts is
not mounted.
General Wayland toolkit compatibility is not claimed until the missing core
protocols have explicit tests. Hardware acceleration, niri, portals, PipeWire,
Xwayland, and a C desktop stack remain optional consumers rather than
foundations of td's UI.

## 9. td-term boundary and philosophy

`td-term` is td's native terminal for this compositor. Its product reference
is foot: one process per terminal, native Wayland, immediate startup, a quiet
interface, and no server process or application framework. It is not a foot
reimplementation and does not inherit foot's implementation or compatibility
claims.

The terminal is the third argv[0] entry point of the existing compositor
multicall, alongside `td-compositor` and `td-ui-demo`. The package installs a
relative `td-term -> td-compositor` symlink. This reuses the one Wayland wire
implementation and the existing confined SCM_RIGHTS transport instead of
creating a second target-side unsafe surface. The client and server run as the
same graphical user, and the shared artifact is not a privilege boundary.
`td-ui-demo` remains a source and target-recipe protocol fixture. The boot
cutover removes its final-image symlink when td-term replaces it as the visible
client.

All terminal code is dependency-free Rust built by td's source-built stage2
toolchain. It has no toolkit, GPU API, dynamic font system, terminal daemon,
configuration language, plugin interface, or external crate. Its first
renderer is software XRGB8888 into persistent `wl_shm` buffers.

The implementation has four separable layers:

- a byte-stream parser that emits bounded terminal actions;
- a terminal model that owns grids, modes, cursor, history, and replies;
- a bitmap renderer that converts an explicit model snapshot to pixels; and
- PTY, Wayland, keyboard, and clock adapters around those pure layers.

The parser, terminal model, and renderer read no descriptors, sockets, clocks,
environment, or global process state. Tests can therefore drive every state
transition with explicit bytes, sizes, keys, and time values. Adapter failures
close the affected terminal without corrupting model state.

## 10. First terminal profile

The first profile is a bounded, keyboard-first ECMA-48/DEC terminal sufficient
for td's shell and userland. It implements:

- streaming UTF-8 decoding with replacement of malformed input;
- a primary grid, an alternate grid, a cursor, tab stops, scrolling margins,
  origin mode, autowrap, and bounded primary-screen history;
- C0 bell as a coalesced visual notification, backspace, tab, line feed,
  vertical tab, form feed, carriage return, shift-in, shift-out, escape,
  cancel, and substitute controls;
- index, next-line, reverse-index, tab-set, save/restore, and reset escape
  operations, plus G0/G1 ASCII and DEC special-graphics designation;
- cursor movement and position, erase in display and line, insert/delete/erase
  characters, insert/delete lines, scroll, margins, tab clearing, and repeat;
- SGR reset, bold, faint, italic, underline, inverse, strike, default colors,
  the 16-color palette, indexed 256 colors, and 24-bit colors;
- normal and application cursor keys, primary device attributes, cursor
  position reports, and the replies required by the claimed profile; and
- DEC cursor preservation for mode 1048 and alternate-screen mode 1049.

UTF-8 scalars are initially single-cell glyphs. Wide cells, combining
sequences, grapheme clustering, bidi, shaping, and emoji presentation require
a separately pinned Unicode-data design. A missing glyph renders a visible
replacement cell. This limitation is part of the claimed profile rather than
an accidental difference hidden by the test overlay.

Ordinary C0 controls and DEL execute or are ignored without cancelling a
partially received UTF-8 scalar; ESC, CAN, SUB, and malformed non-continuation
bytes retain their parser recovery behavior. Color parameters use the
semicolon forms in the native corpus; colon subparameter forms are deferred.

The initial cursor is steady rather than clock-blinking. Shift+PageUp and
Shift+PageDown navigate scrollback. Ordinary text input returns to the live
bottom. An unmodified End key is consumed for the same purpose while viewing
scrollback and is forwarded in the selected cursor-key mode at the live
bottom. Mouse reporting, selection, clipboard, hyperlinks, images, sixel,
ligatures, search, and shell integration are deferred. A protocol is not
parsed merely because another terminal implements it.

Unsupported CSI operations are ignored as complete sequences. OSC, DCS, SOS,
APC, and PM strings enter allocation-free streaming ignore states and cannot
execute commands or open paths. BEL or ST terminates an ignored string; CAN
and SUB cancel one. ESC either begins ST or recovers through the normal escape
state. Unsupported input must not leak printable fragments or desynchronize
subsequent supported input.

Resource ceilings are part of the model contract:

- at most 32 CSI parameters;
- at most 1,048,576 history cells, 16,384 history lines, and 16 MiB of history
  storage;
- at most 1 MiB of queued PTY output, 64 KiB of queued keyboard input, and
  64 KiB of queued terminal replies; and
- screen dimensions bounded by the compositor's existing dimension and pixel
  ceilings.

Exceeding a syntactic ceiling transitions to a sink state that consumes through
the sequence's final byte before returning to ground. A full PTY-output channel
blocks its reader thread, applying kernel PTY backpressure without dropping
bytes. A keyboard sequence is enqueued atomically; if the complete sequence
cannot fit, td-term drops that whole input event and marks the visual bell
rather than truncating stream bytes or closing the session. History evicts
only complete oldest lines. A reply is also admitted atomically; if it cannot
fit, td-term drops that whole reply and marks the visual bell rather than
deadlocking the child's input and output paths. No queue or storage grows
without limit.

The child environment is cleared and reconstructed. A bounded parse of
`/proc/self/status` selects the matching unique `/etc/passwd` entry and
supplies `HOME`, `USER`, and `LOGNAME`; a missing, duplicate, malformed, or
mismatched account closes the terminal before child creation. Malformed is
whole-file, not per-entry: any line without seven fields or with a non-numeric
uid closes it, wherever it sits. td owns this file, so a line it cannot account
for is a system-integrity problem rather than an entry to skip past on the way
to the one being looked up. That file is the
whole account namespace: td resolves no Name Service Switch, so an account that
exists only in LDAP, NIS, or a directory service does not exist for td-term.
This is the same single-local-seat assumption `td-seatd` is built on, and
lifting it is a separately reviewed design change, not a parser change. The
remaining
values are `TERM=td-term`, `COLORTERM=truecolor`, `PATH=/bin`,
`SHELL=/bin/sh`, and `TERMINFO=/etc/terminfo`. The package carries its td-owned
entry under its store `share/terminfo`, and the system closure exposes that
immutable directory through `/etc/terminfo`; no new top-level image root is
needed. `XDG_RUNTIME_DIR=/run/user/UID` comes from the verified numeric uid and
`WAYLAND_DISPLAY=wayland-0` names the compositor socket. A dependency-free
encoder produces the entry from a human-readable capability source; `tic`,
ncurses, and a host terminfo database are not build inputs. Every boolean,
number, output sequence, and input key in that entry names a blocking native
case. A structural test decodes the installed entry and compares it
field-for-field with the source capabilities.

The encoder emits the legacy binary format rather than the 32-bit-number one,
which exists to carry `pairs#65536`; this entry's largest number fits the
signed 16-bit field, so the older format every reader understands is enough.
The three capability arrays are declared only one past the highest index
claimed, and a reader treats the rest as absent — which is also why the pinned
`Caps` ordering stops after `setab` instead of continuing through printer and
bit-image capabilities this profile will never claim.

That ordering is the whole trust surface. Position in the name tables IS the
wire index, so a capability written at the wrong one is a well-formed entry
that means something else, and no round-trip through the module's own decoder
can see it — the encoder and decoder would share the mistake. It is therefore
pinned as ordered lists rather than per-capability integers, so what a reviewer
checks is one list against `Caps`, with the counts and the order's
non-alphabetical joints asserted separately: `kf10` between `kf1` and `kf2`,
`lf10` between `lf1` and `lf2`, and `kf11` after `rfi` rather than after
`kf10`.

Attribution is checked, not merely declared. Key capabilities are compared
byte-for-byte against the sequence the keyboard adapter generates for that key,
because a key is emitted exactly as the entry spells it; the same comparison
runs against the corpus case's own input expectation. Output capabilities are
compared by escape-sequence shape — introducer, private flag and final byte,
and, for the finals where a parameter SELECTS the operation rather than
counting or positioning, the parameters too — because a capability spells the
default-parameter form (`\E[A`) of a sequence a case naturally writes with
parameters (`\E[3A`), and demanding the literal bytes would only push the
corpus into writing degenerate sequences to satisfy a test.

Attribution alone is not enough, and the gap is worth stating precisely. It
asks whether the named case exercises an operation; it cannot ask whether that
is the RIGHT operation for that capability. Where a family shares one case the
distinction is the whole point: the cursor case writes all four of `CSI A/B/C/D`,
so exchanging `cuu` and `cud` satisfies every attribution and ships an entry
that moves the cursor the wrong way — as do exchanging `il1`/`dl1`, `ich`/`dch`,
`indn`/`rin`, or any two of the nine renditions that share a case. Each such
capability is therefore pinned twice more: its declared spelling must be the
same operation as a concrete form written beside it, and feeding that concrete
form to the model must produce the effect its name promises. A capability that
shares a case with another and has no such check is refused, so the coverage
cannot quietly lapse as the entry grows. The colour capabilities are pinned by
expansion instead — every branch of `setaf`/`setab` is instantiated and driven
through the model — because a redirected branch still emits a well-formed SGR,
just for the wrong channel.

The entry is not yet reachable at runtime. The child is given
`TERMINFO=/etc/terminfo`, and the image does not expose the package's store
`share/terminfo` there, so ncurses cannot look `td-term` up: every curses
program still fails to start. Closing it requires an immutable-symlink category
in the read-only-`/etc` invariant — today every `/etc` symlink must be a
reviewed `MUTABLE_ETC` entry, and an immutable store path is not mutable state —
which is a separate reviewed landing, not part of producing the entry.

What the entry omits is as deliberate as what it claims. `cols`/`lines` are
absent because td-term sets and verifies the PTY winsize before the child
starts, so the pre-winsize fallback they exist to serve is unreachable by
construction. `smir`/`rmir` are absent because this profile implements no ANSI
insert mode, and `blink`/`invis` because it has no SGR for either — an entry
that claimed them would be describing a terminal td-term is not. `bel` is
absent for a different reason: BEL sets the model's coalesced visual-bell bit,
which no corpus observation can see until the renderer that presents it lands,
and a capability whose case would be a fiction is worse than a missing one.

An outer `TERM=foot`, `TERM=linux`, or other value describes the parent
terminal and is never an oracle or a capability claim for td-term. An optional
developer check may ask a pinned host `infocmp` to decode the generated entry,
but neither that tool nor its result participates in the required gate. The
check remains green when the optional host tool is absent.

## 11. Font, keyboard, and rendering

The first implementation pins one licensed PSF2 bitmap font with a Unicode
table: GNU Unifont 16.0.04, single-width, 8x16, 20673 glyphs. Its exact bytes
and license are committed under `td-compositor/assets`, while the archive hash
and upstream provenance are recorded there in `PROVENANCE`.

That face is derived rather than downloaded, which the provenance record and
`tools/import-unifont.rs` exist to make reproducible: upstream publishes no
full-coverage PSF2, only an APL-specific PSF1. The importer pins the upstream
`.hex` by hash and takes only its single-width records, which is not a
narrowing of Unifont so much as the only thing PSF2 can express -- one fixed
cell for every glyph -- and matches section 13 making double-width cells a
deliberate first-profile exclusion. It also excludes the two jiskan16 files
COPYING carves out of the dual license, by construction rather than by choice,
since both are 16x16.
Host tests and the target recipe consume those same bytes; no host font lookup
or fetched-only test input participates. The PSF2 reader checks headers,
dimensions, glyph counts, table bounds, scalar validity, and all pixel
arithmetic before use.

The renderer gives every claimed rendition a deterministic presentation from
that one face. Bold adds a clipped one-pixel rightward copy of set glyph bits,
faint blends foreground halfway toward background with integer channel
arithmetic, and italic applies a bounded row-dependent one-pixel shear.
Underline and strike draw fixed clipped cell rows, and inverse exchanges
foreground and background. Blocking PPM cases prove that each claimed
attribute differs from an otherwise identical normal cell.

The fixed palette is xterm's: its sixteen base entries are a table, and the
remaining 240 are computed from the arithmetic that defines them -- the
six-level cube on 0, 95, 135, 175, 215, 255, then the grey ramp from 8 in
steps of 10 -- so those entries cannot drift from their own definition.
Default ink is entry 7 on entry 0 rather than a seventeenth colour, so
`SGR 39` and `SGR 49` land back on a palette the child can also name.
Faint follows inverse rather than preceding it: after the exchange the
drawn foreground is the one to dim, and blending before it would brighten
an inverse-and-faint cell instead.

The cursor is a presentation of that same exchange. Focused, it is its cell
drawn with inverse toggled, so a cursor over an already-inverse cell reads
as the surrounding text. Unfocused, it is a hollow one-pixel box in the
cell's foreground, leaving the glyph legible underneath: present, but not
claiming the keyboard. That box is the same colour as the glyph it rings, so
over a cell whose border pixels are all set -- `U+2588`, and some
box-drawing -- an unfocused cursor is invisible. That follows from drawing it
in the cell's own foreground and is accepted for the first profile, where an
unfocused terminal has nothing to locate; a focused cursor is never affected,
since exchanging the ink is visible against any glyph. A pending wrap does
not move either, because the model already reports the column the cursor
still occupies.

The renderer consumes a complete terminal snapshot, a fixed palette, focus
state, and cursor state. It performs no allocation in the cell loop. A full
redraw is acceptable for the initial QEMU profile, but rendering is coalesced
behind at most one frame callback. A submitted persistent buffer is reused or
mutated only after its `wl_buffer.release`; the initial fill precedes its first
submission. Resizing creates a replacement while retaining the old buffer
until release. Every glyph and decoration is clipped to the surface before
pixels are visited.

C0 BEL, an atomically dropped keyboard event, or an atomically dropped reply
sets one coalesced visual-bell bit in the snapshot. The next submitted frame
inverts the one-pixel ring inside the client surface and clears the bit after
release; repeated notifications before that release do not queue additional
frames. A blocking PPM case and the file-backed gallery cover the exact
presentation.

td-term binds the compositor's keyboard and validates that the received
keymap descriptor contains exactly the shared `keyboard::XKB_KEYMAP` string
followed by its NUL terminator, matching the server's advertised size. A
mismatch closes the client before child creation; running under another
Wayland compositor is outside the first profile. The terminal translates the
fixed evdev key codes and standard XKB modifier masks itself; it does not
import libxkbcommon. A pinned in-tree table marks text and navigation keys
repeatable and modifiers non-repeating, mirroring the exact keymap's repeat
exclusions. Validation uses positioned `FileExt::read_at` calls so reading one
SCM_RIGHTS duplicate cannot advance the shared open-file-description offset
seen by a restarted or second client.

The input adapter covers text keys, Enter, Tab, Backspace, Escape, arrows,
Home, End, PageUp, PageDown, Insert, Delete, and F1 through F12. It selects
normal or application sequences from explicit terminal modes. Ctrl produces
the specified ASCII C0 bytes, Alt prefixes the resulting sequence with ESC,
and Shift selects the defined text or navigation variant; unlisted modifier
combinations produce no bytes.

Those three rules are exhaustive, and what they exclude is deliberate. Ctrl
reaches printable keys only, and only where a C0 spelling is defined; the
character it maps is the one Shift already selected, so `Ctrl+Shift+6` needs no
second rule to reach `RS`. Alt prefixes ESC uniformly rather than folding a
modifier into a CSI parameter, because this profile does not claim the
modified-key encodings such a parameter implies, and a sequence it does not
claim would be indistinguishable to the child from one it does. Shift on an
arrow or a function key is therefore silent, and `Shift+PageUp` and
`Shift+PageDown` belong to the scrollback viewport rather than to the child.
Keys the pinned keymap publishes but this profile does not translate — Print,
Pause, Menu, and the media keys — are silent for the same reason.

Shift is not silent everywhere: on the fixed keys it passes through, because
they have one spelling at both levels and real terminals send CR for
`Shift+Enter` and DEL for `Shift+Backspace`. Tab is the one fixed key with a
defined second spelling. It is the navigation and function keys, which have no
shifted spelling in this profile, that Shift silences.

A modifier the profile does not translate makes the whole chord unlisted rather
than a bare key press. The compositor forwards Super chords it has no binding
for, so without that rule `Super+q` would type `q` and `Super+Enter` would
submit whatever the shell had half-typed. Shift, Caps Lock, Control, and Alt
are the handled set; Num Lock is handled-and-inert, because this profile's
keypad is digits-only; any other bit, including an undefined one, silences the
chord.

The key table is checked against the compositor's published keymap in both
directions, and the keys it marks repeatable are exactly that keymap's
`repeat=no` exclusions inverted, so a client using the published keymap and
td-term cannot disagree about which keys autorepeat.

Those are checks on codes and characters, and neither reads a roster name. A
code set is a set, so two entries that trade codes leave it identical; the
character check walks the keymap into a spelling without consulting the name
that selects it, so two entries that trade names leave it identical too — and
then a corpus chord names one physical key and reaches another. Every roster
name is therefore pinned to the keysym the keymap publishes for its key, and
that pin covers the roster exactly in both directions, so a key added later
cannot arrive unpinned. It is also the keypad's only per-key identity check,
since the character check excludes `KP_7`-style symbols on purpose. Caps Lock behaviour is read
from the keymap's declared `type="ALPHABETIC"` rather than inferred from a
key's symbols looking like a letter pair, because retyping a key changes what
Caps does for every xkbcommon client while its symbols stay as they were.

Backspace emits DEL (`0x7f`) to match the
slave's Linux-default canonical `VERASE`; Alt prefixes that byte with ESC. The
compositor suppresses evdev repeat and publishes a repeat rate of 25 Hz with a
600 millisecond delay, so td-term implements repeat from an injected clock.
Release, focus loss, or any modifier snapshot change cancels the corresponding
repeat; this also covers compositor chords whose command-key events are
intercepted. Releasing some other key leaves a held key repeating, and
repetitions missed while the main loop was busy are dropped rather than
delivered as a burst. A repetition is routed when it is emitted, not when
the key went down: the child can change cursor-key mode while an arrow is held,
and a stored sequence would keep sending the spelling that was correct at the
press. A held chord that scrolls repeats as a scroll, since walking back
through scrollback is what holding it is for and the compositor sends no
repeat events of its own to fall back on. Routing per repetition is also
what makes a held End coherent: the first repetition closes the viewport and
the ones after it are the child's, because by then the view is at the live
bottom.

Keyboard bytes reach the PTY writer through a bounded queue that admits a
sequence whole or drops it whole: half a `CSI` arriving at the child would be
worse than the key never having been pressed, so an overflowing queue rings the
visual bell instead of truncating. The writer consumes only what the kernel
accepted, so a partial write leaves the remainder queued; keystrokes have
nowhere to come back from. Because the master is blocking, a child that stops
reading blocks that writer once the line discipline fills — which is why the
writer is its own thread and the main loop only enqueues.

`Shift+PageUp` and `Shift+PageDown` move the viewport a screen less one row
at a time, so the line last read is still on screen to read on from, and a
one-row grid still scrolls by one rather than not at all. Both stop at the
ends: there is nothing above the oldest retained line and nothing below the
live screen, so a chord at either end is inert rather than an error.

The viewport stores the line it is looking at in a monotonic numbering of
lines ever pushed to primary history, not a distance from the live bottom,
because the bottom moves. A stored distance would let a child writing
underneath an open viewport drag the view along with it, one line per line
of output. Clearing that history — a reset, or `CSI 3 J` — retires the
numbering along with the lines, so an anchor is tagged with which numbering
it belongs to. Zeroing the count alone would not close the view: the old
anchor's line number comes back around as new lines arrive, and the view
would reopen on lines that have nothing to do with it.

The distance the anchor implies is clamped on every read rather than stored,
because eviction drops the oldest lines and a resize can shorten the history
an anchor lives in. An anchor whose line has been evicted rides the top of
what history still holds, rather than being thrown back to the live bottom:
that is where the reader was heading, and on a full buffer the alternative
moves the view on every further line of output. Riding the top is therefore
the end of what scrolling back can reach, and a retired numbering is the
only thing that returns a view to the live bottom without a key. A silent
key does not re-anchor at where a clamp put the view — the anchor still
names the line asked for. End's two meanings follow that position rather
than whether the viewport was ever opened, since what it asks is whether
anything but the live screen is showing.

The renderer's half of that viewport is landed: a snapshot carries how many
lines back it is scrolled, rows above the split come from the primary
history and the rest from the live screen, and a request deeper than the
stored history clamps rather than blanks. History is primary-screen only,
so the viewport reads it even while the alternate screen is active -- which
is what lets it show the shell a full-screen program is covering. A line is
stored at the width it scrolled off with, so a widening resize leaves the
tail of an old line blank rather than fabricating cells for it.

The keys that select it are landed too, so the viewport is complete as a
pure pair: the adapter routes a press to the child, to the viewport, or
nowhere, and the viewport turns those into the offset a snapshot takes.
What is left for the client is calling them — nothing yet asks the adapter
what a key means, and the corpus is what drives it instead.

## 12. PTY and process lifecycle

After mounting devtmpfs and before graphical services, the system creates
`/dev/pts`, mounts devpts there with
`newinstance,ptmxmode=0666,mode=0620,gid=5`, removes devtmpfs's existing
`/dev/ptmx` node, and creates the relative `ptmx -> pts/ptmx` symlink. The
image pins `CONFIG_UNIX98_PTYS=y` and its existing `tty` group owns gid 5.
td-term opens `/dev/ptmx` with safe `std` file operations, unlocks it, and
obtains the slave as an owned descriptor with `TIOCGPTPEER` and
`O_RDWR | O_NOCTTY | O_CLOEXEC`. No `/dev/pts/N` path is reopened.
The image proof pins the startup mount command and checks the effective slave
gid/mode plus `pts/ptmx` mode; it does not require `/proc/mounts` to echo the
modern kernel's accepted no-op `newinstance` token.

Stable Rust does not expose the required PTY operations. The widening adds
x86-64 `SYS_IOCTL=16` to the existing raw body. Its safe wrapper accepts
exactly four request values:

- `TIOCSPTLCK=0x40045431`, to unlock the slave;
- `TIOCGPTPEER=0x5441`, to obtain the slave as a new owned descriptor;
- `TIOCSWINSZ=0x5414`, to publish rows and columns; and
- `TIOCGWINSZ=0x5413`, to verify every published size before it becomes
  visible to the child.

The confinement tests pin the `SYS_` constant count, raw-body call count,
request values, and callers. This setter applies only to td-term's newly
created PTY; it does not weaken the separate repository prohibition on
resizing an operator's terminal. A request outside the four is refused by the
one `ioctl` entry point before the syscall is issued, so a mistyped or newly
invented number cannot reach the kernel without amending both the roster and
the test that pins it.

The wrappers use a four-byte native-endian `int` for `TIOCSPTLCK` and an
eight-byte `[u16; 4]` of native-endian rows, columns, and the two pixel fields
for both winsize requests. The array rather than a `#[repr(C)]` struct because
the language guarantees that layout, which turns the field ORDER into an
ordinary tested function: a swapped rows/columns pair is a well-formed resize
to a different size, and an attribute nobody can observe would not catch it.
The kernel never receives a pointer to a temporary or shorter object, and the
existing assembly body remains memory-aware: it does not acquire
`options(nomem)`. `TIOCGPTPEER` receives the open flags as an immediate value
rather than a pointer, and the flags are pinned in `sys.rs` rather than chosen
by a caller, so `O_NOCTTY` cannot be forgotten by the one call site that must
not acquire the terminal.

Its nonnegative return is adopted exactly once, through the same
`/proc/self/fd/N` duplication the received-descriptor path already uses, and
the raw number is closed. `OwnedFd::from_raw_fd` is `unsafe`, and a second
scoped allow of a different SHAPE — a descriptor adoption rather than the
syscall-instruction layer — is a wider amendment than this adapter needs when
the crate can reopen the descriptor by identity instead. The reopen is by
descriptor number, not by terminal name: no `/dev/pts/N` path is resolved, so
it retains the property the peer request was chosen for.

No termios construction, signal syscall, process creation, or descriptor
duplication enters that unsafe surface. The slave's kernel defaults provide
canonical input and echo. Safe `Command` and `Stdio` operations wire three
slave clones to the child.

All SCM_RIGHTS operations for td-term remain in the existing `client.rs`
transport boundary, including keymap receipt and wl_shm submission. The
client landing updates that caller inventory and §4's recvmsg and sendmsg
descriptions; terminal parser, model, renderer, keyboard, and PTY policy
modules do not call the descriptor-transport wrappers.

Safe `Command` cannot call `setsid(2)`, and `pre_exec` would introduce a second
unsafe surface. The declared td-init input therefore extends `cttyhack` with
an explicit `--stdin` mode. That mode always creates a new session and claims
descriptor zero without stealing a terminal, even when the wrapper inherited
an outer controlling terminal. Unlike rescue mode, `--stdin` exits nonzero if
`setsid(2)` or `TIOCSCTTY` fails. td-term invokes
`/bin/cttyhack --stdin /bin/sh`, or the command supplied on its own command
line. The td-term recipe and system integration tests assert that the staged
td-init advertises and exercises this exact flag, tying the absolute path to
the declared runtime input. Ordinary rescue-console behavior remains
unchanged. The child starts in the verified account home: setting `HOME` does
not move a process, so without an explicit working directory the shell would
start wherever td-svc left the graphical service and disagree with its own
environment. A home the child cannot enter fails the spawn rather than silently
landing in `/`. Immediately after a successful spawn, td-term drops the original
slave and all three parent-side `Stdio` clones, retaining only the master.
Closing that master produces the kernel's normal PTY hangup; child exit unmaps
the surface and terminates the client.

The PTY reader thread owns a master descriptor and parks in `read` whenever the
child is idle, and safe `std` cannot interrupt that: there is no poll, no read
timeout, and closing a descriptor another thread is reading is not something
this crate may express. Its only retirement is the child's exit closing the last
slave. That is sound because td-term is one process per terminal: closing the
terminal IS exiting, process exit closes the descriptor, and the kernel then
sends the child `SIGHUP` for its controlling terminal. The consequence is a
contract rather than a mechanism — a teardown path must not join that thread —
and interrupting the reader for any other reason requires a separately reviewed
wakeup surface.

The client first completes the required empty XDG commit and initial
configure/ack. It maps a bounded blank placeholder and waits up to the same
20-second absolute startup deadline as the existing demo for the compositor's
nonzero tile configure. Expiry closes the client before child creation; tests
inject the clock and never sleep. The client derives the exact cell grid, sets
and verifies the PTY winsize, and only then starts the child. A tile smaller
than one font cell uses a logical 1-by-1 grid whose pixels remain clipped to
the actual surface. Later nonzero configures preserve horizontal overlap
without reflow. On primary-screen vertical shrink, blank tail rows disappear
first; otherwise top rows move to primary history so the lowest content and
cursor survive. The alternate screen discards removed rows, and resizing the
hidden grid never adds history. Growth appends blank rows to both grids. The
client updates and verifies the PTY size before rendering the replacement
buffer.

A blocking Wayland reader, PTY reader, PTY writer, and child waiter send
bounded messages to one main loop. A full PTY-output channel blocks its reader
thread and lets the kernel PTY buffer backpressure the child. The main loop
alone mutates the terminal model and writes Wayland requests. No correctness
condition relies on poll, elapsed sleeps, or scheduler order; the startup
deadline bounds failure detection rather than ordering state transitions.

td-term exposes a mode-0600 readiness socket and prints `TD-TERM-READY` with
its rows and columns only after the exact tile-sized buffer receives both
`wl_buffer.release` and its frame callback. td-svc's `ready=` command uses the
existing credential-switch pattern to invoke
`/bin/td-term probe /run/user/1000/td-term-ready` as the graphical user. The
probe requires a ready state and nonzero internally consistent rows and
columns; its output and the matching `TD-TERM-READY` QEMU diagnostic are
compared in integration tests. The boot profile atomically replaces the
visible `td-ui-demo` service and removes its final-image symlink when this proof
is complete. The compositor and serial recovery greeter remain independently
restartable.

## 13. Native terminal corpus

td-term behavior is specified in one td-native text corpus. Imported and
td-authored cases use the same format and live together by subject:

```
td-compositor/spec/term/
  README
  parser.term
  cursor.term
  editing.term
  wrapping.term
  modes.term
  color.term
  replies.term
  input.term
  resize.term
  unicode.term
  expectations.txt
  LICENSE.libvterm
  visual/*.ppm
```

The visual oracle is two-tiered, and only the lower tier is built. Goldens
that pin the renderer itself live beside it in `td-compositor/spec/render/`,
driven by native Rust fixtures that name a snapshot, a surface size, focus,
and a cursor directly; they are what the renderer's own landing proves.
`spec/term/visual/` above is the upper tier -- a corpus case rendering its
own final grid through an `expect ppm` statement -- and neither those images
nor that statement exist yet. A corpus case cannot render until the parser
below it also carries a surface size and a focus state, which is a corpus
format change rather than a renderer one, so it lands with the Wayland
client that gives a frame those properties in the first place.

The model starts with a small td-authored seed corpus. The bulk migration then
converts a source archive and SHA-256 pin of the MIT-licensed libvterm 0.3.3
suite. A sibling license file retains the complete upstream copyright and
permission notice. The archive and original harness do not enter td's build or
repository. A dependency-free Rust importer accepts an explicitly supplied
verifies its source-file manifest, rejects every unknown source command or
assertion, and emits deterministic native cases. Its migration report counts
source files, cases, assertions, converted assertions, and every intentional
exclusion. The landing records those counts and reasons.

Each derived case retains its source release, path, and original case identity.
The conversion targets externally observable cells, cursor, modes, history,
properties, and replies rather than libvterm callback names. After the
migration the native cases are normative and maintained with td-authored
cases; provenance remains even when a derived case is clarified. There is no
separate upstream test directory or legacy-format reader in the blocking
corpus or target artifact; the developer-only importer is the reproducer.
Pinned cases are classified against the first-profile feature matrix:
upstream-positive tests for deferred protocols are exclusions, not product
xfails, excluded sections roll back to their last reset, and retained cases
never replay deferred control sequences. Primary DA is normalized from
libvterm's identity to td's. The first profile accepts semicolon-delimited SGR
colors; colon-separated color subparameters remain an explicit exclusion.

The std-only importer remains a non-shipped developer provenance tool, not a
runtime or build reader. Its unit tests and committed complete source manifest
exercise the upstream parser without the archive; when an explicitly supplied
tree is available, its exact check verifies all source hashes and reproduces
the committed corpus and report. No upstream-format case runs in the gate.

The native language has stable case identifiers and a deliberately small
vocabulary:

- `case`, `source`, `tags`, `size`, and `end`;
- `write`, `resize`, and `key` operations; and
- `expect` statements for rows, imported text and glyph observations, cells,
  cursor, modes, cumulative terminal replies, cumulative keyboard input,
  history, the scrollback viewport, and an optional rendered PPM -- the last
  of these deferred, as §13's tree records. Cursor expectations accept only
  the optional `pending-wrap` flag.

Every case has a source. td-authored cases use `source td`; derived cases name
the pinned release, path, and original case. `size` is rows followed by
columns, byte strings use Rust-like ASCII escapes, and cursor coordinates are
zero-based. A representative case is:

```
case wrapping/right-margin
source "libvterm-0.3.3:t/20state_wrapping.test:right margin"
tags core wrapping
size 2 5
write b"ABCDE"
expect cursor 0 4 pending-wrap
write b"F"
expect row 0 "ABCDE"
expect row 1 "F    "
expect cursor 1 1
end
```

Byte literals use one specified escape syntax and reject ambiguous or invalid
escapes. Row expectations are shorthand for default single-width cells; cell
expectations state scalars, colors, and attributes explicitly. Imported
character-only observations use `text` and `glyph`, which deliberately ignore
rendition absent from the source oracle. Replies are ordered byte strings. The
parser rejects unknown fields, duplicate stable identifiers, empty cases,
assertions before initialization, and expectations that escape the declared
grid. Reply expectations name the complete byte stream emitted since case
initialization; the PTY adapter drains that bounded stream after each
successful master write. Input expectations separately name the keyboard
adapter's complete generated byte stream, making `key` operations observable
before the PTY writer merges the two bounded sources.

Feature tags distinguish deliberate profile exclusions such as mouse or
double-width cells from missing behavior inside the first profile. A generated
`expectations.txt` records in-profile known failures by case and expectation,
so another observation cannot regress behind an existing failure. Every
in-profile case still runs. An unlisted failure, unexpected pass, stale entry,
unmatched case, unknown tag, or malformed corpus reds the gate.

Every byte-stream case runs as one write, one byte per write, at every
two-piece split, and under deterministic pseudorandom chunkings. All forms
must produce identical cells, cursor, modes, history, and replies.
Deterministic arbitrary-byte cases additionally enforce total parsing,
resource ceilings, valid cursor/grid relationships, and absence of panics.

The committed native expectations are the blocking semantic oracle. No host
terminal or external emulator runs in the gate. `$TERM` is only a capability
label. Foot remains a product reference and an optional black-box comparison,
not the normative state model.

## 14. Visual and end-to-end proof

The pure renderer's blocking visual oracle is exact P6 PPM output. Selected
cases render with the pinned font, palette, surface size, focus, and cursor.
Those five are parameters rather than defaults, so no case can be green
against a face or a palette it did not name. A mismatch reports the first
differing coordinate and writes an actual image plus a high-contrast PPM
diff beneath the build's temporary output; no PNG encoder or image library
is required. The cases are Rust fixtures today and native corpus cases once
the corpus format carries a surface, per §13.

Exactness is the contract in both directions: a golden whose bytes differ
from what the encoder emits fails even when it decodes to identical pixels,
because the only thing that could produce one is a hand-edit, and a hand-
edited golden is no longer an oracle. Goldens are generated by the renderer,
so what makes them evidence is not their provenance but the structural
assertions beside them -- that each rendition differs from an otherwise
identical normal cell, that bold only adds pixels and each added one is a
step right of a set one, that italic's every top-half pixel is its normal
neighbour shifted one column, that underline and strike are exactly one full
row each. Those are what a wrong renderer fails; the goldens are what a
CHANGED one fails.

A smaller integration gallery runs td-term against a real td-compositor with
a file-backed framebuffer. It compares the compositor's exact final XRGB8888
frame, including tile geometry, borders, clipping, buffer replacement, and
frame-callback lifecycle. This is the pixel-parity gate for the shipped stack.

Foot comparison is a separate, non-blocking developer operation. It uses a
pinned foot binary, font, configuration, fixture, geometry, and isolated
headless Wayland environment to produce side-by-side captures. Different font
and rasterization stacks make exact cross-terminal pixels a false contract;
the gallery adjudicates taste and exposes behavioral disagreements for a
native semantic case to settle. The required check remains green when these
optional host-side comparison tools are absent.

The complete terminal landing must prove:

- the native corpus is structurally valid, attributed, consistent with the
  committed migration counts and digests, and guarded by a generated
  no-regression expectations overlay;
- parser and model results are invariant under every required input chunking
  and remain bounded for malformed streams;
- exact model-renderer PPM and full-compositor framebuffer goldens pass;
- the shipped artifact is static, the `td-term` entry point is a relative
  symlink, and target selftests run without host paths or libraries;
- the installed `td-term` terminfo entry decodes to exactly the capabilities
  exercised by the native corpus;
- a PTY fixture sees the peer descriptor's `/dev/pts/N` as its controlling
  terminal and reads back the grid's exact winsize;
- focused keyboard input reaches the PTY, echoed output changes the framebuffer,
  and repeat cancellation is deterministic;
- two sequential clients validate the shared keymap descriptor without
  advancing its open-file-description offset;
- a compositor configure replaces the wl_shm buffer, preserves the specified
  grid overlap, and updates the PTY size before the child observes it;
- child exit, PTY hangup, compositor disconnect, queue saturation, and malformed
  Wayland input terminate without a stuck worker or leaked surface;
- the image creates the devpts mountpoint after devtmpfs, replaces its
  `/dev/ptmx` node with the specified symlink, mounts devpts with the specified
  options, contains the selected font and required multicall entry points,
  starts td-term as uid 1000, passes the readiness-socket probe, and observes
  the matching `TD-TERM-READY` diagnostic; and
- graphical failure leaves the serial recovery path and existing compositor
  readiness proof intact.
