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

A status bar owns the top 24 rows. It is BUILT IN rather than a client,
because a client would need a way to be placed and to reserve space —
layer-shell, or a private protocol standing in for it — and none of that is
here. The tiling area is therefore the output minus those rows, and every
consumer of tiling geometry goes through one pair of helpers in `scene.rs`:
the renderer, the layout published to clients, and the pointer hit test.
That is not tidiness. Two of them disagreeing is a click landing on a tile
other than the one under the cursor, with nothing on screen to say so. It
half-happened while this landed: the two entry points left reaching
`layout.placements` directly were the `#[cfg(test)]` hit-test wrappers, not
the production one, so what it would have shipped is not a mis-aimed click
but tests measuring a geometry the compositor no longer used — the same
defect one step further back, since those tests are what certify the click.
A click anywhere in the bar's own rows reaches no tile at all. FULLSCREEN
fills the tiling area rather than covering the bar, which is deliberately
unlike i3 — a user-visible divergence from the window manager this is modelled
on, and the reason the reservation has a test of its own: fullscreen is the
one arrangement with no gap, so it is the only one whose pixels would reach
row 0 if the reservation were dropped.

It shows load, memory, uptime and a UTC clock, all read from `/proc` as
ordinary files, so the bar adds no syscall and needs no `UNSAFE.md`
amendment. Memory is total minus MemAvailable rather than minus MemFree,
which counts neither cache nor reclaimable slab and reads alarmingly low on
an idle machine. A reading that could not be taken shows its label with `?`
rather than vanishing, so a broken source looks broken instead of looking
like a machine with less to report; each field fails on its own, so a
garbled `loadavg` does not take the clock down with it. The `?` is why the
font grew a period: an unmapped byte used to draw a glyph shaped like a
question mark, so `LOAD 0.42` rendered as `LOAD 0?42` and a healthy reading
was indistinguishable from a failed one. The fallback is a box now, `.` and
`?` are glyphs of their own, and both the bar's line and the help sheet's
rows are held to a font that has every character they spell.

Before any of those fields the strip names the WORKSPACES, which is the one
thing on it that is not a reading. A workspace switch is otherwise invisible:
moving to an empty one leaves a bare desktop, and that is what the workspace
just left looks like from behind a fullscreen window, so an operator who
pressed `Super+7` by accident has nothing to tell them where they are. The
strip names every workspace holding a window, plus the ACTIVE one whether or
not it holds anything — a workspace exists in the layout's map as soon as
anything asks for it, so holding a root is what separates one somebody is
using from a number nobody has been to.

The active one is the strip's own two colours EXCHANGED. It needs no third
colour and no glyph beside the number, and inverse video says "you are here"
without an operator being told which of two shades of one hue means what. Each
cell is its number and a little air either side rather than a fixed column, so
a workspace costs the fields beside it twenty pixels and not a reserved strip.
The number is CENTRED in that cell rather than padded equally, because a
glyph's advance carries a trailing column it never inks: equal padding leaves
it a pixel left of centre, which nothing shows on the bar and the block of ink
around an active cell does.

The workspaces are painted from the LAYOUT on every frame rather than folded
into the status line, and that is what makes the mark follow a switch at once:
the line is composed by the bar's own thread on a one-second tick, so a
workspace in it would lag the keystroke by up to a second. Every tiling
command repaints unconditionally, which is what makes a switch between two
EMPTY workspaces — where the strip is the only thing that changes — reach the
screen at all.

Beside those the strip always names one SPARE workspace: the lowest-numbered
one holding nothing, which is never the active one. That cell is the only way
the pointer can reach a workspace that does not exist yet. Without it the strip
names only what is already there, so a machine using one workspace offers
nowhere to drop a window except where it already is, and a second workspace can
be reached solely from the keyboard. The lowest free number rather than one
past the last, so a workspace emptied by moving its last window away is offered
again instead of the range walking off its end; and never the ACTIVE one even
when it is empty, because a window can only be dragged from the workspace in
view, so a cell naming it is a drop that moves nothing — which would leave an
operator standing on an empty workspace with no way to reach a new one. There is
no spare only when every workspace in 1..=9 holds something except the active
one, and the strip says so by not growing.

They are not CLICKABLE, and that is still true: the strip answers no press
(§3's hit test stops at the tiling area), so nothing starts there and a click
on a number switches nothing. What they now answer is a RELEASE — the end of a
drag that began on a title band, which is a different gesture with a window
already in hand. Making a region of the bar answer a PRESS would be a pointer
surface of its own rather than a label; answering a drop adds no such surface,
because the drag is already in flight and the strip is only being asked where
it ends. The keys reach every workspace meanwhile, and `Super+Shift+N` remains
the way to send a window to one without the mouse.

A drop on a cell moves the DRAGGED window rather than the focused one. A press
on a band focuses it, so the two name the same window today; they are separate
questions all the same, and a drop that moved whatever happened to be focused
would be a different gesture the moment that stops being true. The workspace
the window is already on promises nothing, for the same reason its own tile
does: there is no move to make, so no block goes up and the release is a
cancelled drag. Only the CELLS answer — the status line beside them names no
workspace, so a release there cancels rather than falling through to whichever
number was drawn last, or to the window the bar is covering.

A drop on the strip is NOT refused under fullscreen, where a drop on a tile
is, and the asymmetry is deliberate rather than an oversight. `drop_onto`
refuses because a fullscreen window has no tile to land beside and pulling it
into an arrangement is incoherent; sending it to another workspace is perfectly
coherent, and is exactly what `Super+Shift+N` already does to one. The gesture
is reachable only mid-drag — a band press, then `Super+f`, then a release on a
cell — since a fullscreen window's band is zero-height and cannot be pressed,
and Alt-drag refuses it outright. The window arrives on the other workspace
having given up fullscreen, which is what the keyboard does too.

The block for a workspace drop is the one thing drawn OVER the bar. Every other
block goes under it, deliberately: a block that hid the bar would be a worse
lie than the one it answers. This block IS the bar, so drawn in that order it
would be painted away by the very strip it is promising.

The NETWORK field is leftmost of the readings, as the ethernet stanza is in
the config this follows. It names the interface, whether it is up, and the
address — and the address is the part with a story. Nothing td writes down
records it: td-netd prints the acquired lease to stdout and drops it,
`/run/resolv.conf` gets nameservers only, and the generated `/etc/hosts` is
loopback. So the address comes from `/proc/net/fib_trie`, the kernel's own
routing dump, where a local address is a `/32 host LOCAL` leaf. That file is
the only one that has it without a `SIOCGIFADDR` ioctl, which would be a new
syscall on a surface that has none and so an `UNSAFE.md` amendment for a
status field.

The dump says which addresses EXIST and never whose they are, and that governs
the whole field. An address is attributable only when there is exactly one
non-loopback interface it could belong to; with a second one present the named
interface may not be the one holding the lease, and `NET eth0 <eth1's address>`
is a well-formed line that is wrong. Wrong is worse than absent on a status
bar. More than one non-loopback local address is refused for the same reason,
and loopback — present in every routing table — is never the answer. The one
residual is an address aliased onto `lo` itself, which the `127.` filter does
not catch and nothing here can attribute.

So the address is THREE states rather than two, and they read differently:
`NET eth0 10.0.2.15` when it is known, bare `NET eth0 UP` when the table says
there is genuinely none, and `NET eth0 UP ?` when there is one but nothing
attributes it — `?` keeping the meaning it has everywhere else on this bar,
could not be determined. Collapsing the last two would tell an operator their
link has no lease while the machine is reachable. `DOWN` outranks any address
still configured, since a stale address on a link with no carrier is not
somewhere to reach this machine.

Link state comes from `operstate`, and only `up`, `down` and `lowerlayerdown`
are answers. A driver with no carrier reporting writes `unknown`; reading that
as down would put `DOWN` beside a working interface and suppress its address
with it, so `unknown`, `dormant`, `testing` and `notpresent` leave the state
unknown instead.

The interface is chosen by td-netd's own rule (skip `lo`, sort, prefer a name
beginning with `e`), which is a SECOND COPY of that convention rather than a
shared one, since the two crates share no library: a change there is a bar
naming an interface nothing configured. Non-UTF-8 names are kept lossily for
that reason too — dropping one would be the two crates sorting different
lists.

There is no WIRELESS field. td's kernel forces `CONFIG_VIRTIO_NET` and
`CONFIG_E1000` on and no wireless driver at all, so the field could never be
anything but `?` — a permanent question mark is worse than an absent stanza.
Temperature is out for the same reason until a target has a thermal zone.

The fields are otherwise ordered as the i3status config they are modelled on
has them, which puts the clock last. The line begins after the workspaces and
is clipped to what they leave, so a line long enough to reach one loses its
own end rather than being drawn over the cell that says where the operator is.
The CLOCK is therefore what clips first on a narrow output — the field the bar
most exists for. The network field made that a live concern rather than a
theoretical one: without it the line is 752 pixels, with `NET eth0 10.0.2.15`
it is 992, and a longer name and address (`NET enp0s3 192.168.100.42`) reaches
1076. 1024x768 is an ordinary virtio-gpu mode, so this now clips on a target
td plausibly runs on, where the previous claim that no such output exists was
true and is not any more. The workspaces moved that line further: one costs 20
pixels and the strip shows as many as are in use, so the long case reaches
1096 with one workspace and 1256 with nine. It is a real cost of following the
model rather than an oversight, and nothing is lost but pixels —
`draw_text_clipped` clips. The fix, when it matters, is dropping whole FIELDS
rather than clipping a glyph, which is also what would keep the clock while
the workspaces grow.

The clock is UTC and SAYS so. There is no TZif parser here, and a
local-looking time that is silently UTC is worse than a UTC one that admits
it. The civil date comes from days-since-epoch by the shift-the-era method,
which is integer-only and needs no month table; its test pins 1972, 2000 and
2100 — 1900 is the other side of the century rule and cannot be asked here,
since these are UNSIGNED days since 1970. It also walks every day of a leap
year and the year after, requiring each step to ADVANCE the calendar by one
day against month lengths written out longhand. The round trip alone would
not: it closes over the test's own inverse, so a matched pair of wrong
functions satisfies it, which is what the walk is there to refuse.

Nothing else in the compositor wakes without input, so the bar is the one
thing in the process with a timer: a thread samples every second and hands
the runtime a line. The runtime repaints only when the TEXT changed — which
for the shipped line is EVERY tick, since the clock shows seconds. What that
tick costs is worth stating rather than waving at: a repaint re-renders the
whole shadow frame and compares it against what the device holds, so it is
about two passes over the framebuffer a second (~16 MB at 1080p), and only
the DEVICE write is confined to the bar's own band. `RESEND_INTERVAL` counts
paints, so one tick in 240 writes the whole frame — every four minutes here,
where before the bar it was however long the session went without input. The
lever, if that idle cost ever matters, is the clock's resolution rather than
the tick: at minute resolution the equality check starts refusing 59 of every
60 repaints. Nothing exempts a blanked VT or a closed lid — the sampler knows
nothing about either and loops regardless — so this is paid whenever the
process runs.

A failed paint puts the previous line back, or the scene would hold text the
screen never showed and the next identical sample would decide nothing had
changed. That restore is also what makes the retry unconditional, so a
failure is reported ONCE and again only when a DIFFERENT one arrives or one
returns after a good paint — an output broken for good would otherwise write
a line a second forever, burying whatever else the session had to say. A
paint failure there is reported and not fatal: the bar is the least
important thing on the screen and must not take the session down with it.
Reporting is bounded twice over — deduplicated, and capped at four named
failures between good paints — because a fault that ALTERNATES defeats
deduplication on its own and is a line a second again.

Deliberately not here: disk free, which needs `statfs(2)` and so an
`UNSAFE.md` amendment; a local timezone, which needs a TZif parser; and the
wireless, ethernet and temperature fields of the i3status config this is
modelled on. All three are additions rather than changes to what is above.

Input is QEMU's PS/2 keyboard and pointer plus its virtio tablet, all through
evdev. The compositor
supports EV_KEY, EV_REL, EV_ABS, and EV_SYN. It has a fixed US key map. Every
binding is ONE chord on `Super`:

- the arrow keys focus left, right, up, and down;
- adding Shift to a focus binding MOVES the focused tile in that direction,
  re-parenting it rather than trading places (see below). The sheet spells
  that row `MOVE A TILE / SPLIT OUT`, because leaving is what an operator
  cannot otherwise find: moved ACROSS the grain a tile leaves its container
  and makes the perpendicular one, which is the only way out of a column that
  keeps the window on the workspace, and was not derivable from `MOVE A
  TILE`. Not `NEW COLUMN`, which is only half true — out of a column the tile
  becomes one, and out of a ROW it becomes a full-width row;
- `Super+1` through `Super+9` switch workspaces, and adding Shift moves the
  focused tile to that workspace;
- `Super+v` STACKS the container the focused window sits in and `Super+h`
  TABS it — the two presentations a group can take — while `Super+s` groups
  an ungrouped container or ungroups a grouped one, and `Super+f` toggles
  fullscreen. The three presentations are also buttons at the right end of
  every title band wide enough to hold them, which is the pointer's route to
  the same commands and needs no chord to arrive;
- `Super+t` starts a terminal, which is the one registry entry anybody opens
  repeatedly;
- `Super+?` shows this table on screen. `?` is Shift+`/` here and Shift is
  not required, since the sheet is what someone reaches for when they do NOT
  know the bindings and demanding an exact chord to see them would be the
  wrong way round. Any NON-MODIFIER key dismisses it: there is nothing to
  type into and nothing to select, so such a key can only mean "seen it".
  Modifiers are deliberately excluded, which is what lets someone let go of
  the keyboard to read and then press a whole chord that is swallowed whole
  — dismissing on the modifier would eat it and leave the chord's key to
  act alone, so reading the table would start a terminal;
- `Super+Enter` — or `Super+KPEnter`, since the open overlay activates on
  either — opens the launcher, from which everything else is reachable.
  `Control+n` and `Control+p`, or Down and Up, move its selection; Enter
  activates it; Escape and `Control+g` close it. ASCII letters, digits,
  space, and hyphen filter its registry, and Backspace edits that filter.

Shift is read only where the list says so: `Super+Shift+f` is fullscreen and
`Ctrl+Super+t` is a terminal, since the letter chords and the launcher one
look at Super alone. That is deliberate — a chord the operator got a spare
modifier onto should do what it says rather than nothing — and it matches
how the workspace and arrow bindings already treated Control and Alt. An
overlay outranks all of it: while one is up it owns every non-modifier key,
so `Super+t` behind it neither starts a second terminal nor types `t` into
the query. The launcher outranks the sheet in turn, so the two can never
both be up: `Scene::set_help` REFUSES to raise the sheet while the launcher
is visible. That refusal is the invariant, not the fact that `/` is no
character the launcher accepts — the dispatch already cannot ask for it, so
before the refusal existed the property held only because nothing happened
to call it, and any new caller would have broken it silently. The sheet is
checked FIRST on dismissal, though, so a sheet is always dismissable
whatever else believes itself up.

The sheet is PAINTED text beside bindings that live in the dispatch, and
nothing the compiler sees connects the two: they are unrelated string
literals. So a test drives every row's real chord through `KeyBindings` and
derives BOTH columns from what came back — the keys from the codes it
pressed, the action from the effect it produced — and it counts the rows, so
one added without a probe fails rather than going unchecked. A binding that
changes without this table changing is therefore a failing test rather than
a screen that lies, which is the only guarantee available for painted text.
Three rows no chord can drive — `HOVER`, `CLICK`, and `DRAG A TITLE` — are
pinned by name and by an EFFECT that same table turns into words, so a mouse
row cannot invent words of its own. The limit of THAT is worth stating beside
the keymap one: a keyboard row derives its effect from the dispatch that just
ran, and a mouse row has none to derive from, so a row and its probe changed
together would agree about something untrue. What each gesture actually does
is proved where it happens, in the runtime's hover, click, and band-drag
tests. The drag row is on the sheet for the reason the move chord's wording
changed: the mouse's way out of a container is a title dragged against the
side of another tile, and a sheet naming neither left an operator with a
column of windows and no way to find either. The drag-to-the-bar row is there
for a sharper version of the same argument: a drop zone is invisible in a way a
chord is not, since there is no key to press by accident and discover it, and
the spare workspace it aims at exists only to be dropped on. The limit of
that check is worth stating: it links the table to the DISPATCH, not to the
keymap, so the glyph names (`?`, `V`, `T`) are literals — remapping a key in
`keys.rs` would make the sheet lie with the suite green. The card is
sized FROM the table too, so adding a row cannot push the last one past the
bottom edge, and every pixel is clipped to it on an output too small to
hold it.

Everything in that table is taken FROM the focused client, and this list
takes more than the one it replaces: the four arrows, Enter, and `t`/`v`/`h`
where `b/f/p/n`, `x` and the digits were taken before. Anything else under
Super still reaches the client, which is what td-term's own
untranslated-chord rule below turns on.

A prefix buys keys at the cost of a press, and this table is nowhere near
running out of them: the movement bindings were `Super+b/f/p/n` and the
layout ones `Super+x` followed by `1`/`2`/`3`, both from Emacs, so the
operations done most often cost two presses each. The arrows say the same
thing as `b/f/p/n` without a mnemonic to learn, and `v`/`h`/`f` name their
own axis. If the table ever does outgrow one modifier the answer is a
second modifier rather than a prefix, since a chord is one motion and a
prefix is a mode with a state nothing on screen reports.

Left and right modifier keys are tracked independently. A compositor chord
consumes both the press and release of its command key. Its modifier
transitions still reach the focused client, as do ordinary keys and their
releases. Arbitrary keymaps, touch, calibration, gestures, and real GPUs
are later increments.

A pointer device reports one of two different things, and the compositor
accepts both. A RELATIVE device (EV_REL) reports a distance to add to
wherever the cursor already is; an ABSOLUTE one (EV_ABS) reports a PLACE in
its own units, which is what a tablet, a touchscreen and QEMU's
`-device virtio-tablet-pci` are. Only the second can be trusted to arrive at an
edge. A relative pointer inside a VM is integrating deltas the host stops
sending the moment the host's own cursor leaves the guest window, so the
guest cursor and the host cursor drift apart and the last column of the
output becomes somewhere the operator cannot point — the compositor has no
way to tell that from a mouse simply not being pushed further. Nothing in
the guest fixes that, because the missing motion never happened; an absolute
device does not have the problem to fix, since each report says where rather
than how far. QEMU's default PS/2 mouse is relative, so an image wanting the
edges needs an absolute device attached, and this one attaches
`-device virtio-tablet-pci`. Virtio rather than the more familiar
`-device usb-tablet`: the USB tablet would want a host controller and the
guest's whole USB and HID stack built in for one device, where the virtio
tablet rides the VIRTIO_PCI transport already carrying the disk and the GPU
and costs one Kconfig symbol under a menuconfig parent the erofs root
already pins. The PS/2 mouse stays attached beside it, since a relative
device is still what an ordinary machine has and the compositor must keep
serving one.

That a device is ATTACHED is not the same as a device ANSWERING, and only
the second is worth a check. So the compositor prints `TD-POINTER-ABSOLUTE`
with the node and the span for each device that returned one, and the
headless boot oracle latches that line. Headless there is no host cursor and
so no motion at all; what the boot proves is enumeration and the answer,
which is exactly the half no unit test can hold up. The gate machine has no
absolute device, so a compositor that never asked — or that asked and
dropped the answer — passes every test in `input.rs` and still reaches an
image whose pointer cannot cross the screen. The span rides along because it
is what the mapping divides by, and a WRONG one is invisible everywhere
else: `declared` refuses only a span of zero, so `0..1` is admitted and maps
every report to one of two positions. Nothing parses the numbers — the
oracle latches the substring — so they are there to be read by a person
looking at a console, which is the only thing that can tell a plausible
range from the device's real one.

