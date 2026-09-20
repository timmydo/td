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
validated UID-1000 account lookup; account publication and remaining
home-path consumers still need that cutover. Authd's Claude workspace mapping
and task directory and the jail's Firefox download probe use the same
primary-account lookup. The compositor sends typed launch requests without
paths in the installed authority profile; its direct development launcher
still has its separate fixed task directory. The stock account is `tester`.
The stock console uses `td-login login-primary`, and human-UID service
commands use `td-login exec-primary`. Both selectors resolve the current
UID-1000 account and retain td-login's existing authorization and credential
checks. The initial terminal, its readiness probe and workspace selection
use literal argv through that launcher. The existing `env` program sets
the terminal control socket after the credential transition. The paired
terminal authority uses `terminal-serve --primary` and
passes the resolved name through its existing ledger and credential checks.
Firstboot's `--application-primary` selects the same account's canonical
home before provisioning writes, retaining application state migration
and ownership checks. The VM helper's private identity, SSH
configuration and development layout also derive their home from that
primary account and retain the persistent mount and ownership checks.
The boot state-ownership and SSH host-key permission checks run as the
validated primary account; home writes and cleanup use its runtime HOME
with dropped credentials. Their root pre/post cleanup clears only the
fixed system-state probe paths.
Boot health resolves the primary name once through the checked launcher
and passes it as a quoted argument to its existing `su` and `exec-as`
probes. The network self-test does the same. SSH and Git clients use the
credential-selected USER value, including when a probe substitutes a
temporary HOME. The physical-input download oracle obtains HOME through
that launcher and removes only its two fixture names after dropping to the
primary account. A failed lookup or nonzero removal status prevents the
input test; an absent directory is still diagnosed by the later grant
and download probes. The immutable `/home` alias to `var/home` makes the
account's canonical HOME and the provisioner's persistent home agree.
The SSH server's self-test Match configuration also uses the admitted primary
account, as specified below.
The early account-profile operation below activates the saved username before
these consumers start. Updates retain the saved identity and settings and
revalidate them against the newly selected deployment. An update that introduces
a collision with the saved name refuses at boot. Update-time account preflight
before publication is still required as a future availability improvement;
rollback does not repair malformed shared state.

`td-firstboot check-primary-name ROOT NAME` is a read-only preflight for a
proposed name against an already verified, staged deployment. It shares the
primary-account grammar: 1–32 lowercase ASCII letters, digits, underscores
or hyphens, beginning with a letter. Names formed from `tda`, `tdb`, `tdc`
or `tdp` followed only by digits are reserved even before that principal
is enrolled, so later enrollment cannot collide with the installed name.
Rechecking the deployment's current primary name is allowed when it passes
these checks. It requires regular, consistently owned
account tables (passwd/group 0644, shadow 0600, principal reservations 0444),
rejects account/group collisions and reserved service/application names,
and requires a matching human primary group and complete shadow roster.
Its exact public account-table modes match the generated image and shared
live account reader; the general `check-principals` command instead checks
reservation integrity and excludes unsafe writers without pinning those
public modes.
Unknown or duplicate supplementary-group members refuse, so choosing a
previously unresolved member name cannot silently acquire group authority.
It prints `TD-PRIMARY-NAME-CHECK-OK` only on success and never writes settings
or account files. The caller supplies the staged root and must separately
verify deployment authenticity; this command does not authorize erasure,
enroll retained reservations, validate service configuration or activate the
selected account. The volume formatter and full-system installation fixture
use this preflight as described below.

`td-firstboot stage-primary-name ROOT NAME OUT` prepares that account
configuration from the same caller-verified deployment. It validates the
complete input and renamed tables before creating a new private output
root. `OUT/etc` contains passwd/group at 0644, shadow at 0600 and the
unchanged principal reservations at 0444. Only the human account name,
canonical `/home/NAME`, matching primary group name and exact
supplementary-group member references change. Numeric identities,
password/lock fields, other account fields and application reservations
remain intact; the renamed result passes the same complete admission.

The output parent must have the input tree's owner and exclude other
writers. The invoking uid/gid must match that owner, and `/proc` must be
mounted for descriptor-relative access. Parent and output directories are
descriptor-pinned, existing
output roots refuse, and files are created exclusively with their exact
modes. The output root is 0700 and its `etc` directory 0755, independent of
umask. Success is reported as `TD-PRIMARY-NAME-STAGED` only after files and
directories are synced. A write failure can leave a private incomplete
output; callers must require success before consuming it and use a new
output for a retry. This command does not modify the input account files or live `/etc`,
create homes, save an installer choice or start a session. Deployment
verification and boot-time publication remain the caller's responsibility.

`td-install volume` accepts `--username NAME VERIFIED-ROOT TD-FIRSTBOOT`
after the regional settings and before its destination operands. The caller
binds the absolute source-built validator and authenticated, stable deployment
root, just as it binds the formatter and publisher tools. These paths are
control-plane inputs, not wizard fields. Before opening the destination or
clearing scratch, the formatter validates the bounded name syntax locally and
requires successful `check-primary-name` for that root and name. Complete
roster, collision and reservation admission remains the validator's job.
The volume formatter seeds only `@var/lib/td/username`, a mode-0644 file
containing the name and one newline under mode-0755 parents. A caller doing
layout first must perform the same preflight before layout; volume cannot
retroactively protect an earlier write. This primitive supplies no consent.

