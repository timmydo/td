# Native QEMU installation fixture

This crate is a diagnostic PID-1 program for `td-recipe-eval qemu-install`
and the live installation phase of `qemu-install-system`.
It is built from source through the ordinary Rust ladder and is never packed
in the system or graphical installer profile. It adds no user-facing device
admission, destructive consent, account configuration or signing identity.

The host oracle exclusively creates each sparse target inside its private
scratch directory and exposes it as a writable virtio, AHCI or NVMe disk
with the fixture serial. The ISO stays read-only. The fixture refuses outside PID 1 and admits
exactly one whole virtio, SCSI-named AHCI or NVMe namespace target with
that serial; it accepts
no command-line destination. This is test identification, not a real installer authorization
mechanism. Firmware variables are private copies, networking is disabled,
and the host boot runner owns deadlines and QEMU teardown. Each boot defaults
to 180 seconds; a positive TD_QEMU_BOOT_TIMEOUT_SECS overrides that limit.
Target discovery also retries a matched node that has not yet appeared in
/dev or whose read-only open returns ENXIO, within its thirty-second
deadline. Other metadata/open errors and non-block nodes refuse.
A refusal keeps PID 1 alive for the host to collect its diagnostic and
terminate the VM at the deadline.

The complete crate tree is a `local_source` input, declaration-pinned by
seed/local-source-roster.txt and re-derived live from the checkout on every
run (`DEVELOPMENT.md` "Ready"). It is not a production system input.

The live initramfs holds source-built tools and the public trust root. The
tiny signed deployment and fixed selector are streamed into separate ISO
files. Boot files and tools read into the fixture initramfs are bounded at
256 MiB. The shared composer accepts larger ISO payloads under MEDIA.md,
while the full-system diagnostic supplies the built system deployment.
The private key remains on the host outside derivations. The fixture calls the
actual td-install layout and volume primitives, including td-boot's verified
publication. Its success marker follows formatting, partition refresh,
mounted publication and sync.

Before any layout write, the guest polls for thirty seconds among exactly
three fixed candidate paths: /dev/sr0 (SATA optical) and /dev/sda or
/dev/sdb (USB), excluding the selected installation target. QEMU
adds an empty default CD-ROM in both attachments: /dev/sr1 during optical
boots (outside this candidate list) and /dev/sr0 during USB boots. A read-only
block-device open must succeed; Linux ENOMEDIUM (123) skips an empty drive
even when the driver publishes a placeholder capacity. ENXIO (6) also
retries within the same deadline: USB can publish its node before the
block driver accepts opens. A permanently absent/unready source still
times out, and no install step runs before successful read-only access.
Other open errors refuse. Zero-capacity
devices are ignored. Malformed or unreadable capacities refuse. The host's
VM deadline also bounds time spent in kernel operations during discovery.
Absent, ambiguous or non-block candidates refuse. This convention applies
only to this disposable QEMU profile, not production media discovery. Linux
mounts ISO9660 read-only, nodev, nosuid and noexec. Individual read-only file
binds adapt ISO filename case to td-boot's canonical payload names without
copying the source into RAM or introducing symlinks. Every file must refuse
a write-open with ReadOnlyFilesystem before the media-access marker prints
the selected device. The host requires that device to match its attachment.
The write-open check proves payloads are unwritable, not each individual
bind-mount flag; all mount commands must independently succeed.
The fixed selector is bound outside /source, which holds only the signed
deployment bundle. Discovery checks only currently visible candidates;
it does not establish exclusive admission against later hotplug.
Missing payloads refuse before layout; no initramfs copy is available as a
fallback. The formatter stages only a trust layout and sparse Btrfs metadata
image in RAM; the deployment is published directly onto the mounted disk.

The host chooses the UUID from the run's throwaway provisioning public key
before assembling the ISO. Its canonical text is placed in the live and
selector initramfs at `/etc/td/volume-uuid`. The live fixture supplies that
value through `td-install volume --uuid`; formatting cannot silently choose
a different identity. Optical and USB destinations deliberately share that
run's UUID but are never attached together.

