# Signed update transfer under response delay and bandwidth limits

Measured on 2026-11-05 using the production CDC downloader and two actual
system builds. This is a shaped loopback experiment, not an observation of
Cloudflare, R2, TLS, or an Internet connection.

## Inputs and method

The old image is built from source tree at
`ac81674a3569bccedfb00b637c5a87f31aabc6b0`; the new image and host downloader
are from `795cbc960bc82f318dc07b859a340cc6d066934b`. The change adds the
bounded object cache and descriptor-backed seed lookup. Kernel and initrd
bytes are identical; the changed td-net and its deployment composition
change root.erofs. This is one real program update, not a kernel or runtime
upgrade. The images' store basenames are
`g60r478xzy2ggrwi9b35dlkakyvcivrq-system-x86-64-0.2` and
`fr31p3znxx3wmnifslwsj0fjrg4kxzws-system-x86-64-0.2` respectively.

A temporary repository uses ephemeral Ed25519 keys and ordinary `publish`,
`promote`, `init`, and `fetch` commands. Its signed source revision is an
all-`f` fixture marker, not a production source assertion. All cases select
the new index. A fresh client state and output directory are used each time:

- Cold: empty cache, no installed seed.
- Baseline cache: a private copy containing only objects acquired from the
  old image; no changed-image objects carry over from another case.
- Installed seed: empty cache, `--seed` pointing at the old deployment.
- Seed without retention: the same seed with `--cache-limit 0`.

The default cache limit is two GiB. All reconstructed payloads are checked
with both the signed `verify` command and independent SHA-256 comparison.
Times include acquisition, hashing, reconstruction and output/cache sync;
the subsequent independent verification is excluded. Request counts and
response-body bytes come from the downloader. Headers, TCP overhead and
TLS are not included. MB below means decimal megabytes.

The Python standard-library HTTP/1.1 server uses numeric loopback,
persistent connections and TCP_NODELAY. Each response waits either 0,
20 or 80 ms before headers. For the limited cases, body writes are paced
in 16-KiB pieces against a monotonic timer at 12,500,000 or 2,500,000 bytes
per second. The delay is an application-level proxy for per-request
latency; it does not model network handshakes, congestion, packet loss,
TLS, or CDN cache misses. The client fetches objects sequentially.

Host: AMD Ryzen 7 5700G, Linux 7.0.14, NVMe-backed storage. One run per
case, with other builds and VM tests active and uncontrolled OS page cache.
Small timing differences are not statistically meaningful. Publisher work,
local seed/cache preparation and post-fetch verification are outside the
reported times. Every case writes the same 126,626,304-byte new payload set.

## Results

| Response delay / body cap | Acquisition state | GETs | Body bytes | Seconds |
| --- | --- | ---: | ---: | ---: |
| None / unlimited | cold | 1,548 | 126,737,764 | 42.614 |
| None / unlimited | baseline-cache | 47 | 5,230,618 | 2.490 |
| None / unlimited | installed-seed | 47 | 5,230,618 | 38.981 |
| None / unlimited | installed-seed-no-cache | 47 | 5,230,618 | 3.564 |
| 20 ms / 100 Mbps | cold | 1,548 | 126,737,764 | 81.503 |
| 20 ms / 100 Mbps | baseline-cache | 47 | 5,230,618 | 3.982 |
| 20 ms / 100 Mbps | installed-seed | 47 | 5,230,618 | 47.557 |
| 20 ms / 100 Mbps | installed-seed-no-cache | 47 | 5,230,618 | 5.685 |
| 80 ms / 20 Mbps | cold | 1,548 | 126,737,764 | 211.757 |
| 80 ms / 20 Mbps | baseline-cache | 47 | 5,230,618 | 8.772 |
| 80 ms / 20 Mbps | installed-seed | 47 | 5,230,618 | 47.222 |
| 80 ms / 20 Mbps | installed-seed-no-cache | 47 | 5,230,618 | 9.117 |

The cold cases fetch 126,626,304 new object bytes. All other cases fetch
5,119,158 new object bytes and reuse 121,507,146 output bytes. Their
111,460 metadata bytes are included in the response-body total. The seed
cases rehash those same reused bytes from the installed deployment; their
network traffic is identical to the baseline-cache cases.

## What this supports

Local reuse reduces this update's response-body traffic from 126.74 MB to
5.23 MB. With an existing cache, the 80 ms / 20 Mbps case takes 8.8 seconds
instead of 211.8 seconds. This is evidence for this change and these
artifacts, not a promise for kernel upgrades or unrelated system builds.

