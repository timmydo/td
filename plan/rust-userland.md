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

1. [x] claim (this record + plan-index) → draft PR #80
2. [x] extend injected set in oracle + typed (+ rust-apps import); convergence verified
3. [x] boot.scm durable leg (tools run); verified-red ✓
4. [x] full ./check.sh green (CHECK_EXIT=0); DIGESTS + guix-dependence re-baselined
5. [x] contract self-review (inline); flip done, mark ready, arm auto-merge

## Verified-red evidence

### Convergence (oracle == typed) holds with the tools added
Host repl, both lower to the SAME system drv:
`oracle == typed == 9yiz1dq6jss2pcdhvny4lkb4b5xblx4c-system.drv`; perturbed
(ssh-port 2222) `7z0igfjg…` differs → self-discrimination intact.

### Boot leg has teeth (the durable assertion)
Removed the six tools from BOTH constructions (assertion left exactly as it
ships), lowered `%test-td-disk-boot` and `guix build`-ed it directly
(`760kspx4…-td-disk-boot-test.drv`). Result — the rust-userland leg RED in
isolation, every other leg green:

```
PASS boots from the qcow2 disk via firmware->GRUB (no direct-kernel)
PASS qcow2 disk boots through GRUB; kernel matches declaration
PASS ssh-daemon shepherd unit is running
PASS declared sshd port is listening
PASS daemon denies password authentication (default-deny)
PASS key-based SSH login succeeds and command output is captured
PASS base is a container host: cgroup2 mounted and crun shipped
    rust userland procs: present=#f exit0=#f   (… fd/rg/sd/eza/bat all present=#f)
FAIL rust userland shipped and runs (procs/fd/rg/sd/eza/bat --version exits 0)
# of expected passes 7 / # of unexpected failures 1  → drv build exit 1
```

Restored the tools → the same leg goes green in the full check (below).

### guix-dependence re-baseline (honest snapshot move)
Only line changed: `shipped-system 1405 → 3130` derivations, ratio
`1.00% → 0.45%` (the six apps' build-time crate graphs join the closure;
numerator/owned 14 unchanged). Re-baselined via `TD_DEPENDENCE_WRITE=1`.
