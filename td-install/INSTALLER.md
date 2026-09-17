# Offline graphical installation

This is the target contract for the `installer-rolling` workstream. It
extends [DESIGN.md](DESIGN.md), which owns the disk layout and the single
deployment publisher. Nothing in this document alone activates a new boot
profile or claims that an installation image exists.

## Version 1

Produce one hybrid ISO that boots through x86-64 UEFI both as optical media
and when flashed byte-for-byte to a USB drive. Boot into td-compositor and
an automatically opened native Rust installer. Installation uses the
deployment carried on that media, with no downloads, package selection,
network setup, or source compilation on the destination machine.

The first supported test platform is QEMU with UEFI firmware. ThinkPad
T430s hardware validation is the next milestone, not a v1 hardware claim.
Legacy BIOS, partition preservation, resizing, dual boot, RAID, and
installation into an existing filesystem are outside v1. One selected
whole disk is erased and receives GPT, a FAT32 ESP and the td Btrfs volume.

The wizard collects one human username, hostname, keyboard layout and
timezone. Storage is unencrypted and the installed account automatically
enters the desktop. The welcome and final review screens disclose those
facts. There is no PIN field, password substitute or inert enrollment
screen. [ENCRYPTION.md](ENCRYPTION.md) owns the later complete encrypted
boot, hardware-backed PIN, recovery and session-authentication cutover.
No account secret is needed to install. Networking can be configured after
booting the installed system.

## User flow and authority

The sequence is welcome, destination disk, account and regional settings,
review, installation progress, and completion. Back preserves valid input.
Errors remain visible and explain the failed operation. The review includes
the exact disk identity and capacity, the selected settings, and an explicit
confirmation that all data on that disk will be lost. Success offers an
orderly reboot with an instruction to remove the installation media.

The UI is a td-owned, dependency-free Rust Wayland client. Follow td-editor's
software rendering, font and input conventions where useful; a general UI
toolkit and GPU renderer are not prerequisites. Shared source reuse must
retain its owners' contracts, and any syscall boundary needs its own
UNSAFE.md authorization. No browser, webview or HTTP service is required.

The UI runs without disk-writing privileges. A root-owned installation
service admits only typed installation operations over a private local
channel. The UI cannot select executables, shell commands, arbitrary paths,
mount options or a different deployment source. The live profile grants
only the paired installer session access to this service. This authority
does not depend on `su`, empty passwords, or a reusable elevation grant.
Compositor-owned trusted consent must bind destructive execution to the
exact reviewed request under the existing elevation contract; ordinary
client pixels or synthetic input are not authorization evidence.

Disk enumeration is read-only and bounded. Show model, serial when supplied
by the device, capacity and a distinguishing device identifier. These are
descriptions, not proof of device authenticity. Exclude the installation
medium and every disk backing its mounted files, mounted/in-use targets,
read-only devices, partitions and unsupported device topologies. Failure
to resolve backing devices refuses installation rather than guessing.

Review produces one immutable installation plan. Before any destructive
write, the service validates its opened destination against that plan,
rechecks eligibility and size, and retains the destination identity for the
entire operation. Hot removal, replacement, a changed plan or lost consent
requires a new review. A stale device pathname never identifies permission
to erase whatever later appears there. Descriptor pinning alone does not
prove that an unrelated process cannot mount the device; the implementation
must specify exclusive admission before activating disk writes.

No write occurs while navigating the wizard. Once destructive execution
starts, cancellation or UI loss cannot promise restoration of old contents.
The service retains bounded progress and an explicit outcome and never
automatically retries a destructive operation after reconnect or restart.
Completion requires durable filesystem and deployment publication, verified
boot artifacts, and settings publication. A queued request is not success.

## Media, boot and persistence

[MEDIA.md](MEDIA.md) specifies the hybrid format and its current formatter
boundary.

Use source-built target executables and declared inputs throughout image
composition. Host-seeded control-plane executables never enter the image.
Any marked foreign application payload remains subject to AGENTS.md and
APPLICATIONS.md, including the dedicated read-only payload-input channel.
No new external dependency is approved by this document.

