# td-btrfs: new Btrfs filesystems from a directory tree, and their check

td-btrfs makes the single-device Btrfs filesystem every td volume is:
td-install's installed and device-bound volumes, the live profile's
`/dev/ram0`, and the host oracles' fixture volumes. It replaced
btrfs-progs, whose only use in td was `mkfs.btrfs --rootdir --subvol`
and, in tests, `btrfs check`, `restore` and `inspect-internal
dump-tree`. It is pure `std`, depends on no crate (td-fs only for its
tests), forbids `unsafe` and adds no syscall surface to `UNSAFE.md`:
its I/O is std's directory walk, `lstat`, positioned reads and writes,
`set_len` and `fsync`, and its one open flag is `O_EXCL` on a block
device. It never mounts, never issues an ioctl and never discards.

One binary serves two names. Invoked as `mkfs.btrfs` (by its argv[0]
basename, as `/bin/mkfs.btrfs` links it), it is the formatter; invoked
as `td-btrfs` it takes `mkfs`, `check`, `list` and `restore`. The
image links only `/bin/mkfs.btrfs` to it; the other verbs, in the same
ELF, are for tests and hosts.

## The formatter's arguments

The btrfs-progs spellings td's callers use, and nothing else:
`-b/--byte-count SIZE` (bytes, or a binary `K`…`E` suffix),
`-U/--uuid UUID` (canonical, nonzero), `-L/--label LABEL` (at most 255
bytes, no NUL or newline), `-r/--rootdir DIR`, any number of
`--subvol [rw:]PATH` (a directory below the root, a plain relative
path; nesting is allowed; `ro:` and the default-subvolume kinds are
refused), `-q/--quiet`, and exactly one target. Any other option is
refused. Without `-q` a short report goes to stdout; errors go to
stderr as `ERROR: …` with the failing path and errno text, and the exit
status is 1.

The target must exist. A block device is opened read-write with
`O_EXCL`, so one that is mounted or held exclusively is refused with
`EBUSY`, and its size is its end offset; `--byte-count` may not exceed
it. A regular file shorter than `--byte-count` is grown to it with
`set_len` once every input is accepted (the host oracles pass an empty
file); a longer one keeps its
length, the filesystem occupying its first `--byte-count` bytes, as
td-install's pre-sized scratch image requires. Without `--byte-count`
the whole target is used. The size is rounded down to 4 KiB. A target
reached through a symlink (a `/dev/mapper` name) is followed.

## What is written

The feature set is btrfs-progs v7.0's default, which td's kernel
mounts and td-boot's superblock reader admits: CRC-32C checksums, 16 KiB
nodes, 4 KiB sectors, incompat `MIXED_BACKREF | BIG_METADATA |
EXTENDED_IREF | SKINNY_METADATA | NO_HOLES`, compat_ro
`FREE_SPACE_TREE | FREE_SPACE_TREE_VALID | BLOCK_GROUP_TREE`, one
device. Every block, item and root is written in generation 1.

The layout is td's own. The first MiB is the boot area and the primary
superblock. A DUP system chunk of 8 MiB, a DUP metadata chunk and
single data chunks follow in device order; a chunk's logical address is
its first stripe's device offset, which keeps logical ranges disjoint.
No device extent covers a superblock copy (64 KiB, 64 MiB, 256 GiB): a
chunk that would is moved to the next MiB past the copy's 64 KiB stripe,
so no block group carries the range the kernel excludes around one and
the free space tree needs no exclusion. The metadata chunk is the
kernel's default stripe (256 MiB, 1 GiB above 50 GiB), held to a tenth
of the device but never below 32 MiB, and grown if the trees need more.
Data chunks hold exactly the data rounded to a MiB, at least 8 MiB, at
most 1 GiB each; the rest of the device is left unallocated for the
kernel. A device too small for all of it is refused before anything is
written. The metadata chunk never shrinks toward what the trees need,
so the smallest empty filesystem is about 105 MiB, where btrfs-progs
goes lower; td's volumes are gigabytes.

The trees are those btrfs-progs makes: root (with the `default`
directory items of `ROOT_TREE_DIR`), chunk, device (with its device
statistics item), extent, checksum, UUID (every subvolume's UUID, and
`uuid_tree_generation` set to the generation so the kernel does not
rescan), free space (one extent per block group from the end of what is
used), block group, data relocation, the top-level tree and one tree
per subvolume. Each tree is built whole from its sorted items: leaves
filled greedily, then nodes. The extent, free space, block group and
root trees describe the others' blocks and so their own; they are built
to a fixed point, and a metadata chunk too small is grown and the plan
redone, before any write.

Writing is ordered so that a failure or a crash leaves no filesystem:
first both ends of the device (2 MiB each) and every superblock copy's
place are zeroed, `fsync`, then the data is copied and checksummed,
then every metadata block is written to each of its stripes, every
superblock copy is built, `fsync`, then each copy that ends before the
device does (where the kernel keeps one) is written, `fsync`. Nothing else on the device is
written, discarded or read, which is what `td-install/ENCRYPTION.md`
relies on for a fresh mapping.

## What is copied