Target discovery waits up to thirty seconds for its fixed serial. Virtio
uses the root serial attribute; AHCI disks use the SCSI device serial.
Missing serial attributes and the pinned kernel's absent-VPD ENXIO are
not matches. The selected node must also accept a read-only open; ENXIO
retries target discovery within the same deadline. That probe descriptor
closes before installation. Oversized serials and other errors refuse. Partition names
and unsupported buses are excluded. This remains a private test topology,
not discovery or admission for an operator's physical disk.

Before invoking layout, the live fixture calls `td-boot validate-source` on
the read-only source and the provisioned public key. Missing or invalid
signatures, malformed authenticated manifests, missing or symlinked payloads,
and payload hash mismatches refuse before the first disk-writing command.
The preflight uses the publisher's bundle verifier, retaining manifest-only
`authenticate` for callers that ask only about the signing identity. Source
bytes stay stable on the read-only ISO across preflight and publication;
the result is not a retained snapshot. Volume publication still repeats
authentication and verifies copied payload hashes. Space, firmware-selector
and target admission remain requirements for the production installer service.

After both formatting commands, the guest asks td-init to reread the target's
partition table. It resolves the configured UUID through the production
reader and requires the expected target partition, then mounts it through
td-boot. Both raw formatter commands must fail their destination open with
exit status 1, EBUSY and no stdout while that partition is mounted. An
assertion failure terminates the installation sequence and leaves PID 1
parked for host collection and VM teardown, as with other fixture failures;
no later installation step runs against the failed disk. A bounded 64 KiB
read before and after each attempt also requires unchanged protective MBR
and primary GPT metadata at both supported sector sizes. This exercises
Linux's block claim through the production destination wrapper; it is not
a whole-disk hash comparison while the filesystem is active. A second reread
must fail specifically with EBUSY while mounted;
no force or unmount fallback is accepted. The guest unmounts and requires a
final successful reread before emitting the partition-refresh evidence.
The host requires the exact configured UUID and the planned /dev/vda2 or
/dev/sda2 target partition in that evidence on both media attachments. This exercises
partition publication during the same boot; firmware reboot cannot mask a
missing reread. UNSAFE.md §3 owns the td-init request; this crate still has
no raw syscall surface.

The live guest supplies `--trusted-key` to `td-install volume` instead of
the three publishing operands. The formatter initializes the publication
directories and key without a deployment or selector. The fixture requires
empty staged boot, deployment and incoming directories, plus only the
expected timezone, hostname and (for the full system) username settings
beneath @var, then deletes its entire private scratch directory after
partition refresh, then calls `td-boot install` with the resolved partition,
/source and the same read-only live public key. Successful mounted
publication and sync precede the direct-publication and installation markers;
the host requires both. The full cold-boot oracle still proves the expected
deployment is installed. The separate full-system diagnostic checks its
installed desktop under a RAM ceiling. Neither diagnostic admits operator
target capacity.

The fixture supplies `--timezone Europe/London --hostname td-qemu-installed`
to the volume formatter. It checks exact mode-0644 regular saved files in
the staged @var, again after mounting the published full-system volume,
and on both small-fixture cold boots. The full-system host oracle requires
exactly one `TD-HOSTNAME-READY td-qemu-installed` line on every installed
boot, including the additional application-evidence boot. That production
marker follows setting and reading back the kernel hostname; merely
saving the file cannot satisfy it.

The full-system profile also chooses `alice`. After authenticating all ISO
payloads, it read-only loop-mounts the signed EROFS and runs the source-built
`td-firstboot check-primary-name` before layout. Volume repeats admission
through `--username alice /root-image /bin/td-firstboot`; the verified source
remains stable on read-only media. The fixture checks the exact mode-0644
saved file in staged and mounted @var. Every installed full-system boot must
report exactly one `TD-PRIMARY-PROFILE-READY alice`, after production account
publication and home preparation. The existing SSH, terminal, ownership and
application probes then run as that account. The live fixture unmounts
its temporary EROFS view after formatting, retaining the read-only loop
binding until its VM ends. That phase neither reuses loop0 nor unmounts
the source; installed boots start in a fresh kernel. The tiny sentinel
deployment has no account database and keeps its existing settings and
boot protocol.
This is a fixed diagnostic choice, not an operator account-configuration UI.