The media's reproducible content is built without signing secrets. As in
DESIGN.md D4, deployment signing occurs outside derivations. The assembly
interface must explicitly bind the signed deployment and trusted public
key; development fixture keys must not become an implicit distribution key.
Neither installer nor installed boot silently accepts unsigned deployments.
UEFI bootability does not claim Secure Boot authentication.

Optical and USB boots enter the same live profile, with the installation
source read-only and live mutable state volatile. The installed profile
uses persistent Btrfs state. Installed boot must select the destination's
volume without assuming `/dev/vda`, and continue working when device order
changes or installation media is absent. Selector and deployment initramfs
must agree on volume identity across kexec. The fixed ESP stub retains the
deployment-selection boundary in DESIGN.md D5.

Publish the bundled deployment through td-boot, preserving signature
verification and transactional current/previous bookkeeping. Reuse the
td-install GPT/FAT32 implementation and its file-image test path. Check
scratch-space requirements before erasure; the current formatter stages
deployment contents and a filesystem image, so free destination capacity
alone cannot establish that a live session has enough temporary space.

Machine settings are bounded, validated persistent data outside immutable
deployment bytes. The installed account retains the existing single-human
UID/GID allocation; user-selected names must not collide with system or
application accounts. Account databases, home paths, application grants,
service configuration and automatic login must agree before the session
starts. Existing `/home/tester` assumptions need an atomic cutover in the
installed profile. Grant preparation and jail admission already share a
validated UID-1000 account lookup; account publication, login and remaining
home-path consumers still need that cutover. These include authd's Claude
workspace mapping and task directory, the compositor's paired task directory,
and the jail's Firefox download probe. The stock account is `tester`.
Updates must retain the installed identity and settings.
Keyboard and timezone choices must actually affect the installed session;
only supported choices with available data may be offered.

`td-install timezones` provides the read-only catalog for settings
selection. It accepts no operands and reads only the deployment's
`/etc/zoneinfo`: `zone1970.tab`, `iso3166.tab`, and referenced TZif files.
Its JSON `version: 1` is a schema version, not a tzdata release. `source`
is `zone1970.tab`; `timezones` is sorted by IANA `id`. Each entry carries
`countries` with upstream `code` and `name`, and an upstream `comment`
(empty when absent). Country order follows the upstream zone row.
`Etc/UTC` is added with no countries and the comment `Coordinated Universal
Time`. Backward aliases and fixed-offset alternatives are not enumerated.

The catalog permits at most 512 countries and 1,024 zones including UTC.
Each table is limited to 128 KiB, each line to 2,048 bytes, country
names to 256 bytes, zone IDs to 64 bytes, comments to 512 bytes, and
each zone file to 64 KiB. Empty tables, duplicate or unresolved entries,
malformed UTF-8, unsafe zone IDs, and missing or non-regular files
refuse the whole catalog before JSON output. Zone IDs have two or three
slash-separated components, each starting with an ASCII uppercase letter
and otherwise containing ASCII letters, digits, underscores, plus or
minus signs. This matches the application launcher's name bounds. TZif
screening requires a complete 44-byte v2/v3 header; it does not parse
transitions or establish semantic validity. The tzdata recipe's native
checks own that validation and also run this exact catalog reader
against every realized output. File leaves cannot be symlinks, but the
deployment's root symlink into its immutable store is supported. Parent
paths are trusted deployment data, not a defense against a concurrent
privileged writer. Output errors fail the command and may leave partial
JSON. This command neither selects nor persists a timezone. The
compositor reads the selection written by the volume formatter below.

`td-install volume [--uuid UUID] [--timezone IANA-ID] [--hostname NAME]
DESTINATION MKFS SCRATCH --trusted-key KEY` accepts an optional catalog selection. It
validates the entire offline catalog and selected identifier before
opening the destination or creating scratch state. Unknown names and
backward aliases refuse. The formatter seeds one mode-0644,
newline-terminated name at `@var/lib/td/timezone`; its parent
directories are mode 0755. Omitting the option leaves that file absent.
The deployment-owned optional persistent `/etc/timezone` link points to
`/var/lib/td/timezone`, so deployment updates preserve the choice and
first boot does not overwrite it. The QEMU installation oracle selects
`Europe/London` and verifies the file after formatting and on both cold
boots with the installation media removed. The full-system oracle also
requires mail, news, Firefox and Claude startup evidence and clean guest
shutdown in one additional direct selector boot of that installed disk.
Only this additional boot supplies the existing autotest command-line
token; the two firmware boots remain unchanged. The full-system
diagnostic ISO seeds the standard VM loopback-restricted SSH self-test
authorization and mode-0600 test key in its disposable volume, which the
autotest health probe requires. Ordinary installer artifacts do not
contain that diagnostic fixture. The full installed-system boots use a
4 GiB test VM: the current roughly 3 GiB deployment verification can
fill a 2 GiB guest's page cache before kexec allocates its control page
without reclaim retries. This is an oracle budget, not a minimum-memory
hardware qualification. The small diagnostic matrix retains 2 GiB.
The application-evidence boot uses the shared system-test timeout,
which covers the longer autotest profiler prerequisite; installation
and ordinary firmware boots retain their separate 900-second default.

