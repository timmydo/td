# td-fat: the ESP's bounded FAT32 code

td-fat is the FAT32 code td-install and td-boot share for
`td-install/ENCRYPTION.md`'s selector update ("Selector update"): it
admits the ESP td's layout writes, reads a file there, reclaims
clusters no entry reaches, stages a new kernel and selector pair, and
commits a pair or deletes a staged one with one write of the `EFI/BOOT`
directory's first sector. AGENTS.md principle 2 puts code two crates
need in one sibling crate. It is pure `std`, depends on no crate,
forbids `unsafe` and adds no syscall surface to `UNSAFE.md`: its I/O is
std's positioned reads and writes and `fsync` on a file the caller
holds. It formats nothing; td-install formats the ESP with the engine's
writer (`engine/src/fat.rs`). Since ENCRYPTION.md increment 8e's second
commit it is library code with no consumer; 8e's later commits link it
into td-install's `storage-operation` and td-boot's selector.

## Device and volume

`Device` is positioned whole reads and writes and `sync`, implemented
for `std::fs::File`; the caller passes the ESP's byte offset and length
on it, the td layout's first GPT partition. `Esp::open` admits a volume
only as td's writer lays it out, reading the boot sector and FSInfo
first:

- a jump at byte 0 and the `0x55aa` signature; 512, 1024, 2048 or 4096
  bytes per sector; a power-of-two cluster of at most 32 KiB; two FATs,
  both live (ExtFlags zero) and FAT32 version 0; no FAT12 or FAT16
  root-entry, sector-count or FAT-size field; `FAT32   ` as its type;
  FSInfo at sector 1 with its three signatures and the backup boot
  sector at 6, inside the reserved sectors;
- the volume no larger than the partition, at least 65 525 clusters
  (below that it is FAT16 whatever it says), at most `MAX_CLUSTERS`,
  2^20, so the two FATs td-fat holds are at most 4 MiB each, a FAT
  large enough to address every cluster, and the root cluster inside
  the volume.

It then reads both FATs' first `clusters + 2` entries and walks every
directory from the root, at most `MAX_DEPTH` (8) levels and
`MAX_ENTRIES` (65 536) entries in all, skipping deleted entries and
long-name ones (attribute bits `0x3F` exactly `0x0F`) and stopping each
directory at its end marker. One classifier serves the walk and the
search for `EFI` and `BOOT`, so a name the search finds is a directory
the walk descends: a volume label is the volume bit alone, perhaps with
the archive bit, owning no cluster and no size, and is skipped; any
other entry with the volume bit (a directory or file marked as a label,
say `0x18`, or `0x1F`, which the long-name test leaves to it) is
`Damaged`.
Every chain is followed in the first FAT, the second required to agree
on each entry it passes; a chain that leaves the volume, loops, reaches
a free or bad cluster, or does not hold exactly the clusters its file's
size needs, and two entries sharing a cluster, are `Damaged`. Each
cluster an entry reaches is marked, those of a staged pair apart.

`EFI` in the root and `BOOT` in `EFI` must each exist once. `BOOT` is
admitted only as td's layout writes it, else `Foreign`: its first
sector holds `.` naming `BOOT` and `..` naming `EFI` first, then
`BOOTX64.EFI` and `INITRD` and, at most once each and both or neither,
`BOOTX64.NEW` and `INITRD.NEW`, each a non-empty regular file whose
attributes are archive or none, and otherwise only deleted entries and
end markers; once an end marker appears every later entry of the
directory is one, and nothing beyond the first sector is live. Every
boot file lies in the sector's first 512 bytes, the first sixteen
entries, and staging takes its two slots there. The walk must have
reached `BOOT`'s chain and every boot file's, or the ESP is `Damaged`:
nothing the directory names can be taken for unreachable and reclaimed.
The admitted directory is a `BootDir`: the sector's offset and bytes,
the pair, and the staged pair if any.

td-fat assumes it alone writes the ESP while a value holds it: nothing
else mounts it or writes the partition. td-install's worker and the
selector each hold the disk for one operation and mount nothing there.
Secure Boot enrollment (ENCRYPTION.md increment 8i) adds `TDCONFIG` and
`TDCONFIG.NEW` beside the pair; admitting them is a change to this rule
in that increment, not here.

## Reading

`Esp::read` streams a file's chain to the caller, in chain order and at
most 1 MiB per call, exactly the entry's size, so a caller hashes a
40 MiB kernel without holding it. The selector's pre-seal reads each
staged file once this way and computes every digest from that one pass.

## Writing

Every write is positioned within the volume and whole sectors, and each
step below is made durable (`sync`) before the next begins. FAT entries
keep their reserved high four bits. FSInfo's free count and next-free
hint, both copies, are rewritten after each operation; they are
advisory, and td-fat never reads them.

- **Space** is checked first and writes nothing: the new pair's
  clusters must fit in what the deletion and reclaim below leave free,
  every cluster no entry outside a staged pair reaches, bad ones
  excepted; otherwise `Space`. So is the staging's room for two entries
  in the first sector. `fits` makes both checks alone, writing nothing,
  so a caller can refuse before anyone approves the update (td-install's
  storage worker does, before its presentation).
- **Removing an earlier staged pair**, staging's and the direct form's
  first write when one is there, marks both entries deleted in one
  sector write.
- **Reclaim** frees, in both FATs, every cluster allocated in either
  that no entry reaches and that is not marked bad: an interrupted
  update's leaks and the removed staged pair's chains.
