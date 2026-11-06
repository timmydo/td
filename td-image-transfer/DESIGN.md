# Offline image transfer experiment

`td-image-transfer` is a host-only, dependency-free measurement tool. It
compares two local deployment directories and reconstructs the newer
payloads from old byte ranges and simulated missing objects. It has no
recipe, network access, publisher, signatures, privileged operation or
integration with td-admin. It neither authenticates deployment manifests
nor changes td's build or distribution trust policies.

## Run

From the checkout root:

```text
cargo build --release --manifest-path td-image-transfer/Cargo.toml
td-image-transfer/target/release/td-image-transfer OLD NEW OUTPUT > report.tsv
```

OLD and NEW each contain `root.erofs`, `bzImage` and `initramfs.cpio`.
OUTPUT must not exist; its parent must exist. The tool reads inputs only,
creates OUTPUT and writes seven reconstructed payload files. A failure
leaves partial outputs for inspection; only exit zero and the final
`complete` record signify a successful experiment. Run against stable,
trusted local artifacts. This is not an adversarial filesystem reader or
an EROFS admission validator. Each payload is limited to 512 MiB; both
versions of one payload are held in memory at once. Source manifests are
not used as authentication or as the comparison's identity: the report
records SHA-256 of the actual bytes read.

Delete the experiment's outputs explicitly when no longer needed. The
tool never overwrites an existing output directory or manages shared
caches. Use worktree-owned `.td-build-cache/agent-scratch/` for disposable
experiments. Test fixtures use td-fs's TempDir guard.

## Partition plans for the signed transport

```text
td-image-transfer partition cdc|hybrid DEPLOYMENT NEW_PARTITIONS
```

This reads one payload at a time under the same 512 MiB bound and writes
three files containing canonical decimal object lengths, one per line.
`hybrid` selects physical EROFS extents for root.erofs and CDC for bzImage
and initramfs.cpio; `cdc` selects CDC for all three. Physical spans larger
than 256 KiB (possible for flat/plain files) are split at that limit. The new output directory
must not exist. These are trusted producer hints, not authenticated metadata.
`td-net release publish-partitioned` bounds and validates the plans, hashes
source bytes and signs the ordinary release index. See `net/RELEASES.md`.

## Methods and measurements

- `fixed`: 64-KiB chunks of each finished payload, including its tail.
- `cdc`: Gear-style content-defined boundaries, 16-KiB minimum, 64-KiB
  boundary mask, 256-KiB maximum. The rolling value shifts left and adds
  a deterministic SplitMix64-derived value for each byte; it resets at
  every boundary. This is a baseline, not an implementation of FastCDC,
  and 64 KiB is not a promise of the resulting mean chunk size.
- `extents`: root-image physical compressed/raw file extents, deduplicated
  by address, with all intervening metadata and padding split into at
  most 64-KiB pieces. This covers every image byte exactly once and needs
  neither decompression nor recompression to reconstruct the image.
- `files`: decode and SHA-256 each regular file's logical contents;
  deduplicate by hash and length across paths and hard links. Report
  unique and missing logical bytes, independently of image compression.
  Empty files count; directories, symlinks and metadata do not. This is
  a file-content reuse baseline, not a reconstructable filesystem format
  or an estimate of compressed network transfer size.

The EROFS reader accepts td's compact inode table at block one, 4-KiB
blocks, flat/plain files and its fixed 128-KiB logical extents with
16-byte records, raw or zstd. It bounds inode and extent counts, rejects
partial physical overlaps and includes metadata in reconstruction. It
does not walk directory names or claim to accept arbitrary EROFS images.
Logical file decoding uses the builder's existing bounded zstd code;
tests use the real packer through shared source, not a second writer.

Each method has a cache consisting of objects from the corresponding old
payload alone. Missing objects repeated in the new payload count once;
no sharing across payloads or earlier deployments is assumed. The old
index points into old bytes, rather than a second disk copy of a cache.
The simulator reads missing objects from NEW. It rechecks each object's
hash and hashes the reconstructed stream against the entire new payload.
Outputs are flushed and synced before their timing record is emitted.

`missing_object_bytes` is the sum of distinct missing encoded bytes.
`cold_object_bytes` is the distinct object total with an empty cache.
Neither includes HTTP, signatures, release metadata or additional transport
compression. `reused_output_bytes` counts output bytes supplied by the old
cache, including repeated references. `records` counts ordered placements;
`unique_objects` counts distinct hash/length pairs. The modeled index is
48 header bytes plus 48 per placement (SHA-256, offset, length); it is an
explicit comparison model, not a serialized format or a measured download.

`index_ms` includes indexing both payloads. For extents it also includes
both EROFS parses; for files it includes both decodes and hashes, after
parsing. `reconstruct_sync_ms` includes object hash checks, reconstruction,
whole-payload verification and output file sync. It excludes index creation,
initial file reads and network latency. The experiment writes all seven
reconstructions even for identical images, so its time and disk use are
not a production updater estimate. It writes `3 * new_root_size +
2 * new_kernel_size + 2 * new_initramfs_size` bytes, plus a small report.
External per-process measurement supplies elapsed time and peak RSS.

## Interpretation

RESULTS.md records the earlier offline simulations. CONTROLLED-RESULTS.md
records controlled comparisons through the streaming signed HTTP downloader,
including artifact identities and measurement limits. LATENCY-RESULTS.md
records response-delay and bandwidth-limit measurements for the bounded
cache and installed-image seed path, including its local write costs. The
simulator's byte reuse establishes no authentication or update guarantee.
`net/RELEASES.md` owns the implemented signed transport, freshness, rotation,
resumption and retention contract; `td-update/ADMIN.md` owns admission to
installation. This tool remains an offline producer and measurement helper.