`--hostname` selects the installed machine name before destination or
scratch access. Options have the displayed order; duplicate, misplaced,
missing and non-UTF-8 values refuse. Names contain 1 to 63 lowercase ASCII
bytes in dot-separated labels. Each label starts with a letter, ends with
a letter or digit, and otherwise permits letters, digits and hyphens.
The formatter seeds `@var/lib/td/hostname` as one newline-terminated name,
mode 0644 beneath mode-0755 directories. Omission leaves the file absent.

The deployment owns `/etc/hostname` as a persistent link to that file and
ships its default in immutable `/etc/hostname-default`. The existing
`hostname` startup unit runs `td-firstboot hostname` before other sysinit
units. It validates an existing saved name without changing it. Only
absence initializes the deployment default, using the provisioner's
synced temporary-file and rename protocol. An invalid, symlinked,
non-regular, oversized, wrongly owned or wrongly permissioned file
refuses; it is never replaced with a fallback. Reads are bounded to 65
bytes, accept at most one trailing newline, and require mode 0644 and
root ownership. Parents are trusted root-owned system directories;
concurrent privileged replacement is outside this boot-time contract.
This operation has no alternate paths or name operand and is not a
post-install rename interface. Provisioning remains a serialized startup
operation, not a concurrent settings writer.

The provisioner sets the kernel name through the shipped `/bin/hostname`
applet, then reads it back from procfs before reporting
`TD-HOSTNAME-READY NAME`. Deployment health requires that unit to succeed;
the serial console retains its independent recovery path. `/etc/hostname`,
the kernel name and the existing network/app-jail consumers therefore
share the installed choice. Later defaults do not replace saved state.
Malformed shared state therefore prevents acknowledgement across deployments;
rolling back does not repair it. Recovery requires restoring a canonical,
root-owned mode-0644 `lib/td/hostname` in the volume's `@var` subvolume from
a trusted recovery environment. There is no supported in-system rename or
repair UI yet; activation of the complete installer profile still awaits
that recovery flow. The current `su` escape hatch is not its intended API.
The QEMU installer selects `td-qemu-installed`, checks its saved bytes
alongside timezone state, and requires activation on both full-system
cold boots and the additional application-evidence boot. Unit tests retain
the same saved inode and bytes across a changed deployment default.

The existing application launcher reads that name and binds each runtime's
own zone file at its jailed `/etc/localtime`. Static mail and news carry
the source-built data in `static-runtime`; Firefox and Claude use their
reviewed Freedesktop runtime data. Missing runtime zones refuse launch.
The compositor snapshots the saved name and source-built TZif rules at
startup, rendering local civil time with its actual UTC offset. Invalid
settings or unsupported data show `CLOCK ?`; an absent setting stays UTC.
See `td-compositor/DESIGN.md` for reader bounds and DST proof. Account and
keyboard settings and a post-install timezone setter remain separate
increments.

The `tzdata` recipe compiles the approved IANA 2026d data-only source
with td's existing source-built glibc `zic`. Its output contains fat
TZif files at `share/zoneinfo`, including backward-compatible aliases,
geographic tables, and the upstream version and license. It uses
ordinary POSIX time without leap-second corrections. Fat output is
required by the pinned glibc compiler: its older `zic` miscompiles the
2026 Canadian transitions in slim mode, dropping the correct
daylight-saving flags and abbreviations. The native check covers those
transitions in all six affected zone names. The upstream `Factory`
placeholder for an unspecified timezone is intentionally omitted; it is
not a selectable civil timezone. The upstream default source set omits
`backzone`; merged locations share their canonical zone's pre-1970
history. This data does not claim complete local histories before 1970.
The recipe does not install a second libc or timezone compiler. Its
native recipe check reads the realized geographic tables and requires
their named zones to exist, then uses td's `zdump` to verify exact 2026
Canadian and 2027 UTC, daylight-saving and fixed-offset behavior.

