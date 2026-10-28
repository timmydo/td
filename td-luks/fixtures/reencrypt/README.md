# Reencryption write-prefix fixtures

cryptsetup 2.8.8 produced these bytes in a QEMU 10.2.1/KVM guest using td's
Linux 7.2.13 and the same static cryptsetup and td-init inputs recorded in
`../README.md`. The capture helper is `../generate_reencrypt.rs`; the
bounded container reader is `../replay.rs`. This is increment B4b.

Each mode starts with an 18 MiB device: the profile's 16 MiB header/keyslot
area and 2 MiB of data. The plaintext is 2 MiB of byte `0x5a`. The old volume
key is bytes 0 through 63; the new volume key is bytes 64 through 127. Both
normal keyslots use 48 ASCII zeroes. These are public test keys. The helper
formats, writes the plaintext through stock dm-crypt, then initializes
reencryption with a 1 MiB hotzone. Both checksum and journal modes use
4096-byte encryption sectors; checksum mode uses SHA-256.

The helper records a successful stock resumption with QEMU's
[blklogwrites driver](https://gitlab.com/qemu-project/qemu/-/blob/v10.2.1/block/blklogwrites.c),
then retains every write and flush before the first old-keyslot erasure.
It reconstructs the initial image and each successive prefix on a fresh
copy of the guest disk. For every prefix it runs stock `repair`, conditionally
`reencrypt --resume-only`, then `open`, reads all plaintext, and checks the
final dumped volume key. `repair` can itself finish the last hotzone and
remove the reencryption requirement, so the helper checks metadata before
asking for resumption. No td-luks mutation creates or repairs these states.

| Mode | Initial image plus retained write/flush prefixes | Correct plaintext after stock recovery | Expected stock end-state failures |
| --- | ---: | ---: | ---: |
| checksum | 58 | 51 | 7 |
| journal | 48 | 41 | 7 |

`oracle.txt` is the corresponding subset of the guest's verification log.
An ordinary Rust test independently checks every prefix's header selection,
both normal AF keyslots and digests, segment ownership of every ciphertext
block, and the active checksum/journal bytes against the old ciphertext.
It includes partially rewritten hotzones and the final all-new state. The
reencryption digest's binary encoding remains an increment-D reader/writer
responsibility; stock recovery validates it in this capture procedure.

## The stock end-state failure

After syncing the final rewritten data, cryptsetup commits a clean,
single active segment bound to the new digest while retaining the
reencryption requirement and both backup segments. Its forward-resume
offset calculation treats this layout as offset zero. Stock repair plus
resumption then rewrites already-new ciphertext using the old key and
returns success with corrupted plaintext. The capture checks this expected
failure separately; these seven prefixes per mode are negative evidence,
not a claim of successful interoperability. Before resumption, their data
bytes equal those of the successfully completed stock run.

A separate single-snapshot reproduction of the checksum case also logged
`reencrypt offset: 0`, `reencrypt progress: 0`, and a successful exit with
2,088,890 mismatching plaintext bytes out of 2,097,152. The prefix selected
a valid sequence-10 primary header over a valid sequence-9 secondary.
The committed capture's random salts and request segmentation differ;
the retained failure is the layout, not that particular sequence number.

DESIGN.md's final transaction must therefore publish the all-new segments
and remove the requirement together, directly from the last dirty hotzone.
It must never publish this intermediate stock layout. The later stock
window that destroys old keyslots while retaining the requirement is
outside these traces: the converter stops before the first such erasure.

## Container and scope

A `.base` contains the first 548,864 bytes of the initialized device
(two headers and the two normal keyslot areas), immediately followed by
the 2 MiB ciphertext data region. Reconstruction inserts zeroes from
548,864 to 16 MiB. The original random, inactive resilience-area contents
are omitted; the trace supplies every subsequently used checksum or journal
byte. Every stock recovery check above uses this reconstructed image,
including its zero-filled gap, rather than an uncropped original.

A `.trace` begins with the eight ASCII bytes `TDLUKSR1`. Each record is a
little-endian u64 byte offset, a little-endian u64 byte count, and exactly
that many bytes. An empty record preserves a flush boundary. Offsets and
counts are multiples of 512; the bounded parser refuses writes outside the
18 MiB device or into normal keyslot storage. EOF ends the records. The
converter accepts QEMU's version-1, 512-byte-sector log and refuses other
flags or geometry. It stops before old-slot erasure, without modifying any
retained write payload.

These are simulated completed-write prefixes, in the recorded order.
They do not cover torn individual sectors, every possible write completion
reordering, or interruptions inside stock recovery itself. Stock recovery
and the independent ciphertext checks establish which retained prefixes
are useful positive or negative evidence. Increment C's live activation
oracle and increment D's td-luks interruption/resumption oracle remain
separate requirements.

## Regeneration

Run from the repository root with KVM and QEMU's `blklogwrites` driver.
The helper is host test preparation, never a target recipe input. It needs
the host's static C runtime as described in `../README.md`, and compiles the
repository's std-only td-json locally. No external Rust crate is used.

```text
mkdir -p .td-build-cache/agent-scratch
rustc --edition 2021 --crate-type rlib --crate-name td_json -C opt-level=2 \
  td-json/src/lib.rs -o .td-build-cache/agent-scratch/libtd_json.rlib
rustc --edition 2021 -D unused -C opt-level=2 -C relocation-model=static \
  -C target-feature=+crt-static -C linker=gcc \
  --extern td_json=.td-build-cache/agent-scratch/libtd_json.rlib \
  td-luks/fixtures/generate_reencrypt.rs \
  -o .td-build-cache/agent-scratch/generate-reencrypt
TMPDIR="$PWD/.td-build-cache/agent-scratch" \
  .td-build-cache/agent-scratch/generate-reencrypt \
  KERNEL/bzImage TD_INIT/bin/td-init CRYPTSETUP/bin/cryptsetup \
  .td-build-cache/agent-scratch/reencryption-captured
```

The output directory must not exist. The helper runs preparation, complete
resumption with write logging, and prefix verification for each mode. Every
guest has two virtual CPUs, 512 MiB RAM, no network and a disposable disk;
no host disk is attached. The maximum guest duration is three minutes for
preparation/resumption and fifteen minutes for each verification sweep.
Only after all six guests pass does it publish the `.base`, `.trace` and
full guest logs. Random salts and request segmentation can change bytes
and prefix counts on regeneration.

Copy both modes' `.base` and `.trace` files together. Extract the `CUT ...
PASS` and `VERIFIED ...` lines from the two verification logs into
`oracle.txt`, keeping the mode names and input versions. Update this receipt
and counts, then run:

```text
cargo test --release --manifest-path td-luks/Cargo.toml --test reencryption_fixtures
```

Preserve any new finding in the design and commit review record. Remove
owned disposable logs, binaries and capture scratch after validation;
retained reviewer audit traces are unrelated and must remain.

## Capture receipt

Captured on 2026-10-27 with the three input hashes in `../README.md`.
SHA-256 of the retained containers:

```text
48cdfc11b268e8a477407d27bc5c15de16a7519d7ef1915c8c9f6930a2f1b719  checksum.base
cae2c951260216d4fd00355e4a3b20d7ce4d46d632bba84d614e425ee1a3cd59  checksum.trace
4edbc8bf374c432fd12c58ccbc69296e4b5453d90fbcf309b025c276c804c046  journal.base
1c8a692ec640b0f1ff4bc272007992ce5ea0c98e97cdcbe5a242655e955051fa  journal.trace
```