First-time seeding has a separate local cost. With the default retention,
it takes 39–48 seconds here; with retention disabled it takes 3.6–9.1
seconds while downloading exactly the same bytes. This is consistent with
the cost of persisting and syncing reusable local chunks into a second
cache copy, rather than a network difference. Disabling retention also
loses interrupted-download reuse for new remote objects, so it is not a
blanket recommended default. A useful next experiment is to retain newly
downloaded objects but serve installed seed chunks directly without also
persisting them in the cache. That changes future reuse tradeoffs and
needs its own implementation and validation.

Cold acquisition remains sensitive to sequential request count: 1,548
requests add about 124 seconds of configured response delay at 80 ms,
before considering payload delivery, hashing and disk writes. A bounded
pipeline of four object requests is worth measuring next. Keep metadata
authentication ahead of acquisition, bound any reorder buffer, retain
per-object and final payload verification, and preserve the shared deadline
and atomic output. A concurrent benchmark must impose an aggregate body
rate across connections; simply reusing this per-response limiter would
multiply the available bandwidth and misstate the gain. Pack/batch formats
would add repository and transport complexity and are not justified by
these measurements alone.

The implementation measured here remains sequential and retains seed
chunks by default. Public mirror measurements, TLS/CDN behavior, multiple
runs, disk-pressure behavior, and larger kernel/runtime changes remain
separate validation work. Public publishing is deferred while domain setup
is pending.

## Artifact identities

| Image | Payload | Bytes | SHA-256 |
| --- | --- | ---: | --- |
| old | bzImage | 21181440 | `02b9e1200c4b272f7a976d08e761661f58bf051b5d11a26f02f0ce134f885e77` |
| old | initramfs.cpio | 12367360 | `bdee6667b98f9ebbc4a43bddfaa7534325965254f2b96b5ba339dc86fcdd615c` |
| old | root.erofs | 93065216 | `9379062041dc3ebcdabc94272d48f9670fa126a29702d72188933592e0c89aa0` |
| old | manifest | 249 | `e3ac708dc9cb1ed7550312b6b7668e095e2752267d480a21c4da5b4bbefc9738` |
| new | bzImage | 21181440 | `02b9e1200c4b272f7a976d08e761661f58bf051b5d11a26f02f0ce134f885e77` |
| new | initramfs.cpio | 12367360 | `bdee6667b98f9ebbc4a43bddfaa7534325965254f2b96b5ba339dc86fcdd615c` |
| new | root.erofs | 93077504 | `940be4ea46b16b77fa107b5016138251edd8e24bed42bac64f7c4b533bd5c012` |
| new | manifest | 249 | `03b6ccf5c33fb93aa2f7e1da9f45168cd8480167e3fd4b55c0f1cec62754eedc` |

## Direct seed reads with remote-object retention

A follow-up on 2026-11-05 compares the downloader at
`35a8d944dd6063404ca86fa06e75f097095fb839` with
`a8826d6a795f63edf613b0bb5cf0049a8ff314e8`. Both are release-mode musl
builds using the same toolchain. The latter reads verified seed chunks
directly into the output without also persisting them in the object cache;
newly downloaded chunks still enter the cache. This implements the experiment
proposed above; the earlier table describes the previous behavior.

The image pair and independent payload verification are unchanged. Each
case starts with an empty cache and the old deployment as its seed, using
the default two-GiB cache limit. The HTTP shaping method is the same.
These are single runs on the shared host with concurrent builds and VMs
and uncontrolled page cache, not statistically controlled speed estimates.

| Response delay / body cap | Downloader | Seconds | Retained object bytes |
| --- | --- | ---: | ---: |
| None / unlimited | before | 69.022 | 126,626,304 |
| None / unlimited | direct seed | 7.671 | 5,119,158 |
| 80 ms / 20 Mbps | before | 64.906 | 126,626,304 |
| 80 ms / 20 Mbps | direct seed | 13.425 | 5,119,158 |

Every case makes 47 GETs, receives 5,230,618 response-body bytes, downloads
5,119,158 object bytes, and reuses 121,507,146 output bytes. Each reconstructs
126,626,304 payload bytes. The improvement therefore concerns local cache
writes and synchronization, not reduced network traffic. The changed default
retains remote objects for resume without duplicating the installed seed.
Future updates may rescan the installed image; a cache alone cannot reuse
seed-only bytes after that image is removed.

The 27 release tests pass, including repeated seed-only acquisition with
repository objects absent, a complete cache avoiding a missing seed, and
an interrupted mixed acquisition retaining completed remote chunks but
neither seed chunks nor partial remote objects. The regression assertion
failed before the change because seeded objects were retained. Public
mirror, TLS/CDN, repeated timing runs and larger update pairs remain
separate validation work.
