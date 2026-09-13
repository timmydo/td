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
The native `qemu-install` diagnostic extends the firmware evidence by
mounting the ISO read-only in Linux and installing its signed payload files.
It exercises both attachments with the same image and then boots the
destination with media detached. Its exact bounds and fixture-only device
conventions are specified in [the fixture design](../td-install-qemu-test/DESIGN.md).