WHERE it is printed is the load-bearing part, and it is not beside the
`EVIOCGABS`. The line comes off the argument the reader is about to build
its `DeviceState` from, because the property is not that an answer was
ASKED FOR but that it is USED. Printed at the ask, an answer dropped
between the two — the reader handed `None` — would leave the marker and
the whole unit gate green while every report on that device was read as
relative motion of zero, which is the same dead pointer by a different
route.

Only the HEADLESS invocation is covered that way. The interactive runner
attaches the same device and nothing tests its argv, which is where an
operator would meet the failure — and is also why it is left: that runner
has a person in front of it, and a tablet missing from it presents as the
cursor not reaching the right edge, which is the complaint this whole
mechanism came from.

And "covered" means by the deployment boot proof rather than by the
branch gate, which is a weaker guarantee than the rest of this document
describes. `system-x86-64` owns no gated check, so the oracle that latches
this marker runs only in a full image build; `td-builder ready` goes green
over a commit that broke the pointer entirely. Every claim above about
what the marker catches is therefore a claim about `qemu-boot-system`, and
whoever changes this path should run it rather than trusting the gate.

Absolute axes are declared per device rather than per report, so each node is
asked when it is opened — and only then, bar the recoveries below — for where
ABS_X and ABS_Y are and what they
report over; a device that refuses is relative, which is the ordinary case and
not an error. A report is then mapped in two steps that keep evdev's units out
of the scene: `input.rs` turns a raw value into a `Fraction` of that declared
range, and the scene turns a fraction into a column or a row of the output.
The fraction is an exact rational — the device's own offset over the device's
own span, neither reduced nor rescaled — because the device is not the only
thing quantising, and each quantisation costs a pixel.

The second one is QEMU's. It scales the host pointer onto `0..=0x7fff` with an
integer division that FLOORS, so the rightmost column of an 800-wide surface
arrives as 32726 rather than 32767 and every other column arrives a shade low
too. Under a mapping that floors a second time, that shade becomes a whole
pixel almost everywhere and the far edge becomes unreachable — which is the
original complaint, arrived at from the other direction. So the scene ROUNDS
and then clamps, and `every_host_column_survives_qemus_own_scaling` holds
every column of five resolutions to coming back as itself. A fixed-point
intermediate between the two steps would have put a third flooring in the
middle, which is why the fraction carries the raw pair.

A value OUTSIDE the declared range is the edge it went past rather than an
error: drivers do report past their own bounds, and the operator pointing off
the end of the tablet means the end of the screen. Where a device reports
both kinds in one frame the PLACE wins, since a delta is a correction to a
position the device has just restated.

A report reaches the scene with BOTH coordinates even though the kernel omits
an axis whose value has not changed. The missing one is answered from where
THAT DEVICE last was, held by its own reader — and before its first report,
from the `value` field of the same `EVIOCGABS` that gave the range, which is
the axis's position at the moment it was asked. Leaving the cursor's own
coordinate in place would look right until anything else moved it: a mouse
between two stylus reports would leave the axis the stylus did not mention
wherever the MOUSE put it, which is nowhere the stylus is.

The same argument decides a frame that names NO axis. An absolute device's
frame is a place unless the only thing in it is a distance, so a BUTTON alone
is placed too — a click that did not move is the ordinary case rather than a
corner one, since the kernel drops both axes as unchanged when a tablet is
tapped twice in one spot. Read as a zero delta it would click wherever the
shared cursor happened to be. That makes the rule broader than "a position
beats a delta": a button beats one as well, so a hybrid device sending a
button and a delta together in one frame has its delta dropped. Nothing on
this hardware profile is such a device, and the alternative — clicking at a
place the delta has not been applied to — is worse than losing the motion.
A dropped batch (`SYN_DROPPED`) and a button
overflow both abandon the half-built frame and every button believed held,
and both KEEP that position rather than reverting to the one the kernel gave
at open — it is not frame state. Both then RECOVER by the same path, which
is worth stating because only one of them is the kernel's doing: an overflow
discards this crate's own half-built frame, `EV_ABS` values and all, and
goes on discarding to the next `SYN_REPORT` exactly as a drop does, so the
held position is stale in the same way and for the same reason.

Keeping it is not enough on its own, though, and the recovery boundary is
where the difference shows. The kernel compares an axis against the value IT
last emitted, not the one that arrived, so an axis that moved while reports
were being discarded is never re-sent: the held coordinate would be wrong
until that axis happened to move again, and a click in between would land at
the wrong place indefinitely. Only the device still knows, so the
resynchronising `SYN_REPORT` re-reads both axes and the freshly declared
`value` replaces the held position AND is published as a frame of its own.
Publishing is the half that is easy to leave out and useless to omit: from the
recovery onward the kernel sends only changes, so a device that moved during
the gap and then stopped would leave the cursor wherever it was until it
happened to move again. The reader cannot ask a device itself — it takes an
`impl Read` so its tests can drive it from a byte slice — so it is handed a
resync it can call, which production closes over a second handle to the same
node and a test answers from a fixture. That handle is a `dup` rather than a
second `open`: two opens of an evdev node are two CLIENTS, each with a buffer
the kernel fills, and the one nothing drains is what produces dropped batches
in the first place. A resync that fails leaves the held position standing,
which is the best remaining answer rather than a jump back to open.

What that answer CANNOT be is a position at the boundary, and it is recorded
because it looks like an oversight and is not. `EVIOCGABS` reports where the
device is NOW, while the reader is replaying records read earlier — up to a
batch of them — and the kernel may hold more still. So a device that moved
between the resynchronising `SYN_REPORT` and the ioctl answers with the later
place, and a frame replayed from that batch carrying only a BUTTON is put
there rather than where it was pressed. Reading one record at a time would
not close it: the kernel's queue is ahead of the reader whatever the batch
size, and the events reconciling the two are exactly the ones not yet
delivered. Nor is discarding the rest of the batch a fix — those records are
valid, and throwing a click away outright is worse than landing it a little
late. The error is bounded by how far the device moved in one batch and ends
at the next report for that axis, where the alternative — not publishing —
is stale without bound and is the failure this recovery exists for. It is
also self-limiting in the case that matters: a button-only frame is a device
holding still, and a device holding still is one the snapshot agrees with.

What this does NOT do: it does not calibrate (the declared range is taken as
true), does not map to one output among several (there is one), does not
change the WIRE — clients still receive surface-local coordinates from the
cursor's position, and `wl_pointer` has no absolute motion to receive — and
does not move the cursor for a device that has produced no events at all.
Anything that device DOES produce places it, though, and from a position no
report carried: the first frame, even one carrying nothing but a button, and
the recovery frame after a dropped batch. Both are the seeded position above,
and both are a jump by design rather than an exception to this.

Nor does it ask what KIND of device declared those axes, and that is a stated
limitation rather than an oversight. ANY node whose ABS_X and ABS_Y both
declare a span is admitted, which is a wider class than a tablet and wider
than the reading it is tempting to give it. A gamepad's left stick is
ABS_X/ABS_Y, so moving it would take the cursor over. A laptop TOUCHPAD is
the case that matters more, because it is the common one and it fails
harder: it reports an absolute finger position and no `EV_REL` at all, so
under this rule the cursor would teleport to wherever a finger lands rather
than being dragged by it. An accelerometer node declares those axes too and
would fly the cursor around with the tilt of the machine. The
fix needs no new ioctl — `/sys/class/input/*/properties` carries the
`INPUT_PROP_*` bitmap as an ordinary file, and `INPUT_PROP_POINTER` versus
`INPUT_PROP_DIRECT` is the distinction wanted — but the property a QEMU
tablet actually sets cannot be checked from here, and gating on the wrong
bit would make the feature refuse the one device it exists for. The hardware
profile above has none of them: it is a PS/2 keyboard, a PS/2 mouse, and a
virtio tablet.

Compositor commands act only on key presses. Evdev autorepeat records are
ignored for both compositor and client delivery. A held `Super+v` therefore
splits once rather than once per repeat interval. Ordinary keys omit XKB's
`repeat=no` property and libxkbcommon 1.11 treats symbol keys as repeatable
by default. Clients combine that per-key property with
`wl_keyboard.repeat_info`.

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
paints, not seconds -- but since the status bar, its interval is effectively
BOTH, and the difference matters to what is written above. The clock changes
the line every second, so paints now have an unconditional 1 Hz floor that no
batching lowers: the bar's own rows are overwritten within one second and the
whole screen within four minutes, on an idle machine nobody is touching. What
that costs is a recovery console's output. Before the bar, an idle compositor
wrote nothing and a message fbcon had printed persisted indefinitely; now the
rows the bar owns are gone at once and the rest within the interval. That is
the price of a clock on a device with a second writer, and it is stated here
rather than left to be discovered on the console it affects. There is still no
page flip, vblank, acceleration, DMA-BUF, or tear-free claim.

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

The SCREEN is the only thing that debt covers, which is why settling a change
runs every step whatever the one before it did and reports the failures
together. Nothing owes the clients their configures: a settle that gave up at
a failed paint lost them outright, and every client would go on drawing at the
size it was last told, which the compositor clips into a rectangle of the wrong
shape — a window visibly the wrong size with nothing in the log and no further
event coming to correct it. Publishing cannot itself fail, so running it after
a failure costs nothing and risks nothing. The same reasoning orders an
overlay's cancelled drag BEFORE the paint that may fail, and promotes a drop
the release has already taken out of the drag's hands: a block left standing
with nothing able to commit or clear it is the same loss seen from the scene's
side. The paint debt is a weaker backstop here than elsewhere, which is part of
why none of this may lean on it: an error out of a device's `apply` ends that
reader thread, so the flush that would have paid the debt is gone with it and
another device or the next client frame pays it instead.
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
- zxdg_decoration_manager_v1 and zxdg_toplevel_decoration_v1
- wl_callback completion and wl_buffer release

Those six globals are the whole of what the registry advertises, and the set is
a TEST rather than a sentence here: the name, order, and version of each are
pinned by
`the_registry_advertises_exactly_the_globals_td_serves`.
A document cannot notice when the code moves under it, and this list has been
read as a state claim by work outside this crate.

The E2 application-compatibility experiment on 2026-08-25 fixes the priority
of the next globals without changing that current-state list. GTK 4.22.1's
`gtk4-demo`, run against Weston through a registry-listener filter, completed
its XDG configure and attached a shm buffer with `wl_subcompositor` hidden. It
refused the display before creating a surface when
`wl_data_device_manager` alone was hidden. The data-device manager is therefore
a first-window blocker for current GTK; the subcompositor is a usability
requirement whose synchronized-child paths remain to be exercised, not a
first-toplevel requirement. This compositor's exact six-global registry still
lacks both, so it does not yet claim a GTK window.

The same experiment closes the no-GPU presentation question. GTK 4.22.1's
forced Cairo configuration attached shm with and without the subcompositor.
More importantly, a Wayland-EGL smoke client compiled by exact Freedesktop SDK
25.08 and executed against the exact pinned Freedesktop Platform 25.08
reported llvmpipe and created, attached, and committed a `wl_shm` buffer when
the reference compositor advertised neither dmabuf nor `wl_drm`. Exact pinned
Firefox 154.0 also acknowledged its XDG configure and attached two shm buffers
on that no-dmabuf display. These are development-host compatibility results,
not tests of td's wire implementation; `APPLICATIONS.md` §F records the exact
commits, limitations, and runtime-major repeat rule.

**Decorations are the compositor's.** td answers every
`zxdg_toplevel_decoration_v1` with `server_side` and never `client_side`: a
tile already carries a title band this compositor draws, so a client drawing
its own would be a second title inside the geometry the layout gave it, and the
band a drag reaches for would not be the one the operator sees. `set_mode` is a
PREFERENCE the protocol allows the compositor to disagree with, so it is read —
a value outside the enum is refused rather than rounded to the answer td was
going to give anyway — and then answered the same way `unset_mode` is. Every
ask is answered, including one for the mode already in force: `configure` is
the only way a client learns the mode, so silence on a repeat request is a
client waiting on an event that never comes.

Advertising this is what makes a cooperating toolkit stop drawing its own
titlebar, which is most of the `set_window_geometry` problem: a client with no
CSD has no shadow margins for td to tile as dead borders. What a client that
draws them anyway gets is the geometry clauses below.

**One divergence, and it is deliberate.** The protocol's `destroy` means "switch
back to a mode without any server-side decorations at the next commit", and td
does not: the title band is drawn for every tile whether or not a decoration
object exists. The band is LAYOUT rather than decoration — it sits outside the
client's geometry, the client is placed under it, and it is the handle a drag
takes hold of — so there is nothing for td to withdraw, and withdrawing it would
leave a tile that cannot be dragged and a gap where its neighbours expect a
band. A client that destroys the object and resumes drawing its own titlebar
therefore gets the double title this interface exists to prevent, and td cannot
stop it. Closing that properly means making the band optional per window, which
is a layout question and not this one. In practice a toolkit creates the
decoration with the window and destroys it with the window, so the path is
reachable rather than ordinary.

Two things follow from the mode being carried by a configure. `set_mode` and
`unset_mode` are answered with the decoration event AND an `xdg_surface`
configure, because the client applies the mode on that configure and
acknowledges ITS serial — the decoration event alone leaves a mapped window
waiting and still drawing its own titlebar. The layout has not moved, so that
configure is asked for rather than arising: the tracker's deduplication, which
is what keeps a still window still, is exactly what would swallow it. CREATION
is answered the same way, and by the same code. It is tempting to argue it need
not be — the mapping commit's initial configure would follow it — but that is
an argument about the ordinary sequence rather than about every legal one: a
client may commit empty, acknowledge that configure, and only then ask for a
decoration, which `unconfigured_buffer` permits because it has attached no
buffer. Its first frame would be drawn against a mode it never received an
ack-able serial for.

The interface's three errors are all client mistakes and each is raised with
its own code rather than as a disconnect carrying nothing: a second decoration
for one toplevel is `already_constructed`, a toplevel destroyed while its
decoration still lives is `orphaned`, and asking for a decoration for a window
that already has pixels is `unconfigured_buffer`. That last is checked in both
senses the protocol means it — a buffer ATTACHED and not yet committed, which
is still the server's pending state, and one COMMITTED, which has left it and
is the scene's — because the ordinary way to reach it is a client that mapped
its window and asked afterwards. The refusals are raised BEFORE the decoration
is created, so a client that got it wrong is not left holding an id the
compositor never made; `orphaned` likewise leaves the toplevel standing, since
it is the object the diagnostic names.

Each error names the DECORATION object, which is not the object whose request
raised it in any of the three cases — two come from the manager and one from the
toplevel's destroy. Wayland scopes an error code to the interface of the object
it is reported against, so the alternative is not merely untidy: the manager
defines no errors at all, and `orphaned` is 2, which on an `xdg_toplevel` is
`invalid_size`. A client would be told its window was the wrong size. The
decoration is a safe object to name even where td refused to create one, because
the client allocated that id and holds a proxy of the right interface for it.

The protocol's other half of `unconfigured_buffer` — "any attempt to attach a
buffer before the first configure" — has no check behind it, and that is worth
stating as the limit it is rather than as a guarantee. Answering inside
`get_toplevel_decoration` means td never leaves a window in which a client that
is READING could see no configure yet; it does not stop one that pipelines the
request and an `attach` into a single write, having read neither. No reference
compositor catches that either, and the cost of not catching it is a frame
drawn against a mode the client had not yet been told.

**A window geometry is what td tiles, not the buffer.**
`xdg_surface.set_window_geometry` names the part of a surface that is the window
a person sees; the rest is the invisible margin a client-side-decorated toolkit
draws its shadow into. td used to parse that rectangle and discard it, which
tiled the margin as a dead border and put every click that far off its target.
It is now the CROP: a tile shows the geometry's own rectangle, starting at the
geometry's origin, and a pointer over that tile arrives in the client's own
coordinates with the origin added back. The protocol says a compositor may clip
rendering to the geometry, and a tiling one must — a margin drawn outside the
tile would be over a neighbour's window. Nothing about the LAYOUT changes: a
tile's size is the layout's, the configure td sends already means the geometry
size, and a buffer larger than that is exactly the case this reads.

ONE rectangle answers both questions, and deliberately: the crop the renderer
takes as a source offset is the crop the hit test adds to a pointer, from one
function over one clipping arithmetic. Two of them a pixel apart is a click
landing beside what it was aimed at with the ink saying otherwise, which stays
invisible until a client changes its margins.

It is double-buffered state, as the protocol requires — the request records a
rectangle and the wl_surface's own commit applies it, since a geometry applied
on arrival would crop a window to bounds measured for a buffer not yet
attached. Everything a commit carries is then applied TOGETHER, under one lock
and settling once: a commit is atomic in the protocol, and the pixels, the
input region and the crop answer the same question between them — which pixels
of this surface the pointer is over. In halves, the input thread could take the
lock in the middle and route a click through the new buffer under the previous
crop, and the frame painted in that window would show it that way too. The one
shape applied on its own is a commit that takes the buffer AWAY, where it can
be: a surface with no pixels is drawn nowhere and aimed at by nothing, so the
unmap and the crop cannot be told apart from outside. What each half owes
differs and is kept: pixels and a crop change what is DRAWN, an input region
only where the pointer lands, and a crop on a surface with no pixels changes
neither — which is the ordinary opening sequence, a geometry set on the empty
commit before the first frame.

A geometry before the role object is REFUSED, with `not_constructed`: the
protocol says a role must be assigned before any other request is made to the
xdg_surface, so a rectangle arriving earlier is a client mistake rather than
state to hold for a role nobody has asked for. The role is checked before the
arguments are judged, since a client that has not asked for a window yet is
better told that than told about its arguments.

Its lifetime is the xdg_surface OBJECT's: a toplevel
destroyed and re-created on the same xdg_surface keeps it, which is a client
reusing the window it already measured, and destroying the xdg_surface gives
the whole surface back. An unmap keeps it too, for the title's reason — a
client re-mapping does not re-send a geometry it sent once at startup.

The rectangle is resolved against the pixels a client committed rather than
trusted, because it may name coordinates outside the surface and the protocol
allows that. The effective crop is therefore the intersection, and with no
`wl_subcompositor` the surface's own bounds ARE the bounding rectangle the
protocol asks for. Two divergences follow, recorded here rather than found in a
diff later. The intersection is taken where it is USED rather than frozen by
the commit that applied it, where the protocol says the effective geometry is
not recalculated until the next `set_window_geometry`: a geometry outlives the
buffer it was measured against, so a later, smaller buffer would otherwise
leave a crop reaching outside the pixels that exist. And a geometry naming NO
part of the surface leaves the whole surface standing rather than cropping to
nothing — the protocol makes an effective geometry with a non-positive side an
`invalid_size` error, but that same shrinking buffer reaches the case with no
client mistake at all, and a window cropped to nothing is a black tile with
nothing on screen saying why.

What IS refused is the request: a width or height that is not positive raises
`invalid_size`, the code the protocol names for it, in place of the generic
`implementation` every other refusal on this interface carries. Nothing is
recorded before the refusal, so a client is never left holding a geometry the
compositor also thinks is pending.

