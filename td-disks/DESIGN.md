# td-disks — extra volumes and removable media

`td-disks` mounts the machine owner's extra volumes: data disks other
than the td volume, such as a backup array or a previous system's disk
kept during a migration. Which volumes mount is per-machine state that
the session owner changes through one consented operation; nothing in
the recipe graph names a machine's disks, and nothing mounts a disk the
owner did not name. It also mounts removable media for the session
owner, without a consent, while the session is unlocked ("Removable
media" below).

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

FAT, exFAT, ISO 9660 and UDF, and any filesystem on USB or an SD card,
are removable media (below), not extra volumes.

Candidates are `sd*`, `vd*` and `nvme*n*[p*]` disks and partitions,
never device-mapper, md, loop, RAM or optical nodes, never a device
another block device holds (`holders/`), never a whole disk that has
partitions (its old superblock may outlive the repartitioning), and
never a device on a USB bus (reported: those are removable media).
Each is opened read-only and `O_NOFOLLOW`, and its node must be the
block device sysfs names before and after the open. Nothing is
written, scanned into the kernel, checked or repaired.

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
eSATA, Thunderbolt or hot-swap bay included, is trusted as named.
Between the probe and `/bin/mount` the device is named by path: a disk
swapped onto that node in between is what mounts. Physical
access defeats this boundary, as it does td-boot's.

## Removable media

Removable media are USB mass storage (`sd*` on a USB bus) and SD cards
(`mmcblkN[pM]` whose card reports type `SD`; soldered eMMC reports
`MMC` and is never media) carrying FAT, exFAT, ext4, ISO 9660 or UDF.
They are not named in advance and need no consent. Two halves keep it
to the privilege it already has:

- `[media]`, a root daemon, runs `td-disks media-serve OWNER_UID` and
  owns every mount. The owner's primary group must be its own (named
  for it, no other member). It binds `/run/td-disks/media.new` in
  root's `/run/td-disks` (mode 0755, refused if not root-owned or if
  group- or world-writable), hands it to the owner with `lchown` at
  mode 0600 and renames it to `media`, so only the owner and root may
  connect whatever the umask: filesystem permission, no
  peer-credential check. One request per connection, at most 256
  bytes, whole within five seconds however slowly it arrives: `mount
  DEVICE`, `unmount NAME`, `eject NAME` and `list`, answered `ok …` or
  `error …`. A name matches a mount point before a device. It rechecks
  everything the asker could have lied about: the device is on USB or
  an SD card, has media (non-zero size), no holder, is not a
  partitioned whole disk, is mounted nowhere, shares its disk with no
  partition the system holds or has mounted (so a td installed to a
  USB disk keeps its EFI system partition from the owner, and an eject
  of that disk is refused), and its node is the block device sysfs
  names before and after an `O_NOFOLLOW` open. Its type comes from the
  device, never the request: UDF's volume recognition sequence from 32
  KiB (BEA01 then NSR02/NSR03, 2 KiB or 4 KiB apart) or ISO 9660's
  primary descriptor there, then an exFAT or FAT boot sector, then
  ext4's superblock. An ext4 superblock that names an external journal
  (which would have the kernel open a device the stick chose) or
  carries its own mount options is refused. Root does not read those
  bytes itself. It hands the descriptor it opened, as stdin, to
  `td-disks media-identify`, started with an empty environment at `/`
  as UID/GID 65532, which no account names and nothing else runs as
  (td-time's network worker is 65534). The worker refuses to read if
  any uid or gid column is 0 or it holds a permitted, effective or
  ambient capability, and answers one line of at most 256 bytes,
  `none`, `ok WORD [LABEL]` or `error WHY`. Root accepts only a word
  it knows and maps the label through its own rule, and a failed or
  malformed answer fails the request. So a parser fault costs that
  request rather than the daemon. Code execution in the worker is
  bounded to an unprivileged uid and no further: nothing confines that
  uid, so it keeps the network and the device's bytes and may name any
  type, as a stick's own bytes can, or skip the ext4 refusal, whose
  purpose is to keep such a stick out of the kernel's reach. A process
  it forks outlives the request, since only the worker itself is
  killed: holding the device open stops the kernel rescanning that
  disk, and with no Yama in this kernel it can attach to later workers
  of the same uid, reading their sticks and forging their answers,
  until `media` restarts. It has no process or memory limit either.
  td-disks forbids `unsafe`, so a namespace or seccomp filter for the
  worker is a separate design. The mount's options, source and target
  stay root's. Those reads get fifteen seconds and `/bin/mount` or
  `/bin/umount` sixty, then the request fails and a killed child is
  reaped on another thread, so a stalled device holds the daemon only
  that long, once it has opened; the open itself has no deadline. Every
  second it lazily unmounts what a vanished device held, or a card
  whose reader now reports no media: the whole disk's size drops to 0
  while a mounted partition keeps its own, as the kernel rescans no
  partitions while one is open. Without udev
  nothing opens a card reader's LUN, and the kernel notices a card
  arriving or leaving only on an open, so every two seconds a second
  thread opens, `O_NONBLOCK`, each whole USB disk sysfs calls
  removable, as udev's poll does.
- `[media-watch]`, a session daemon run as the owner through `td-login
  exec-primary`, lists candidates every second. When one is new it
  asks the compositor's control socket `lock-state`
  (`td-compositor/DESIGN.md` §15), which admits only the human peer.
  On `unlocked` it asks `media-serve` to mount each new candidate once;
  one root did not answer is asked again next time. On `locked` it
  passes the new candidates over, logging each, until they leave and
  return: a stick inserted at the lock screen, or present at a boot
  that starts locked, does not mount when the session unlocks either.
  `td-disks media mount DEVICE` mounts one explicitly. Any other answer,
  an unreachable compositor included, decides nothing until the next
  answer. Locking unmounts nothing. The watcher keeps its decisions in
  memory, so a restart of it mounts what it had passed over.

A medium mounts at `/run/media/<owner>/<label>`. `/run/media` is
root-owned 0755 and `/run/media/<owner>` root-owned 0750 in the
owner's private group, both on `/run`'s tmpfs, so the owner cannot
swap a mount point for a link and no other account reads a disc or an
ext4 stick. An existing directory is brought to that mode only if it
was never group- or world-writable; otherwise serve refuses it. The
label is the FAT BPB label, the ISO 9660 volume id or the ext4 volume
name, mapped to `[A-Za-z0-9._-]` (other bytes as `_`),
at most 32 characters, without a leading `.` or `-`, and never a
device name. With no usable label the name is the type and the device
(`vfat-sdc1`), and a taken name gets `-2` to `-9`. Every mount is
`nodev,nosuid,noexec`. FAT and exFAT get `uid=,gid=` of the owner,
`umask=077` and `errors=remount-ro`. ext4 gets `errors=remount-ro`,
which overrides a superblock's `errors=panic`, and keeps its on-disk
owners, as an extra volume does. ISO 9660 and UDF are read-only, UDF
so its write paths stay unreached from a stick, with `uid=,gid=`. A
device sysfs reports read-only mounts read-only. The mount is
`/bin/mount`, as above.

`td-disks media` lists the mounts. `td-disks media unmount NAME`
unmounts one (refused while in use). `td-disks media eject NAME`
unmounts every mount on that disk, then, for a USB disk, writes `1` to
its SCSI device's sysfs `delete`, so the kernel flushes its cache and
detaches it. No ioctl powers a port off. A live boot runs neither
daemon: its own medium is a USB stick.

What this does not establish: any process running as the owner can ask
for a mount while the session is locked. The lock rule is the
watcher's, and it keeps media inserted at the lock screen from being
parsed by itself, not the owner's own programs from asking. A resume
from a sleep begun outside the Sleep action locks only on the first
input after it (td-login/TOKEN-LOGIN.md, "Session lock"); until then
`lock-state` says `unlocked`, and a stick swapped in during that sleep
mounts. Mounting hands the medium's bytes to the kernel's FAT, exFAT,
ext4, ISO 9660 or UDF code, as any mount does, so a hostile stick
reaches that parser once the owner is at the unlocked session. A USB
device can be any device: the kernel binds what it claims to be to
every built-in driver whatever td-disks does. A stick pulled without an
eject loses what was not written back.

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