`td-firstboot prepare-primary-profile ROOT` runs over an authenticated
deployment with persistent `/var`, fresh volatile `/run`, and procfs mounted.
The calling initramfs retains devtmpfs while this operation runs: spawning
its mount applet with null stdin/stdout requires the real `/dev/null`.
Devtmpfs is unmounted only after early preparation, before switching roots.
Before writing, it admits the complete root-owned account and principal
tables. An absent saved username retains the deployment's primary name.
When present, the file must be a single-link root-owned regular file at
exactly mode 0644, at most 33 bytes, with the same name grammar and one final
newline. The setting read descends through descriptor-pinned real root-owned
mode-0755 ancestors; missing
optional `var/lib` or `var/lib/td` means no choice, while aliases, bad modes,
wrong ownership and malformed content refuse without repair.

A selected name is staged through the complete account-table admission above
in a new private `/run/td-primary`. The provisioner binds its prepared passwd,
group and shadow files over the corresponding deployment paths and remounts
each read-only, nodev, nosuid and noexec. It checks inode identity and requires
write-opens to fail specifically with EROFS, then re-admits the complete live
account set and verifies the selected name. The principal reservation table
stays on the immutable deployment. The saved-setting descriptors close
after reading; later staging and mount paths rely on the caller's serialized
boot environment and the private prepared tree. No user process, SSH
daemon or competing root writer may run until this sequence succeeds. A partial failure stops
boot; the next boot starts with fresh volatile state. This is publication
before consumers start, not a multi-file atomic update for running readers.
The raw signed EROFS payload is never edited. With no saved name, its original
account tables must also pass the read-only checks.

Only after coherent publication does the operation derive `/var/home/NAME`
from the UID/GID-1000 account and prepare the primary home. ROOT, its `var`,
and `var/home` must be real root-owned mode-0755 directories. The caller
supplies trusted ancestors and serializes the operation; the provisioner does
not verify the deployment signature or establish the persistent mount itself.

An existing primary home must be a real UID/GID-1000 directory. Its
inode, contents and ownership are retained; after ownership validation,
its open descriptor restores mode 0700 and syncs any permission change.
Aliases, different owners and non-directories refuse without repair.
A new home is prepared in an exclusive hidden sibling, assigned its final
ownership and mode and synced before rename and parent sync. Up to 64
exclusive creation attempts handle collisions; time and PID are only name
hints, and a pre-1970 clock is allowed. Old staging names are not reused
or removed. Successful stdout is exactly the canonical persistent home path and a newline.
A failure may leave a hidden staging directory after an abrupt stop, or
a complete final home if the final sync fails; it cannot publish a
partially owned home. There is no live rename or home-adoption interface.

The image recipe normalizes the deployment root to mode 0755 and checks
its complete primary-account tables before packing. The deployment
initramfs packs the source-built static provisioner and runs this operation before unmounting procfs or entering the root. The
selector does not pack it. Generic early directory creation excludes the
primary UID. Downloads preparation derives its path from the returned
home and retains its existing grant policy; it records the successful
self-bind path in a root-owned mode-0600 file under `/run`. Shutdown uses
that private record to release the same bind before `/var`, after the
application and portal views are released. Invalid home ownership or
type stops boot and requires inspection from a trusted recovery
environment; user changes to home permissions do not prevent boot.
A successful profile emits `TD-PRIMARY-PROFILE-READY NAME` on stderr after
account readback and home preparation; stdout contains only the home path.
The full-system QEMU installer selects `alice` from the verified ISO payload
before formatting and requires exactly one matching marker on every installed
boot. This covers persistence across media removal and repeated boots.
The stock QEMU boot instead requires exactly one `tester` profile marker,
covering the branch with no saved username. This does not add a live account
rename, migrate another home, enroll a PIN or provide the wizard's
destructive authorization.

`td-firstboot render-primary-sshd ROOT` is a read-only early boot operation
using the same complete root-owned account admission as home preparation.
It prints the full fixed SSH server policy with one Match block for that
primary account. The caller owns deployment verification and serializes
account publication before rendering. No name or policy fragment comes
from argv, the environment or mutable home content. It preserves the
per-machine administrator authorization and the distinct volatile,
loopback-restricted human self-test key. It runs after profile publication.

The deployment initramfs writes that output to fresh volatile
`/run/td-sshd.conf` with a private creation mask and final mode 0600,
before releasing procfs or starting users.
Failure stops boot; an incomplete file is never used by a started daemon.
The supervised server explicitly requires that path. There is no optional
include and no immutable config with a stale human name. Only root can
modify its file or parent. The complete policy formatter is shared with
the source-built OpenSSH test, which checks effective authorization for
a renamed human, the stock name and root. The generated policy contains
no secrets and is recreated on every boot, including deployment updates.

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
repair UI yet. The saved account and hostname activate at boot, while the
user-facing setup and recovery flows remain to be implemented. The current
`su` escape hatch is not their intended API.
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

## Session validation

For one-boot session validation, run `td-recipe-eval qemu-boot-session`
(optionally followed by `system-x86-64`). It builds a fresh disposable
system volume and boots it under headless QEMU TCG with networking disabled.
The full system validator requires firstboot identity, immutable deployment
configuration, owned writable state, component health, the compositor and
terminal, application workspace placement and browser support. The check
also requires Claude terminal admission, clean shutdown and an offline
Btrfs check. It does not request the physical-input or audio-capture oracles.
Use `qemu-install-system` for the optical/USB installation matrix and
`qemu-boot-system` for the longer install, update and recovery sequence.

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