The complete data output ships at its canonical store path in the system
deployment, including live and installed profiles. The immutable
`/etc/zoneinfo` link exposes compiled zones and geographic tables for
offline selection. Source-built td glibc's compiled `TZDIR` is
`/td/store/glibc-2.41-x86_64/share/zoneinfo` and its `TZDEFAULT` is
`/td/store/glibc-2.41-x86_64/etc/localtime`. Later settings consumers
must explicitly connect the selected data to native readers; libc does
not discover `/etc/zoneinfo` automatically.

## Independently landable increments and evidence

1. Record the agreed v1 scope and its activation boundaries here.
2. Add EFI-capable kernel artifacts and verify their realized format. Finish
   fixed-stub/initramfs packaging and a firmware boot oracle; keep the
   existing direct-kernel oracle as a separate test.
3. Implement deterministic hybrid media composition and the read-only live
   boot profile. Boot the same artifact as optical media and USB mass
   storage, without QEMU `-kernel` or a supplied initrd.
4. Add validated persistent machine configuration and its installed-profile
   consumers, including volume discovery across selector and deployment
   boots. Test settings retention across a deployment update.
5. Add bounded device discovery, immutable plans, trusted destructive
   consent and the service's installation execution. Prove refusal of
   installation media, in-use targets, stale identities, unsupported
   destinations and invalid plans without modifying their bytes.
6. Add the native wizard, target recipe and live startup integration. Use
   native compositor tests for navigation, rendering, input, errors and
   progress; fixtures cannot grant ordinary clients trusted consent.
7. Activate the complete profile only after the end-to-end QEMU evidence:
   boot the ISO, complete the UI flow onto a disposable disk, detach the
   media, boot that disk through firmware, and observe the configured
   account in the compositor with its settings and persistent home.

Use per-run disposable disks and firmware variables. No test discovers or
opens an operator's real disk for writing. Exercise both supported media
attachments, wrong deployment signatures, insufficient capacity and scratch,
interrupted installation, changed disk ordering and a second installed boot.
Require actual rendered/input and installed-session evidence, not only
serial markers printed before the relevant operation completes.

## Read-only block inventory diagnostic

`td-install inventory` takes no operands and prints one version-1 JSON
object with `scope: "inventory-only"` and a `devices` array. It observes the
fixed `/sys/class/block` tree; it never opens `/dev`, invokes a child,
mounts, writes sysfs, or calls the layout/volume writers. There is no
alternate sysfs root in the CLI. The internal root parameter supports
owned filesystem fixtures in tests.

This diagnostic deliberately includes partitions, mounted disks, media,
read-only disks and stacked devices. It is not the eligible destination
list specified above and is not connected to the wizard or write path.
No field grants permission to install. Source/mount/swap backing-device
resolution, unsupported-topology filtering, exclusive admission, immutable
plans and trusted consent remain required before destination selection can
activate writes. In particular, `holders` and `slaves` alone do not account
for loop backing files, mounted filesystems or every Btrfs member.

Each record contains the kernel name, decimal major:minor device number,
capacity in bytes, read-only flag, whole-disk parent name for partitions,
the partition number, and sorted holder/slave names. A whole disk reports its disk
sequence number, logical sector bytes, removable flag, model, serial and
WWID. Logical sector bytes are reported as observed, including zero for
some no-media devices; consumers must validate geometry before using it
in arithmetic or admitting a destination.
Descriptions absent from the driver are JSON null; existing empty fields
remain empty strings. Serial prefers the disk's own `serial`, then
`device/serial`; WWID likewise prefers `wwid` then `device/wwid`, and model
reads `device/model`. Serial and WWID remain distinct even when a driver
provides only one. For identification reads only, ENXIO means absent VPD
data under the pinned Linux 7.1.4 SCSI interface and produces null (or the
fallback attribute). Other read errors still refuse the observation. These
descriptions and the
kernel identifiers do not authenticate hardware or survive every reboot.
Partitions have no separate disk record: their parent's geometry applies.
Linux's `size` is multiplied by 512 even for a 4096-byte logical sector.