**An attach's offset is the cursor's to move.** `wl_surface.attach` carries an
x and a y placing the new buffer's corner relative to the one it replaces. At
the wl_surface version td advertises they ARE the surface offset —
`wl_surface.offset` replaces them only from version 5 — so a client with
contents to move has no other way to say so. td used to parse both and drop
them.

They now reach the CURSOR, the one role where the protocol makes them a visible
move: `wl_pointer.set_cursor` says a hotspot is DECREMENTED by an offset
applied to the pointer surface. A cursor has no anchor but its hotspot — td
draws the image at `pointer - hotspot` — so decrementing is what slides the
image the way the offset points, and the pointer, which has not moved, ends up
that much NEARER the image's own top-left corner. A TILE ignores the offset,
which is the
answer rather than an omission — the layout fixes where a window is and the
geometry above names which part of its buffer fills that place, so shifting the
contents on top would move a window inside its own tile and leave a gap at the
edge it moved from.

The offset travels with the contents rather than in a call of its own, for the
reason the crop does: one commit is one paint. In halves, the frame between
them draws the new image under the old hotspot — a cursor that jumps and
settles, which is what double-buffering exists to prevent. A null attach
carries one like any other and it is kept: nothing is drawn while the surface
has no pixels, and the image that arrives next lands where the offset left the
hotspot.

What an offset cannot reach is a surface nobody is pointing with. A hotspot is
the POINTER's state rather than the surface's — it arrives with `set_cursor`,
which a client sends on every enter — so there is nothing to move until a
client names that surface, and the request naming it supplies one. That is the
limit rather than an approximation of it.

The hotspot is held WIDER than the `int` a client names one with, because it is
an ACCUMULATOR rather than a value any single request sets: `set_cursor` gives
it a starting point and every attach after that decrements it, so three
attaches a client can send in one breath take it outside the `int` range and
bring it back. Clamped at those ends the excursion loses a pixel on the way
home and the cursor returns beside where the client put it, with nothing on
screen saying why. At `i64` the arithmetic is exact over anything a client can
send in a session; the subtraction still saturates, because nothing may panic,
but that bound is billions of attaches away rather than three.

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

**A popup is placed by its client and floated, not tiled.** `xdg_positioner`
and `get_popup` were hard errors that disconnected the client outright, so an
application opening its first menu died. They are implemented: a positioner
records the rules — size, anchor rectangle, anchor, gravity, constraint
adjustment and offset — and `get_popup` derives a rectangle from them, which is
what the client is told in `xdg_popup.configure`, before the
`xdg_surface.configure` that makes the pair one configuration. A PAIR on the
popup object's first map only — §3's re-map paragraph is the qualification,
and on a re-map the xdg_surface event goes alone.

The rules are COPIED at `get_popup`, as the protocol requires, so the client may
reuse or destroy the positioner immediately and nothing already placed moves.
The derivation is an anchor point on the anchor rectangle, the surface hung off
that point by its gravity, and the client's own offset translating the result —
the protocol's worked example is that an anchor at (x, y) with a bottom-right
gravity and an offset (ox, oy) gives (x + ox, y + oy). Both axes resolve
independently through one function, which is what stops one of the eighteen
enum arms reading the other axis's field.

An xdg_surface holds ONE role object, a toplevel or a popup, in a single field:
"has a role object" is one question, and two `Option`s would be two places for
it to disagree. A popup is not in the layout at all — it floats over its parent
at the offset the client was told, is drawn above every window and below td's
own bar and overlays, and is hit-tested BEFORE the tiles so a click on a menu
does not reach the window it is covering. Its placement is kept RELATIVE to the
parent, so a parent that moves takes its menu with it; a submenu's parent is the
menu that opened it, and the chain is walked back to the tile it belongs to.
A popup whose parent is not on screen is drawn nowhere.

What the placement puts at its corner is the popup's WINDOW GEOMETRY, not its
buffer's corner, exactly as a tile's is: a toolkit draws a menu's shadow outside
that rectangle, so a popup that ignored the geometry would anchor the shadow
where the client asked the menu to be and clip the menu away at the far edge.

Four properties are worth stating because none of them follows from "floats over
its parent", and each was a defect before it was a rule.

- **A popup abuts its parent.** The protocol requires a child surface to
  intersect its parent or be at least partially adjacent to it, and td checks it
  — the ADJACENT half being the ordinary case, since a submenu is hung off the
  menu's right edge and overlaps it by nothing. The check is what stops a client
  dropping an input-taking rectangle anywhere on screen: popups are asked before
  the tiles, so an unanchored one would take the clicks of whatever window it
  was placed over, including another client's.
- **A popup takes no clicks over the bar.** td's own surfaces are painted after
  the popups, so a menu reaching into that strip is already invisible there, and
  one still answering for it would take clicks over pixels that are td's. Tiles
  get this for free — every rect is offset below the bar — and a popup is placed
  by its client instead.
- **Stacking is td's own order, not the object id's.** A submenu is created
  after the menu it hangs off and the protocol stacks it above, but ids cannot
  say which came first: libwayland recycles them, and td retires them with
  `wl_display.delete_id` precisely so a client may. One order serves both the
  paint and the hit test, because whichever is drawn last is what a click has to
  land on. A popup that recommits keeps its place — a toolkit repaints a menu on
  every hover, and a raise there would put the menu over its own submenu.
- **The pointer over a menu is over the WINDOW that opened it.** Focus follows
  the mouse, so answering the tile a menu overhangs would deactivate the window
  that owns the menu — and a toolkit told its window was deactivated closes the
  menu, which would then shut as the pointer moved onto it.

A popup dies with its own unmap, with its parent, and with its client, and its
SUBMENUS die with it in each case: pixels included, since a chain nothing can
reach would otherwise hold the scene's byte ceiling, and one left behind comes
back the moment anything remaps its parent. Its bytes are given back to the
CLIENT's accounting too, on the destroy as well as the null attach — a menu that
returned its pixels to the scene and not to the ledger would have an
application that opens and dismisses menus disconnected for buffers td is not
holding. A destroyed popup also leaves a FRESH configure tracker behind, as a
destroyed toplevel does: one left initialised says the first configure has been
sent, so the next role on that xdg_surface would never get one and a client
waiting on it hangs with no window.

A ROLE is permanent where a role OBJECT is not. A client may destroy an
xdg_toplevel and build another on the same xdg_surface — reusing the window it
already measured — but the surface that carried a menu may not come back as a
window and be tiled, nor a window's as a menu. And a menu may not be destroyed
while a submenu hangs off it: that is the protocol's `not_the_topmost_popup`,
which for a tree is a popup with a live child.

A popup's parent edge is BROKEN when the surface it names is destroyed, rather
than left holding the number. An id is not an identity: Wayland recycles them
and td retires them with `wl_display.delete_id` precisely so a client may, so
an edge that kept the number would come to name whatever took it next. The
symptom is not subtle — a menu drawn on, and taking the clicks of, a window
that never opened it — and it has a second face, since the scan behind
`not_the_topmost_popup` looks for a popup naming this one's surface and a stale
edge answers for a submenu that does not exist, disconnecting a client for
destroying a menu with nothing hanging off it. The break is per surface, not a
sweep: one window closing must not shut the menus on every other.

The OTHER shell edge is the same bug and is broken the same way. A wl_surface
names its xdg_surface by number, and that number goes back to the client when
the xdg_surface is destroyed, so a role still holding it resolves on the
surface's next commit to whatever the client made next. That is worse than a
misplaced menu: the old surface takes a STRANGER's role object and configure
tracker — so it answers `initial_sent` and `can_attach` for a window that is
not it, and a detach unmaps that stranger's tracker. So destroying an
xdg_surface RETIRES the role — `SurfaceRole::XdgRetired` — which keeps
everything the role was for while keeping none of the number.

Retiring also repairs a refusal in the other direction. `wl_surface.destroy`
is an error while the role object lives, and asking that question by looking
the id up meant a client that tore its shell objects down in the right order
and then reused the number was refused for a role object already gone. A
retired role answers it without a lookup: `Xdg` now names a LIVE xdg_surface
and nothing else, so the check is the role itself.

What that role then FORBIDS is a divergence, and stricter than the protocol
rather than looser. `wayland.xml` says destroying a role object does not
remove the role but may stop the surface playing it, and explicitly permits
taking the SAME role again; td refuses a second xdg_surface on that surface
and makes a later commit fatal, where wlroots lets the surface go inert. The
cost is a client disconnected for something legal, and the shape that reaches
it is not exotic: destroy the role object and the xdg_surface, keep the
wl_surface, then attach a null buffer and commit. That is the ordinary
hide-a-window path — GTK4's `gdk_wayland_surface_hide_surface` is exactly it,
and its `unmap_popups_for_surface` recurses first, so a window hidden with a
menu open reaches the same refusal through the MENU. Going inert instead is
the fix and it is not here; what this commit changed is which number the role
holds, and the refusal predates it. That is a deferral rather than a defence,
and it should be read as the next thing this area owes.

The ACCOUNTING follows that cascade rather than the surface named. Taking a
surface down discards the pixels of every popup over it and theirs in turn, and
the scene REPORTS what it dropped — so the client's ledger gives back exactly
what went, rather than what a second walk of the parent edges made here would
say, which by then would be reading the edges that walk removed. Before this,
`wl_surface.destroy`, `xdg_toplevel.destroy` and a null-buffer commit on a
window or on a menu each left a client charged for descendants td no longer
held; an orphan's own submenu was the same shape seen from the orphan. Nothing
accumulated, because a repaint REPLACES a surface's charge rather than adding
to it, but the charge stood for as long as the client abandoned the surface,
and what it cost was that client's own ceiling.

Of the three take-downs that refund, the one at `wl_surface.destroy` looks
unreachable and is not. `get_popup` refuses a parent xdg_surface with no role
object, so no NEW menu can be hung off a window whose toplevel has gone — but
an existing menu does not have to be created, only REPAINTED. Its popup object
and its xdg_surface both outlive the toplevel, so the client may map it again
and the placement goes back into the scene naming a parent that is no longer a
window. The destroy then reaches `remove_surface` with a live tree under it,
and its refund is the only thing that gives those bytes back. This paragraph
claimed the opposite until review built the sequence; the correction is
recorded rather than quietly swapped, because a wrong unreachability claim is
the kind that gets a live refund deleted later as dead code.

What has changed since is the COST of that repaint and not its reachability.
The tracker outlived the toplevel too, so the second buffer used to attach
with no fresh acknowledgement; a dismissal unmaps now, so the same client has
to perform the whole initial commit again. It reaches the same refund by a
longer road, which is why the paragraph above is a correction rather than a
deletion.

With that, a popup CYCLE is unbuildable — and the argument is written out
because review has had to correct it THREE times, so the conclusion is worth
less than the ability to check each leg. Read it in two halves, because the
halves are about different graphs: the rules below are about popup OBJECTS,
and what the renderer walks is a graph of SURFACES.

Over objects, "points at" is a strict order by creation. A popup's parent is
fixed when it is created and never re-pointed. The parent must already hold a
role object at that moment, and a surface's role KIND is permanent, so where
the parent is itself a menu the object answering for it is a popup and stays
one. A popup cannot be destroyed while a popup names its surface, which is the
leg that does the work: the edge PINS the popup it points at, so the object
answering for a parent is the one that was there when the edge was made rather
than a later one standing in the same place. That pin has two halves and the
quiet one is `already_constructed` — an xdg_surface holds one role object at a
time, so a second popup cannot be added ALONGSIDE the pinned one and leave the
edge naming something else. And NEITHER shell edge survives
the thing it names — a popup's parent edge is broken when its wl_surface is
destroyed, a surface's role edge is retired when its xdg_surface is — so no id
can smuggle a younger object in behind an older name. Every popup was
therefore constructed before the one naming it, and a cycle would need one
constructed before itself.

The SIXTH leg is the bridge to the graph that matters, and it is the one an
enumeration keeps losing because it is not a rule about popups at all: a
surface's placement leaves the scene when its popup role object is destroyed.
Placements are keyed per surface and written only by that surface's own
commit, so without this one a destroyed popup's placement would sit there
until the surface committed again — and a surface can hold a stale edge from a
dead popup while a fresh popup points back at it, which is a cycle built
entirely out of objects that each obeyed the five rules above. It is
`xdg_popup.destroy`'s `unmap_popup`, and deleting that one line builds the
cycle.

Take any leg away and a loop can be built; all three corrections were a leg
that was missing or misstated. The pinning is worth stating as pinning rather
than as age, because "the parent is older" was the first wrong version: that
orders only the objects standing there at the moment you look, and a destroy
plus a rebuild puts a NEWER popup at an older popup's address. The role edge
above was the second, a real route rather than a misstatement. The sixth leg
was the third, and note what it does to the first: "a popup's parent is fixed
at creation" is true of the OBJECT and false of the menu a user sees, since
destroying a popup and building another on the same xdg_surface re-points that
surface wherever the client likes.

That is a reason to expect no cycle, not a guarantee of termination, so the
renderer's depth bound stays. Six rules across two modules is exactly the kind
of conjunction that has now been corrected three times, and what it costs to be
wrong is a compositor that never paints again.

Two parts are incomplete and two have landed, and each is a landing of its
own. **Constraint adjustment** is recorded and not acted on: every bit of
it is permission for td to move a popup that does not fit, so a menu near an
edge extends past it rather than sliding or flipping. **Grabs** are recorded,
checked, and acted on: the topmost grabbing popup has the KEYBOARD, bounded as
below, and a press with none of the grabbing menus under it CLOSES them — td
takes the pixels down and sends `xdg_popup.popup_done`, rather than leaving a
client to notice for itself. td also dismisses a popup whose grab it will not
hold, which is the protocol's own answer to a denied grab rather than td
closing a menu on a user's behalf.
**Dismissal is signalled where td dismisses, and nowhere else.** Every popup a
take-down cascades over is sent `xdg_popup.popup_done`, deepest first — the
order the protocol makes a client destroy nested popups in, so a client that
destroys each one as it hears does not arrive at td's own
`not_the_topmost_popup` — for the popups a cascade reaches, which is the
qualification recorded further down. That order follows the CHAIN and not the
stacking, and the two really can disagree. The protocol requires a popup's
parent to be mapped before the popup itself; td does not police that, as
`get_popup` checks only for a role object, so a client that ignores the rule
can place a sub-submenu before the submenu it hangs off and leave a child
BELOW its own parent in the stack. Sorting the dismissal by stacking order —
which reads as the obvious way to spell "topmost first" — would then put a
parent ahead of its child and hand a client the very destroy order td refuses
— and it is exactly a client already ignoring the protocol that would get it.
Reversing the cascade cannot: it is breadth-first, so a parent's index is the
lower one whatever order the placements happened in, and whatever the client
did. That td does not enforce the mapping rule is a gap of its own, recorded
here rather than relied on.

What a recorded grab buys is the CHAIN, the answers a client can already
earn, the KEYBOARD, and the PRESS. The protocol lets a grabbing popup hang off
a toplevel or off another popup that grabbed, and nothing else, so one popup's
answer is the next one's precondition — which is why the flag is kept rather
than the request being dropped on the floor.

A PRESS with none of the grabbing menus under it closes them. "Outside" is a
question about the CHAIN and not about one rectangle: a grabbing menu survives
a press on itself or on anything hanging off it, so pressing a submenu is not
pressing outside the menu it hangs off — the alternative closes a menu as the
pointer reaches the item that opened it. Everything else in the seat's set
goes, deepest first, and a press on the bar, on a gap or on another window is
outside all of them.

The set asked here is the seat's UNFILTERED, where the keyboard's is bounded
by being drawn and by hanging off the focused window. The two questions differ
on purpose: those bounds say which menu the seat answers to, and this one says
which menus the client believes are open. A menu off screen holds no keyboard
and is still one a press ends, and leaving it recorded would leave a grab
nothing could reach.

The press itself is CONSUMED — filtered out before the pointer model sees it,
by the road a modal overlay already uses. Delivering it as well would close the
menu and click whatever the menu was covering, which is the one thing a toolkit
relies on not happening; the cost is that dismissing takes its own click, which
is what every other compositor does too. Only a press asks: motion must not
close a menu, and a release is owed to whoever took the press. Dropping the
press is safe because the model suppresses a release it matched no press to.

The work is SPLIT across two threads and WHERE the split falls is the
load-bearing part. Everything that decides a dismissal happens on the thread
that read the press: the grab is released, the pixels leave the scene, focus is
answered, the configure state machine is reset and the menu is recorded as
over. Only writing the event is the client's own seat thread.

The split has to fall there rather than one step earlier, and review found td
with it in the wrong place. A menu comes back by being COMMITTED again, so a
dismissal that dropped the pixels and left the record for the client's seat
thread leaves a gap: the client's own commit lands in it, paints the menu back,
and nothing takes it down a second time.

What closes it is the RUNTIME LOCK rather than the order of two lines. Every
commit path takes that lock to reach `commit_popup`, and the whole dismissal
runs inside one hold of it, so no commit can land between the pixels going and
the record being written. The seat thread holds no such lock, which is exactly
why the record could not stay there. Within the call the order is free, and the
cascade is recorded after the unmap because it is not known until then.

The COMMIT ITSELF is refused by the runtime, and it has to be. The shell reads
its configure gate at the top of `wl_surface.commit` and reaches the runtime
whole buffer copies later, so a commit that passed that gate honestly can
arrive after the press has taken the menu down — an ordinary repaint on the
motion just before the click is enough. Review found td's enforcement resting
on that gate, which is read too early to give it. So `commit_popup` refuses a
key it has recorded as over, under the same lock the dismissal ran in. The
client is not at fault and is not disconnected for it: it committed before it
could know, and it gets the menu back by destroying the popup and asking
again, which is what `popup_done` asked for.

For the runtime to do that it needs the client's own popup registrations, so it
holds a handle to each connected client's map. `dismissed` lives there and
nowhere else — the deciding thread writes it, the dispatch thread reads it, and
one copy cannot drift from a second. The dispatch thread is not an option for
any of it: a client that is sending nothing never reaches it.

Writing the event stays on the seat thread because ADDRESSING it needs those
registrations, and the delivery carries the xdg_popup as well as the surface so
that thread can prove the surface still wears the menu the press was about. An
id is not an identity: a client may destroy one popup and get the next in a
single buffer. The destroy path drops the registration BEFORE it retires the
id, and the seat thread holds the OUTBOUND lock across the lookup — the same
lock `wl_display.delete_id` goes out under — so while it holds, no id can be
retired, and a registration still naming this popup proves none has been.

Outbound before registrations, and never the other way. `Outbound::send` is a
blocking write on the client's own socket, so a client that stops reading holds
it for as long as it likes; the registrations are taken with the RUNTIME lock
held, by the dismissal above. A seat thread that held the registrations inside
that blocked write would park the input thread on them while it holds the
runtime lock, and every other client's input, commits and repaints behind that
— one quiet client stopping the compositor. Review built that interleaving out
of td's first two fixes taken together. The registrations are a leaf: nothing
is acquired while they are held.

One consequence is recorded rather than fixed: the client's byte ceiling is not
refunded until it DESTROYS the popup, because `mapped_bytes` is the dispatch
thread's own. A client that never destroys keeps its own quota spent, which is
its ceiling and nobody else's.

