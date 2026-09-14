# td-taskmgr

td-taskmgr is a graphical Linux task manager built with td-ui. Resource
history occupies the upper pane; a parent/child process tree remains in the
lower pane across graph tabs. CPU and memory plots connect a resource spike
to the processes observed at that time. Process actions send signals under
the caller's existing authority.

This is the version-1 target contract, not an implementation claim. There
is no td-taskmgr crate or executable yet. Root AGENTS.md and DEVELOPMENT.md
govern implementation and landing; [td-ui](../td-ui/DESIGN.md) owns the
shared widget contracts. Each increment below must update its status and
record its actual validation without claiming later increments are done.

## Scope and portability

Version 1 is one self-contained Rust executable for Linux x86-64 Wayland,
on td and other distributions, including Guix. It uses std and the sibling
td-ui crate, with a standalone Cargo.lock and no registry or git crates.
The font and toolkit are compiled in. It requires no td daemon, systemd,
external commands, libwayland, shell, runtime asset lookup, or fixed store
prefix. A host build uses the host Rust toolchain and declared linker;
a shipped td build follows the source-built target graph. Self-contained
does not promise that a dynamically linked host build works with another
distribution's dynamic loader: host packaging may build for its own libc.

The initial host build command, once the crate exists, is:

```text
cargo build --offline --release --manifest-path td-taskmgr/Cargo.toml
```

Build from a checkout containing td-ui and its declared shared source and
font trees. The installed executable does not need that checkout. The
application reads WAYLAND_SOCKET, WAYLAND_DISPLAY and XDG_RUNTIME_DIR and
passes their values to td-ui's endpoint resolver; td-ui reads no environment.
The user runs the program
as their desktop identity. Missing display support produces a useful startup
diagnostic; unavailable metrics leave a usable window with field-level
diagnostics. --help and --font-license work without a display.

There is no X11 backend or ARM deliverable in version 1. Keep architecture
details in audited Linux adapters: no x86 syscall numbers, register layout,
page-size assumption, native-endian record encoding, or tick-frequency
constant in parsers, metrics, widgets, or application state. Pass runtime
page size and clock tick frequency explicitly into metric conversion. A
future ARM port should replace adapters and extend td-ui's platform support
without redesigning the application model.

Version 1 excludes elevation, remote monitoring, per-process network and
disk attribution, application/service grouping, thread rows, priority and
affinity editing, persistent recordings, and a profiler integration.

## Window and navigation

The window title is Task Manager and the application ID is td-taskmgr.
It uses td-ui's palette, font, input handling, and software draw stream.

```text
+------------------------------------------------------------------+
| Overview | CPU | Memory | Network | Disk                          |
| CPU history                     Memory history                    |
| Network receive/send            Disk read/write                   |
| Live / inspected time           contributor legend or device list |
+===================== draggable divider ==========================+
| Search processes...             Process actions                   |
| Process       PID   UID   State   CPU %   RSS   Tree CPU %  Tree RSS |
| v parent                                                          |
|   > child                                                         |
|   > child                                                         |
|                         scrollable process tree                   |
+------------------------------------------------------------------+
| Sample age / coverage / action result                             |
+------------------------------------------------------------------+
```

Overview shows four compact graphs in a two-by-two layout. Dedicated tabs
give one resource more room and expose its legend or device selector.
CPU includes total and per-logical-CPU detail; Memory includes RAM and
swap; Network and Disk show separate directions. These are resource tabs,
so they have no close buttons. Switching tabs preserves process selection,
search, sibling sort, expansions, scroll anchor, and the inspected time.

The divider initially splits usable height equally and is draggable and
keyboard adjustable. Both panes retain useful minimum extents. Narrow
windows reflow Overview and scroll graph content; the lower pane retains
its search, column headings and at least one process row when space permits.
At smaller extents show a resize message without losing state or extending
hit regions outside the surface. The tree has horizontal and vertical
scrolling; rendering visits visible rows rather than the whole tree.

