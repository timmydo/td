# rust-userland — ship the Rust-native base userland

Handle: claude-fable-a00773 · claimed 2026-06-17 · section: side

## Goal (human-directed, 2026-06-17)

Spitball-turned-build: "if a Rust alternative exists, ship it." The genuine
*replacements* (uutils-coreutils → coreutils, youki → crun, russh → sshd) are
NOT in the pinned channel and would each be a milestone-scale packaging effort,
so they are out of this track. What the pinned channel DOES carry, already
substitutable (0 derivations to build, ~8 MB download), is a coherent
Rust-native userland: **procs, fd, ripgrep, sd, eza, bat**.

This track ships those six in the base, as injected base capabilities sitting
beside `crun` — every image gets them, the manifest cannot remove them.

### Honest framing (surfaced in the PR)

- This is **additive**: it does NOT remove glibc/coreutils/bash — a Guix-built
  closure bakes references to them in activation/shepherd/gexp builders, so they
  cannot be deleted while td is Guix-built. "Replace where we can" = ship the
  Rust tool on PATH alongside the GNU one, not delete the C package.
- procs is a fair functional `ps`/`top` replacement; fd/ripgrep/sd/eza/bat are
  *adjacent* (not CLI-compatible drop-ins for find/grep/sed/ls/cat).
- It **raises the `guix-dependence` denominator** (the six apps drag in large
  build-time crate graphs), so td's build-time ownership ratio drops. That is a
  true measurement moving, not a weakened gate — re-baselined via
  `TD_DEPENDENCE_WRITE=1`, delta shown in the PR. It runs *against* the
  corpus-independence arc; included only because the human directed it.

## Where it attaches (keep the differentials converging)

The shipped package set is `crun + %base-packages` (+ guix-free-marker) in BOTH
constructions:
- oracle `system/td.scm`: `(cons crun %base-packages)`
- typed `system/td-typed.scm`: `%base-capabilities = (list crun)`, prepended.

Extend the injected set identically in both:
- oracle: `(cons* crun procs fd ripgrep sd eza bat %base-packages)`
- typed: `%base-capabilities = (list crun procs fd ripgrep sd eza bat)`

Same packages, same order ⇒ byte-identical system derivation ⇒ the M4/M5/M6
differentials (`diff`, `oci-diff`, `manifest-diff`, `generation-diff`,
`typed-coverage`) keep converging. `manifest-diff` (d) checks `crun`
specifically, so it stays green; the drift safety-net is `manifest-diff` (a).

## Durable assertion (what survives with no Guix oracle)

`tests/boot.scm` already asserts `crun` is on PATH in the booted qcow2. Add an
analogous leg: each Rust tool exists in the system profile AND actually runs
(`--version` exits 0 in the guest). Behavioral, oracle-free — the leg we keep.

## Re-baselines (exclusive-landing files)

- `DIGESTS.md` — closure changed; new system/OCI digests.
- `tests/guix-dependence.expected` — denominator rises (additive snapshot).

## Sub-task ladder

1. [ ] claim (this record + plan-index) → draft PR
2. [ ] extend injected set in oracle + typed (+ rust-apps import); eval/diff green
3. [ ] boot.scm durable leg (tools run); verified-red
4. [ ] full ./check.sh green; re-baseline DIGESTS + guix-dependence
5. [ ] sub-agent contract review; mark ready, arm auto-merge

## Verified-red evidence

(to fill in as gates are exercised)
