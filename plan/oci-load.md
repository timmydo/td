# Track: oci-load (side-track)

**Claim status:** see `PLAN.md` (the single source of truth for claims).
**Origin:** deferred from M10.1 — the generation image is structurally a valid
2-layer OCI image but was never verified to LOAD into a foreign runtime.
**Scope authority:** DESIGN §7.1.

## Goal

Verify the bootc generation image (and the plain td OCI image) is consumable by an
independent OCI implementation, not just by our own placer.

This track also carries the DESIGN §2.7 identity end-state: generation identity is
the digest of the distributed artifact, and the *canonical OCI layout* this track
introduces is what moves that digest from "sha256 of the docker-archive tarball"
(today) to the OCI image **manifest** digest (the registry-addressable form M12
signs). The move is a representation change to record — a `DIGESTS.md` re-baseline
(exclusive landing, §7.3) — not a change of convention.

## Acceptance

A rung that loads/validates the image with a foreign tool and asserts success
(and rejects a deliberately malformed image — verified-red), without breaking the
offline loop.

## Constraints

- podman was rejected at M8: 1238 derivations + 290 cold fetches breaks the offline
  loop. Probe cheap alternatives first (e.g. skopeo, umoci, a spec validator) by
  derivation count, exactly as M8 probed crun vs podman; if nothing is
  offline-buildable, structural conformance against the OCI image-spec is the
  fallback vehicle.
- Adds a rung → touches shared-spine `Makefile`/`check.sh`: standalone commit
  (DESIGN §7.3).

## Working state

Agent: claude-fable-a03d13 (claimed 2026-06-11; claim status lives in PLAN.md).

### Sub-task ladder

1. [x] **Vehicle probe** (M8-style, criterion: derivation count + cold fetches,
   offline-buildable from the pinned channel) — 2026-06-11:
   - `skopeo` 1.22.0: **0 drvs to build, 0 cold fetches** (fully warm in the
     shared store; `guix build --no-substitutes skopeo` resolves immediately).
   - `oci-runtime-tool` 0.9.0: 53 drvs — and it validates the *runtime* spec,
     not the image spec; wrong tool regardless of cost.
   - `umoci` 0.6.0: 113 drvs.
   - podman: 1238 drvs + 290 cold fetches — rejected at M8, unchanged.
   **Adopted: skopeo.** Independent OCI implementation (containers/image Go
   stack — not Guix's exporter, not our placer), and `skopeo copy
   docker-archive:… oci:…` both foreign-validates the artifact and emits the
   canonical OCI layout §2.7 needs for the manifest digest.
2. [ ] **`oci-load` rung** (HEAVY pool): skopeo loads the plain td OCI image
   AND the gen-1 bootc generation image from docker-archive into an OCI
   layout; asserts a `sha256:` manifest digest from `skopeo inspect`; negative
   control IN the rung: a copy of the archive with bytes flipped inside the
   inner layer.tar must be REJECTED. Verified-red for both legs before
   trusting green.
3. [ ] **§2.7 identity move**: record the manifest digests in `DIGESTS.md`
   (re-baseline — exclusive landing, §7.3, announced here).

### Functional probe evidence (2026-06-11, host store, offline)

- `skopeo copy --insecure-policy docker-archive:<default image .tar.gz>
  oci:layout:td` → exit 0 in ~3s; layout contains `oci-layout`, `index.json`,
  `blobs/`. (skopeo reads the gzipped docker-archive directly.)
- `skopeo inspect --format '{{.Digest}}' oci:layout:td` →
  `sha256:714045afa001bab1ce90744ff77c885e4faae1573570de753e6906a5bc5c80ff`.
- Corruption probe: gunzip the archive, flip 8 bytes at midpoint (inside
  layer.tar payload), regzip → `skopeo copy` fails:
  `Digest did not match, expected sha256:298a2e3b…, got sha256:ffff4cf1…`.
  So the foreign tool enforces layer digests, not just archive framing.
- Rung-implementation notes: capture skopeo's exit status directly (a `| tail`
  pipe masks it); corrupt with `dd conv=notrunc` (sandbox has coreutils, no
  python3); `--insecure-policy` only disables *signature trust policy* (M12's
  territory) — digest integrity stays enforced, proven by the corruption probe.
- Vehicle access: the rung resolves skopeo via `$(GUIX) build skopeo` from the
  warm store instead of adding it to check.sh's shell packages — one fewer
  spine file touched.

### Exclusive-landing announcements

- Upcoming: new rung touches `Makefile` (HEAVY pool) — standalone commit.
- Upcoming: `DIGESTS.md` re-baseline for the §2.7 manifest-digest move.