The check is a WALK rather than a look at the parent. The rule reads one level
but is inductive: a popup is only a grabbing parent while the thing IT hangs
off can still hold a grab. Checking one level lets a menu whose window has
gone keep handing grabs to submenus opened under it afterwards, since neither
its own grab nor its role object goes away when its edge breaks. So the walk
runs up to a toplevel, and any popup on the way that has been DISMISSED, or
whose own edge is broken, ends it. It is bounded at the depth the renderer's
popup walk uses and for the same reason: the edge rules should make a cycle
impossible, but that is an argument across modules, and running out here says
the grab cannot be held.

Only ONE way to fail that walk is an error. The protocol names one for a popup
parent that took no explicit grab, and answers every other unholdable parent
with a dismissal — "if the parent is a grabbing popup which has already been
dismissed, this popup will be immediately dismissed", and "if the compositor
denies the grab, the popup will be immediately dismissed". td follows that
split: `invalid_popup_parent` for the ungrabbed parent, carried on the shell
object because `xdg_popup` has no code for it, and `popup_done` for a chain
that is dismissed, orphaned, or gone. Ending a connection where the protocol
says to close a menu would be td being stricter than the thing it implements,
and it would cost a client that destroyed its own window with a grab already
queued everything rather than one menu.

`invalid_grab` is the popup's own and only error, and is exactly "tried to
grab after being mapped". That is a fact about the popup's LIFE, not its state
now: the byte ledger is cleared on unmap, so a popup that mapped, took itself
down and then grabbed would slip past a check on what is mapped, and the
object keeps the fact instead. It is not the configure being answered, though
— a toolkit asks for the grab on the button press that opens the menu, long
before it has painted, and dating the refusal from the acknowledgement would
refuse every real menu. A fourth refusal is not the protocol's: the seat
argument must name a `wl_seat`, and an id that names something else is a plain
error, since xdg-shell gives no code for an argument of the wrong interface.

The SERIAL is read and not checked. It should name the input event that
justifies the grab, and td keeps no ledger of the INPUT-event serials it has
issued: `next_serial` mints and forgets, where the configure serials
`ConfigureTracker` holds are a different set for a different purpose.
Refusing what cannot be verified would reject every honest client, so the
argument is dropped and this is the gap — a client may quote any number.
Closing it means recording issued input serials with the events that carried
them and expiring them. That was triaged as worth doing "when a grab does
something", and a grab now does something, so the trigger has fired: an
unverifiable serial is the difference between a menu the operator opened and
one a client asserted. What keeps it from being a way to take the keyboard is
the focused-window bound below, not the serial — a client whose window is not
focused gets nothing whatever it quotes. The ledger is owed with the
dismissal path.

A dismissal is not permanent state. td supports painting a dismissed popup
back — the whole mapping dance after a take-down is for that — so the flag
that ends the walk is cleared when td configures the popup again. Keeping it
for the object's life would turn away a submenu opened under a menu that is up
again, for a take-down that is over.

A grab is recorded TWICE, and the two records answer different questions. The
shell keeps a flag on the popup object: whether this popup may be nested
under, which is what the walk above reads, and which outlives an unmap because
the object does. The scene keeps the SEAT's set: which mapped popup holds the
keyboard now, dropped wherever a MAPPING ends — the popup's own unmap, its
parent's, the cascade under either — and separately wherever the OBJECT or
its wl_surface is destroyed, including with the client. A menu that mapped and
then took itself down with a null attach therefore still answers the walk and
no longer holds the seat, and those are the two right answers rather than a
drift between them.

Those are two conditions rather than one because a grab is taken BEFORE the
first buffer, the protocol having refused one after. `grab`, a null attach and
an initial commit reach the unmap path with nothing ever mapped, and a first
draft dropped the grab there — deleting it before the menu had opened, which
review caught. The destroy roads carry it instead, and they are the ones that
have to: a wl_surface outlives its xdg_popup and may be given another, and a
destroyed surface's id is reusable once `delete_id` has been sent, so those
are the two moments a key can come back naming something else.

Dropping the seat's record with the popup is not hygiene. td supports painting
a dismissed menu back, and `invalid_grab` means a popup that has mapped can
never ask for another grab, so a second mapping is a menu that provably holds
none. A grab kept beside the popup rather than dropped with it would hand the
keyboard to exactly that menu.

**The topmost grabbing popup has the keyboard**, which is the protocol's rule
and not a preference: "the top most grabbing popup will always have keyboard
focus". Topmost is read off the scene's own stacking order, the same one the
paint and the hit test use, so the menu that has the keyboard is the menu
drawn over the others and the one a click lands on. A popup that grabbed and
has not mapped is not a candidate: the grab request must precede the first
buffer, and focusing pixels that do not exist would take the keyboard off the
window behind for a menu the operator cannot see.

Two bounds narrow "always", and both are DIVERGENCES from that word rather
than readings of it. They are stated here as such.

The first is that the menu must be on screen. A menu over a window on another
workspace, or behind a stacked sibling, is RETAINED rather than dismissed —
the paragraph on workspace switches below is the statement of that — so its
grab is still recorded, and one holding the keyboard there would leave the
window the operator is looking at unable to be typed into. Review argued the
protocol's alternative: dismiss the whole grab chain on a workspace switch and
send `popup_done`. That is the more faithful reading and td cannot take it
here, because td retains popups across a switch as a matter of policy already
and dismissing needs a path that landing did not build.

That path EXISTS now: the press above dismisses a chain, and a workspace switch
would use the same `dismiss_popups`. The blocker is gone and the divergence is
not — it is held open deliberately, as one gesture per landing. A switch is a
different question from a press about which menus it should end and whether a
retained non-grabbing popup should go with them, and it is owed its own tests
rather than a line folded into these. Recorded here so the next landing finds
the argument rather than the excuse.

The predicate is the drawn-ness one, in two halves. `popup_rect` resolves a
chain to a visible tile with every link abutting; `on_screen` then asks whether
the rectangle it resolved has a pixel inside the output and below the bar,
because `popup_rect` does not clip. Two shapes fail the second. A CHAIN does — a
TILED leaf is inset from the output by the gap, so a menu abutting one always
keeps a pixel, and it takes a menu at the edge with a submenu hung off the part
of it already outside. A popup on a FULLSCREEN window does too, which the
confirmation pass pointed out: that leaf is flush with the output on three sides
and with the bar on the fourth, which `on_screen` measures alike, so a single
menu abutting it has nowhere inside to land. It is NOT the same predicate as the
hit test, which also asks about the input region: a menu that takes no clicks
can still take keys, and should.

What the drawn check adds over the focus bound is narrower than the two cases
above suggest, and writing the bound down is what showed it. EVERY way a parent
hides is already under the focus bound. `Layout::focused` is the ACTIVE
workspace's focused leaf, so a parent on another workspace is not it. A GROUPED
container — stacked or tabbed alike, the two being one `run` — shows the leaf it
has focused, so a grouped-away parent is not it either, and cannot be made
focused without being shown. A leaf a sibling's fullscreen hides is not it
because the layout holds fullscreen and focus equal, and checks that it does.
The drawn check does its own work where the parent is focused and shown and the
MENU is what is not drawn: placed clear of its parent so no chain resolves, or
resolved and then clipped off the output.

The second bound is that the menu must hang off the FOCUSED window, and it is
the one that makes a grab safe to honour at all. Review demonstrated the
alternative against a first draft: a background client with any visible tile
called `grab`, mapped a one-pixel popup, and took every key the operator
typed. Nothing could get the keyboard back — the override beats click-to-focus
and `Super+arrow` alike, a grab suspends focus-follows-mouse, and no click
outside closes a menu yet — short of closing the offending window. Tying the
grab to the focused window leaves the menu of the window in use in charge,
which is the case the protocol is about, and gives the operator back the two
gestures they already have. The grab is not dropped when focus leaves, only
outranked; focusing that window again puts its menu back in charge.

Focus-follows-mouse is a way IN and not a way out, which the confirmation pass
drew out and which is a consequence of the suspension rather than a rule of
its own. The pointer crossing onto a background client's tile hands its
grabbing menu the seat; the grab then suspends the pointer, so moving off that
tile does not hand it back. `Super+arrow` and a click do. It is the reason the
recovery gestures matter and not only the bound that makes them work.

Reading the top off the stack also answers what happens when it goes, and for
a LINEAR chain answers it the way the protocol asks: "if the topmost grabbing
popup is destroyed, the grab will be returned to the parent of the popup, if
that parent previously had an explicit grab". A submenu closing hands the
keyboard back to the menu it hung off when that menu grabbed, and to the
window when it did not, because up one link and "whatever is left holding one"
are the same place.

They are not the same place in a BRANCH, which review pointed out and which is
the branch the paragraph below declines to refuse. With a grabbing menu A, a
grabbing child B, and a second grabbing child C mapped last, destroying C
returns the keyboard to B where the protocol names A. The stack answers "the
topmost grab left" and the protocol answers "up one link", and only a branch
tells them apart — a state the protocol's own parent rule reads as
disallowed.

Popups are in no layout, so this is an OVERRIDE rather than a focus the layout
could hold. `Layout::focused` goes on naming the window underneath for as long
as the menu is up, which is what the menu closing falls back to with no second
record to keep in step. A modal overlay is not consulted, because it never
was: the launcher and the cheat sheet stop keys at the input layer rather than
by moving focus, so a menu under one is focused exactly as the window under
one is, and hears no more than it does.

One thing the walk does NOT enforce is that the parent is the topmost grabbing
popup. The protocol requires the parent to be "an xdg_toplevel surface or
another xdg_popup with an explicit grab" and names no error for a second child
opened under a menu whose submenu already holds the top, so refusing one would
turn away a client the protocol permits. Review read the rule as demanding the
active top, and the strongest text for that reading is the sentence after it —
"if the parent is another xdg_popup it means that the popups are nested, with
this popup now being the topmost popup" — which describes what a grab MAKES
true rather than naming a fault when it is not. The seat has an ordered answer
now, so the consequence is no longer abstract: a client that opens such a
branch gets the keyboard on whichever branch it mapped LAST, and the sibling
that was up first keeps none. That is deterministic and it is not what a
branching client means; the reading that would have refused the second grab is
recorded here rather than acted on.

Signalling is only HALF of a dismissal, and the protocol puts both halves in
one sentence: "a popup_done event will be sent out, and at the same time the
surface will be unmapped". So a popup td tells is also unmapped — its
configure tracker is reset, and a newly-unmapped surface "must perform the
initial commit again before attaching a buffer". Without that the event is
ADVICE. A client that misses `popup_done` and repaints on its next frame gets
its menu straight back, which is exactly the client a dismissal is written
for; with it, the repaint is `unconfigured_buffer` and the client is
disconnected. That is the harsher answer and the one the protocol names.

The two halves are ONE call rather than an event sent here and a state change
made there, because a caller reaching for only one of them would leave a
client told its menu was gone by a compositor still willing to show it — and
which half you got would depend on which helper was nearest.

