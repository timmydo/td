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
