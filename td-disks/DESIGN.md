# td-disks — extra volumes

`td-disks` mounts the machine owner's extra volumes: data disks other
than the td volume, such as a backup array or a previous system's disk
kept during a migration. Which volumes mount is per-machine state that
the session owner changes through one consented operation; nothing in
the recipe graph names a machine's disks, and nothing mounts a disk the
owner did not name.

## What a volume is

An extra volume is a Btrfs or ext4 filesystem, named by its filesystem
UUID and given a short name, its mount point under `/mnt`.

- Btrfs is identified by its primary superblock at 64 KiB, by identity
  alone: the magic, the recorded offset 64 KiB, its crc32c checksum
  (another checksum type is refused and reported) and a device item
  naming the same filesystem (its `metadata_uuid` under
  `INCOMPAT_METADATA_UUID`). Its `fsid` is the UUID every member of a
  multi-device filesystem shares; `devid` and `num_devices` say which
  member this is, and devid 0, a running replace's target, is refused.
  Node size, features and profiles are not td's to admit: td-btrfs's
  reader, which holds td's own volume to td's mkfs, is not used, and the
  kernel decides whether the filesystem mounts.
- ext4 is identified by the magic 0xEF53 at 1 KiB + 0x38 and its
  `s_uuid`. An external journal device (`INCOMPAT_JOURNAL_DEV`) is not a
  filesystem. ext2 and ext3 carry the same superblock and mount through
  the ext4 driver (`EXT4_USE_FOR_EXT2`).

FAT, exFAT and ISO 9660 are removable media's, not extra volumes.

Candidates are `sd*`, `vd*` and `nvme*n*[p*]` disks and partitions,
never device-mapper, md, loop, RAM or optical nodes, never a device
another block device holds (`holders/`), never a whole disk that has
partitions (its old superblock may outlive the repartitioning), and
never a device on a USB bus (reported: removable media are another
increment's). Each is opened read-only and
`O_NOFOLLOW`, and its node must be the block device sysfs names before
and after the open. Nothing is written, scanned into the kernel, checked
or repaired.

A name is 1–16 bytes of `[a-z0-9-]` not starting with `-`, so the
longest request fits the settings intake's 79-byte payload.

## State

`/var/lib/td/volumes/table` holds the table: a `td-volumes-v1` header
and one `NAME UUID KIND MODE` line per volume, at most 16, no name or
UUID twice. KIND is `btrfs` or `ext4`; MODE is `data`
(`nodev,nosuid,noexec`) or `exec` (`nodev,nosuid`). It is written only
under the exclusive `/var/lib/td/volumes/lock`, by whole-file durable
replacement (`td-fs::replace`), mode 0644. A table that is not a bounded
regular file owned by root and writable only by it, or that holds any
line td-disks would not write, refuses: boot then mounts nothing and
the status says why. `/var/lib/td` and the state directory must be
root-owned and not group- or world-writable.

## Changing the table

`td-disks add NAME UUID btrfs|ext4 [--exec]` and `td-disks remove NAME`
ask td-authd, as `td-authd volume add|remove ...`, on the settings intake
hostname and time changes use (td-authd/DESIGN.md, "Extra volumes
through the protected settings intake"). The `manage-volumes` elevation
operation must grant the owner and the requester. Consent tag 42 binds
the exact canonical change; the review shows the name, the UUID, the
filesystem kind, that it mounts under `/mnt` at every boot, whether
programs on it may run (setuid bits and device nodes never apply), that
files keep their on-disk owners, and that any disk carrying this UUID
mounts. A remove shows that a busy volume is refused.
After the owner's commit td-authd runs only `/bin/td-disks apply
add|remove ...` with the same words.

`apply` takes the lock and rechecks the change against the table and the
attached devices. An add mounts the volume first and saves the table
only once it is mounted (and unmounts it again if the save fails). A
remove unmounts first, refusing while the volume is busy, then saves.
A refusal leaves both the table and the mounts as they were. Only the
changed row is mounted or unmounted; the status is then republished
without touching the other rows.

td-authd allows `apply` sixty seconds and then kills it. A kill after
the mount and before the save can leave the volume mounted but not in
the table, and its `/bin/mount` child may still finish; the next boot
then does not mount it, and status shows the table as it is.

The unprivileged CLI cannot open block devices, so it names the kind
itself; `apply` refuses an add whose kind does not match what is
attached.

## Mounting

The `[volumes]` oneshot runs `td-disks mount-all` after td-firstboot.
Nothing requires it, and it never waits for a disk. It mounts a 64 KiB
`nodev,nosuid,noexec` tmpfs over the image's empty, read-only `/mnt`
once, then for each row finds every attached member with that UUID and
kind. A row mounts only when its members are the whole filesystem (one
ext4 device; as many distinct Btrfs devids as every member's agreed
device count, gaps allowed, since Btrfs numbers an added device past the
highest and never reuses a removed one's)
and no member of that filesystem is mounted anywhere already, which
also keeps the td volume itself from being mounted again. It runs the
image's `/bin/mount` (td-init's applet, so no new syscall surface) with
the row's flags and, for Btrfs, `device=/dev/X` for each member so a
multi-device filesystem assembles without a scan ioctl. An absent,
incomplete or refused volume is reported and skipped; the oneshot exits
non-zero when any row did not mount.

Files keep their on-disk owners: a disk written under another system's
UIDs shows those UIDs. td does not map them.

## What this does not establish

A volume's identity is its UUID, which is not secret: the table and the
status are world-readable, and anyone can write a filesystem carrying a
chosen UUID. While the owner's disk is detached, an internal disk
fitted with that UUID mounts in its place at the next boot, its
contents handed to the kernel's filesystem code before anyone signs in,
and in `exec` mode its programs may run (never setuid, never device
nodes). Two devices claiming one ext4 UUID, or one Btrfs devid twice,
refuse instead. USB devices are not candidates, which keeps a stick
plugged in from standing in, but any other disk, one in an external
eSATA, Thunderbolt or hot-swap bay included, is trusted as named. Between the probe and `/bin/mount` the device is named by path:
a disk swapped onto that node in between is what mounts. Physical
access defeats this boundary, as it does td-boot's.

## Status

`mount-all` and `apply` publish `/run/td-disks/status` (mode 0644):
each row with its outcome, every Btrfs and ext4 member found with its
UUID (and devid of count), and a note for each device that could not be
read. `td-disks status` prints it for anyone, so the owner can find a
disk's UUID without privilege.

## Not in this increment

Encrypted extra volumes, Btrfs subvolume selection, idmapped mounts for
another system's UIDs, mounting a disk attached after boot without a
consent (a later `apply` or reboot mounts it), fsck, and a compositor
control.