- **The new files** take the lowest free clusters, kernel then
  initramfs; their data is written run by run, the last cluster padded
  with zeros, from a reader that must yield exactly the declared length
  and then end (`Input` otherwise), and their chains are written to both
  FATs and synced with the data.
- **Staging** then adds `BOOTX64.NEW` and `INITRD.NEW` in the first two
  free slots after `..`, in one sector write.
- **The direct form** instead points `BOOTX64.EFI` and `INITRD` at the
  new chains, first cluster and size, in one sector write, then frees
  the old chains.
- **The staged commit** (`commit_staged`) takes the `BootDir` its
  caller read and hashed, requires the device's sector to be exactly
  those bytes (`Changed` otherwise, writing nothing), points the pair's
  entries at the first clusters and sizes that `BootDir` names and
  deletes the staged entries, in one sector write, then frees the old
  chains. It never re-reads which clusters to point at. Its error
  (`CommitError`) says where it failed: `Before` that write, the ESP
  unchanged; `Uncertain`, at the write or its sync; or `After` it,
  freeing the old chains or updating FSInfo, the new pair already
  `EFI/BOOT`'s and the clusters it left allocated the next reclaim's.
- **Deletion** (`delete_staged`) under the same check marks both staged
  entries deleted in one sector write, then frees their chains.

New entries are written as td's layout writes one: the archive
attribute, no creation or access stamp, and FAT's epoch, 1980-01-01,
as their write date. A deletion marks the entry's first byte only.

Every byte a commit, a staging or a deletion changes lies in the
directory sector's first 512 bytes, the smallest logical sector any
device has. The rest of a larger sector is written back unchanged, so a
write torn inside the sector leaves those bytes as they were. The
commit therefore assumes only that the device writes one 512-byte unit
whole on power loss, whatever sector size the BPB claims, as
ENCRYPTION.md says and discloses. Before writing it requires the caller's
`BootDir` to be exactly the one this value admitted, every field, and
the device to still hold its sector (`Changed` otherwise), so a caller
cannot point the pair at clusters its snapshot does not name.

## Crash behaviour

Each cut leaves a volume td-fat admits, with the pair alone, the pair
beside a complete staged pair, or the new pair alone, plus at most
clusters no entry reaches, which the next update reclaims. The sync
after each sector write is what keeps a later free from landing before
it; the sync after the chains, what keeps the sector from naming chains
not yet written.

Why the cut tests' loss patterns suffice: between two syncs every write
but the directory sector's touches only state no admitted entry reaches,
data clusters and FAT entries of chains not yet named or no longer
named, and the directory write is alone between its two syncs. The
admission's outcome depends only on reachable state and on the FATs
agreeing where an entry reaches, so any subset of a phase's other writes
admits as the phase's start or end does, differing only in unreachable
clusters the reclaim frees. The tests drop every unsynced write, none,
and each one alone, at every cut, which exercises each phase boundary
and each write's own loss; a failure of that argument (a phase writing
reachable state beside another write) shows as a single lost write
leaving an ESP td refuses, the case the dropped-sync mutation produced.

## Evidence

Host tests (`tests/esp.rs`) build each image with the engine's writer,
the code td-install formats the ESP with, onto a sparse in-memory disk
at a nonzero offset, whose atomic unit is the logical sector:

- admission of td's 512 MiB layout, of 512- and 4096-byte sectors, of a
  staging and its commit, a second staging over a first, deletion and
  the direct form;
- refusal as `Foreign` of an extra file, a missing initramfs, a
  subdirectory, no `EFI`, a long-name entry, an entry after an end
  marker, half a staged pair, a read-only boot file and a live entry
  past the first sector; as `Damaged` of FATs disagreeing on a
  reachable cluster, two entries sharing one, a size its chain does not
  match and a loop; as `Volume` of each boot-sector field td does not
  write and a volume larger than its partition; and of too little space,
  with no write made, while a pair that fits beside the old one stages
  and a replaced staging's space counts;
- the device's atomic unit is 512 bytes for every layout, so a
  4096-byte sector write is cut inside it; a staged commit on 4096-byte
  sectors cut after each unit, and a boot file at entry 16 refused;
- `EFI` or `BOOT` with the volume bit (`0x18`, `0x38`, `0x19`) and a
  volume label owning a cluster refused as damage with the disk
  unchanged; a `BootDir` whose staged cluster or pair size the caller
  changed refused as `Changed` with no write;
- staging, restaging, both commits and deletion cut after every unit
  write each makes, a multi-sector write so cut inside, each cut then
  booted with every write since the last sync landed, with none, and
  with each one of them lost alone: each next boot finds only the
  allowed pairs, and the next staging over it succeeds with no cluster
  lost;
- reclaim of a stray in one FAT and in both, a bad cluster kept and the
  reserved high bits kept;
- a changed sector refusing a commit and a deletion with nothing
  written, and a short, long or empty new file refused;
- agreement with an independent reader: `tests/fat_reader.py`, written
  from Microsoft's FAT32 specification in stdlib Python, reads the seven
  scenario images the ignored `write_reader_images` test writes
  (`TD_FAT_IMAGES=DIR`), and `tests/fat_reader.txt` holds its output:
  each image's digest over its non-zero 512-byte units, its geometry,
  every file's size and SHA-256, free and lost clusters and FSInfo. The
  test rebuilds each image, requires the same digest, and requires
  td-fat's own view to give exactly those lines, the leaked image alone
  with lost clusters and every other's FSInfo exact.