The host then detaches media and cold-boots the destination through
firmware. The selector emits read-only discovery evidence for its configured
UUID and invokes production `td-boot on-volume boot`. That entry reads the
selector's own identity file, rejects an existing handoff token, and binds
verified kexec to the configured volume. The selected fixture requires one
UUID handoff and resolves it again. The host requires both phases to report
the exact preselected UUID and expected device, plus the production bound
entry's marker; agreement with an arbitrary discovered UUID is insufficient.
The selected fixture uses `td-boot on-volume` for both mounts and
acknowledgement. Before acknowledgement it deliberately leaves a writable
mount sourced through a descriptor it then closes. Success must recover
that stale mount and leave no mount at `/ack`; the host requires the
post-recovery marker on every normal boot. Named-marker checks also inspect
that marker's own complete line; a different boot target being reached is
insufficient. Neither installed phase uses the fixture serial or partition
suffix.

Before successful boots, a private decoy gets a copy of the installed
primary superblock at its own whole-disk superblock offset. The duplicate
identity must refuse before deployment selection. This is an identity
collision fixture, not a second mountable filesystem. First normal boot
uses the destination alone; the second attaches a fresh blank disk on the
same bus first, moving the destination from /dev/vda2 to /dev/vdb2 for
virtio or /dev/sda2 to /dev/sdb2 for AHCI. Firmware still
boots the destination by explicit boot index. Both phases must report the
expected device and preserve the same UUID across those cold boots.

The fixed selector authenticates and kexecs the installed tiny deployment. Its init verifies and loop-mounts the installed EROFS payload,
reads its sentinel, and writes a synced count in Btrfs @var. A second cold
boot must observe and advance that count. The reported deployment ID must
match the host's signed manifest. Successful installed boots also acknowledge
that deployment through td-boot. An otherwise identical source offered under
a second public key must produce an authentication refusal and no installation
success marker on both optical and USB boots. The host exclusively creates
each wrong-key target, seeds nonzero canaries at its beginning, middle and
end, and records its byte length and a SHA-256 over the entire disk. After
QEMU exits, both must remain identical. This also detects changes between
the canaries and inside sparse gaps; merely observing a refusal is not enough.
Each negative boot must independently prove read-only access to the expected
media device. Both valid installation boots share one ISO; both wrong-key
boots share another, with unchanged signed payloads and only a different
live trust root. A third ISO replaces only `root.erofs`, retaining its
authentic manifest/signature and the correct trust root. Both optical and
USB boots must report the root payload hash mismatch and preserve every
target byte under the same whole-disk comparison. Unit tests in td-boot
exercise corruption, missing files and symlinks for all three payloads.
A fourth ISO selects an interruption phase with otherwise identical valid
payloads and the same selector. After layout and partition refresh, the
fixture starts the production publisher as its owned child. It observes one
real private .install-* directory and a nonempty kernel shorter than the
source kernel, then kills and reaps that child. Every observer error also
kills/reaps the child. The fixture checks the length again after death and
requires both current and previous to be absent, then syncs the volume.
Only then does it report the written/expected byte counts. Missing, empty,
complete or indirect payloads cannot stand in for partial publication.
The report triggers the host VM power cut. Afterward, the host verifies
that QEMU was killed and checks the counts against its source kernel. This is a deliberate publisher kill followed by a VM power cut,
not arbitrary timing of unassisted power loss or a media-flush fault model.

