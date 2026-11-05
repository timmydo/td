# Controlled signed HTTP image-transfer measurements

These experiments build three independent variations of the same source
baseline and acquire each through the signed publisher, HTTP downloader and
persistent disk cache. They supersede the causal uncertainty of the older
retained-cache comparisons in RESULTS.md; those remain historical simulation
results, not controlled measurements.

## Inputs

Every image starts from `784cfcf6185445a33e8fb858ef53d1c95caa06bf`.
Each variant resets to that baseline before applying its one change:

- One program: in `td-update/src/admin.rs`, replace the help text
  `Jobs survive disconnects.` with `Jobs survive SSH disconnects.`.
- Kernel configuration: in `recipes/src/recipes/linux-x86-64.rs`, replace
  `CONFIG_NR_CPUS=256` with `CONFIG_NR_CPUS=128`, including matching guards.
  This is a configuration change, not a kernel version upgrade.
- Runtime/build-root baseline: replace only `recipes/src/debian-root.lock`
  with its blob from `b51ae1d3ff8202de297bb1c03aab94662c92ca78^`.
  This uses an earlier reviewed Debian root. The glibc/libgcc package
  versions are unchanged; their input-addressed store identities and
  dependent outputs change. It is not a glibc or Rust version upgrade,
  and it is not a recommendation to deploy older build-root packages.

The evaluator was rebuilt after each change because it embeds source and
recipe text. Each image used `td-recipe-eval build-run system-x86-64
system-x86-64` after warming declared inputs. Experimental changes never
enter the submitted production source or pins. Fixture indexes use a
40-character `f` commit marker; the source identities above describe the
actual measured builds. These indexes are not production releases.

## Measurement method

Host: AMD Ryzen 7 5700G, Linux 7.0.14, NVMe-backed filesystem, host rustc
1.99.0-nightly (6f72b5dd5 2026-07-22). Target inputs remain the pins in the
baseline. One run per case, on a shared host with concurrent builds and
uncontrolled page cache; wall times are observations, not throughput or WAN
estimates. Do not attribute small timing differences to the chunk method.

For each method, `td-image-transfer partition` emits plans for all three
payloads. `td-net release publish-partitioned` signs and publishes them;
`promote` selects the index. A Python stdlib static HTTP/1.1 server listens
on numeric loopback with TCP_NODELAY and persistent connections. The ordinary
`fetch` command downloads either into an empty cache or a private clone of
that method's baseline cache. Each changed-image run starts with only the
baseline objects, not objects accumulated from earlier experiments.
Independent whole-file SHA-256 comparisons and `verify` check each result.

The hybrid method means EROFS physical extent boundaries for `root.erofs`,
plus CDC for `bzImage` and `initramfs.cpio`. It does not download logical
uncompressed files or repack EROFS. Both methods reconstruct the exact
original bytes, including metadata and padding, through the same receiver.
All these compressed extents fit the 256 KiB transport maximum; flat/plain
larger extents are split by the plan generator.

Requests and response-body bytes come from actual HTTP acquisition counters.
Body bytes include signed metadata and fetched objects, but exclude HTTP
headers and TCP/TLS overhead; loopback uses no TLS. New-object bytes count
verified objects downloaded. Reused-output bytes count reconstructed bytes
supplied by cache. RSS is wait4 ru_maxrss for the fetch child and includes
process-launch inheritance from the Python driver; treat it as a conservative
process upper bound, not a precise measurement of downloader allocations.
Publisher and offline plan-generation memory are outside that fetch number.

## HTTP results

Exact byte counts. “Baseline cache” means only objects acquired for the
baseline image are present. Seconds include reconstruction, verification and
file sync, but exclude the subsequent independent verification command.

| Change | Method | Cache | GETs | Response body bytes | New object bytes | Reused output bytes | Seconds | Peak RSS KiB |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| baseline | cdc | cold | 1,619 | 131,805,493 | 131,688,960 | 0 | 27.79 | 23,504 |
| baseline | hybrid | cold | 4,471 | 132,004,062 | 131,688,960 | 0 | 80.23 | 24,272 |
| one-program | cdc | cold | 1,618 | 131,805,420 | 131,688,960 | 0 | 31.24 | 24,272 |
| one-program | cdc | baseline | 24 | 3,806,865 | 3,690,405 | 127,998,555 | 1.81 | 24,272 |
| one-program | hybrid | cold | 4,471 | 132,004,062 | 131,688,960 | 0 | 74.35 | 25,712 |
| one-program | hybrid | baseline | 39 | 2,388,529 | 2,073,427 | 129,615,533 | 1.85 | 25,808 |
| kernel | cdc | cold | 1,602 | 131,800,186 | 131,684,864 | 0 | 46.76 | 23,544 |
| kernel | cdc | baseline | 241 | 21,309,916 | 21,194,594 | 110,490,270 | 5.73 | 24,232 |
| kernel | hybrid | cold | 4,454 | 131,998,755 | 131,684,864 | 0 | 108.82 | 24,360 |
| kernel | hybrid | baseline | 241 | 21,362,853 | 21,048,962 | 110,635,902 | 6.86 | 25,384 |
| runtime | cdc | cold | 1,619 | 131,805,491 | 131,688,960 | 0 | 36.24 | 23,000 |
| runtime | cdc | baseline | 130 | 16,562,423 | 16,445,892 | 115,243,068 | 4.30 | 23,768 |
| runtime | hybrid | cold | 4,471 | 132,004,062 | 131,688,960 | 0 | 102.34 | 23,768 |
| runtime | hybrid | baseline | 122 | 6,792,085 | 6,476,983 | 125,211,977 | 5.97 | 25,368 |

