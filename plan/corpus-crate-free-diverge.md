# corpus-crate-free-diverge — working notes (Phase 2b)

Handle: claude-fable-65585b · claimed 2026-06-24

## Goal

The "diverge" half of own-then-diverge. The whole crates.io corpus rust userland is now OWNED
guix-free (#166 + #167). Phase 2b retires the guix path: DROP the /gnu/store crate strings from
the corpus locks + RETIRE the guix-path crate FODs. The guix-daemon stops being a crate fetcher
for the corpus. Only the toolchain seed (rust/gcc) remains guix-built (retired last).

## Mechanism (migrate-in-place)

For each of the 8 corpus packages (ripgrep/sd/fd/procs/eza/bat/coreutils/youki):
- The crate-free gate body (which calls tests/crate-free-build.sh) is moved INTO the canonical
  guix-path gate file, renaming the target `rust-<P>-crate-free` -> `rust-<P>` (sed). So the
  canonical `rust-<P>` gate now builds its crate closure guix-free.
- The now-duplicate `rust-<P>-crate-free` gate file is DELETED.
- tests/<P>.lock is stripped to the toolchain SEED only (drop `.crate ` dep FOD lines + the
  `^<sourcekey> ` source FOD line; keep rust/cargo/gcc-toolchain/coreutils/bash/tar/gzip). The
  crate-free gate already ignored those lines (it greps them out), so the build is unchanged —
  only the now-unused /gnu/store crate strings leave the lock.
- Doc headers reflowed to drop stale Phase-1 phrasing ("Contrast rust-<P>…guix build", "PoC #163").

affected-checks: the warm-cargo-proxy/crate-free-build mapping + self-test now point at the
canonical `rust-<P>` targets (the `-crate-free` targets are gone). The recipe/lock canonical
mappings (`ripgrep -> rust-ripgrep`) were already correct.

## Out of scope
- td-fetch: rust-fetch (348, guix-path) + rust-fetch-crate-free (354) both stay — td's own
  fetcher seed, not the shipped corpus.
- russh (td-russh-demo): LOCAL demo source, no crate-free replacement (deferred from #167).
- rust-vendor (335) / rust-ts-eval (350): seed/demo, still use TD_VENDOR_CRATES guix FODs — so
  the TD_VENDOR_CRATES code path in run_rust keeps coverage.

## Brick status
- All 8 migrated (sed body move + git rm duplicate + strip lock); doc headers cleaned.
- VALIDATED: `./check.sh rust-ripgrep` GREEN — the migrated canonical gate builds ripgrep
  guix-free from the stripped (toolchain-seed-only) lock; all durable legs (supply-chain ∈
  Cargo.lock, structural .drv has TD_VENDOR_DIR + no /gnu/store crate, behavioral rg greps a
  needle, repro). The other 7 use the identical transform; the full loop validates them.
- self-test green; make list-gates shows 8 canonical gates, 0 corpus crate-free duplicates.

## Verified-red
The durable legs are the SHARED tests/crate-free-build.sh, verified-red in #166 (ripgrep
corruption + cross-path against the guix .drv). The migration is a body-move + lock-strip; the
"no /gnu/store crate path" structural leg remains the guard (and the stripped lock has no crate
strings at all — `grep -c '\.crate ' tests/<P>.lock` == 0 for all 8).
