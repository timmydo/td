# Native QEMU installation fixture

This crate is a diagnostic PID-1 program for `td-recipe-eval qemu-install`.
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
but this diagnostic still constructs only a small deployment. The private
key remains on the host outside derivations. The fixture calls the
actual td-install layout and volume primitives, including td-boot's verified
publication. Its success marker follows both successful commands and sync.

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
fallback. The formatter still stages the deployment and Btrfs image in RAM.

The host chooses the UUID from the run's throwaway provisioning public key
before assembling the ISO. Its canonical text is placed in the live and
selector initramfs at `/etc/td/volume-uuid`. The live fixture supplies that
value through `td-install volume --uuid`; formatting cannot silently choose
a different identity. Optical and USB destinations deliberately share that
run's UUID but are never attached together.

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
success marker. Both optical and USB installation boots
use the same ISO bytes and fresh destination disks.

The fixture's serial convention does not implement production installation
admission. Volume discovery uses the production read-only primitive under
the oracle's fixed topology; it does not establish exclusive admission.
It does not launch the compositor, configure an account, retain an
installation signing key, or test a full desktop deployment.
The final installer still needs the activation evidence in
../td-install/INSTALLER.md, including a complete desktop installation and settings.

All operations use safe Rust and existing td-init/td-boot applets. No syscall
surface or external dependency is added. Unit tests cover refusal outside
PID 1; the host oracle is the executable integration test. A serial marker is
accepted only after its named operation and persistence barriers finish.