## Interpretation and implementation limits

For the one-program change, CDC reduced response-body bytes by 97.1%;
hybrid reduced them by 98.2%. For the kernel configuration change both
saved about 83.8%: the new kernel accounted for almost all new object bytes,
and hybrid's larger index outweighed its small object-byte advantage.
For the runtime/build-root baseline, CDC saved 87.4% and hybrid saved 94.9%.
Hybrid needed fewer requests as well as fewer bytes in that warm case.
The earlier uncontrolled broad comparison's roughly 5% saving was therefore
not evidence of a typical runtime-update cost.

Hybrid is valuable for warm-update bandwidth, especially in this runtime
case, but cold downloads needed roughly 4,470 requests against CDC's 1,620
and were slower in these runs. CDC remains the default streaming publisher:
it needs no EROFS layout assumptions. Hybrid is an opt-in producer experiment
through partition plans; its offline planner currently caps each payload
at 512 MiB. These observations do not select a universal winner or predict
reuse across an actual Rust/glibc version upgrade. WAN latency, HTTP/2 or
batched object transport, concurrent clients and statistical timing runs
remain unmeasured.

The first acquisition starts cold; this implementation does not seed its
object cache from an already-installed locally built image. Later downloads
reuse verified objects. Cached objects and reconstructed files still need
disk space; ENOSPC is reported, and already verified objects survive retry.
The cache has no automatic quota/collection policy. Repository GC is separate
and deliberately retains the currently selected image even after expiry;
expiry refuses new admission rather than removing installed rollback data.

## Validation scope

Every changed-image cold and baseline-cache download above passed independent whole-file
SHA-256 comparison and the signed verifier. Delivery tests cover actual
truncated HTTP transfer/resumption, corrupt-cache replacement, missing or
corrupt objects, injected ENOSPC, expiry, replay/equivocation, root rotation,
revoked-key counter poisoning, failed-transfer privileged-state learning,
and conservative repository GC. These are bounded synthetic failures, not
power-cut tests or a physically full installed machine.

The desktop KVM run acquired and independently verified its signed candidate
and emitted deployment installation, rollback and boot-success markers.
The complete desktop oracle subsequently failed at Firefox readiness/input
(`TD-FIREFOX-INPUT-FAILED`), so it did not reach its later automatic-fallback
cases. This report does not call that full oracle a pass. The separate
headless oracle exercises the shared installation and failed-health boot
rollback path without the Firefox dependency. Its signed-delivery result
is recorded in the validation commit message.

The network and boot tests do not exercise the complete SSH td-admin job
inside a VM. The stock server has no td-admin service: the headless proof
covers the shared deployment mechanism through host-side fixture setup,
not an installed-server remote update path. No production signing keys,
machine publisher policy or hosted
endpoint have been provisioned by these experiments. The repository remains
local-first; live hosting is a separate deployment choice.

## Payload identities

SHA-256 of the actual measured bytes, independently recalculated.

### baseline

```text
manifest 249 bytes
4ad8d8383ad9162f8c2ad781b51bfca95c33555200055e7b877d8e053c9dc4da
bzImage 21042176 bytes
bfd7d1647e812234759a7fa114b109b23c938a651d854be761d0b93a49c3936d
initramfs.cpio 12367360 bytes
afb27f53f3cd3fc39fb4449e85bfeafe5415a4d09873b833ab326d164f11b1ce
root.erofs 98279424 bytes
d9ee4a33da7faa61bdbe54f41511526c1e04ea2a961afa12942507af2e7da460
```

### one-program

```text
manifest 249 bytes
22555e29481ad39baf47676c1da118afc61329328e59ab3c2d6980f27ffc6e2b
bzImage 21042176 bytes
bfd7d1647e812234759a7fa114b109b23c938a651d854be761d0b93a49c3936d
initramfs.cpio 12367360 bytes
afb27f53f3cd3fc39fb4449e85bfeafe5415a4d09873b833ab326d164f11b1ce
root.erofs 98279424 bytes
d95d39ad4eec12ca0e1ffd8b3683c69ba2c1902fa6539faf198a8ff4f63a1bb7
```

### kernel

```text
manifest 249 bytes
4545adba01fdc35663316365ae9a577610d826131446f0db5bc866550521b4f9
bzImage 21038080 bytes
a1ba941fb35e4f910922ce961daa5f239a628df002796711278d411d73dbda40
initramfs.cpio 12367360 bytes
afb27f53f3cd3fc39fb4449e85bfeafe5415a4d09873b833ab326d164f11b1ce
root.erofs 98279424 bytes
a3cc6451189d193b62eace8c192fbaf3a40ad1f493516a8e407c093cb124555a
```

### runtime

```text
manifest 249 bytes
1ea727d3ed0460953b52fc251c5fb307133c1580cd47f72c3ac51f29d59387be
bzImage 21042176 bytes
bfd7d1647e812234759a7fa114b109b23c938a651d854be761d0b93a49c3936d
initramfs.cpio 12367360 bytes
5fde092161b8a033da4b9f08b0c0579adedff2d3d30a55c6ac29d6800b17200d
root.erofs 98279424 bytes
805ee962bceecf223e617584ef0add585d48e5f5643836b5af78d4ab4755b629
```

