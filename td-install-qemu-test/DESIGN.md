# Native QEMU installation fixture

This crate is a diagnostic PID-1 program for `td-recipe-eval qemu-install`
and the live installation phase of `qemu-install-system`.
It is built from source through the ordinary Rust ladder and is never packed
in the system or graphical installer profile. It adds no user-facing device
admission, destructive consent, account configuration or signing identity.

The host oracle exclusively creates each sparse target inside its private
scratch directory and exposes it as a writable virtio disk with the fixture
serial. The ISO stays read-only. The fixture refuses outside PID 1 and admits
exactly one whole virtio installation target with that serial; it accepts
no command-line destination. This is test identification, not a real installer authorization
mechanism. Firmware variables are private copies, networking is disabled,
and the host boot runner owns deadlines and QEMU teardown. Each boot defaults
to 180 seconds; a positive TD_QEMU_BOOT_TIMEOUT_SECS overrides that limit.
A refusal keeps PID 1 alive for the host to collect its diagnostic and
terminate the VM at the deadline.

The complete crate tree is a local_source input pinned by
seed/seed-digests.txt. The catalog and affected-check routing include it in
local-source verification; it is not a production system input.

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
two fixed candidate paths: /dev/sr0 (SATA optical) and /dev/sda (USB). QEMU
adds an empty default CD-ROM in both attachments: /dev/sr1 during optical
boots (outside this candidate list) and /dev/sr0 during USB boots. A read-only
block-device open must succeed; Linux ENOMEDIUM (123) skips an empty drive
even when the driver publishes a placeholder capacity. Other open errors
refuse. Zero-capacity
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
reader and requires the expected virtio partition, then mounts it through
td-boot. A second reread must fail specifically with EBUSY while mounted;
no force or unmount fallback is accepted. The guest unmounts and requires a
final successful reread before emitting the partition-refresh evidence.
The host requires the exact configured UUID and /dev/vda2 in that evidence on both optical and USB installs. This exercises
partition publication during the same boot; firmware reboot cannot mask a
missing reread. UNSAFE.md §3 owns the td-init request; this crate still has
no raw syscall surface.

The live guest supplies `--trusted-key` to `td-install volume` instead of
the three publishing operands. The formatter initializes the publication
directories and key without a deployment or selector. The fixture requires
empty staged boot, deployment, incoming and @var directories, then deletes
its entire private scratch directory after partition refresh, then calls `td-boot install` with the resolved partition,
/source and the same read-only live public key. Successful mounted
publication and sync precede the direct-publication and installation markers;
the host requires both. The full cold-boot oracle still proves the expected
deployment is installed. The separate full-system diagnostic checks its
installed desktop under a RAM ceiling. Neither diagnostic admits operator
target capacity.

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
uses the destination alone; the second attaches a fresh blank virtio disk
first, moving the destination from /dev/vda2 to /dev/vdb2. Firmware still
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
All twenty small-fixture boots use fresh private firmware variables.

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