A detached firmware boot must bind the expected volume, refuse specifically
because both selectors are absent, and select no deployment. A subsequent
explicit installation from the normal ISO must succeed; its detached boot
must authenticate the expected deployment and start with fresh persisted
state. No automatic resume or preservation of the erased disk is claimed.
Both optical and USB attachments run all four interruption/recovery boots.
Four further boots use the valid normal ISO with undersized and read-only
targets, through both media attachments. The host requires the specific
layout refusal, no partition refresh or publication, and an unchanged
whole-file length and digest after QEMU is reaped. The read-only target is
protected by QEMU's block backend; this proves guest refusal and byte
preservation under emulated write protection, not exclusive disk admission.
The successful installation/duplicate-identity/reboot matrix also runs
with 4096-byte logical and physical sectors on the virtio target and its
reordered companions. The fixture reads the target's bounded sysfs logical
sector size and reports it; the host requires the planned value alongside
the completed-installation evidence. The ISO attachments keep their normal
geometry.
Both detached boots retain the installed target's sector size, so firmware
must interpret the GPT and ESP actually written for that geometry.
The positive four-boot sequence also runs at 512-byte geometry with an
AHCI target, attached as ide-hd on its own ich9-ahci controller. It retains
that bus through detached boots and gives the reordered decoy its own
preceding AHCI controller. Live USB media is expected at /dev/sdb beside
the AHCI target at /dev/sda. Linux allocates SCSI disk names asynchronously;
the oracle deliberately requires the planned order to prove changed-path
discovery. A timing-dependent order mismatch fails this fixture and does
not by itself demonstrate a UUID discovery defect. AHCI targets are
restricted to 512-byte logical sectors before constructing QEMU arguments;
QEMU ide-hd does not support the virtio 4Kn case.
The same ISO bytes serve all target buses.
The complete small oracle has fifty-six boots, all with fresh private firmware
variables. Interruption and refusal cases retain virtio/512-byte geometry;
the full-system diagnostic installs through both media attachments onto
virtio and AHCI at 512-byte geometry, then boots each installation twice
with media detached and a same-bus preceding decoy on the second boot.
Those twelve full-system boots use the same ISO and require stable
machine identity per installation and distinct identity across all four.

The fixture's serial convention does not implement production installation
admission. Volume discovery uses the production read-only primitive under
the oracle's fixed topology; it does not establish exclusive admission.
The small fixture does not launch the compositor, configure an account or
retain an installation signing key. The separate full-system diagnostic
uses the ordinary system init after installation to test the desktop.
The final installer still needs the activation evidence in
../td-install/INSTALLER.md, including a complete desktop installation and settings.

All operations use safe Rust and existing td-init/td-boot applets. No syscall
surface or external dependency is added. Unit tests cover refusal outside
PID 1; the host oracle is the executable integration test. A serial marker is
accepted only after its named operation and persistence barriers finish.
The fixture formats a complete protocol line before write_all, avoiding
formatter-induced split writes that allowed kernel console messages to
separate the refusal prefix from its reason. This is not an atomic-console
guarantee; incomplete or interleaved evidence still fails the host oracle.

The fixture deliberately inspects the formatter's staging layout as an
internal regression oracle. A formatter rename requires updating that
oracle; cleanup removes the whole fixture-owned /scratch directory rather
than depending on the formatter's image filename or retention policy.

The interruption observer intentionally depends on td-boot's private
.install-* staging directory and kernel-first publication order. A change
to that transaction layout must update this internal oracle. It neither
adds a production pause hook nor teaches the publisher a testing mode.
Polling may miss a fast kernel copy and fail the test; complete copies
never count as interrupted publication. Preallocation or delegation to a
child publisher also requires revisiting this observer. The detached-boot
refusal oracle depends on td-boot reporting both missing selectors and
on the fixture reporting its exact nonzero exit status.

After the source mounts read-only, before source verification, and again
after partition refresh, the live fixture runs the source-built
`td-install inventory` command. Its owned
child's stdout is read through a 32 KiB limit and must be one complete
UTF-8 line; capture failure kills and reaps the child, and successful
capture still requires a successful child exit. The host's VM deadline
bounds blocked reads and child completion. Stderr goes to the bounded
console capture. The fixture emits each JSON report in one formatted
protocol line, with separate before/after prefixes and an explicit JSON
byte count. The host consumes exactly that bounded number of UTF-8 bytes;
console text after a complete JSON frame is outside the report. This
handles kernel messages that join the line before its final newline.
Truncated, malformed, duplicate or missing reports fail; interleaving
inside the JSON still fails rather than repairing or stripping its bytes.
This does not claim atomic serial-console writes.