The root directory's tree is read into memory first. The root itself
must be a directory, not a symlink to one. Directories,
regular files and symbolic links are copied; a device node, FIFO,
socket or other type is refused, naming the path, as is a name longer
than 255 bytes and the target itself appearing in the tree. Extended
attributes are not copied, and nothing td stages carries one. Each
inode records the source's numeric uid and gid, its mode bits
(setuid, setgid and sticky included) and its atime, mtime and ctime as
`lstat` reports them; otime is zero. A regular file reached by several
paths in one subvolume (the same device and inode) is one inode with
that many links; across a subvolume boundary each copy is its own
inode. The source's own link count, which counts links outside the
tree, is never used.

Entries are named in byte order. A directory's children are numbered
together, from 257, its subdirectories then expanded depth first;
directory indexes start at 2; subvolumes are numbered from 256 in the
order found, each after the tree that holds it. So the trees depend only
on the source and the drawn UUIDs. A file of 1 to 2048 bytes and every
symlink target are stored inline; larger files get extents of at most
128 MiB, split at chunk ends, packed in inode order with their sector
padding zeroed. A file whose length changes while it is read is refused.

## The check

`td-btrfs check IMAGE` reads every tree block, every copy of it, and
every checksummed data sector, and fails (exit 1, one `error:` line
each) on any inconsistency. It accepts a filesystem td-btrfs wrote and
one the kernel has since modified, including after an unclean stop:

- superblocks: the primary admitted as td-boot does; every copy within
  the filesystem valid, of the same filesystem, not newer than the
  primary, and equal to it but for the log root fields a log sync
  writes to the primary alone;
- blocks: checksum, address, filesystem and chunk-tree UUIDs, level,
  owner, the generation its parent expects, key order within and
  between blocks, exact leaf packing; no block reached twice, by its
  own tree or another, which is refused when it is reached so a shared
  subtree is not read again; a chunk that is not sector aligned, or
  whose address or stripe plus its length overflows, is refused, so
  every chunk ends before `u64::MAX`; a read whose end overflows or
  passes its chunk's, and a data extent, file extent or free space
  range that overflows, are refused; every other sum of on-disk values
  is bounded by an admitted chunk or a checked range, or saturates and
  so fails any chunk bound rather than wrapping past it;
- the chunk map: the system chunk array inside the chunk tree; single or
  DUP profiles only; chunks against device extents, which match the
  stripes exactly and the device's used bytes (the chunk tree's own
  DEV_ITEM and the device extents' header fields are not compared);
- the extent tree: every tree block has exactly one skinny or full
  extent item with its level and one reference from its tree; every data
  extent's inline and keyed references equal the file extents that use
  it, by root, inode and offset; no extent overlaps another or leaves a
  chunk of its kind; nothing is leaked;
- block groups against chunks, each one's used bytes against its
  extents, and their sum against the superblock;
- the free space tree, extents or bitmaps, against the complement of
  the allocated extents, excusing log-tree blocks and the kernel's
  superblock stripes;
- checksums: every item inside a data extent, no two overlapping, every
  sector matching its data on every copy, and every written extent of
  an inode without `NODATASUM` covered;
- each subvolume: directory items and indexes paired with their hashes
  and with the inode references of what they name, link counts, the
  sizes directories and inodes record, names that are one path
  component and appear once per directory, extents ordered and not
  overlapping, symlinks inline, and every inode reached from the root
  directory or an orphan (a directory removed while open is one);
  root references and back references paired with the directory
  entries naming subvolumes; UUID tree entries naming the subvolumes
  that carry them.

A log tree is read and verified block by block but not accounted, since
the kernel replays it at mount. Snapshots (shared blocks or shared
references), compression, RAID profiles, several devices and features
outside the set above are reported as unsupported, never passed. It
proves consistency, not that the kernel will accept every detail; the
kernel is the integration tier's oracle.

`td-btrfs list IMAGE` prints one line per path across subvolumes, in
name order: type, mode bits, uid, gid, size and path, then a symlink's
target and a subvolume root's id and `rw` or `ro`. `td-btrfs restore
IMAGE DIR` recreates the tree in an empty directory: directories, file
contents (streamed) and symlinks, without owners or modes, which `list`
reports instead. Both refuse a directory reached twice and a name that
is not one path component, so a hostile image can neither loop them
nor write outside `DIR`.

## Tests

The crate's tests make filesystems from real trees (inline boundaries,
thousands of entries for multi-level trees, hardlinks within and across
subvolumes, nested subvolumes, modes) and read them back with `check`,
`list` and `restore`; they pin the superblock fields td-boot admits,
the size rules, every refusal (and that a refused run leaves the
target unchanged), damaged data and metadata, a name that would escape
the restore, and the
`mkfs.btrfs` command line. The `td-btrfs-test` recipe check does the
same with the target-built binary. The format was cross-checked once,
by hand, against btrfs-progs v7.0's `check --check-data-csum` and
`restore` and against the 7.1 kernel mounting, writing and remounting
td-btrfs images, formatting a block device in a guest, and an unclean
stop; that record is in the landing commit, and the qemu integration
oracles mount these volumes on every main run.
