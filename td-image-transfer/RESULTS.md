# Image transfer prototype measurements

These are local-cache simulations on retained real system deployments,
not network downloads or controlled release-to-release benchmarks. Store
names identify cached builds; their source commits and causal changes were
not established. In particular, the broad pair has a changed kernel but
is not a kernel-only or toolchain-only experiment.

## Environment and reproduction

AMD Ryzen 7 5700G (8 cores / 16 threads), Linux 7.0.14,
`rustc 1.99.0-nightly (6f72b5dd5 2026-07-22)`, release profile defaults,
local NVMe-backed filesystem. One run per pair, sequentially, on a shared
host; page-cache state was not controlled. Times are illustrative, not
statistical estimates. Peak RSS is Linux `wait4`/`getrusage` ru_maxrss.

Build and invoke as DESIGN.md specifies, with deployment directories under
`~/.td/build-daemon/ladder-shared-v1/build-cache/store/NAME/deployment`:

| Label | OLD store name | NEW store name |
| --- | --- | --- |
| root-only | qzxd0ilfgdn2l6apcq9c7mz1cnkql3s2-system-x86-64-0.2 | aq5bv9sfi6k3mijiqmh1kn7q3fhflfs3-system-x86-64-0.2 |
| near | aq5bv9sfi6k3mijiqmh1kn7q3fhflfs3-system-x86-64-0.2 | s65qhfpkbvz86bq77lm2r7bm9bxf1k42-system-x86-64-0.2 |
| broad | 9gym9bvh3lhx361fbf41xg0rjnz98a24-system-x86-64-0.2 | s65qhfpkbvz86bq77lm2r7bm9bxf1k42-system-x86-64-0.2 |
| identical | s65qhfpkbvz86bq77lm2r7bm9bxf1k42-system-x86-64-0.2 | s65qhfpkbvz86bq77lm2r7bm9bxf1k42-system-x86-64-0.2 |

The root-only pair has identical kernel and initramfs bytes; near has an
identical kernel and a changed initramfs. These labels describe observed
payload equality, not commit history. Retained artifacts may be collected;
the identities below permit comparison with copies retained elsewhere.

## Missing object bytes

Exact bytes, excluding modeled indexes and all network/signature overhead.
The hybrid column uses extent objects for root.erofs and CDC for the
kernel and initramfs. This sums separately measured payload methods; no
cross-payload cache sharing is credited.

| Pair | Complete new payload bytes | Fixed missing | CDC missing | Hybrid missing | Hybrid reduction |
| --- | ---: | ---: | ---: | ---: | ---: |
| root-only | 131,649,024 | 97,980,416 | 32,198,952 | 29,603,667 | 77.51% |
| near | 131,676,672 | 108,275,200 | 20,653,723 | 18,999,445 | 85.57% |
| broad | 131,676,672 | 131,676,672 | 125,843,671 | 124,525,146 | 5.43% |
| identical | 131,676,672 | 0 | 0 | 0 | 100.00% |

| Pair | Root CDC objects missing/total | Root extent objects missing/total | Root CDC index model | Root extent index model | Missing unique logical files | Missing logical file bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| root-only | 390/1221 | 602/4074 | 58,656 | 195,600 | 33 | 75,035,207 |
| near | 205/1228 | 307/4076 | 58,992 | 195,696 | 14 | 36,005,511 |
| broad | 1159/1228 | 2778/4076 | 58,992 | 195,696 | 1201 | 224,641,822 |
| identical | 0/1228 | 0/4076 | 58,992 | 195,696 | 0 | 0 |

The new roots contain 2,419 distinct regular-file contents, totaling about
238 MB uncompressed, versus about 98 MB of image bytes. Logical-file bytes
are not comparable to compressed missing-object bytes as a bandwidth claim.
The file baseline omits filesystem metadata and needs a repacker to make
an image; the chunk methods reconstruct every byte, metadata included.

## Reconstruction and resource cost

All seven reconstructed files in all four runs passed whole-payload SHA-256
verification, and the identical run needed zero object bytes for every
method. Cold-cache bytes reported by all image methods equaled the complete
payload size on these artifacts (no duplicate chunks within those indexes).

| Pair | Entire experiment seconds | Peak RSS KiB | Output bytes | Root CDC index ms | Root CDC reconstruct+sync ms | Root extent index ms | Root extent reconstruct+sync ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| root-only | 13.06 | 195,904 | 361,540,608 | 949 | 1138 | 680 | 1154 |
| near | 14.78 | 196,352 | 361,620,480 | 932 | 1312 | 900 | 1437 |
| broad | 12.79 | 195,756 | 361,620,480 | 951 | 1116 | 653 | 1146 |
| identical | 13.06 | 196,624 | 361,620,480 | 958 | 1132 | 665 | 1159 |

