# Track: oci-load (side-track)

**Status:** UNCLAIMED.
**Origin:** deferred from M10.1 — the generation image is structurally a valid
2-layer OCI image but was never verified to LOAD into a foreign runtime.
**Scope authority:** DESIGN §7.1.

## Goal

Verify the bootc generation image (and the plain td OCI image) is consumable by an
independent OCI implementation, not just by our own placer.

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

(claiming agent: notes here)