Tab moves focus among tabs, graph, divider, search, tree, and actions.
Arrow keys navigate the focused control. Tree Left collapses or moves to
the parent; Right expands or moves to the first child. Up/Down, Home/End,
and Page Up/Down navigate visible rows. Ctrl+F focuses search; Shift+F10
opens the selected row's context menu, and F10 opens the Process menu.
Escape dismisses the innermost menu
or confirmation before affecting other state. Graph timestamps and
contributors are keyboard selectable, with a visible focus indicator and
text values so color and precise pointer positioning are never required.

## Linked history and process tree

Collection continues while inspecting history. Live follows the newest
sample; clicking a historical time pins an immutable snapshot and shows its
age prominently. The lower pane remains the same tree widget but displays
the processes and parent relationships recorded in that snapshot. A Live
control explicitly returns to current observations. Historical rows never
offer process actions, even if an apparently matching PID still exists.

CPU and Memory have system line plots and separate stacked-area
process-contribution plots. Process series use stable colors and explicit
names; selecting a
series reveals and selects its process row, expanding recorded ancestors.
Search cannot hide a graph-selected row: reveal it and its ancestors as an
explicit selection exception without discarding the query. Clicking a time
outside a particular series opens a ranked contributor list for that time.
The list uses the same metric and sample as the graph and can reveal any
retained process, including one now exited. Network and Disk clicks inspect
device/interface values at that time and never invent a process selection.

At most eight named process series are drawn together. Choose them by peak
value across the visible history; retain the selected process as one of
the eight and break ties by process key. Keep their vertical order fixed
over that view. Remaining measured processes form Other observed, which
opens the contributor list rather than selecting an arbitrary PID. Missing
observations are gaps, not zeroes. System CPU activity and RAM consumption
are not forced to equal the sum of process observations.

A display key contains the collection-session generation, PID, and kernel
start-time ticks. A PID reused with a different start time is a different
row and series. This key correlates observations; it is not authority to
signal a process. Parent links resolve within each snapshot. Missing,
inaccessible, or inconsistent parents place children beneath an explicit
Parent unavailable root. Reject self-links and break cycles deterministically
with a diagnostic. Walk and aggregate iteratively, with bounded depth.
Synthetic grouping roots can receive navigation focus and expand/collapse,
but have no process identity, action menu or actionable subtree.

Show all visible processes, including other users' processes when readable.
Search matches the displayed command/name, PID, and numeric UID; matching
descendants keep their ancestors as context rows. Search does not change
subtree totals or the scope of a subtree action. Sort only siblings, with a
stable process-key tie-break. Default is PID ascending; clicking a numeric
heading starts descending; a text heading starts ascending, comparing the
displayed text by Unicode scalar value. Repeated clicks on the active sort
heading toggle ascending/descending. Refresh preserves selection and the
first visible row by key. While a pointer gesture or menu is active, freeze row geometry;
actions bind to the captured target, never a later row at the same index.

The initial columns are name, PID, numeric UID, state, own CPU %, own RSS,
subtree CPU %, and subtree RSS. Command-line detail is available on demand
with a bounded read and clear truncation. Escape control bytes and display
invalid UTF-8 safely; process-controlled text never supplies menu syntax,
markup, commands, or authority. An unreadable field is Unavailable, not 0.
An absent row is marked no longer observed; call it exited only when there
is positive exit evidence. Visibility loss alone does not prove exit.

Subtree values sum each observed process once, including the parent.
Collapsed children remain included. Partial coverage is visible on totals;
there is no accounting for unobserved children between samples. No process
action interprets the visual indentation or a substring match as its target.

## Measurements

Sampling defaults to one second; supported choices are 0.5, 1, 2 and 5
seconds. Retain up to 120 seconds and 240 samples, subject to the memory
budget below. These are simultaneous ceilings: at five-second intervals
the time limit retains at most 24 samples, while the byte budget may shorten
history further on a large system. There is no guaranteed 120-second window.
Retain identity strings once in a budgeted shared table, with compact
per-snapshot metric and parent records; charge all referenced identities
until their last snapshot is evicted. Record monotonic collection start/end
and actual elapsed time;
wall-clock labels do not determine rates. A scan is an observation interval,
not an atomic kernel snapshot. Counter regression, reset, a new identity,
missing predecessor, zero elapsed time, or changed device membership starts
a fresh baseline and a gap. Do not interpolate across gaps or catch up with
a burst of scans after a stall. Stale readings display their age.