Collection admits at most 4096 block nodes, 16384 directed relationship
entries in total, and 256 bytes per attribute. Exceeding a bound, invalid
UTF-8/numeric metadata, missing required metadata, an unresolved parent,
duplicate identity or inconsistent reciprocal relationship is an error.
Canonical relationship destinations must match the referenced node.
Partitions have a `holders` directory and no required `slaves` directory.
Optional descriptions and the partition discriminator may be absent;
other I/O failures propagate with their path. Text is JSON escaped, and
node and relationship order is deterministic. A successful empty inventory
is an empty array, not evidence that installation is possible.

Two complete observations must agree before any JSON is printed. This
catches changes between reads but is not an atomic snapshot or protection
against removal/replacement after collection, or a change and reversal
between observations. Bounds limit bytes and entries, not elapsed kernel
I/O time. A failed collection prints no partial inventory; an output I/O
failure can leave partial JSON and a nonzero exit status. Consumers must
require successful exit and a complete document.

The QEMU installation diagnostics exercise this inventory before source
verification and after partition refresh. Their host compares it with
oracle-owned optical/USB images and writable, read-only, undersized and
4Kn targets, plus SATA/AHCI targets at 512-byte geometry and direct NVMe
namespaces at both geometries. Partition parents and disk identity
continuity are checked on successful/recovery installations; refusals must
have no post-format report and retain the existing whole-disk byte preservation proof. These
are observations under a disposable topology, not destination eligibility.

## Read-only layout preview

`td-install layout-preview <logical-sector-bytes> <capacity-bytes>`
takes two unsigned decimal byte counts, each at most twenty ASCII digits
and within u64. Leading zero padding is allowed within that length
bound; output numbers have no padding. It computes the same GPT
partition layout that `layout` writes, for v1's supported 512-byte or
4096-byte logical sectors. Invalid geometry, unaligned capacity or
insufficient space for the fixed ESP and minimum system volume refuses
before output. It opens no files or devices, reads no sysfs attributes,
generates no identities and starts no child process.

Success prints one version-1 JSON object with `scope: "layout-preview"`,
`logical_sector_bytes`, `capacity_bytes` and exactly two `partitions` in
disk order. Each partition has `number`, `purpose` (`efi-system` or
`system-volume`), inclusive `start_lba` and `end_lba`, `offset_bytes` and
`capacity_bytes`. Integer fields require exact unsigned integer handling;
consumers must not round large capacities through floating-point numbers.
The output is deterministic and newline-terminated. Invalid inputs produce
no partial report; output/flush errors return failure and may leave partial
bytes, so consumers require both complete JSON and successful exit.

This is a geometry preview for a future disk-review page, not an immutable
installation plan or a destination eligibility decision. It describes the
numbers supplied by its caller and does not bind a device identity. It
does not establish source authenticity, boot-file fit, payload capacity,
scratch availability, settings validity, exclusive admission or trusted
destructive consent. Those checks remain required before installation.

The QEMU installation diagnostics also invoke this command on actual
virtio/NVMe 512-byte/4Kn and AHCI 512-byte disk geometry. Their host compares
its complete partition report against the attached private disk and GPT
boundaries, alongside post-format inventory and detached firmware boots.
The diagnostic queries after successful layout so negative cases still
exercise formatter refusal; this is not the future wizard's sequencing.

## Installer-oracle failure diagnostics

The small and full-system QEMU oracles keep their exact requirement that
selector volume binding precede deployment selection. On a binding failure,
they report the retained line/byte counts, the first exact binding and
selection line offsets, and up to eight lines containing binding or
selection prefixes, including malformed selection delimiters. Each excerpt retains at most 256 UTF-8 bytes before escaping
control characters; escaping can expand the excerpts beyond those input
byte counts. Clipping and omitted matches are explicit. A labeled console
tail follows. These excerpts can expose earlier malformed records
that the tail omits. They are diagnostic context, never substitute evidence
for a missing, reordered or interleaved exact record. Output is limited to
the console retained by the runner, not a complete boot transcript guarantee.
