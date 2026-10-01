# Hybrid installation media format

The offline installer uses one image for optical and USB boot, as required
by [INSTALLER.md](INSTALLER.md). `engine/src/iso9660.rs` defines the initial
metadata writer. It is a pure formatter: it neither opens a destination nor
supplies the FAT contents, deployment, signatures or a live boot profile.
Its unit tests establish layout properties; firmware and installed-session
proofs remain separate activation requirements.

The format uses a primary ISO-9660 descriptor, a flat root directory and
level-3 file sections. It follows
[ECMA-119](https://ecma-international.org/wp-content/uploads/ECMA-119_6th_edition_december_2025.pdf)
and the EFI optical boot rules in
[UEFI 2.10 §13.3.2.1](https://uefi.org/specs/UEFI/2.10/13_Protocols_Media_Access.html#iso-9660-and-el-torito).
No Joliet, Rock Ridge, UDF, legacy BIOS boot code or third-party image
formatter is needed for this profile.

The ISO logical block size is 2048 bytes. A protective MBR and the primary
512-byte-sector GPT occupy the ISO system area. Descriptor blocks 16–18,
boot catalog 19, little/big-endian root path tables 20/21 and the root
directory from block 22 precede a one-MiB-aligned ESP. The backup GPT ends
at the image's final byte, after the payload and inside the ISO volume.

The caller supplies a FAT32 ESP of 64–512 MiB, in whole MiB units. Both the
GPT ESP entry and the EFI El Torito no-emulation entry identify that same
region. The catalog platform is 0xEF and its validation checksum sums to
zero. The catalog sector count is one, UEFI's encoding for image-to-disc-end;
the FAT BPB records the filesystem's exact smaller size. ISO root entries
`BOOT.CAT;1` and `EFI.IMG;1` expose the catalog and ESP respectively.

Payloads are at most 64 flat files of at most 64 GiB each. Names contain
uppercase ASCII letters, digits, underscores and at most one dot. The name
and extension separator occupy at most 31 bytes before the `;1` version.
The formatter inserts a missing dot and rejects normalized duplicates,
reserved names and invalid names. Records follow ECMA-119 §10.3 name and
extension ordering, with shorter prefixes first and a fixed version one. Large
files use consecutive, block-aligned file sections with continuation flags;
the caller still streams one contiguous file. No 32-bit length truncation
is permitted.

No wall clock is read. Directory timestamps use the Unix epoch and volume
timestamps are unspecified. The caller supplies nonzero distinct disk and
ESP GUIDs; deterministic inputs produce identical metadata. The fixed
volume label is `TD_INSTALL`.

The result supplies metadata extents, the ESP offset, total size and exact
payload placements. All unwritten space must initially be zero, including
padding. The composition caller must write every declared byte, detect
short reads and synchronize publication. These format APIs grant no device
access or erase authorization. A flashed image on a larger physical device
has a backup GPT at the image boundary; v1's QEMU attachment uses the image's
exact size, and hardware compatibility remains a separate milestone.

## Retained image composition

The host control-plane command consumes already-built, stable inputs:

```text
td-recipe-eval compose-iso OUTPUT KERNEL INITRAMFS [ISO-NAME=FILE ...]
```

It writes the kernel to `EFI/BOOT/BOOTX64.EFI` and the supplied live
initramfs to `EFI/BOOT/INITRD`. Optional named payloads occupy the ISO root.
The boot inputs are nonempty regular files, each at most 256 MiB. The FAT32
ESP uses the smallest of 64, 128, 256 and 512 MiB that fits both files and
metadata; their combined size must fit 512 MiB less FAT metadata. Each of at
most 64 payloads is a nonempty regular file of at most 64 GiB. Level-3
sections permit files larger than four GiB. Files stream
through held descriptors; composition does not buffer the deployment in RAM.

For example, after building and provisioning a live initramfs and signing a
deployment outside derivations, its assembly inputs can be named explicitly:

```text
td-recipe-eval compose-iso installer.iso bzImage live.cpio \
  BZIMAGE=deployment/bzImage INITRAMFS.CPIO=deployment/initramfs.cpio \
  ROOT.EROFS=deployment/root.erofs MANIFEST=deployment/manifest \
  MANIFEST.SIG=deployment/manifest.sig SELECTOR.CPIO=selector.cpio
```

This is a byte-composition primitive, not a build/profile selector or an
installer release command. It does not sign, parse deployment manifests,
authenticate payloads, provision keys, validate PE executability or create a
live environment. The profile producer must supply the validated source-built
boot artifacts, signed deployment, and matching public trust roots required
by INSTALLER.md. Arbitrary command-line files carry no source-bootstrap claim.
No host executable is added to the image by this tool. The same writer is
used by the optical/USB boot and native installation oracles; those callers
own their diagnostic profiles and signing. A complete desktop installation
profile remains an independent activation requirement.

The output parent must exist and support hard links and directory sync.
The caller controls its writable ancestors and keeps the namespace and
input contents stable during composition. Symlink and special-file inputs
are refused using td-boot's shared real-file opener; input lengths are
checked again after streaming. This pins the opened files, not a snapshot
against in-place writes, and inherits the opener's documented device-swap
residual. Same-sized concurrent content changes are outside the contract.

Input admission and layout construction precede staging. Internal placement
checks and input-length rechecks also guard streaming. The writer creates a
private sibling directory and an exclusive file with permissions no broader than
0600, initially sparse and zero-filled. A private hard-link probe checks
filesystem support before streaming; later publication can still fail.
It syncs the complete image, then publishes it with a no-replacement hard
link and syncs the parent directory.
Existing destinations, including dangling symlinks and block devices, are
never overwritten. A failed copy publishes no output. A failure of the last
directory sync reports an error even though the complete output may exist.
Ordinary cleanup removes only the owned temporary file and directory;
process death can leave a private `.td-iso-*` staging directory. There is no
replace or flash-device option. Flashing onto physical media and selecting a
physical installation target remain separate operations.

The metadata GUIDs and timestamps are fixed for reproducible media bytes;
these GUIDs are template identifiers, not unique installation or disk
identities. Identical stable inputs produce identical ISO contents. Firmware
and filesystem hardware compatibility still require the v2 device tests,
including simultaneous attachment of two media with these template GUIDs.

## Live installation media

`./build-iso [--out FILE] [--force]`, the wrapper for `td-recipe-eval
build-iso`, is the live profile's producer. It builds `system-x86-64`,
verifies the deployment and selector against their build manifests, and
generates a signing key for this run only, as `bundle` does; the medium
carries its public half and nothing retains the seed. It signs a copy of the
deployment manifest and writes, through the same writer as `compose-iso`:

- `EFI/BOOT/BOOTX64.EFI`: the deployment's kernel;
- `EFI/BOOT/INITRD`: the live selector, which is the stock selector with the
  public key at `etc/td/deployment.pub` and the marker `etc/td/live-media`
  appended, and no `etc/td/volume-uuid`;
- the ISO root: `BZIMAGE`, `INITRAMFS.CPIO` and `ROOT.EROFS` streamed from
  the verified store deployment, the manifest, and its new signature.

The default output is `dist/td-install-x86-64.iso` in the checkout; `--out`
names another file, relative to the caller, outside the ladder work tree
(which `clear-store` deletes). An existing file is refused. `--force` admits
only a regular file whose primary volume descriptor identifies a td
installation medium, as td-boot identifies one. The image is staged as a
hidden sibling, `.NAME.PID.partial`, which process death or a refused
publication can leave behind; a run whose staged name is taken is refused
before its build and never removes that file. A destination that was absent
is published with a no-replacement hard link, so a file created there during
the build is kept and the run fails. An admitted ISO is replaced by a rename,
checked immediately before to be still the same file and still a td
installation medium, under an exclusive lock on the directory that other
build-iso runs take; if it is gone the image is linked as for an absent one,
and otherwise it is kept. A process that ignores the lock can still replace
the file between the check and the rename. A refused publication keeps the
staged image and the error names it. The directory is synced after
publication.

Firmware boots the selector, which takes the "Live boot" path below. Each ISO
has its own key, so a deployment signed for one medium does not authenticate
under another, and an installed system's key is unrelated to the medium's.
The medium holds no private key, no volume identity and no operator data.

## Interactive QEMU run

`./test-iso ISO [--usb]` boots an existing nonempty regular ISO through OVMF
and host QEMU with a graphical host display. Optical attachment is the default;
`--usb` attaches the same file as USB mass storage. The runner opens the ISO as
a regular file and
passes that held read-only descriptor through QEMU's standard input, which
QEMU opens as `/proc/self/fd/0`. Path replacement after admission cannot
redirect it to another file. The media is read-only, networking is disabled and
the sole writable destination is a fresh private 16 GiB sparse raw file.
The runner tries KVM on x86-64 when `/dev/kvm` opens read-write, then falls
back to software emulation if KVM initialization fails. `TD_QEMU_ACCEL=tcg`
forces emulation; `TD_QEMU_ACCEL=kvm` requires usable KVM. Firmware variables
are private copies. No operator destination disk or block device is selected.
After the first
QEMU exit, typing `boot` starts a cold boot of the destination with
the installation media detached and fresh firmware variables. The command
does not inject a kernel, initramfs or command line, or interpret a guest
success marker. It returns QEMU launch and exit errors, not an installation
verdict: the operator inspects the interactive installer and installed desktop.
Pressing Enter, typing another answer or reaching input EOF skips that boot.
A reboot of the installed guest ends the second QEMU session; it does not
start a third boot.
The private files are removed when the command returns normally; Ctrl-C or an
abrupt process death may leave its `td-test-iso-*` directory under the checkout's
`target/` directory (or `TMPDIR` when set). Remove that run's leftover directory
manually. QEMU reports target-disk I/O errors rather than pausing indefinitely.
Both boot stages write serial logs there for inspection while the command runs.
On QEMU failure, the final 8192 bytes of the serial log are
printed before cleanup. Set `TMPDIR` to choose where the sparse disk grows.
`TD_QEMU_EFI_CODE` and `TD_QEMU_EFI_VARS` select the same optional
firmware pair as the existing firmware oracles.

This runner is useful for a retained development ISO now. The complete
desktop installer producer and its automated end-to-end oracle remain the
activation requirements in INSTALLER.md.

## Repeatable firmware oracle

`td-recipe-eval qemu-boot-media` builds the declared source-built kernel
and tiny diagnostic initramfs and assembles one disposable hybrid image
with the Rust ISO and FAT writers. It boots those same bytes first as
optical media, then as USB mass storage behind an xHCI controller. Both
boots use cold private firmware variables and read-only media. Networking
is disabled; firmware loads the kernel and initrd from the image without
QEMU kernel injection or a command-line override. The
firmware discovery and configuration requirements are the same as
`qemu-boot-uefi` in DESIGN.md.

Both attachments must reach the diagnostic initramfs's actual userspace
marker. This proves firmware loading of the shared ESP, kernel and initrd;
it does not prove the kernel can mount the ISO, authenticate a deployment,
run the compositor or boot an installed disk. Those remain live-profile
and installed-session activation requirements. The command accepts no
output or device destination; exclusive files in its private scratch
directory are removed on completion.

## Linux payload access

The source-built kernel enables ISO9660, SCSI disk and CD-ROM, AHCI SATA,
and USB mass-storage support as built-ins. The recipe checks the resolved
configuration so media access needs no modules from the media it must read.
It also builds in the RAM block driver with one device, `/dev/ram0`, created
at boot for the live profile's volatile volume. The device allocates pages
only when written and frees them on discard. The live selector sizes it with
`brd.rd_size=` (KiB) on the command line it hands the deployment (see "Live
boot"). An installed boot passes no size and keeps an unused device of the
default size, which holds no memory. Destination discovery
never offers it, because its allow-list admits only virtio, SCSI-named and
NVMe disks.
The native `qemu-install` diagnostic extends the firmware evidence by
mounting the ISO read-only in Linux and installing its signed payload files.
It exercises both attachments with the same image and then boots the
destination with media detached. Its exact bounds and fixture-only device
conventions are specified in [the fixture design](../td-install-qemu-test/DESIGN.md).

## Live boot

A live boot runs the signed system deployment from the medium, with the root
image read-only and all writable state in RAM. It reuses the installed boot
chain: a selector authenticates and kexecs, and the deployment initramfs
mounts a td volume named by `td.volume=` and loop-mounts `root.erofs`. The
differences are where the selector finds the deployment, where the root image
stays, and what backs the volume.

Install media carry the signed deployment in the ISO root as `BZIMAGE`,
`INITRAMFS.CPIO`, `ROOT.EROFS`, `MANIFEST` and `MANIFEST.SIG`
(`td-boot/src/protocol.rs` `MEDIA_DEPLOYMENT_FILES`). Linux mounts the medium
`ro,nodev,nosuid,noexec` with ISO-9660 `map=normal`, which shows those names in
lowercase. td-boot reads the lowercase names, so `bzImage` is `bzimage` on the
medium, and a live boot copies or renames no payload. Its source reader,
behind `validate-source`, `install` and `publish`, also accepts `bzimage` for
`bzImage` (the one name the mount changes) and refuses a directory holding
both as different files, so the mounted medium is a source directory as it
stands; the bytes are verified against the manifest whichever name they came
from, and publication writes `bzImage`. A published deployment is still read
only under its own names.

td-boot identifies the medium by its primary volume descriptor: type 1,
`CD001`, version 1 and the space-padded volume identifier `TD_INSTALL`, at
2048-byte sector 16 of a whole optical, SCSI-named, virtio or NVMe device.
Partitions are not candidates. It opens each candidate read-only by its
sysfs-verified device number and mounts the held descriptor, so a renamed
node cannot substitute another device; the device and inode checks are
repeated after the read, as the volume probe does. An empty drive
(`ENOMEDIUM`) is not a medium, and a device that cannot be read is skipped
with its reason kept for the not-found error, so a card reader or failing
disk cannot stop a live boot. A node or sysfs value that is still appearing
makes the whole scan incomplete: nothing is selected, because that device
may be a second medium, and the scan is repeated. Discovery waits up to 30
seconds for slow USB enumeration. More than one medium is refused rather
than chosen between: attach only the one to boot. Probes run one device at a
time with the same bound as volume discovery: a read that stalls is ended by
the kernel's command timeout, not by td-boot. The descriptor is an identity,
not a credential; authenticity comes from the signature below.

The live selector is the stock selector initramfs with two appended entries:
the trust root at `etc/td/deployment.pub`, as for an installed selector, and
the marker `etc/td/live-media` holding `td-live-media-v1`. It has no
`etc/td/volume-uuid` and no `etc/td/boot-measurement` policy. `td-boot
live-boot MOUNTPOINT CMDLINE` refuses without the exact marker, and refuses a
selector carrying either of those files, since it would apply neither. Then
it:

1. reads the trust root and takes half of `MemTotal` as the RAM disk size,
   refusing less than 512 MiB;
2. draws a fresh version-4 volume UUID from `/dev/urandom`;
3. finds and mounts the medium;
4. authenticates the manifest under the trust root before parsing it, and
   verifies `bzImage` and `initramfs.cpio` against it;
5. kexecs them with the base command line plus `td.volume=UUID`,
   `td.deployment=ID`, `td.live=1`, `td.trust=KEY` (the trust root as 64
   hex digits) and `brd.rd_size=KIB`.

It does not hash `root.erofs`: with no previous deployment to fall back to,
`live-root`'s hash after kexec is the one that decides. Every payload hash
has the residual `root-loop` has: the medium is read again after it is
hashed (by kexec, and by the loop through the page cache), so a hostile
device controller can serve different bytes later.

It does not extend PCR 11, which stays zero in a live session. Root there can
therefore reproduce an installed selector's PCR 11 value by extending the
same event. No protector binds PCR 11 alone (`ENCRYPTION.md`), so this grants
nothing today; a protector that ever did would need the live selector to cap
PCR 11 first.

The base command line may not carry the literal tokens `td.volume=`,
`td.deployment=`, `td.live=`, `td.trust=` or `brd.rd_size=`. The last three
are refused in an installed selector's base line too, so firmware cannot
steer an installed boot into the live branch or hand it a key. Only those
spellings are refused: the kernel also accepts `brd.rd-size=` and
`ramdisk_size=`, but td-boot's token comes last and wins. A live base line
may not contain a bare `--`, after which the kernel hands every word to init
and `brd.rd_size=` would never reach brd. Whoever writes the base line can
already choose `rdinit=`, so these checks keep the handoff well formed
rather than defend against its author.

In the deployment initramfs, `td-boot live-root MOUNTPOINT ID LOOP` requires
`td.live=1` and `td.deployment=ID` in `/proc/cmdline`, finds and mounts the
medium again, and
requires its manifest to hash to the handed-off id, which the selector
authenticated. A different medium carrying the same signed deployment is
therefore acceptable and any other is refused. It hashes `root.erofs` against
that manifest and attaches the verified descriptor to a read-only loop, as
`root-loop` does for an installed volume, and leaves the medium mounted
because the loop holds its file. On failure it unmounts.

The deployment initramfs takes the live branch only on `td.live=1`, which
only `live-boot` hands over, and requires `td.volume=` with it. It runs
`live-root`, then `td-boot live-seed MOUNTPOINT ID SEED`, which requires the
same handoff and exactly one well-formed `td.trust=`, rechecks that the
mounted medium's manifest hashes to the id, authenticates it again under the
handed-off key, and stages only what the booted system reads from its volume
in a directory that must not already exist: `td/boot/current` naming the
deployment, its `manifest` and `manifest.sig`, `td/trusted.pub` holding the
handed-off key as an installed volume holds its key, an empty `td/incoming`
and `@var`, with directories 0755 and files 0644. A medium without a
signature, or a handoff naming a key the deployment was not signed under, is
refused before anything is staged. `mkfs.btrfs` formats `/dev/ram0` from
that tree with the handed-off UUID, the `td-system` label and a writable
`@var` subvolume, as td-install formats an installed volume. From `on-volume
mount-root` on, the boot is the installed one: the volume is found by UUID,
the root loop is already bound, `@var` is mounted, and the medium is moved
to `/run/td-media` so it stays mounted under the loop. The live volume's
`td/trusted.pub` is the key the running deployment was authenticated under,
both before kexec and in `live-seed`, so a live installer can authenticate
its source under the key that booted it. A live session has no bundled
`td/source`, and everything it writes is lost at power-off.

`build-iso` ("Live installation media") provisions the live selector.