### CPU

Use /proc/stat aggregate and cpuN counters. Total capacity is the sum of
user, nice, system, idle, iowait, irq, softirq and steal deltas; guest fields
are already included and are not added again. Busy excludes idle, iowait
and steal; expose iowait and steal separately. CPU hotplug or an inconsistent
counter interval resets the affected baseline. The system graph is 0..100%
of observed machine capacity; a per-CPU graph is 0..100% of that CPU.
These fields and iowait's limitations follow
[proc_stat(5)](https://man7.org/linux/man-pages/man5/proc_stat.5.html).

Own CPU is the delta of utime + stime, without waited-for-child counters.
Use ticks per second and elapsed time for a one-logical-CPU basis: a
multithreaded process can exceed 100%. Label that basis on the table and
process plot; subtree CPU uses it too. The system and contribution plots
have distinct axes. Include process guest time only through utime.
Source fields follow
[proc_pid_stat(5)](https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html).

### Memory

The system plot shows MemTotal - MemAvailable and available RAM, with swap
used as SwapTotal - SwapFree on its own scale. If MemAvailable is missing,
report that metric unavailable rather than silently substituting MemFree.
Cache and reclaimable fields may be shown as detail, not additional disjoint
pieces summed with used RAM. Definitions follow
[proc_meminfo(5)](https://man7.org/linux/man-pages/man5/proc_meminfo.5.html).

Process contributions use resident bytes from RSS pages and the runtime
page size. RSS is approximate and shared pages appear in multiple processes.
Label the contribution plot and subtree RSS as summed RSS, including shared
pages; they may exceed physical RAM and are not a decomposition of system
used memory. Version 1 does not continuously walk smaps for proportional
accounting. The RSS limitations are documented in the kernel's
[procfs reference](https://docs.kernel.org/filesystems/proc.html).

### Network

Read interface receive/transmit byte counters from /proc/net/dev, with
sysfs identity/detail where available. Display bytes per second and observed
cumulative counters separately. Select an interface or an explicit sum of
selected interfaces. For the initial default, prefer a non-loopback
interface that is operationally up and has a sysfs device link, then any
operationally up non-loopback interface, then any non-loopback interface.
Break ties by name; fall back to loopback, or show no interfaces. Keep a
user's selection until they change it, including while a device is down.
Never infer unique host traffic by summing every bridge, veth, tunnel, and
physical interface. A multiple-interface selection is labelled Sum of
selected interfaces and may count the same traffic at multiple layers.
Interface names can change; use ifindex when available and reset on
disappearance, replacement, or uncertain continuity. Collection sees the
caller's network namespace and labels that scope. The source choices follow
the kernel's [interface statistics](https://docs.kernel.org/networking/statistics.html).

### Disk

Read /proc/diskstats with /sys/dev/block identity and topology where
available. Plot read and written bytes per second; diskstats sectors are
512-byte accounting units regardless of hardware sector size. Show IOPS
and busy-time percentage as detail; busy time is not a reliable measure of
saturation for every device. These semantics follow the kernel's
[I/O statistics](https://docs.kernel.org/admin-guide/iostats.html).

Select one block device by default: prefer a whole device with a sysfs
device link, excluding loop, RAM and zram devices. Then prefer another
whole device excluding those classes, finally any available device.
Break ties by major/minor number; logical dm/md devices are available but
do not displace a physical device in the first preference class. Offer
partitions and logical devices explicitly, and preserve user selection.
An optional sum lists its members and warns of overlapping layers; do not
silently add partitions to their whole disks or device-mapper devices to
their backing devices. Membership changes reset the summed baseline.
Device removal produces a gap, and reappearance requires a fresh baseline.
If sysfs is unavailable, keep individually named counters and mark topology
unknown. Version 1 measures block activity, not filesystem capacity or
network-filesystem traffic.

## Process controls and authority

Use td-ui's shared menu and submenu implementation. The context menu and
keyboard-opened Process menu expose Selected process and Process and
observed descendants scopes, each with Terminate (SIGTERM), Force kill
(SIGKILL), Suspend (SIGSTOP), Resume (SIGCONT), and Send signal. The latter
lists supported named Linux signals, with numbers resolved in the platform
adapter; queued payloads, thread-directed signals and arbitrary numeric
input are outside version 1. Menus are data passed to td-ui, not an
application-specific renderer or submenu state machine.

Every action uses td-ui's shared confirmation dialog, naming its signal,
scope, current target identity and command, with Cancel initially focused. For a subtree, show
the complete bounded member list and count, including hidden descendants.
New observations do not change a pending request. Key repeat and duplicate
clicks cannot send a request twice. Confirming sends one signal to each
captured process; it does not automatically escalate SIGTERM to SIGKILL.
Successful delivery is reported as sent, not as proof of exit or suspension;
subsequent observations report state. Closing the window does not resume
processes it stopped or undo signals already sent.

Actions use stable kernel process references. Open a process's procfs
directory, read its current identity and confirmation details relative to
that retained directory, and hold it through confirmation and delivery.
Linux permits that descriptor as the target of pidfd_send_signal. Never
reopen a numeric PID at delivery time or fall back to kill(PID). A retained
directory cannot retarget a reused PID, even though it does not prevent PID
reuse. See [pidfd_send_signal(2)](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html)
and the [procfs descriptor contract](https://docs.kernel.org/filesystems/proc.html).

Preparing an action is a fresh observation: compare its display key with
the selected live row, refuse a mismatch, and show the descriptor-bound
details in the confirmation. Start-time ticks alone are not an unforgeable
identity or a promise against same-tick reuse; the confirmation authorizes
the currently pinned instance, not an earlier observation. Historical
inspection must return to Live and prepare a new request before any action.
Reject controls if the procfs mount's PID view cannot be established as the
caller's own, rather than mixing namespaces. Missing syscall support or
denied descriptor access leaves monitoring available and controls disabled
with the concrete reason.

Subtree preparation makes a fresh bounded scan, resolves parent links,
pins every listed member, and refuses inconsistent identities or topology
before presenting the list. This is the observed membership, not an atomic
process-family operation: later children are excluded, reparented captured
members remain included, and exited members are reported individually.
Send parents before children for SIGSTOP and SIGCONT in deterministic
preorder; use children-before-parents postorder for the other signals.
Delivery order cannot guarantee scheduling order or an atomic stop/resume:
parents may respawn children or react to child state changes before their
own signal takes effect. Newly spawned processes remain outside the request.
Do not use a process group, cgroup, repeated descendant chase,
or a freezer as a substitute. Refuse an over-budget or incompletely read
subtree rather than silently sending to a truncated selection. An unseen
process excluded by kernel visibility cannot be promised as a member.

The kernel checks each signal using the caller's credentials. Do not infer
permission solely from a matching UID. Report sent, exited, permission
denied, and other errors per member, including partial success; there is
no rollback or automatic retry. The manager itself and namespace PID 1
are not actionable in version 1, including through subtree membership.
Preparing a subtree containing either refuses the whole action and names
the protected member; it never silently drops that member from delivery.
There is no sudo, su, setuid executable, capability grant, password dialog,
or privileged helper.

Keep action preparation, typed intent, and execution separate. A future
authorized executor can accept a descriptor-bound signal and exact member
set after fresh request-bound consent. That increment must amend the
relevant authority/threat contracts and provide the trusted input path;
this seam neither implements elevation nor delegates ambient UI authority.
The Linux signal and relative-descriptor adapters need their own UNSAFE.md
roster entry and confinement tests when implemented. This document does
not authorize a new unsafe surface on its own.

## Collection and resource bounds

Pure parsers consume bounded bytes and explicit conversion constants.
The collector reads only required procfs/sysfs data and optional command
detail, never process memory or environment. The application model owns
snapshots, history, selection and intents. td-ui owns geometry, focus,
hit testing and drawing; it knows no PID, procfs path, signal or privilege.

Use one collection worker and one outstanding action request, with bounded
handoff to the Wayland thread. The application consumes pending snapshots
and action results from td-ui's tick callback under its bounded idle wait;
it does not assume a cross-thread wake mechanism or wait for a frame callback
to collect results. Sampling never blocks input or holds a model
lock during filesystem I/O. Replace an undelivered sample with the newest,
mark skipped intervals, and do not queue unlimited work. Monotonic timers
are independent of compositor frame callbacks; a hidden window does not
accumulate frames or a redraw backlog. Workers check cancellation between
bounded operations, and a stalled worker never causes replacement threads
to proliferate. Closing cannot wait indefinitely for a filesystem read.

Initial ceilings are 32,768 process rows per sample, 256 tree levels,
1 MiB per aggregate source file, 64 KiB per process source file, 4 KiB of
retained command detail per identity, 256 interfaces, 1,024 block devices,
4,096 logical CPUs, and 256 pinned targets per action. Per-CPU detail is
scrollable and draws only visible plots; omitted CPUs are disclosed and do
not change the basis of the independently read aggregate CPU counter.
Limit total retained model/history data
to 64 MiB, including strings, maps, and a pinned inspected snapshot; keep
at most 240 samples. td-ui's separate surface/buffer ceilings still apply.
Implementations must enforce byte budgets before growth, not only count
limits after allocation.

Evict oldest unpinned samples to meet the history budget and display the
actual retained duration. If a pinned inspection prevents a new sample
from fitting, stop admitting new history and explain that Live releases
the inspection; never silently move the inspected time. A current scan
that exceeds a bound is partial with explicit omitted counts where known.
It cannot authorize a subtree action. Process counts may be unknown if
enumeration itself fails. Reuse scratch buffers and reserve fallibly;
enumeration, sorting and aggregation must remain bounded under churn.

## Validation and delivery

The implementation must demonstrate:

- Parser/rate fixtures for malformed names, truncated files, overflow,
  unavailable fields, PID reuse, parent loss, cycles, counter reset, hotplug,
  namespace scope, CPU normalization and shared-RSS overcounting.
- Explicit-clock history tests for gaps, eviction, pinned inspection,
  changing contributor sets, historical exits, and a stable selection
  through refresh, sibling sorting, search and graph-tab changes.
- Shared td-ui draw-stream, pixel and interaction oracles for submenus,
  charts, split geometry, nonclosable tabs and the tree table, including
  clipping, keyboard equivalents, disabled entries and focus loss.
- Kernel process tests using only owned children: stop/continue, graceful
  exit, force kill, stale descriptors, denied or unsupported operations,
  subtree churn, partial success and all-or-refuse preparation. Model
  fixtures supplement kernel tests for deterministic PID reuse; no tests
  signal processes found by command-name matching.
- A native td-compositor process test of the actual window, graph-to-tree
  selection and menu actions against owned fixtures, with captured pixels.
  A host Wayland smoke test on Guix and a mainstream compositor such as
  Weston must exercise startup, resize, keyboard/pointer navigation, live
  collection and owned-child controls. Record versions and actual results;
  a fake server alone does not establish host compatibility.
- Budget and churn tests showing responsive input and bounded retention,
  plus measured collector CPU/memory overhead at idle and under a large
  synthetic process population. No unmeasured performance claim.

Independently landable increments:

1. This design and the td-ui extension contract. Documentation only.
2. Shared td-ui widgets, each with oracles and existing consumer regressions:
   menus with submenus (implemented, including the atomic editor migration),
   confirmation dialogs and nonclosable resource tabs (implemented). No
   task-manager-local fork.
3. Shared charts, split pane and tree table, each in a self-contained
   tested landing. Reuse the existing text entry and list primitives.
4. Standalone crate with bounded collection, metric model and retained
   history fixtures, committed lock and automatically discovered gate.
5. Live Wayland window with all five tabs, process tree and linked CPU/RSS
   history; native compositor and Guix/host smoke evidence. Read-only until
   the following increment, explicitly identified as incomplete version 1.
6. Descriptor-bound process and subtree controls, reviewed unsafe surface,
   kernel tests and real menu interaction, completing the standalone v1.
7. td recipe and image/launcher integration with declared sibling trees,
   target debug/profile policy, runtime visibility and launch authority
   reviewed against APPLICATIONS.md and td-compositor/DESIGN.md. Realize
   and test the output in the image; a host binary is not target evidence.

Standalone support and td image delivery are both required outcomes. Image
integration must give the ordinary desktop identity the intended monitoring
view; it must not place this system tool in an application PID namespace
and claim it can see the full system, nor grant extra signal authority.