Output bytes are calculated from the seven materialized file lengths, not
filesystem allocated blocks or total updater peak disk usage. Input images
are read in place. The prototype holds old and new payload buffers in RAM;
a streaming implementation could have different memory and I/O costs.

## Interpretation

Fixed-size chunks were very sensitive to image layout changes. CDC and
extent boundaries both preserved substantial reuse in the closer pairs.
Extents saved a further 1.3–2.6 MB of root transfer over CDC, but required
more objects and a larger index. On the near pair that is 307 missing root
objects versus 205: request overhead could outweigh the byte savings unless
requests are batched or multiplexed. No latency model is included.

The broad pair saved only about 5% with the hybrid strategy. Chunking cannot
recover similarity absent from the produced bytes, and these results do not
promise small downloads across toolchain/runtime changes. The kernel and
initramfs in that pair reused no chunks. Controlled rebuilds are still
needed to attribute the difference to particular input changes.

These results justify a streaming, resumable transport experiment. They do
not yet justify committing to a signed release format or changing td-admin.

## Payload identities

Each entry is SHA-256 of the bytes measured, recorded by the executable.

### root-only

```text
root.erofs
old f0850c2894abaacae489a30aa18ccba695a76a94250f0d98ccacd74204f8a6a2 97755136 bytes
new 3626d7bd1bece0006b00b1f5938315cfea8c774bc11a81380f7de8139c0b8700 98242560 bytes
bzImage
old bfd7d1647e812234759a7fa114b109b23c938a651d854be761d0b93a49c3936d 21042176 bytes
new bfd7d1647e812234759a7fa114b109b23c938a651d854be761d0b93a49c3936d 21042176 bytes
initramfs.cpio
old 3178f669a12e3e15912103aded5fed4bc89880a232e94bc8e68c9a6e62dce356 12364288 bytes
new 3178f669a12e3e15912103aded5fed4bc89880a232e94bc8e68c9a6e62dce356 12364288 bytes
```

### near

```text
root.erofs
old 3626d7bd1bece0006b00b1f5938315cfea8c774bc11a81380f7de8139c0b8700 98242560 bytes
new 87ae53261dff4a76fe22cf17c13c50d5b6a97eb9fc02c94536ffbac6ec63f57c 98267136 bytes
bzImage
old bfd7d1647e812234759a7fa114b109b23c938a651d854be761d0b93a49c3936d 21042176 bytes
new bfd7d1647e812234759a7fa114b109b23c938a651d854be761d0b93a49c3936d 21042176 bytes
initramfs.cpio
old 3178f669a12e3e15912103aded5fed4bc89880a232e94bc8e68c9a6e62dce356 12364288 bytes
new 1f3f1baa6200a6e93a9a80e48c715ab5b038c25750b8e6736ebf9dad41ba2bdc 12367360 bytes
```

### broad

```text
root.erofs
old 9764264bc6c846c18fd51ded3d2d604484764ebcfe1c81cf37716196a8121f9e 97484800 bytes
new 87ae53261dff4a76fe22cf17c13c50d5b6a97eb9fc02c94536ffbac6ec63f57c 98267136 bytes
bzImage
old 384da03c82adc6caf4371ac999275c7ae76e625a909f84426a91489ce2c90885 8160256 bytes
new bfd7d1647e812234759a7fa114b109b23c938a651d854be761d0b93a49c3936d 21042176 bytes
initramfs.cpio
old 550aee364387904a77c5a5d6ca704368489b0433488622fd3def41d7050cadd6 12923392 bytes
new 1f3f1baa6200a6e93a9a80e48c715ab5b038c25750b8e6736ebf9dad41ba2bdc 12367360 bytes
```

### identical

```text
root.erofs
old 87ae53261dff4a76fe22cf17c13c50d5b6a97eb9fc02c94536ffbac6ec63f57c 98267136 bytes
new 87ae53261dff4a76fe22cf17c13c50d5b6a97eb9fc02c94536ffbac6ec63f57c 98267136 bytes
bzImage
old bfd7d1647e812234759a7fa114b109b23c938a651d854be761d0b93a49c3936d 21042176 bytes
new bfd7d1647e812234759a7fa114b109b23c938a651d854be761d0b93a49c3936d 21042176 bytes
initramfs.cpio
old 1f3f1baa6200a6e93a9a80e48c715ab5b038c25750b8e6736ebf9dad41ba2bdc 12367360 bytes
new 1f3f1baa6200a6e93a9a80e48c715ab5b038c25750b8e6736ebf9dad41ba2bdc 12367360 bytes
```
