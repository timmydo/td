# Track: fhs-app-images (side-track)

**Status:** UNCLAIMED.
**Origin:** DESIGN §6 parking lot; re-scoped by M9 (FHS belongs to *app* images —
the base stays a minimal store-based container host).
**Scope authority:** DESIGN §7.1.

## Goal

Produce Guix-built OCI **app** images presenting a traditional FHS layout
(`/usr/bin`, `/lib`, …) instead of the `/gnu/store` symlink farm, so foreign
software/expectations work inside app containers. The base image is explicitly NOT
flattened.

## Acceptance

An FHS app image builds reproducibly (`guix build --check`), runs on the booted base
via the existing container-host rung (entrypoint honored), and a behavioral
assertion proves the FHS property (e.g. the app binary really resolves at
`/usr/bin/...` inside the container). Verified-red required.

## Constraints

- Guix's native store-based image remains the reproducibility oracle (M5); FHS
  flattening layers on top — diff against it where applicable (§2.5).
- Strict FSDG, offline loop, as always.

## Working state

(claiming agent: notes here)