The host parses a uniquely prefixed report only after bounding its byte
length (including the guest stdout newline) and JSON nesting. It checks
version/scope, unique device names and queried fields, and compares
target/source capacities with the private
files it attached. Target geometry and write protection must match QEMU's
configuration, its serial must match the fixture serial, and the source
must report read-only media with the expected optical/USB sector size.
Fresh targets must report no target partitions before formatting; explicit
reinstallation may retain them. Every reported device number must be unique.
After formatting, exactly two target partitions must name the target
parent and correct partition numbers and report writable partitions.
ESP capacity must match the fixed
layout, and volume capacity must span from its fixed byte offset through
the GPT last usable sector at the requested geometry. These capacity
observations do not themselves report partition start offsets; GPT byte
checks and detached firmware boots retain the layout proof.
Mismatch diagnostics name the device, field and expected/observed value.
Whole-disk device numbers and sequence numbers must remain consistent
between reports. Reinstallation may already have partitions in its first
report. These refusal cases have valid mountable ISO filesystems and
damaged payload/trust data or unsupported target capacity/write protection.
They
require only the before report and prohibit an after report; a future
unmountable-media case needs its own earlier failure expectation. The
existing whole-target byte comparisons still establish write preservation;
inventory alone does not. These observations cover all live
legs of the 56-boot matrix and all four full-system ISO installations without
adding boots or changing target admission.

The live fixture also runs `td-install layout-preview` after successful
layout and before volume formatting. It reads the target's bounded sysfs
size in Linux 512-byte units, checks conversion to bytes and supplies that
capacity and the observed logical sector size. This placement preserves
the negative cases' direct exercise of the real layout writer: a preview
refusal cannot replace an undersized or read-only formatter refusal.
It is diagnostic sequencing, not the future wizard's review sequence.

Inventory and preview share the same owned-child capture and reaping
path. Preview stdout is bounded at 1024 bytes including its final newline.
The host requires exactly one framed version-1 layout-preview report on
successful, interrupted and repaired installations, and none on existing
source or target refusal cases. It compares exact integer geometry,
capacity and all fields of the two ordered partitions against the private
attached disk and GPT boundaries. Malformed, truncated, duplicate, deeply
nested or oversized reports refuse. As with inventory, trailing console
text outside a complete frame is ignored; internal interleaving refuses.
These checks run on both media attachments, virtio/NVMe 512-byte/4Kn targets,
AHCI 512-byte targets and the full-system installations without adding
boots. Partition inventory, actual formatting and detached firmware boots
retain their independent evidence. The report supplies no device identity,
destination eligibility, immutable plan or destructive consent.

NVMe adds the positive four-boot sequence at both 512-byte and 4096-byte
geometry through each media attachment: sixteen additional small-oracle
boots. Every disk gets its own QEMU nvme controller and one namespace;
logical and physical geometry follow that namespace into detached boots.
The reordered disk must move from nvme0n1p2 to nvme1n1p2 while retaining
its configured volume UUID. Inventory partition naming also inserts the
required p separator, as does the guest partition-refresh check. Optical
and USB sources remain sr0 and sda.
Controller discovery order can fail the strict pathname oracle just as
SCSI ordering can; accepting an unchanged path would lose that evidence.

The fixture accepts only nvme<digits>n<digits> whole-namespace names,
reads device/serial and permits trailing ASCII padding on the fixed-width
Identify Controller serial. Partitions and multipath namespace names do
not enter its target roster. The production inventory already normalizes
outer whitespace on descriptions, and production UUID discovery already
recognizes NVMe disks and partitions. Neither gains a new admission rule.
The kernel pins BLK_DEV_NVME and NVME_CORE built-in, enables PCI_MSI
and disables NVME_MULTIPATH, and checks the resolved values before building.
MSI/MSI-X is the chosen PCI profile, not an NVMe requirement: the driver
can fall back to INTx. Enabling PCI_MSI also changes interrupt selection
for other PCI drivers, so the existing virtio/AHCI desktop matrix must
pass with this kernel. This is direct PCI namespace coverage in QEMU,
not a physical NVMe, hotplug,
multipath, RAID or full-system NVMe compatibility claim. The twelve-boot
full-system matrix continues to use virtio and AHCI destinations.