Saying `unconfigured_buffer` on the WIRE took a fix of its own, and the gap
predates the dismissal that made it matter: the pre-configure attach returned
a plain error, so it went out as the generic implementation code against the
wl_surface whose commit raised it. A code is read against the INTERFACE of the
object it arrives on, and 3 on a wl_surface is `invalid_offset` — which is
about the x and y of `wl_surface.attach`, so a client would have been told it
passed a bad offset in a request that carries none. (`invalid_size` is 2 and
a different error; naming it here was this paragraph's own slip.) The
refusal now names the xdg_surface and carries the xdg_surface's own code, the
same override the decoration errors take and for the same reason.

BOTH refusals, because the protocol gives them one code between them. td
checks twice — a buffer attached before the configure was sent, and one
attached before it was acknowledged — and "the client must acknowledge it and
is then allowed to attach a buffer" makes the second an unconfigured surface
exactly as the first is. Fixing one and leaving the other would have made the
code a property of which check happened to fire. Both reach every
pre-configure attach and not only a repainted menu, so this is a conformance
fix riding along rather than the point of the landing.

Re-mapping raises one question in two halves — what should td answer a
re-map's initial commit WITH? — and both halves are answered below.

Where the parent is LIVE the re-map is legal, and what it gets is the
`xdg_surface.configure` ALONE. The popup's own configure is sent once per
popup OBJECT, because "for version 2 or older, the configure event for an
xdg_popup is only ever sent once for the initial configuration" and td
advertises `xdg_wm_base` version 1; repeated role configures begin at version
3 with `set_reactive` and `reposition`, neither of which td has. The
xdg_surface half is still owed, since it carries the serial the client must
acknowledge before attaching — drop both and a re-map has nothing to ack and
no way back on screen.

What the client gives up is a fresh POSITION, and on version 1 that is not
td's to give: the placement was resolved at `get_popup` and this version has
no event that may revise it. The flag sits on the popup object rather than
beside the dismissal deliberately. A client's own null attach unmaps the same
tracker and has done since before td dismissed anything, so a fix hung off
dismissal would have left the older route — the one a toolkit reaches without
ignoring any event — still placing a popup twice.

Where the parent is GONE the answer is a DISMISSAL rather than a configure.
Nothing can resolve a position for that popup again, so configuring it would
ask the client to acknowledge a serial and paint a buffer the commit path is
then obliged to refund — work solicited for an outcome td has already decided.
`popup_done` is what td sends instead, at the first moment it knows: the
protocol has no error code for "not placeable", and this is precisely what
that event means. It goes out through the same take-down every dismissal
uses, so the submenus the scene is holding come down first and their pixels
are refunded — a menu is not left up over a parent td has just disowned.

What that take-down reaches is the PLACED submenus, and the gap between that
and the object graph is recorded rather than closed here. A submenu that
exists as an `xdg_popup` but has committed no buffer is in no placement, so
no cascade tells it — while `not_the_topmost_popup` is raised off the live
parent edges, which do include it. A client told its menu is gone, doing what
the protocol asks and destroying that popup, is then refused for a submenu it
was never told about. This is not new and not this path's: the window-destroy
cascade has the same hole and predates it, which is why the claim above about
a client never arriving at td's own `not_the_topmost_popup` holds only for
the placed subset. Closing it means walking the parent edges for dismissal
rather than the scene, at every dismissal site at once, and that is its own
landing.

A client that ignores the event and attaches anyway meets
`unconfigured_buffer`, but NOT because this dismissal unmapped anything.
Reaching it took the surface being unconfigured already, which is the same
condition the refusal tests; the tracker reset inside the dismissal has
nothing left to clear here and no test can see it. The reset earns its place
on the paths that reach it with a configure outstanding, and the pairing is
what keeps a dismissal one act rather than two. What this path changes is
only that the client was told.

This DIVERGES from xdg_surface's own words, and the divergence is deliberate.
That description is unconditional — a client "must perform an initial commit
without any buffer attached" and the compositor "will reply with initial
wl_surface state ... followed by an xdg_surface.configure event" — and here td
replies with a dismissal and no configure. Sending the configure anyway would
be conformant to the letter and dishonest: it promises a placement td has
already resolved never to make. The risk raised against it is a toolkit that
blocks in a roundtrip on the first popup configure and never wakes. The events
share ONE queue, which is most of the answer: a client waiting out a roundtrip
dispatches whatever arrives in it, and the dismissal is in it. Hanging takes a
client that receives `popup_done`, does nothing with it, and keeps waiting —
which is a client that hangs on any compositor that dismisses a popup it has
not configured, and the spec permits that on a denied grab. The alternative,
configuring and then dismissing at once, was weighed and declined: the
configure would carry a placement measured against a parent that no longer
exists, which is a worse thing to say than nothing. If a real toolkit is found
to hang here, that alternative is the answer, and adopting it costs one send.

The dismissal does NOT short-circuit the rest of the commit, and that is the
part worth writing down: a client asks for a frame callback and commits in one
breath, so the request is already in the buffer when td decides to turn the
popup away. Returning early there would leave a `done` that never arrives —
a client blocked in its own frame loop, which is a worse failure than the menu
not opening and one with nothing on the wire to explain it.

What this does NOT reach is the buffer commit of a popup that was configured
and acknowledged while its parent was still alive and orphaned afterwards.
That one is declined and refunded where it always was, because the invitation
it is answering was legitimate when it was issued. It is also the only
remaining way into that arm: a popup that got as far as being MAPPED has its
tracker reset by the cascade, so its next attach meets one of the two
pre-configure refusals above rather than the orphan check.

The surface NAMED gets nothing: its client took it down and is not owed an
event saying so. One take-down is not a cascade at all and is signalled
separately — a popup still unmapped when its window went was never in the
scene to be cascaded over, so it is told at its own next step, whichever that
turns out to be. A popup already configured and acknowledged is told when it
commits the buffer it was preparing: td declines the menu and says so there.
One that has not made its initial commit yet is told at that commit, which is
earlier and asks it to paint nothing. Without either a client waits on a menu
td discarded in silence. Both are the same call, so the popup is unmapped as a
cascade's are, and a client that simply re-attaches on a timer is refused
instead of being refunded round after round. Not one that repeats the whole
mapping dance, though: that is legal every time and answered every time. What
it is answered WITH is the dismissal rather than a configure, since the parent
is still gone — so no buffer is ever invited, and the round costs a commit and
an event rather than a paint. It is unbounded in rounds: a dismissal with no
configure outstanding retires nothing, so the tracker's ceiling stays out of
reach and leaving the loop is the client's to decide. The dismissal makes the
cheap loop illegal, not the expensive one. What remains is that td closes no
menu because a user clicked elsewhere, which is the grab bullet above rather
than this one — the dismissal it does make there is the protocol's answer to a
grab td will not hold, not a take-down of its own choosing. A workspace switch
is not a dismissal either and correctly sends nothing — a menu on a workspace
that is not showing is RETAINED rather than taken down, so it is still the
client's and reappears on return, which is a different thing from the client
being told it closed. A window hidden in a stacked container is the same case
for the same reason: the placement stops being visible and the menu is not
dropped. **Reposition** is version 3 and out of reach at the `xdg_wm_base`
version td advertises. td also refuses a NULL parent, which the protocol
permits only so that another protocol may supply one before the first commit:
td implements no such protocol, so a popup that arrived that way could never
be placed at all. A zero-area anchor rectangle IS accepted, against the
protocol's "non-zero anchor rectangle": it names a point perfectly well, and
disconnecting a client over a rule with a defined answer is the worse reading.

An error is posted against an OBJECT, so an error whose code belongs to another
interface has to name that interface's object. Five codes are `xdg_wm_base`'s
and only `defunct_surfaces` is raised from a request that arrives at one:
`invalid_popup_parent`, `invalid_positioner` and `role` arrive at an
xdg_surface, where 3 and 5 are `unconfigured_buffer` and `invalid_size`, and
`not_the_topmost_popup` arrives at an xdg_popup, whose only code is
`invalid_grab`. Each xdg_surface therefore carries the shell object it was made
by, and `defunct_surfaces` needs no override at all.

The boot profile starts one `td-term` toplevel; a `td-ui-demo` toplevel is
reached from the launcher instead, and the conformance below is its. It
discovers and binds
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
when an implicit grab ends, and one axis event per axis, so the composed
maximum is 69 events per frame and that is what the client accepts. Its own
model queues at most 67 of them: an axis is validated and consumed where it
arrives rather than joining the pending list, because the demo has nothing to
scroll. So the guard it enforces moved with the routing bound while what it can
actually hold did not, leaving it two looser than tight — noted rather than
tightened, since the constant is the one the SERVER may compose and a second
one meaning "what the demo queues" would be a number nothing checks.
At most one input-driven replacement is in flight; further
updates coalesce into the latest model revision until both buffer release and
frame completion arrive. Configure-driven replacement retains the existing
XDG behavior. The presentation handshake has a 20-second absolute deadline,
shorter than the supervisor's 30-second readiness deadline, so a stalled
compositor makes the client exit and permits `restart=always` to retry.

The launcher is a compositor-owned overlay, so opening it never depends on an
already-running client. Its registry has a terminal entry that starts a
`td-term`, an input-monitor entry that starts another `td-ui-demo`, and an
explicit close entry. The terminal is FIRST, so it is what an unfiltered
Enter opens: it is the entry a person came for, where the monitor is a
diagnostic that happened to be the only client there was. Three entries is
what the card currently holds: a fourth overflows `CARD_HEIGHT`, which
`registry_entries_are_searchable_and_fit_the_card` reds rather than clipping
silently, so adding one means growing the card in the same landing. Each entry
owns a label, lowercase search terms, and a typed launch request. The pure
launcher model stores a bounded 64-byte ASCII filter, requires every whitespace-
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
The compositor receives each client executable as an explicit argument —
`--launcher-client` and `--terminal-client` — and requires both, along with
the Wayland socket, to be absolute. Neither is defaulted: the store path a
package lands at is content addressed, so the compositor cannot derive one,
and a registry entry that spawns nothing is worse than one that never
appeared. The launch request selects the PROGRAM and nothing else, both
personalities of the multicall taking the same two run flags. The compositor
derives a unique readiness-socket name beside the socket and passes both
paths as literal argv values without a shell. It reaps exited children
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
inserted after the focused leaf, in the container that DIRECTLY holds it,
and becomes focused. Directly, rather than in the nearest ancestor running
some chosen way: same-axis nesting is reachable — a drop can leave a
container holding one child, and the collapse lifts it into a parent that
may run the same way — and an ancestor taking the insert would put the new
window beside its neighbour's container instead of in it, which for a
grouped container means outside the run on screen. There is no separately
selected axis to insert along:
`Super+v`/`Super+h` name a PRESENTATION now, so the only axis a new window
could join by is its neighbour's, and joining an existing column or row is
what "open it next to this one" means. `Super+Shift+Arrow` is how a window
leaves the container it landed in, making the perpendicular one where none
runs that way. Directional focus chooses the closest cross-axis-aligned tile
with a stable surface-key tie break. Directional move swaps two leaves while
focus stays with the moved toplevel. Unmapping a leaf collapses one-child
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
`xdg_toplevel.set_title` is RETAINED, keyed by surface: it is what names a
window, and a client MAY send it once, before its first buffer, and never
again — some resend on every tab or directory change, but a compositor cannot
ask, so one that drops a title may never see another. It is bounded at 256
CHARACTERS — a client's string in a map that outlives the request, and counted
in characters rather than bytes so truncation cannot split a UTF-8 sequence.
An empty title is stored as NO title, so what draws one has a single absent
case rather than two that look identical on screen. `set_app_id` is still read
for wire validity and dropped; it is not what names a window, and storing both
would make them indistinguishable downstream. Setting a title does not repaint
by itself — a client that puts its progress or its filename in its title would
otherwise repaint the whole screen per keystroke — so a renamed window shows
its new name at the next repaint anything else asks for.

A title's lifetime is its xdg_toplevel OBJECT's, not its mapped pixels'. It
survives every unmap, because a null-buffer attach is both the transient unmap
AND the opening of the initial handshake, so dropping it there would lose the
name of a window before its first buffer ever arrived. It is dropped when the
toplevel is destroyed, when the wl_surface is, and with the client. An input
region can be dropped with the pixels only because the client re-supplies one
on every commit; nothing re-supplies a title.

Every tile that carries decoration has a title band across its top, 20 pixels
tall,
holding the retained title in 2x glyphs. A tile is therefore a band and the
client's own area beneath it, and a PLACEMENT carries both as separate
rectangles: the layout decides where each goes, and the band's height is
passed in beside the gap rather than known there, because how tall a band is
belongs to whatever draws one. Two rectangles rather than one derived from the
other because the two need not touch — a grouped container puts its children's
bands in a run at its top and gives the content below all of them.

The client rectangle is what the blit covers, what the pointer hit test asks
about, and what is published to clients, and the CARVE that separates it from
the band happens in exactly one place, for the reason the status bar's offset
has one place: two of them disagreeing is a click landing somewhere other than
where it looks. The fullscreen arrangement then overrides that rect outright,
in the two places that build one, and zeroes the band with it. The band
and the client PARTITION the tile — the band is taken first, clipped to a tile
too short to hold one, and the client is what is left — so a short tile is all
band and no client rather than the two overlapping. The BORDER wraps the two
together, since one around the client area alone would leave a window's own
title bar outside its own frame. A STACKED leaf is the exception and the
placement SAYS which it is rather than being measured: the run's last band
abuts the content exactly as an ordinary band does, so adjacency cannot tell
the two apart, and joining it would draw that leaf's border four pixels above
its own band — over the band before it — and only when the last of a stack is
shown. A stack's frame is therefore the client area alone. A leaf its
WORKSPACE presents is NOT that exception, whichever of the three the workspace
is in: it is in no run, nothing sits above its band, and the frame wraps the
two as an ordinary tile's does.

The band takes the focused or unfocused colour with the border, in its own
pair rather than the border's, and its text is clipped to itself so an
overlong title cannot reach a neighbour. A window with no title gets a bare
band and no placeholder: the band is what says the window is there, and
inventing a name for it would put a word on screen no client chose. A click
anywhere in a band therefore reaches no client — the hit test knows only the
client area — which is the seam a drag handle needs.

FULLSCREEN is undecorated: a window with a band across the top of it is not
fullscreen, so a fullscreen leaf's band has zero height and its client area is
the whole arrangement. That is decided where the rect is overridden, which
happens for a fullscreen leaf on ANY workspace, and deliberately not from the
fullscreen STATE published beside it, which is set only for the visible one: a
client on a hidden workspace would otherwise be sized for the whole output and
carved for a band it does not have.

MOVING a window is i3's tree walk, not an exchange of two windows. The
nearest ancestor that RUNS along the direction's axis is the one that acts,
and what it does depends on what is beside the leaf there: a neighbouring
WINDOW trades places with it, a neighbouring CONTAINER is entered at its near
edge — entering from the left lands FIRST and entering from the right lands
last, which is spatially exact along the axis and merely an end when the
container runs across it — and a leaf with no neighbour that way LEAVES the
container it is in and becomes a sibling of it. Where i3 inserts relative to
the target container's own focused child, this inserts at the end, which is
the simpler rule and the one that does not need the target to have a
focus. Where NO ancestor runs that way at all — moving a
window up out of a row of them, the commonest arrangement there is — the
workspace is wrapped in a container that does, as i3 wraps it, rather than
the chord doing nothing.

Running out of ROOM is a different answer to the same walk, and telling the
two apart is what the workspace's own axis is asked for. A leaf that walks
off the end of a workspace already running that way is at its edge and stays
put; wrapping there would nest a container inside one of its own axis and
turn a row of three into a window beside a row of two — every width on
screen changed by a chord that should have done nothing.

Every arm is about the tree and none about the screen, which is what
separates a move from the swap it replaces. A swap exchanges two keys, so the
arrangement it leaves always has the shape it started with and a window can
never enter or leave a column; the geometry it produces is reachable, but
half of what an operator means by "move it there" is not. Directional FOCUS
still asks the geometry, because "the window to the left" is a question about
what is on screen, and a move is a question about where a window belongs.

Nothing mutates until an arm commits, which is a property of the WALK and
not of the whole command: a walk that reaches the root having found no
ancestor to act hands back a tree it did not touch, and what happens next —
the wrap, or nothing — is then decided on a pristine one. A container a leaf
has left may hold a single child, which is collapsed as an unmap's is; and
the lone-window case is a guard rather than something that falls out, since
without it the wrap would build a container with one child in it. Focus does
not follow the geometry afterwards — the window that moved stays focused,
since it is the one the operator was acting on.

DRAGGING a window is the same move with the destination named by the pointer
instead of by a direction. A press on a title BAND picks the window up and
focuses it; from there a semi-transparent BLOCK is drawn over the region the
window would land in, and the release is what moves it. Nothing in the
arrangement moves while the button is down.

A tile has FIVE zones and the one the pointer is in says what landing there
MEANS. Over the middle the two windows trade places, keeping their
containers and their neighbours. Over an edge the dragged window lands on
that side — in a column for the top and bottom edges, in a row for the left
and right — and the drop NAMES that axis rather than reading one off the
target's container, so the insert can make the container it needs. That last
part is what the five zones buy over a two-sided drop: "put it below this
window" over a window in a row reaches an arrangement the keyboard only gets
to as a sequence of commands, and the two-sided drop could not express at
all.

It only MAKES one where the named axis differs from the container the target
is already in. Asking for "below" inside a column, or "right" inside a row,
is asking for a place in a run that already goes that way, so the leaf is
inserted into it rather than wrapped in a redundant container of its own.
That is the ordinary case rather than a corner, and it is why the block for
one is a bar on an edge rather than a half.

A STACKED container refuses the axis outright and takes the leaf into its
run whatever was asked. It draws one band per LEAF beneath it, so a split
among its children is a container it never shows: the operator sees it join
the run exactly as it would have anyway, and finds a row waiting for them
the moment they unstack. What a stack presents is a list, so a drop into one
is a place in that list however it was aimed — the same answer its own bands
give. Leaving a stack sideways is `Move` from the keyboard, or a drop onto a
window outside it.

The middle is the middle NINTH, and outside it the NEAREST edge picks the
side, judged in proportion to the tile rather than in pixels — or a wide
short one would answer top or bottom almost everywhere. The edges get the
bulk of the tile because they are what an operator aims at; a swap is the
one drop with a deliberate target to hit. Every point in a tile answers, and
a tile with no area answers Swap, having no edges to be nearer one of.

A title BAND is the exception and keeps TWO zones along the run it is part
of. Five will not fit in a strip a line of text tall, and a run is a list
rather than an area: a position in it is the only thing a drop onto one can
mean. It therefore names NO axis — the target's own container, whatever that
is — and the half is read along the direction that run actually goes: across
for a row, where each leaf carries its own band at the top of its own tile,
and down for a column. A GROUPED container answers from its presentation
rather than from its axis, since its bands are a list rather than one per
tile: down for a stack, across for tabs, whatever axis the tree beneath has.

Both of those are asked of the tree the drop will be APPLIED to, which is
the one on screen and the only one there is. That was worth saying while a
second, dragged-window-removed tree existed to get it wrong from; it is now
a property of there being nothing else to ask.

The zone is read within whichever rectangle the pointer is in rather than
over the tile as a whole, because a grouped leaf's band and its client area
are far apart and a point between them is in neither.

The dragged window is DETACHED rather than removed, and that is the whole of
this operation's correctness. A removal collapses the container the leaf came
out of, and when the two are siblings that container is the one the TARGET
sits in — so dropping one member of a two-window column onto the other would
collapse the column, land in the row beside it, and flatten the arrangement;
and a two-window stack reordered that way would silently unstack. Collapsing
once at the END leaves the target's container standing.

The whole drag lives outside the pointer protocol and has to: a band belongs
to no client, so a press on one establishes no grab. That is the same seam
that makes a click on a band reach no client, used rather than worked around.

TWO presses start a drag. A bare BTN_LEFT press on a title band, and an ALT
press anywhere on a window — its band or its client area. The band is the
handle that exists without a modifier, so a window can be moved with the mouse
alone; the modifier is what makes the whole window one, so a window whose band
is a sliver in a crowded stack is still draggable. A bare press on a client
area remains that client's.

A band's right end is BUTTONS rather than handle, and they answer BEFORE the
band does. Three of them, one per presentation: stack the container, tab it,
or split it — the same three `Super+v`/`h`/`s` reach, by pointer. The order of
precedence is the whole of the wiring: both gestures open identically, with a
press on the same strip, so a band that picked the window up first would leave
every button a drag handle that never fires. An ALT press is the exception and
keeps the whole window, buttons included, because that gesture exists for
moving a window from anywhere in it.

A button acts on the container the FOCUSED leaf is in, so the press moves
focus to the band it landed on before running the command; without that,
pressing a button on an unfocused window would present some other container
entirely. The button marked in a lit ink is the one the container is already
in, so the band SAYS which of the three it is as well as offering the other
two — and each button draws the arrangement rather than a letter: two
collapsed titles over the window they leave for a stack, one divided row over
a body for tabs, two tiles for a split. A letter would name the chord, and the
chords are already on the help sheet. Three EQUAL bars for the stack would be
a hamburger, which is a menu everywhere else on a screen and says nothing
about which of the leaves under it is shown; each icon also fills its own
rectangle top to bottom — the slot inset on all four sides, so the padding
around it is the same for all three — or one of them reads as a smaller button
beside its neighbours.

TWO bands carry NO buttons, both for want of ROOM, and the painter and the hit
test decide it in one place, because a button drawn where nothing answers is a
button that does nothing when pressed with nothing on screen to say so. A band
too NARROW to hold them beside a name gets none: a tabbed run divides ONE
strip between its leaves, so a column of eight gives each tab a few dozen
pixels, and buttons there would be the whole tab with the title squeezed out —
the room reserved for the name is a glyph at the scale titles are DRAWN at
rather than at 1x, or the reserve is half a cell and the smear is what the
band shows. And a band too SHORT to draw an icon in gets none either, which is
the same rule seen the other way: a tile clipped to a sliver keeps its band
and loses its client, and the run's last band is clipped to whatever the
container has left, so this is reachable rather than theoretical. The keys
still reach every presentation, and any larger band on the workspace still
carries them.

A LONE window was a third case and is not. It is in no container of its own,
so it had no presentation to mark and all three of its buttons would have
changed nothing — which left the band an operator sees FIRST as the one saying
least about how the shell works. Its WORKSPACE presents it instead, and is the
container of last resort the keys reach too.

Grouping one window moves no pixels, and that is arranged rather than
observed. The placement takes the presentation WITHOUT taking a run, so the
geometry is the ungrouped geometry and the border still wraps the band
together with the client. Laying the leaf out as a run of one would draw the
same picture — one band over one content rectangle is what an ordinary tile
already is — but a placement in a run gives up that border, and the operator's
only window would have its own title bar outside its own frame the moment a
button was pressed.

What the choice buys is therefore entirely the NEXT window: it is carried onto
the container the second window creates, and forgotten by the workspace at
that same moment, so `Super+h` while one window is open means the second opens
into a tab rather than being an instruction that quietly expired. An EMPTY
workspace cannot be set up that way — no leaf is focused, so the command never
reaches the workspace — and the first window is where the choice starts.

Handed over rather than copied, in both directions. A workspace that kept its
choice would re-apply it to some later lone leaf that never asked: a container
collapsing back to one child is gone and its presentation with it, and the
survivor must not find the workspace still holding what that container was
given. And a workspace holding NOTHING must not overwrite the container a
window arrives into — `Split` is both "no choice" and a presentation, so
handing it on unconditionally would ungroup a grouped root every time a window
opened in it. A workspace emptied of its last window forgets the choice by
that same rule the tree already follows, so a window opened long afterwards is
not grouped by one that is gone.

Which presentation a band MARKS is read off the placement rather than walked
out of the tree, and the placement carries the presentation ITSELF beside the
`run` its geometry needs. The two are the same fact for every leaf in a
container — the direction its bands travel names which of the two grouped
presentations that container is in — and they are NOT the same for a leaf a
workspace presents, which is in no run at all. Two fields rather than one
because a run is what costs a window the border around its own title bar, and
a run of one leaf would pay that for a picture identical to the one it already
draws. Asking the layout per band instead would search the tree once for every
window on every repaint, which is quadratic in a flat row of them, and would
be a SECOND reading of the rule that picks the container a leaf is displayed
in: a band could then mark one container while its buttons changed another.

An Alt PRESS is withheld from the client. It is what starts the gesture, so
handing it on would leave the client acting on a click the operator aimed at
the compositor. That is the same seam a band uses — the compositor keeping a
button to itself — reached by a CLAIM rather than by geometry, since an Alt
press lands on a client area where a band press does not.

A press that could move NOTHING is not claimed. Under fullscreen the one
placement covers the output, so every Alt click anywhere in that client would
be taken and none of them could ever land — an application silently losing the
modifier everywhere. A lone window USED to be the same case for a different
reason, there being nothing to land it beside, and is not one since the strip
began naming a spare workspace: the only window on one can still be sent
to another, so refusing the claim would leave its title band dragging it to the
bar while the same drag held by Alt never started. So the claim asks whether a
drag of the window under the pointer could reach anywhere — a second window OR
a workspace that is not this one — and leaves the click to its client when it
could not. That the strip always names such a workspace while the range holds
more than one number is an arithmetic coincidence rather than a reason, so the
question is asked instead of assumed. That is also what keeps this gesture
from reaching
around the rule the tiling commands already have, where `Move` refuses to pull
a fullscreen window into an arrangement nobody asked for.

The claim is answered by the pointer model, walking the report's transitions
in ORDER, rather than by the compositor once for the report. A report carries
every transition up to its SYN_REPORT, so the grab a claimed press must not
steal can be established or dropped by an earlier transition IN THE SAME ONE:
answered once, a right press beside a left one would let the left be taken
from a client that had just grabbed it, and a right RELEASE beside one would
refuse a press whose grab had already ended. The model owns the grab, so it
owns the question; what the compositor supplies is only what a claim looks
like. That is also what keeps the claim and the drag from disagreeing about
who owns a button — the drag acts on the presses the model reports having
claimed, rather than on a second answer of its own.

Its RELEASE needs no handling of its own. A claimed press enters neither of
the model's button sets, so its release stops at the first of them — the one
that asks whether the button was down at all — and never reaches a client.
Withholding it as well would be a second copy of that bookkeeping, and one
that can DISAGREE with it: a batch that overflows its transition limit is
reset, and a genuine press delivered after such a reset would have its release
swallowed by that second copy and its grab left standing, which is worse than
the case it was guarding.

Letting ALT go before the button PUTS THE WINDOW BACK. The modifier is half of
what holds that gesture open, so releasing it abandons the drag rather than
completing it, and the picture goes with it — which is the whole of the
revert, since the layout underneath was never touched. The release that
eventually follows still reaches no client, by the rule above: the press it
belongs to was never delivered, whatever became of the drag in between.
Pressing Alt again does not resume — the gesture ended, and only a new press
starts one. Which gesture a press begins is decided by whether Alt was down AT
THE PRESS, so a band pressed under Alt is an Alt drag, and a band drag is not
ended by a modifier that never held it open.

A client already holding an implicit grab is the exception, and it owns every
button until it lets go. The pointer model routes a press made during a grab
to the grabbing surface, so that press IS delivered wherever it lands: taking
it as a handle would make one button both the window's and the compositor's,
and would move focus mid-grab, which both halves of the focus policy refuse
for the same reason. Neither press begins a drag while a grab is held —
which is a rule about the grab AT THE PRESS, not for the length of a
gesture: a second button pressed during a live drag is still the client's
and still establishes one.

A drag is also forgotten when the window it names goes away — on a single
toplevel's removal, on a whole client's, and on an unmap. Object ids are
recycled per client, so a stale one does not merely name nothing: it can come
to name a DIFFERENT window, and the release would then move one nobody picked
up. The unmap case is the one that can be UNDONE — the same surface maps again
and is the same window — so a drag that survived it would come back to life
and move that window under a button pressed before it ever vanished.

A press that picks NOTHING up ends whatever was live, rather than leaving a
block standing with no drag able to commit or clear it. That is not
reachable while every release arrives, since a second press of a held button
is not forwarded, but a batch that overflows its transition limit is reset and
the release in it is what goes.

A release off every tile cancels rather
than moving to nowhere, and an overlay going up drops what was held, since it
covers the screen the operator was aiming at. That cancel happens where the
overlay BECOMES visible rather than on the next modal pointer frame, because
an overlay is opened from the keyboard: raising and dismissing one without
moving the mouse would otherwise leave the drag standing. It is folded into
the paint the overlay already owes rather than settling on its own, since a
failure of its own would be the one path through those two that returns
without restoring what it changed.

A drop reports whether the ARRANGEMENT changed rather than whether it was
asked for, so putting a window back exactly where it came from — the
commonest gesture there is — costs no round of configures. It still costs a
REPAINT, and those are two answers rather than one for exactly that reason: a
block came down whatever the drop landed, so a release that reported "nothing
happened" to both would leave a blue rectangle standing with the drag already
over and nothing left to clear it.

The drag INDICATOR is a BLOCK over the region a release would land in, drawn
ON the arrangement rather than in place of it. It is blended in integer
thirds, two parts what is already there to one part blue: this framebuffer
has no alpha channel to carry transparency, and mixing at draw time is the
whole of what "semi-transparent" can mean for a surface that gets composited
once.

A swap covers the target's whole frame — the dragged window really does end
up there. Everything else is one of two shapes, and which one is decided by
the TREE rather than by the zone that was aimed, because `insert_beside` only
SPLITS where the asked-for axis differs from the target's own container and
the target is not in a stack. Anywhere else the drop degenerates to a plain
insert into a run, where the dragged window takes a whole slot and every
sibling shrinks. A split therefore gets the HALF it would leave; an insert
gets a twelve-pixel bar on the edge it goes in at, running the way that run
travels. Promising the half for an insert would be a picture the release
cannot keep — dropping "to the right of" a window in a ROW is an insert, not
a split, and that is the ordinary case rather than a corner.

A STACKED target then differs once more in WHERE the bar goes. Its leaves all
share one content rectangle, while the run itself is the column of BANDS at
the container's top — so a bar on the content rectangle would mark a place
the new band does not appear. It is drawn on the target's own band instead,
where the run is and where the operator was pointing. The swap stays the
exception in the tree too, keeping the content rectangle it really does take.

The dragged window's OWN tile promises nothing, because a window cannot be
moved beside itself, so a gesture that never leaves it lands nothing on
release.

CLIENTS are told nothing until that release. A block is pixels over the
arrangement, so a client redrawing mid-drag is still drawing the tile it
actually has, and an abandoned gesture has no round of configures to undo —
which is what makes every cancel (an overlay going up, Alt released, the
pointer leaving every tile) cost a repaint alone. A mutation that takes a
block down therefore reports its OWN change and not the block's: every caller
repaints unconditionally, and answering otherwise would reconfigure every
client for a rectangle none of them was ever told about.

FOCUS moves at the PRESS and at the DROP and nowhere between. Picking a
window up focuses it, as clicking it would, and the drop focuses what it
moved, as every other way of moving a window does. Aiming touches neither.

ONE arrangement is in play, and that is what replaced two. The drop used to
be DRAWN — the layout re-flowed with the dragged window already moved to
where the pointer said — and aimed at a third geometry, the layout with that
window taken OUT, which existed so the picture could not push its own target
away. It was coherent and it was hard to use: tiles slid out from under the
pointer as the operator approached them, and the answer was computed against
a geometry that was on no screen, so a point read off the screen asked a
different question than the drag did. Holding the picture still and drawing
the answer on it makes the target stable without a second geometry to be
right about.

A press alone must not move a window, and a click on a title bar has to stay a
click. That is said as a THRESHOLD from the press point: a drag aims at
nothing until the pointer has left where the button went down by more than
eight pixels on either axis, and once it has it goes on aiming for the rest of
the gesture. Eight is GTK's `gtk-dnd-drag-threshold` default, and per axis
rather than by true distance is GTK's shape too — no multiply, so no overflow
to reason about.

It LATCHES because not aiming leaves the last block STANDING rather than
clearing it: a block is recomputed only on the frames a drag aims, so one
that un-aimed on the way back would promise a landing the pointer had left
and the release would take it. Before the latch nothing has been promised,
which is what makes skipping safe at all.

What the threshold replaced was a REGION — the dragged window's whole tile
was a dead zone and every aim inside it was refused. The trouble with the
region was that it was read in the SCREEN geometry while the target was read
in the AIM one, and the two did not correspond: with the dragged window taken
out, the neighbour that grew into its place lay mostly underneath it, so the
zone covered that neighbour's live drop points along with the window's own.
Swept pixel by pixel across a 1600-wide output, `H[1, 2]` through `H[1 … 5]`
all had 2's left third entirely dead; its middle ninth kept 271 columns of
517 at two windows, 8 of 255 at three, and none at all from four on. That
defect went with the second geometry rather than with the region, and the
threshold remains what a region could never be: the thing that tells a click
from a drag.

The region and the block refuse the same gesture, and the period between them
is where they differed — which is a real behaviour change and not only a cost
removed. Under the two geometries, pressing a band in `H[1, 2, 3, 4]` and
pulling straight down 300 pixels without ever leaving that window's own tile
traded it with 2, because the pointer was over the dragged window on SCREEN
and over its neighbour in the AIM one. With one geometry the pointer is over
the dragged window, full stop, and the release lands nothing. It also costs a
STACK nothing, where the region cost something real: for the leaf a stack was
showing, the zone was its band plus the whole shared content rect, which left
the other leaves' bands as the only live targets inside that stack.

The block is DERIVED, never a second source of truth: it is recomputed on
every frame a drag aims and DROPPED by every change to the arrangement, so a
window arriving or leaving mid-drag cannot leave one promising a landing in a
tree that no longer has it. The drag itself survives that and re-aims on the
next motion; a release in the window between them lands nothing, which is the
honest answer when the arrangement the drop was aimed at is gone. The release
APPLIES the block rather than recomputing one, so what lands is exactly what
was on screen.

Button transitions are handled in the ORDER they happened, one pass over the
frame. A frame can carry several — evdev keeps every transition up to its
SYN_REPORT — and a release followed by a press is one window dropped and the
next picked up in a single batch. Handling all the presses first would let the
new drag consume the old one's release: the old drop lost and the new drag
ended where it began.

The block is a frame behind the pointer when a batch carries both the motion
that chose it and the release that takes it, so a release brings it up to
date first — but only when that frame MOVED. On a still pointer there is
nothing new to account for, and computing an answer anyway is how a release
after an INVALIDATED block would land one the operator never saw. A still
release over a block that is still up does land it: what the guard skips is
the re-aim, not the drop.

GROUPING is what a container does instead of splitting, and it takes two
forms. A STACKED container runs its leaves' bands DOWN its top, one after
another; a TABBED one divides a single band-high strip ACROSS it, one tab per
leaf. Either way ONE leaf gets everything below the run and the rest are a
band alone. `Super+v` stacks the focused leaf's container, `Super+h` tabs it,
and `Super+s` groups an ungrouped one or ungroups a grouped one.

The run is clipped to the container, so one too short to hold every band is
all band and no content rather than the two overlapping — the short-tile rule
again, one level up. That clipping is why the two forms are not one mode with
a flag rather than a presentation each: a stack's run costs a band PER LEAF,
so a column of twelve on a short output is all run and no window, while tabs
cost ONE band however many leaves there are and leave the same content height
for twenty as for two. What tabs give up for it is the titles: a stack shows
every name in full, and a tab shows a name in its share of one strip, which
past a handful of leaves is a few characters.

A lone window is in no container of its own, so its WORKSPACE is the one that
presents it and all three bindings land there. That moves no pixels while the
window is alone — the placement takes the presentation without taking a run —
and what it decides is the container the second window makes.

Which leaf is shown is the container's own most recently focused, which is
the focused one whenever focus is in the stack at all. Focus alone cannot
answer it: it names one leaf per workspace, so every stack the operator is
not in would fall back to its first and snap there the moment focus left.
The workspace therefore keeps an MRU record of its leaves, and a stack reads
the first of its own that appears in it. A workspace never yet visited has no
record and shows its first leaf, which is what makes a hidden stack publish
the sizes it will have when it is shown.

All three chords GROUP the focused leaf's own parent, and reach the outermost
GROUPED ancestor when there is one. The asymmetry is not one: a group runs
every leaf beneath it, so it hides what the containers under it are doing,
and that ancestor is what the leaf is displayed in whatever they say.
Descending past it would present a container nothing can see — and a group
whose direct children have all since become splits could then never be undone
from the keyboard, since no leaf in it is a child of it any more.

A split container records no former presentation, so `Super+s` on one always
groups it as a STACK, including one that was tabbed a moment earlier.
`Super+h` is how the other is asked for, and the pair of set chords is why
that is a simplification rather than a gap: the operator who wants tabs back
names them, and a third state on `Presentation::Split` would exist only to
remember something two other chords already say.

Bands in a run are NOT separated by a border, and adjacent unfocused ones
therefore read as one block distinguished only by their titles. That is a
limitation rather than a decision deferred: the border is 4 pixels against a
20-pixel band whose text already occupies rows 3 to 16, so a border per band
would draw over the name it was meant to delimit.

Where this diverges from i3 is WHAT is presented: i3 groups a container's
CHILDREN, so a nested split shows as one title, and td groups its LEAVES, so
every window in the container has a band of its own. A group is a way to see
what is in a column without giving each window a share of it, and a title
naming a container rather than a window answers a question nobody asked.

A grouped-away leaf keeps the SIZE of the content area rather than being
zeroed, so a client does not resize its buffer down and back across a
toggle. That is why "shown" cannot be read off the rectangle — a hidden
leaf's rectangle is exactly the shown one's — and a placement carries an
explicit flag instead. Five sites read it: the border pass, the blit, the
pointer hit test, the GRAB — which answers nothing for a leaf that is not
shown, the way it already answers nothing for one on another workspace — and
`views`, which is what the CLIENT is told, a grouped-away toplevel being
published NOT visible at the size it would have so it holds a buffer ready
for the moment it is shown. The band pass is the one that deliberately does
not ask: a band is drawn whether or not its client is.

Bands are drawn in a pass of their own, before any border rather than beside
each one. The two are separate rectangles that OVERLAP in a group — the shown
leaf's border rides four pixels up into the run's last band — so interleaving
them lets a band belonging to a later placement erase a border already drawn,
and only when the shown leaf is not the last of its run. That is the same
argument one level down from decoration preceding client pixels.

All three presentation chords are refused under fullscreen, as directional
focus and move already are: nothing on screen would report the change, and
leaving fullscreen would land the operator in an arrangement they never asked
for.

Directional focus and movement ask for the arrangement the container would
have UNGROUPED, because in a group every leaf shares the one content
rectangle and there is no geometry to rank them by. Presentation is about
what is DRAWN, and the tree a chord walks is otherwise the same either way.

Directional FOCUS is the exception, and movement is not: `Super+Shift+Down`
still reads the ungrouped arrangement, so on a tabbed column it carries the
window OUT of the container rather than along the tab strip — the chord that
walks the bands and the chord that carries a window along them are
perpendicular there. Recorded rather than fixed, and the reason to be wary of
fixing it by symmetry: a move that reordered the run would have to mean
something different again in the other presentation.

The pair ALONG THE RUN is that exception, and it is the one that can be
answered without geometry: a stack runs its bands top to bottom and tabs run
theirs left to right, whatever axis the container beneath has, so
`Super+Up`/`Down` walk a stack's run and `Super+Left`/`Right` walk a tabbed
one's — the leaves the OUTERMOST grouped ancestor presents, in band order.
Outermost because that is where the renderer stops: the first grouped
container met from the root is handed to `place_group`, which draws one band
per leaf beneath it, so a group nested in another is not presented at all and
its run is bands nobody can see.

WHICH pair walks the run is now a property of what is on screen rather than
of the tree, and that is what an explicit tabbed presentation buys beyond its
geometry. Under a single stacked mode the answer came from the container's
hidden axis: a stack made from a row and one made from a column drew
identically and answered opposite chords, the same keystroke meaning
different things for two arrangements the operator cannot tell apart. Now the
bands say it — they run the way the chord that walks them points.

The OTHER pair leaves the group, and leaves it WHOLE. Those steps rank the
ungrouped arrangement, where a group's leaves each hold a fraction of the
container, so ranking against the group's OWN leaves would walk the run a
second way and by the wrong rule: `Up` inside a tabbed column — which is a
vertical split — would step to the tab above it in the TREE, one the screen
shows beside it. The group's other leaves are therefore dropped from the
ranking before the step, which leaves each pair of directions with exactly
one meaning, along the run or out of the group. A step off either END of the
run falls through to that same ranking, which is what lets the walking pair
leave as well rather than trapping focus in the group.

Coming BACK to a group lands on the leaf it is SHOWING, which is a step the
geometry cannot take. The ranking runs over the UNGROUPED arrangement, where a
group's leaves hold a fraction of the container each, and a step in from
outside ranks those fractions — none of which is on screen, since the group
draws one leaf. Ties go to the lowest key, and for the common shape they ARE
tied: leaves split evenly about their container are equidistant from a
neighbour's centre, so `H[1, Vstacked[2, 3]]` answers 2 whichever leaf is
drawn. Worse, it did not come back: focusing the leaf the ranking picked also
puts THAT leaf at the front of the record `place_group` reads, so `Left` then
`Right` returned to a different window with a different one drawn. Only a step
arriving from OUTSIDE is redirected, since a step within a group is the run
walk above and never reaches the ranking at all.

One consequence is recorded rather than fixed: the fall-through search starts
from the focused leaf's ungrouped fraction rather than from the group's whole
area, so a window beside another part of a wide group can be missed. That is
how directional focus has always left a group and is not introduced here.

And the OTHER round trip — INTO a group from outside, then straight back out —
gets worse, which is the price of this one and is paid knowingly. The step
back is measured from the fraction of whichever leaf was landed on, so
redirecting to the shown leaf moves where it starts from. Swept over 4096
generated five-leaf shapes: the trip this section IS about, leaving a group
and returning, goes from 1048 of 1602 broken to NONE, and from 1048 whose
drawn leaf changed to none; the entering trip goes from 246 of 682 broken to
296. Fifty steps worse against a thousand fixed, on a trip that was already
failing more often than not. Closing that one needs the search to measure from
the group's whole area rather than from a leaf's fraction, which is the same
fall-through limit the paragraph above records.

Client-side decoration negotiation, clipboard, drag-and-drop, subsurfaces,
popups, output reconfiguration, fractional scale, screen capture, data
devices, and touch are not yet advertised. Unknown objects,
malformed sizes, invalid object reuse, missing file descriptors, and
unsupported requests disconnect only that client.

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
After `SYN_DROPPED`, the adapter releases that node's state, discards records
through the next `SYN_REPORT`, and resumes without guessing the lost state.
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
borders, TITLE BANDS, clipped-away pixels, empty tile space, and pixels
excluded by the surface's committed input region have no pointer focus. A region retains at
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
the surface under the cursor and the new grab starts there.

KEYBOARD focus FOLLOWS THE POINTER: a motion that leaves the cursor over a
different window aims the keyboard at it, with no click to say so. Only on
MOTION — a cursor that did not change POSITION cannot have changed which
window is under it, so a still pointer never re-answers the question and a
`Super+arrow` is not undone by the next button or wheel report that happens
to arrive. A nonzero delta is not that question's answer: one pointing off
the edge of the output is clamped away, leaving a report that asked to move
and did not. The paint owed, the focus re-answered and the drop re-derived
all read the same "moved", which is what `Scene::move_pointer` reports.
A window is its TILE, and its title band as much as its client pixels: the
band is the handle a drag picks it up by, and the tile is what a tiling
compositor means by the window. So an undersized buffer or a narrow input
region — which DELIVER nothing over the rest of the tile, and under
click-to-focus could not be clicked into focus there — are hovered into
focus anyway. Deliberate: a client cannot decline the keyboard by shrinking,
which would otherwise leave part of a tile nothing could focus. The cost is
that the two halves no longer name the same target over such a spot, where
a press focuses nothing and a hover focuses the tile.

Anywhere belonging to no window — the status bar, a gap, a border — KEEPS
the focus that was there rather than clearing it, so crossing a gap on the
way between two tiles does not leave the keyboard aimed at nothing. A
grouped-away leaf answers for its own BAND alone: every leaf of a group
shares one content rectangle, so that rectangle belongs to the leaf being
SHOWN, and the hit test's visibility guard is what keeps a hidden one from
claiming it.

What that gives up is the argument the previous policy was written on: a
pointer crossing a tile on its way somewhere else focuses it in passing, so
a `Super+arrow` followed by any mouse movement loses the window it chose.
That is what focus-follows-mouse IS, and it is what was asked for. It
reaches one place beyond the keyboard, too: the shown leaf of a STACK is its
most recently focused, so sweeping the pointer down a stack's run of bands
flips through the windows it presents, one per band crossed — the same thing
clicking each band in turn already did.

Four things suspend it, and each would otherwise aim the keyboard somewhere
the operator did not. A modal launcher owns the screen and keeps focus for as
long as it is up. A DRAG carries the pointer across other windows on purpose,
and focus belongs to the window being carried. A client holding an implicit
grab owns the pointer until it lets go, which is the same reason a press made
during one establishes nothing. And a MENU holding the seat's explicit grab
suspends it for the length of that grab — not because the keyboard would move,
since the override above holds it either way, but because the FALLBACK would:
the pointer crossing a neighbouring tile on its way to a menu item would leave
that tile focused when the menu closed, and the band would light up saying so
while the menu was still up. A menu is opened ON a window, and closing it puts
the keyboard back where it was rather than wherever the hand passed over.

Click-to-focus is NOT suspended by a menu's grab, and that is deliberate: it
is half of how the operator takes the seat back. A press on another window
moves the layout's focus, and a grab holds the keyboard only while it hangs
off the FOCUSED window, so the press ends the menu's hold. `Super+arrow` is
the other half and works the same way. Neither DISMISSES the menu — the client
is told nothing and its popup stays drawn over its own window — which is the
part the pointer landing owes.

That grab is sampled BEFORE the frame, because the frame is what ends one: a
report carrying the last release along with its motion clears the grab in the
same call that delivered the motion to the grabbing client, so reading it
afterwards would call that motion ungrabbed and hand focus to whatever the
pointer was dragged over. Whether the motion and the release arrive together
or in two reports is the device's batching, not anything the operator did,
and focus must not turn on it.

That same press ALSO moves keyboard focus, and it is not made redundant by
the above: it focuses a window the pointer is ALREADY over, which is what a
`Super+arrow` leaves behind — focus carried off the hovered window with no
motion coming to bring it back — and what a newly mapped window under a still
cursor leaves. The surface focused is the one the
press ESTABLISHED its grab on — the same surface the button event was routed
to, so keyboard focus cannot disagree with delivery — and only a press that
starts a grab counts, so a second button pressed mid-drag does not drag focus
along with the pointer, and a held grab does not re-assert its focus against a
`Super+arrow` issued while the button is down. A press over the gap between
tiles focuses nothing rather than unfocusing: it is a click on the desktop.

The pointer model REPORTS that surface from the one place it assigns a grab,
rather than the runtime inferring it by comparing the grab before and after
the frame. The two are not the same predicate, and the paragraph above says
why: a frame carrying a release and then a press retargets the grab, so the
comparison sees `Some` on both sides and misses a press that WAS delivered
elsewhere; a press and its release together end with no grab at all, which the
comparison cannot tell from a frame with no press in it. Both shapes are
reachable — a mouse reports its whole button bitmap per poll, so rolling from
one button to another arrives as one report — and the failure is silent: the
button goes to the tile under the cursor and the keyboard stays behind. Within
one report every establishing press names the same surface anyway, since a
press establishes only while no grab is held and focus is the hovered surface
whenever none is.

`Layout::focus_key` then refuses any surface that is not a leaf of the ACTIVE
workspace, and under fullscreen refuses everything but the fullscreen leaf, so
no click can put the keyboard somewhere the screen does not show. A modal
launcher filters presses out before any of this, so nothing is established and
the overlay keeps focus while it is up. A focusing click repaints
synchronously, which settles any paint the same report's motion deferred; and
because it can fail as any other paint can, a click can now end the evdev
reader where only keys and commands could before. A focusing HOVER is the
same paint and carries the same consequence, which widens that from a report
with a button in it to any report that moved.

Removing or hiding the grabbed surface cancels the grab and reconciles focus
without leaving a stale surface reference. Partial button or motion records
are discarded on `SYN_DROPPED`; delivered button state is tracked separately
so recovery releases only buttons the client had actually seen. A report
retains at most 64 button transitions; crossing that limit performs the same
fail-closed release and resynchronization through the next `SYN_REPORT`.
Hiding or
destroying a grabbed surface instead cancels its delivered state and sends
leave, because an unmapped surface is no longer a valid button target.

`set_cursor` accepts a null surface or assigns the cursor role to a
`wl_surface`, preventing later XDG-role reuse, only when its serial matches the
latest enter sent to that client for the seat and the runtime still focuses
that entered surface. Late pointer resources reuse the client-wide serial.
Stale, pre-enter, and logically post-leave requests are ignored without
consuming a role even when socket delivery of the leave is delayed; a valid
incompatible role uses `wl_pointer.error.role`. Cursor buffers are released as
soon as their pixels are copied and never enter the tiling scene: a cursor is
arranged by nothing, focuses nothing and is drawn over everything.

That image is what the pointer PAINTS, at the position the hotspot names —
the pixel of the image that sits on the pointer's own coordinate, so the
image's corner is that far back from it and lands off the top or left edge
whenever the pointer is near one. Clipping there is ordinary rather than an
error, which is why the cursor's origin is signed where a placement's is not.
A null surface asks for NO cursor and is the whole of the request: there is no
surface to give a role to and no commit to wait for. td's own cross is the
FALLBACK rather than the default, and it stands for three situations an
operator cannot tell apart anyway — no client focused, a focused client that
has asked for nothing, and one that named a cursor surface whose pixels have
not arrived.

A cursor belongs to the client the pointer is FOCUSED on, as the pointer model
answers it rather than as the geometry under the pointer does: a grab holds
focus off its own surface, so a client that set a resize cursor before a press
keeps it for the whole drag. Any change of focused client drops the cursor,
because `wl_pointer.leave` makes it undefined — a client sets one on every
enter, and keeping the last one would show a departed client's cursor over the
bar and the gaps. That drop is taken wherever focus is RECOMPUTED and not only
on a pointer report, because the changes that strand a cursor are exactly the
ones that arrive without one — a window closing under a stationary pointer, a
workspace switch, a client disconnecting. One slot serves the whole scene for
that reason: remembering what every client that ever held focus last asked for
would be unbounded state nothing reads.

What a cursor SURFACE contains is separate state from which surface is being
pointed with, and only the second is the cursor's. A surface's contents are
the surface's: they are retained per surface, survive the client pointing with
something else, and are there again when it points back. That is what a client
switching between pre-rendered cursor surfaces relies on — `set_cursor` alone,
with no commit behind it, is a legal way to change cursors, and a compositor
that dropped the contents on the way out would draw nothing for it. Committing
to a surface nobody is pointing with is likewise retained rather than ignored;
it just owes no repaint. Conflating the two was this increment's own bug: the
selection is undefined after a leave, the contents are not.

Those contents go when the SURFACE does. Destroying it takes them, as
destroying a toplevel takes its tile's pixels — what td holds is a COPY, so
nothing dangles, but a copy of a surface that no longer exists is not what the
operator should be pointing with — and so does the client's departure, which
the tiling sweep would otherwise miss because cursor surfaces are not tiles. A
null attach takes them too and leaves the surface named and still aimed, so a
later commit is adopted without another `set_cursor`. None of the three is
read as "hide": a client asking for no cursor says so with a null SURFACE,
which cannot be confused with one between two frames of an animated cursor.

A WHEEL reaches clients as `wl_pointer.axis`. `REL_WHEEL` and `REL_HWHEEL`
accumulate into the same evdev frame the deltas and buttons do, so one report
is one delivery and one `wl_pointer.frame`: a notch is not a path of its own.
The high-resolution codes (`REL_WHEEL_HI_RES` and its horizontal twin) are
deliberately NOT read — they report the same motion again in units of 120, and
a device sending both would scroll twice.

Two conversions happen once, in `PointerScroll::steps`, and both are the kind
nothing observable would catch. The vertical SIGN flips: evdev counts a wheel
pushed away from the operator as positive, while the protocol's value is a
movement of the surface's own content, where positive is downward. Horizontal
agrees in both, so it is carried through — which is why this is not one
negation over a pair. The SCALE is ten surface units a detent, weston's, and a
choice rather than a conversion: the protocol gives a wheel no unit. Both
numbers come back together as an `AxisStep`, because `axis_discrete` must
agree in SIGN with the value beside it and a pair passed separately would not
enforce that.

A notch goes to the pointer FOCUS, as a button does, so a wheel turned during
a drag belongs to the surface being dragged rather than to whatever the cursor
has moved over. A notch over nothing is DROPPED rather than queued: there is
no surface to owe it to, and delivering it with the next enter would scroll a
window the operator never scrolled over. A modal overlay swallows it outright
where it lets a RELEASE through — a release is owed to a client already
holding the button, and a notch is a whole gesture rather than half of one.

Version 5 and newer also receive `axis_source` naming the wheel and
`axis_discrete` carrying the notch count. Version 4 and below receive the axis
alone, those two events not existing for them. The source is named rather than
omitted because the protocol's silence means "unknown", which would be false
here: a client uses it to decide whether to kinetic-scroll, and a wheel that
claimed to be a finger would coast.

The two are scoped DIFFERENTLY, and that difference is the whole of how they
are emitted. `axis_source` "carries the source information for all events
within that frame" and only one is permitted per frame, so it is the FRAME's:
the first axis in a frame carries it and a second — which a tilting wheel
produces, reporting both axes at once — carries none. `axis_discrete` is the
AXIS's, one per axis, each immediately before the `axis` it belongs to. So the
wire for a tilting wheel is one source, then a discrete-and-axis pair per axis,
then one frame. Deciding the source per axis event instead is a spec violation
on a path this compositor explicitly serves, and it is why the encoder is told
whether this is the frame's first axis rather than working it out: it sees one
event, and the question is about the frame.

`axis_discrete` is DEPRECATED at `wl_pointer` version 8, which gets
`axis_value120` instead, so it is gated at both ends rather than from below.
That upper bound is unreachable while `wl_seat` is advertised at 7 — and is
pinned precisely because nothing else relates those two numbers. `SEAT_VERSION`
exists so the relation is checkable, and a test holds it inside the bound, so
raising the seat reds rather than silently sending a version-8 client an event
the protocol forbids it.

Two silence tests had to learn about the wheel, and each would have swallowed
every ordinary scroll on its own: a notch moves the pointer nowhere and
presses nothing, so both the evdev frame and the delivery path would have read
a wheel-only report as a report about nothing.

Detents are CLAMPED before they are scaled, because the wheel is the first
input to reach `wl_fixed` unbounded. A delta is clamped to the framebuffer
before it is encoded and enter/motion coordinates are surface-local, but
detents are summed straight off the device — and the reader's saturating sum
lands a wheel-spamming device on exactly `i32::MAX`, the one value 24.8 cannot
carry. The bound is an ENCODING one derived from `AXIS_STEP` rather than a
physical guess, and it is a clamp rather than an error because the error would
propagate out of the seat worker: one malformed report would end a client's
whole event delivery rather than one absurd scroll.

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
single-client tile. Cursor-role pixels are outside that total and carry their
own PER-CLIENT ceiling of 1 MiB, which is four cursors at the maximum
dimension below or an ordinary animated set hundreds of frames deep. Per
client rather than one shared ledger, for the reason the tile path is: a
single first-come total lets one client that pointed with a few full-size
cursors deny every other client a cursor for as long as it stays connected,
and the denial is silent — the others simply show td's cross. A framebuffer's
stride-padded shadow allocation has a
separate 64 MiB ceiling. Framebuffer and buffer dimensions share a
16,384-pixel ceiling. At four bytes per pixel the area ceiling is 8,388,608
pixels: tight 3840x2160 is accepted, while 4096x2160 is rejected. A CURSOR is
bounded separately and far more tightly, at 256 pixels on a side: themes stop
there, so nothing an operator would see is refused, and what it bounds is a
client naming a surface of output size as its cursor — which the protocol
permits and which would cost the scene a second framebuffer for an image a few
dozen pixels of are ever on screen at once. An image over that bound is
REFUSED rather than clipped, since a clip would draw part of an image whose
hotspot was computed for the whole of it, putting the operator's point
somewhere other than where they are pointing; td's own cross stands instead.
A refusal DISCARDS whatever that surface held before, whether it was refused
for its size or for the client's ceiling: the surface's contents are now the
image just committed, so keeping the previous frame would paint one the client
has replaced — and, since the buffer is released either way, would freeze an
animated cursor on a frame while the client believed every one of them took.
The cross says something is wrong; a stale frame says nothing. The dimension
check is applied BEFORE the buffer is copied rather than on what is retained,
or the bound would limit what td holds while every connected client could
still make it materialise an image of full output size at once. Its buffer is
released either way, because a client left waiting on a buffer it is entitled
to reuse stops drawing altogether — a worse failure than the cursor it asked
for not appearing. The complete
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
- ioctl(2), for the four pinned terminal-control requests in section 12 and
  the two pinned `EVIOCGABS` requests that read an absolute pointer's
  declared axis range.

No framebuffer, socket, allocation, process, or filesystem operation passes
through that surface, and no input REPORT: every evdev record td acts on is
read as bytes off an ordinary `File`. One POSITION does, and it is stated
rather than glossed, because a resync is the one moment no record can answer
for: `EVIOCGABS` returns `value` beside the bounds, and the frame published
after a dropped batch carries it. That is a cursor move — and, through
focus-follows-mouse, possibly a keyboard focus change — sourced from an
ioctl rather than from a file. It is the only one, it happens only at a
recovery, and §2 is where what it means is argued.

The crate denies unsafe globally; confinement
tests pin the allow count, assembly body, syscall numbers, callers, and the
absence of unsafe from every other target source file. Each developer tool is
a separate crate root that also denies unsafe. Adding a syscall or another
scoped allow amends this document and the repository-wide unsafe inventory.

The three surfaces behind that one body are disjoint and are pinned to
disjoint modules: descriptor transport is reachable only from `client.rs`,
`conn.rs`, and `server.rs`, terminal control only from `pty.rs`, the
absolute-axis range only from `input.rs`, and no other module
names `sys` at all. The extracted connection is crate-visible, so a module
holding one reaches the transport without spelling `sys`: who may NAME `conn`
is therefore pinned by the same confinement test as who may call the
wrappers. That roster is `client.rs`, `conn.rs`, and `term_client.rs` — the
two clients and the transport itself. A transport user is not thereby a
syscall caller: `term_client.rs` names no `sys` and does not appear above.

`ioctl(2)` is the request-carrying one, so its roster is the
confinement: a request outside the six is refused before the syscall, and a
test pins each value, the single guard, the single entry point, and each
wrapper's operand shape. Two of those values also pin a LENGTH. The size
field of an evdev request number encodes `sizeof(struct input_absinfo)`, so
`EVIOCGABS`'s 0x8018 prefix and the crate's own 24-byte buffer are two
statements of one fact, and a test holds them to each other — the kernel copies
the smaller of that size field and the struct's own size, so an oversized
number is harmless while a buffer shortened without the number is an
out-of-bounds write.

## 5. Boot and recovery

This section records the boot profile. The td-term cutover of sections 12 and
14 has landed: it replaced the demo SERVICE and the oracle's marker, and the
devpts setup is in the early mount sequence. It did NOT remove the demo's
final-image symlink, as this section used to say it would — the launcher
spawns the demo by absolute path, so the name outlived the service. The
compositor ordering, readiness, restart, and serial-recovery guarantees remain
in force.

PID 1 still mounts devtmpfs, procfs, sysfs, tmpfs, and the immutable root.
`td-svc` starts `td-seatd` after root checking, then starts
`td-compositor` and `td-term`, in that order, through td-login's credential
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
requires that marker and the first client's later `TD-TERM-READY` marker.

## 6. Required proof

These are the compositor and client proofs. The td-term proof of section 14
has landed and superseded the demo-specific `TD-UI-CLIENT-READY` requirement;
the entry-point and image-roster requirements stand, the demo being a launcher
entry still. Where a bullet below says "the boot client" it now means td-term,
except the pointer clause. The terminal takes a pointer now, for its wheel,
and consumes every framed event on it — but that is proved by HOST tests over
`Surface::dispatch`, not by the boot oracle, which delivers the boot client no
pointer input. So the clause stays the demo's and stays proved from the
launcher; moving it would mean claiming a boot proof that does not run.
Nothing about the terminal REQUIRES the capability either: it binds a pointer
only where a seat offers one.

The landing must prove:

- the kernel pins fbdev, virtio-gpu, PS/2, virtio-input, and evdev built in;
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
  collapse, fullscreen transition, and Super chord is a deterministic host
  test;
- a group's two presentations are proved against each OTHER over one tree,
  which is what makes them a presentation rather than an arrangement: the
  stack's bands run down and cost a band per leaf, the tabs' divide one strip
  across and cost one band whatever the count, and switching between them
  moves no leaf and changes no key order; the pair of directions that walks a
  run swaps with them while the other pair leaves the group WHOLE, so `Up` in
  a tabbed column does not step to the tab the tree happens to put above it;
  and an overlong title is clipped to its own tab rather than into the name
  beside it;
- a band's presentation buttons are proved where they are, where they are
  NOT, and in the order they take a press: the three slots are adjacent and
  flush to the band's right edge with room left for a name, each answers
  over its own slot while the strip's left stays a drag handle, exactly one
  is lit and it follows the presentation, a long title stops before them
  rather than running under them, and neither a tab too narrow for them nor
  a band too short to draw one carries any — both pinned at the threshold
  and one pixel under it, and the short case as the implication that
  matters, that a band which ANSWERS has all three icons to draw. The FIRST
  window's band carries them, which is the case that used to carry none: the
  hit test is driven over a slot on that band, the lit mark is proved to
  follow, and the choice is proved to reach the container the second window
  opens into. That press is also held to moving no pixel OFF the three
  slots, frame against frame and for BOTH grouped presentations — the
  lone-leaf case is served by one line, and a line can be written for one of
  the two. The claim it carries is about pixels, which is where a run would
  have been felt: the border wrapping the window's own title bar. The
  hand-over is pinned in both directions, neither the workspace keeping a
  copy nor an empty one overwriting the container a window arrives into, and
  a workspace emptied of a grouped lone window hands its grouping to neither
  the next window nor the one after. The stack icon is counted as three
  separate runs of ink at EVERY height a band carries buttons at rather than
  at a sample of them — the reachable set is the gate's threshold up to a
  full band, and dividing by a constant other than the gate's own draws
  three equal bars in the middle of it that a sample misses. Two marks
  merged would colour the same rows and read as one thicker mark, so the
  last run is held thicker than the two above it, with ink on the icon's own
  first and last row and none outside it: a hamburger, an icon sitting short
  inside its rectangle, and one that overran onto its neighbour each pass
  the count alone. The press is driven through the runtime, where it both
  presents the container and takes focus to the band, and leaves NO drag
  live — verified red with the interception removed and again with the press
  allowed to fall through to the band — while an ALT press over the same
  pixels picks the window up instead and runs no command, which is the one
  contract that rested on a single token no test covered;
- a tile's five drop zones are proved as a function — each zone by a point,
  the middle ninth by the points just outside it, the nearest edge by a wide
  short tile where pixels and proportion disagree, and a sweep of the whole
  rect reaching all five — then again through the scene, where a band answers
  two along its run — across for a row, down for a column or a stack, and
  across its own share of the strip for a TAB, where a reading taken the other
  way would answer the same for every point in it and leave half of each tab
  unreachable — and a grouped leaf answers for its shared content rect only
  while it is the one shown; a row of two proves the run's direction is read
  from the tree the drop LANDS in rather than the one it aims at, where that
  row has collapsed, and a whole gesture over a stacked pair proves the same
  seam end to end, aimed where the two geometries give opposite halves; and
  the layout's side proves that a swap trades two windows without moving a
  tile or ungrouping the container it lands in, that it is its own inverse,
  that a band drop keeps its container's axis whichever that is, that a drop
  across the axis MAKES the column a row did not have, and that a group takes
  that same drop into its run instead while the ungrouped column still builds
  the row;
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
  order, while pre-registration events and intercepted Super chords do not;
- keyboard model tests cover held-key snapshots, cross-client focus, modifier
  locks, key releases, registration cutoffs, and queue saturation;
- pointer model tests cover enter, leave, motion, cross-client routing,
  duplicate buttons, mid-frame re-grabs, implicit grabs, surface removal,
  workspace cancellation, snapshots, and revision exhaustion;
- every painted help row is driven through the dispatch, both columns
  derived from the result, with the row count pinned; the mouse rows name an
  EFFECT rather than words of their own, though nothing there ties a GESTURE
  to the effect it claims — a keyboard row derives its effect from the
  dispatch and a mouse row has none to derive from, so what each gesture
  does is proved in the runtime instead; the sheet's own tests cover
  toggling, column and card fit, and clipping on an undersized output, and
  runtime tests cover its modal pointer — hover withdrawal and the button
  filter separately, since either alone hides the other;
- the status bar's `/proc` parsing is driven from fixture roots rather than
  the host's own `/proc`, its calendar is walked day by day across a leap
  year, its strip is proved to paint only its own rows and to clip on an
  output shorter than itself, the layout/render/hit-test agreement is
  checked at several output sizes with the bar's rows proved unclickable,
  and the runtime is held to repainting only on a changed line and to
  restoring the previous one when that paint fails; the workspace strip is
  proved as its own list — every occupied workspace plus the active one,
  named once and in order, with an EMPTY active one still named — and as
  pixels: the active cell is the same cell's ink and background COUNTS
  exchanged, the status line starts after the cells and leaves their pixels
  exactly as no status line would, and the mark is driven through the scene
  across two switches to prove it follows the layout rather than a constant;
- the focus policy is proved end to end through the runtime in both its
  halves — a hover focuses, and does so over a band and over a tile its
  client does not fill; a still pointer, a delta clamped away at the edge,
  a modal overlay, a live drag and a held grab each leave focus alone, the
  grab whether or not its last release shares a report with the motion;
  fullscreen and an empty workspace refuse a hover that would reach past
  them; sweeping a stack's bands shows each leaf they name; and a press
  focuses where it lands with the pointer still, not over the gap, not
  mid-drag, and where the press landed when a release and a press share one
  report — with the pointer model's press report and `Layout::focus_key`'s
  refusals tested on their own, each positive case carrying a control that
  the same gesture DOES focus, and a failed focus paint proved not to
  swallow the press in its own report;
- scene tests prove pointer hit testing excludes gaps and clipped-away
  pixels while retaining local coordinates for an implicit grab, and server
  tests pin per-region and aggregate retained-operation ceilings;
- evdev tests prove relative motion and logical multi-device buttons flush
  only at `SYN_REPORT`, while EOF, `SYN_DROPPED`, and an oversized partial
  report cannot strand a delivered button;
- an absolute device reaches the target as a PLACE where a relative one
  reaches it as a distance, a place beats a delta within one report, a value
  past the declared range is the edge it went past, each axis is scaled
  against its own range, and an axis a report leaves out is where that device
  last was — including on a FIRST report, where that is the position the
  kernel gave at open, on a frame carrying nothing but a BUTTON, and across
  the reset a dropped batch performs — where a device that can be re-asked is
  placed at where it went during the gap even if nothing follows the recovery,
  and one that cannot be asked keeps what it last said; the scene's half is
  proved separately,
  where the two
  ends of a range land exactly on the first and last pixel of the output, and
  the composed claim is proved over every column of five resolutions passed
  through QEMU's own floor-scaling and required to come back as itself —
  which is the reachable-edge claim itself, and the one that would have gone
  unnoticed had either half been proved alone; the axis position and range
  are proved to be read from the first three of `input_absinfo`'s six words,
  three adjacent `__s32`s an index cannot distinguish at runtime;
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
  nothing, a one-pixel pointer step writes td's own cursor's 13 rows, a diagonal
  step writes 14 — both figures are the FALLBACK cross's, and a client cursor
  is as tall as its image up to the maximum dimension, so the band is a
  property of what is being pointed with rather than of the compositor —
  a full-size output writes under 2% of its image, a banded sequence
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
focus, move, split, fullscreen, presentation, and workspace operations are all
deterministic
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
tests route parsed launcher chords and the complete accepted character set
through a recording target, while runtime tests prove overlay repaints do not
mutate the tiling tree. Process policy turns a launch request and explicit
paths into literal argv, with its active-child ceiling tested independently.
Every overlay pixel is clipped to the computed card rectangle, including on
an output too short for its normal layout. No launcher model or renderer reads
input devices, sockets, clocks, the filesystem, or ambient environment.

## 8. Deferred UI stack

Focused keyboard and pointer delivery now connect the demo client to the
existing evdev input path, and the launcher has a filterable
application registry with a terminal entry, and the terminal is the first
client the BOOT starts, in place of the demo. Pointer axes are landed on both
sides: a wheel reaches clients as `wl_pointer.axis`, with the source and notch
count version 5 asks for, and the terminal binds a `wl_pointer` and scrolls
its scrollback with one. Client cursor rendering is landed on the COMPOSITOR
side: a client's own image is drawn where its hotspot says, a null surface
hides the pointer outright, and td's cross is what is left when no focused
client has asked for either. No td client sets one yet, so the booted machine
still shows the cross everywhere — the protocol half is what a toolkit needs
and the asking half is a client's own increment. Clipboard, hotplug, and real
DRM/KMS profiles follow. The terminal stack has the separate contract below.

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
Its caller is the Wayland client of section 12, which submits each rendered
frame to its surface, so the frame-callback coalescing, the persistent-buffer
reuse-after-release, and the buffer replacement on resize that section 11 also
specifies are landed with it.

Section 12's PTY writer thread and child waiter are landed with the bounded
keyboard queue they share with the main loop. Section 12's devpts instance is
landed: the image mounts it at sysinit
through a `td-init` applet, pins `CONFIG_UNIX98_PTYS=y`, and re-proves the
mount options, the `/dev/ptmx` symlink and the instance `ptmx` on the booted
machine. Section 12's readiness socket, `TD-TERM-READY` marker, and `probe`
subcommand are landed, along with the `td-term` name itself: one artifact
serves three programs, chosen by argv[0], and the store output carries the
terminal as a symlink beside the compositor. The `/bin/td-term` name §12 spells
and the `ready=` line that calls it are packaging, and are landed. The
publisher has its caller: deciding a terminal IS ready belongs to the Wayland
client, which publishes after `present` has drawn a frame at a size the
compositor CHOSE and taken both the buffer release and the first frame
callback, and then once everything that can still fail has — handshake
finished, reader detached, child started — and before its main loop, so a
probe is never told something true for less than a second. That is strictly
more than the demo's marker proved, which is why the boot oracle could move
to it. That client is landed and is the PTY adapter's production caller; its
host tests still drive
every operation against a real PTY, and the packaged binary's selftest covers
the policy layer, which is what runs where devpts is not mounted. The terminfo
entry and the `/bin/td-term` symlink are landed, the launcher can open a
terminal, and the boot cutover is landed: the `[terminal]` unit replaced
`[ui-demo]`, so the machine comes up on a shell prompt and the boot oracle's
first-client marker is the terminal's. The demo stays packaged as a launcher
entry rather than as a service.
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
cutover KEPT its final-image symlink, superseding the earlier plan to remove
it: the launcher registry spawns the demo by absolute path, so the name is
reachable from the screen rather than only from a recipe. What the cutover
removed is its SERVICE — nothing starts it at boot.

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
Wayland compositor is outside the first profile. It binds the seat at
version 5 through 7, as the demo does, because repeat_info is a version-4
event and the timings below are read out of it rather than restated; a lower
bind would make this section unimplementable and a higher one arrives at a
client whose dispatch treats an unknown keyboard opcode as fatal. The
terminal translates the fixed evdev key codes and standard XKB modifier
masks itself; it does not import libxkbcommon. A pinned in-tree table marks
text and navigation keys repeatable and modifiers non-repeating, mirroring
the exact keymap's repeat exclusions. Validation uses positioned
`FileExt::read_at` calls so reading one SCM_RIGHTS duplicate cannot advance
the shared open-file-description offset seen by a restarted or second
client.

The input adapter covers text keys, Enter, Tab, Backspace, Escape, arrows,
Home, End, PageUp, PageDown, Insert, Delete, and F1 through F12. It selects
normal or application sequences from explicit terminal modes. Ctrl produces
the specified ASCII C0 bytes, Alt prefixes the resulting sequence with ESC,
and Shift selects the defined text or navigation variant; unlisted modifier
combinations produce no bytes. td-term routes that adapter: the keyboard's
modifiers event folds depressed, latched and locked into the one mask the
adapter reads, and a pressed key becomes bytes in the queue the PTY writer
drains. Releases send nothing, and a non-zero keymap group sends nothing
either — the pinned map has one group, so reading another against group 0's
table would send a different key's bytes rather than none. The terminal mode
that picks between two spellings is refreshed from the model before each
event, because a child's reply to one key can change how the next is
spelled. Key REPEAT is wired, and its timings are the compositor's rather
than the client's: `repeat_info` publishes a rate in keys per second and a
delay in milliseconds, and a rate of zero is the protocol's "do not repeat"
rather than an infinite interval. A held key is the only thing that makes
the main loop time-sensitive, so with none armed it blocks on its channel
exactly as before, and with one armed it waits no longer than that key's
next repetition rather than polling. A repetition is re-routed per tick
rather than replayed, so a child that changes DECCKM under a held cursor key
gets the new spelling; a release, any modifier change, a keymap group
change, losing focus, and a republished rate of zero each retire the held
key, because the sequence armed under the old state is no longer the one it
would send — and a key pressed while the group is not td's arms nothing at
all, for the reason a single such press sends nothing. A later `repeat_info`
with a different nonzero rate is not a retirement: it retimes the held key
rather than dropping it. The scrollback VIEWPORT is wired too.
`Shift+PageUp` and `Shift+PageDown` move the view rather than reaching the
child, and a key that scrolls never also sends bytes. The view names the
LINE it is looking at rather than a distance from the live bottom, so a
child writing underneath an open viewport does not drag it along; the anchor
is clamped against what history holds on every read, so eviction and resize
leave it riding the top of what remains rather than snapping back. A clear
retires the numbering, which is what stops an old anchor reopening a closed
view on unrelated lines. End has two meanings and the effective position
decides which: with the view open it returns to the live bottom, and at the
bottom it is the child's key. A held scroll repeats, since walking back
through history is what holding it is for. While the view is open the
cursor is drawn where the shift puts it, and stops being drawn once that pushes it
past the bottom: the renderer shifts the live screen and the cursor by
the same offset, so neither needs a special case.

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
for, so without that rule `Super+q` would type `q`. The rule belongs to the
PROFILE rather than to td's compositor, and so does not shrink as that
compositor's table grows: it holds for `Super+Enter` and `Super+Up`, which td
keeps for itself, under any other compositor that forwards them.
Shift, Caps Lock, Control, and Alt
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

A WHEEL moves the same viewport three lines a detent, and reaches it by a
second route rather than through `Action`: that enum is what one KEY PRESS
does, and a notch arriving as one would be a third thing a key could mean. It
is three rather than a page because a wheel is turned in flicks — a page a
notch overshoots, and a line a notch makes crossing a screenful a dozen
turns. The count comes from `axis_discrete`, which is the event that carries
it; deriving it from the axis VALUE would need the compositor's own
units-per-detent, a number no client is given. An `axis` event arriving with
no discrete beside it is therefore IGNORED rather than read as one notch: the
terminal requires its seat at version 5 or above, where a wheel always carries
its count, so the only source that sends a bare axis is a smooth-scrolling one
— and each of its many small events becoming a whole notch would make a
trackpad scroll uncontrollably.

A `wl_pointer.frame` is what APPLIES the accumulated notches, not each axis
event: the frame is the transaction, so a tilting wheel — which reports both
axes at once — moves the view once and repaints once for one flick. The
horizontal axis is read and discarded rather than assumed absent, since a
terminal has no sideways scrollback and counting the two together would send
a sideways flick up the history.

The pointer takes a DYNAMIC object id rather than a fixed one, and that is
forced by the capability gate rather than chosen. Ids are per-client and must
be DENSE. A fixed id reserved for the pointer is skipped on a keyboard-only
seat, where the object is never created — precisely the gap a compliant
compositor disconnects for, and one nothing in td would report, since td's own
server checks only uniqueness. A dynamic id is dense either way, being handed
out when the object is actually asked for.

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
The client calls them: td-term routes every press through the adapter and
holds the viewport across frames, so the corpus is no longer the only thing
driving either.

## 12. PTY and process lifecycle

After mounting devtmpfs and before graphical services, the system creates
`/dev/pts`, mounts devpts there with
`newinstance,ptmxmode=0666,mode=0620,gid=5`, removes devtmpfs's existing
`/dev/ptmx` node, and creates the relative `ptmx -> pts/ptmx` symlink. The
image pins `CONFIG_UNIX98_PTYS=y` and its existing `tty` group owns gid 5.
td-term opens `/dev/ptmx` with safe `std` file operations, unlocks it, and
obtains the slave as an owned descriptor with `TIOCGPTPEER` and
`O_RDWR | O_NOCTTY | O_CLOEXEC`. No `/dev/pts/N` path is reopened.
The image proof pins the startup mount command and re-checks `mode`, `gid`
and `ptmxmode` out of `/proc/mounts` on the booted machine, in the kernel's
own `%03o` spelling rather than the mount's -- the `mode=0620` asked for
comes back as `mode=620`, so a check written to match what was passed would
red every correct boot, and rootcheck is a gate; it does not require
`/proc/mounts` to echo the modern kernel's accepted no-op `newinstance`
token. The effective SLAVE gid and mode are proven by opening one, which
lands with the client that opens the first pty.

That sequence is one `td-init` applet rather than four sysinit lines. Three
of the four would otherwise be uutils `mkdir`, `rm` and `ln` reached at
absolute paths, with nothing tying them to the boot that needs them. It
composes the mount as the argv the `mount` applet parses rather than calling
`mount(2)` itself, so flag composition stays in the one module td-init's
confinement tests allow it in -- and this mount needs no `MS_*` bit at all,
since every option it passes is filesystem data. It adds no syscall, so it
is not an amendment to `UNSAFE.md`.

It reads its own mount back out of `/proc/mounts` before relinking
`/dev/ptmx`, which is why the sysinit line comes after `/proc` rather than
beside the devtmpfs mount: an option devpts does not know makes the mount
fail outright, so what a readback catches is a known option that took a
DIFFERENT value than the one asked for, and nothing distinguishes that until
a pty is opened. Each option is matched as a whole comma-separated token, so
`mode=620` cannot be satisfied by `ptmxmode=620`, and the expected spellings
are derived from the ones passed rather than restated beside them. The
instance `ptmx` is checked too -- character device, mode 0666 -- since it is
mode 0000 on a mount that dropped `ptmxmode`. Relinking requires a value only
that verification returns, so the order is the compiler's to enforce, and it
is a rename rather than an unlink and a create, so a failure cannot leave the
machine with no `/dev/ptmx` at all. A second run is refused rather than
served: devpts stacks, and an instance mounted over a live one hides every
pty the first is serving while every check still reads healthy.

The symlink is the setup the kernel's own devpts documentation describes.
It is not that a `/dev/ptmx` device node would allocate from the initial
instance -- modern kernels resolve a `pts` directory beside the node and use
that mount -- but that the link makes this instance the answer explicitly
rather than resting on a sibling-directory lookup nothing checks. `mode=0620`
is likewise the tty convention rather than a relaxation: owner read/write and
tty group WRITE, which is how anything reaches a terminal it does not own,
where the devpts default would be 0600 owned by group root.

Stable Rust does not expose the required PTY operations. The widening adds
x86-64 `SYS_IOCTL=16` to the existing raw body. Four of the entry point's
six permitted request values are this section's; the other two are §2's
`EVIOCGABS` pair, which no module here may name:

- `TIOCSPTLCK=0x40045431`, to unlock the slave;
- `TIOCGPTPEER=0x5441`, to obtain the slave as a new owned descriptor;
- `TIOCSWINSZ=0x5414`, to publish rows and columns; and
- `TIOCGWINSZ=0x5413`, to verify every published size before it becomes
  visible to the child.

The confinement tests pin the `SYS_` constant count, raw-body call count,
request values, and callers. This setter applies only to td-term's newly
created PTY; it does not weaken the separate repository prohibition on
resizing an operator's terminal. A request outside the roster is refused by
the one `ioctl` entry point before the syscall is issued, so a mistyped or
newly invented number cannot reach the kernel without amending both the
roster and the test that pins it.

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

All SCM_RIGHTS operations for td-term remain inside the client transport
boundary, including keymap receipt and wl_shm submission. That boundary is
now `conn.rs` as well as `client.rs`: the connection — its id allocation,
message framing, and descriptor queue — was extracted so the terminal is a
second user of one transport rather than a second copy of it, and the
descriptor queue could not stay behind. Section 4 records the same boundary.
`term_client.rs` is the terminal's own client and is on the transport-user
roster; the terminal's parser, model, renderer, keyboard, and PTY policy
modules are not, may not name the transport, and do not call the
descriptor-transport wrappers.

When the compositor declines to choose a size, the terminal falls back to a
grid rather than to a rectangle: 80 columns by 24 rows, multiplied out by the
pinned font's cell, since that is what a terminfo entry and anything drawing a
box assume when they cannot ask. Each axis declines independently. Its fixed
object ids run densely to one past the last it creates, which is one lower
than the demo's: it binds a seat and creates a keyboard, and its `wl_pointer`
is a DYNAMIC object rather than a fixed one because a seat may not offer the
capability. The density is what matters rather than the number — libwayland's
object map refuses an insert past the end of its array, so a client that
skipped an id it never created could be disconnected by a compliant
compositor.

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

The writer differs, but less than it first appears: it parks in a
condition-variable wait rather than in a syscall, so closing the keyboard queue
retires it and its handle IS joinable — for a writer that is waiting for bytes.
Closing sets the predicate the writer checks BETWEEN writes; it does not
interrupt one, and nothing safe cancels a blocking write. A child that never
reads does not by itself park the writer: in the kernel's default canonical mode
the line discipline accepts and discards rather than blocking, and the tests
cover that case against a live child that reads nothing. A child in RAW mode
that stops reading is the case that parks it, and that is every shell and
editor. The child's exit does not free such a writer either — the last slave
closing hangs up the reader, which is the reader's whole retirement, while the
writer stays parked in `write` on the same terminal at the same instant. So the
teardown rule is the reader's rule: td-term ends a terminal by exiting the
process, not by joining either thread, and joining the writer is for a writer
known to be idle. Because the writer can therefore die unobserved, its failure
is recorded where the main loop meets it: a `push` after the writer is gone is
an error rather than the bell §10 rings for a full queue, since a terminal
beeping at every keystroke would be reporting the wrong thing forever. One
bounded queue serves both ends — the
main loop admits a sequence whole or drops it whole and rings the bell, and the
writer drains it —
because a second buffer downstream would be a second place for half a sequence
to sit. Bytes are copied out under the lock and written without it, so a child
that has stopped reading parks the writer in `write` without ever delaying an
enqueue. Only the writer consumes, so a partial write's remainder stays at the
front in order however much arrived meanwhile.

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
`wl_buffer.release` and its frame callback AND its seat still offers a
keyboard AND its keymap is verified. The keyboard half is a precondition
rather than a parallel errand: a terminal that started its shell before
knowing what a key MEANS would take its first keystrokes against no map at
all, so the same wait that makes readiness a frame the compositor chose
makes it a frame it can be typed at. The seat is asked for its LATEST
capability rather than the one that prompted the keymap request, since a
seat may withdraw what it announced; td's own server announces keyboard and
pointer once at bind and withdraws neither, so this bounds another
compositor rather than describing a state this one reaches. It is a startup
gate only — a capability withdrawn after readiness is not noticed, and
noticing it needs somewhere to put a terminal that has lost its keyboard.
Reaching that buffer takes TWO frames, and that is the protocol rather than
a retry: the compositor cannot tile a surface it has not mapped, so its
first configure is zero in both axes, presenting at the client's own
fallback is what maps the surface, and the tile arrives in the configure
that follows. Readiness is therefore a frame drawn at a size the compositor
CHOSE, and choosing is per axis — zero is a declined axis, and a configure
choosing one axis has chosen. An `ack_configure` takes effect on the surface
commit that follows it, so a configure needing no new frame is applied with
a bare `wl_surface.commit` rather than left acknowledged and unapplied; a
chosen tile equal to the client's fallback is exactly that case. One encoder
produces both the diagnostic and the socket's answer, since the integration
test compares them and two spellings could drift while each stayed
plausible. A readiness line is parsed fail-closed and order-pinned, and its
grid is held to the same definition the winsize ioctl is: a line describing
a grid no terminal could have been set to is not readiness. The terminal
refuses to publish a grid its own probe would reject. td-svc's `ready=`
command uses the existing credential-switch pattern to invoke `/bin/td-term
probe /run/user/1000/td-term-ready` as the graphical user. The probe
requires a ready state and nonzero internally consistent rows and columns;
its output and the matching `TD-TERM-READY` QEMU diagnostic are compared in
integration tests. The boot profile has atomically replaced the visible
`td-ui-demo` service with a `[terminal]` one; the demo's final-image symlink
stays, because the launcher spawns it. The compositor and serial recovery
greeter remain independently restartable.

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
