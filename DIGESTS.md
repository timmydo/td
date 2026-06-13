# DIGESTS.md — reproducibility record

The shipped artifacts' deterministic outputs (the digest convention from DESIGN §2.7).
This file changes ONLY on an oracle re-baseline, which is an **exclusive landing**
(DESIGN §7.3): land it as a small standalone commit, announced in your track file,
and expect every other agent to rebase.

**Re-baseline procedure addendum (check-memo constraint 4,
`plan/check-memo.md`):** any change to this file MUST be validated with the
force-full knob — `TD_CHECK_FULL=1 ./check.sh` — so every reproducibility leg
runs the real `guix build --check`, bypassing all memoized verdicts. The same
applies to any suspected nondeterminism.

Current baseline is guix-free (`ship-guix?` defaults to `#f` since the 2026-06-06
sign-off; the single `system/td.scm` lowers to both the qcow2/VM and the OCI image, so the
whole distro is guix-free). The frozen oracle was re-baselined by editing `system/td.scm`
to exactly what `td-config->operating-system` emits for a `#f` config — delete
`guix-service-type`, add `guix-free-marker`, add `guix-free-privsep-service` — so the
differentials still converge, now at guix-free digests, and the differential itself
enforces the marker on the oracle.

- system drv (oracle): `rxbyhfc70s7qldkcah0a8rf29z9pij6p-system.drv`; perturbed
  (ssh-port 2222): `pb06pj1rvca71d7j0lb8ssmisgyllrmm`.
- default OCI image drv (oracle): `d4fn2m2vf6rhhgvj4cish3023a7kvpp4-docker-image.tar.gz.drv`;
  perturbed: `z9f9kjb0rp7y3r7adlr265qiizd5ppd4`.
- default qcow2 output: `rgp5cdjpmjcg5jdzqp85gfc5byv8rhi6-image.qcow2`.
- default docker output: `n3ds4yhw5v49yi53426pc0sbmibc3dl7-docker-image.tar.gz`.
- swapped (+hello) / no-guix hardened drv: `vkm5wlx6fl5ly3c11qplvall1ryhxd17-…` → output
  `z539zlhhj0r35lqj04zqn62z4xcazbr4-docker-image.tar.gz`.
- no-guix control: the explicit `(td-config #:ship-guix? #t)` fixture, OCI drv
  `8v1bdz2v68gkbzybbaq4875a5flh2kvp` (4 guix binaries; hardened ships 0) — decoupled from
  the shipped default so promoting the default never reddens the rung.

The privsep discovery behind the re-baseline: a guix-free system breaks inetd sshd
(`/var/empty must be owned by root and not group or world-writable`) because
`guix-service-type` had created `/var/empty` (root:root 0755) as a side effect of its
build-user accounts. `guix-free-privsep-service` restores it; the boot rung proves
key-based login still works.

## Generation identity — OCI manifest digests (§2.7 representation move, 2026-06-11)

The `oci-load` track introduced the canonical OCI layout (skopeo
`copy docker-archive:… oci:…`, verified by the `oci-load` rung), so generation
identity moves from "sha256 of the docker-archive tarball" to the **OCI image
manifest digest** — the registry-addressable `sha256:…` form M12 signs. This is the
representation change §2.7 staged; the convention (identity = digest of the
distributed artifact, in its canonical form) is unchanged. Digests below are stable
across repeated conversions of the reproducible archives (the rung re-derives a
manifest digest every check; these recorded values are the §2.7 identity record at
this baseline):

- default OCI image (docker output `h8x2qfskf2qzjv4s3w7ah905b1gw5kc6-docker-image.tar.gz`):
  manifest digest `sha256:714045afa001bab1ce90744ff77c885e4faae1573570de753e6906a5bc5c80ff`.
- gen-1 bootc generation image (output `hpv950x5wlvss9gmvgnsgbfv2hl5czkr-td-generation-image-gen-1`):
  manifest digest `sha256:6e18463da4ded2acb48d2115dfbaa9c1a445042ab4a7988dac1c47831e85884d`.
- gen-2 bootc generation image (output `wirzy55s1rgf21v5r54nw6q2bn5f8c2x-td-generation-image-gen-2`):
  manifest digest `sha256:3c6036fc466d0a7e52b1099dc3976d8639742039370b88e64765c8d658dc722a`.

Re-baselined 2026-06-12 (M12 track, deliberate): M11's sealing landings
(tmpfs-root + dm-verity store) changed the generation images after the digests
were first recorded on 2026-06-11, leaving the gen-1 record stale. The M12
`registry` rung — which signs exactly these identities — re-derives both
generation digests every loop (statement == skopeo-inspected manifest digest);
gen-2 is recorded for the first time (the registry distributes both placed
generations). The default (plain) image was NOT changed by M11 (generations
seal; the default stays plain — see `generation-diff`): its digest re-verified
unchanged on 2026-06-12. Historical gen-1 value at the oci-load recording:
`sha256:4076bfe6…` (output `xmgdwy348…`).

Known drift, flagged (not part of this move): the store paths in the section above
date from the 2026-06-06 guix-free re-baseline and no longer match what the loop
lowers today (e.g. default docker drv is now `m40hfv6r…`, output `h8x2qfskf…` —
observed 2026-06-11 via `guix system image -L . -t docker [-d] system/td.scm` at the
pinned channel).
Refreshing them is the next deliberate oracle re-baseline owner's call.
