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
supports EV_KEY, EV_REL, and EV_SYN. It has a fixed US key map for compositor
bindings only. Left and right arrow move focus across columns. Client keyboard
and pointer delivery, arbitrary keymaps, touch, calibration, gestures, and
real GPUs are later increments.

The framebuffer is single-buffered from userspace's perspective. The renderer
allocates its frame storage once, composes a full frame after scene changes,
then writes one stride-complete image. There is no page flip, vblank,
acceleration, DMA-BUF, or tear-free claim.

Pointer motion is a scene change: each evdev `SYN_REPORT` currently performs
that full repaint while holding the runtime lock. This is bounded enough for
the supported QEMU PS/2 profile but is not a high-rate input design. Damage
tracking or a throttled render loop is required before adding such hardware.

## 3. Wayland surface

The server accepts local Unix-stream clients at
`/run/user/1000/wayland-0`. The socket and parent directory are owned by uid
1000 and are not group/world accessible.

The first protocol surface is:

- wl_display and wl_registry
- wl_compositor, wl_surface, and wl_region
- wl_shm, wl_shm_pool, and wl_buffer
- wl_output
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

The boot profile starts one 512x320 `td-ui-demo` toplevel. It discovers and
binds globals rather than depending on registry names, completes the XDG
configure/ack handshake, sends a dependency-free software pattern in an
XRGB8888 wl_shm pool, requests a frame callback, and stays connected so the
surface remains mapped. It exposes its mode-0600 readiness socket and prints
`TD-UI-CLIENT-READY` only after both wl_buffer release and that first callback
arrive. The client has no toolkit, fonts, input handling, animation, or
application model; it is the first live protocol proof and a visible boot
fixture. Its presentation handshake has a 20-second absolute deadline, shorter
than the supervisor's 30-second readiness deadline, so a stalled compositor
makes the client exit and permits `restart=always` to retry.

The layout is one horizontal row of columns. Each toplevel is a column;
left/right focus changes which columns are visible. Decoration, clipboard,
drag-and-drop, subsurfaces, popups, output reconfiguration, fractional scale,
screen capture, data devices, and client input are not yet advertised.
Unknown objects, malformed sizes, invalid object reuse, missing file
descriptors, and unsupported requests disconnect only that client.

Resource ceilings are part of the protocol boundary: at most 32 clients run
at once, each has at most 512 objects, 64 queued descriptors, and 32 MiB of
committed pixels, and the complete scene retains at most 128 MiB. Rendering
clips rows and columns to the output before visiting pixels. These are
availability bounds against a same-user client, not isolation between
mutually distrusting users.

## 4. Unsafe confinement

Wayland carries wl_shm descriptors as SCM_RIGHTS ancillary data on its Unix
stream. Stable Rust 1.96 exposes no stable ancillary-data API. The user
approved one new target-side exception for this transport.

`td-compositor/src/sys.rs` contains the sole scoped `unsafe` block. One raw
`syscall3` body carries exactly:

- sendmsg(2), to send the demo client's wl_shm descriptor or a test request;
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
- the boot client discovers globals, completes XDG configure/ack, receives
  wl_buffer release and its first frame callback, and remains mapped;
- software composition clips surfaces and never indexes outside a frame;
- the image contains all three binaries, the service order is checkable, and
  the compositor and client run as uid 1000;
- existing serial boot checks remain green.

## 7. Deferred UI stack

The next increments may add client input, a bitmap font and launcher,
clipboard, a terminal, hotplug, and real DRM/KMS profiles. General Wayland
toolkit compatibility is not claimed until the missing core protocols have
explicit tests. Hardware acceleration, niri, portals, PipeWire, Xwayland, and
a C desktop stack remain optional consumers rather than foundations of td's
UI.
