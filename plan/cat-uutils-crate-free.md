# cat-uutils-crate-free — working notes

Handle: claude-opus-4b83d3 · started 2026-06-25 · base origin/main @ e81247a (#172).

## Goal
Retire the last corpus rust gate's `/gnu/store` crate FODs. The `rust-uutils` gate
(340, builds the uutils `cat` = crate `uu_cat` 0.9.0) was left out of #169 (which
migrated the 8 corpus tools + rust-coreutils to the cargo-proxy path). Migrate it the
same way: guix-free crate provisioning through td's own cargo-proxy + the shared
`tests/crate-free-build.sh` build/assert helper.

## Sub-tasks
1. [done] Confirm `uu_cat` 0.9.0 ships a Cargo.lock so the generic warm works —
   `tools/warm-cargo-proxy.sh uu_cat 0.9.0 cat` → source + **139 crates** provisioned
   guix-free (matches the old lock's 139). VERIFIED.
2. [done] Strip `tests/cat-uutils.lock` to the 7 toolchain-seed lines (0 crate FODs, 0
   `cat-source` FOD).
3. [done] Rewrite `mk/gates/340-rust-uutils.mk` to call
   `crate-free-build.sh cat uu_cat-0.9.0 tests/cat-uutils.lock cat-source tests/ts/recipe-cat.ts`
   + the cat file/stdin behavioral leg (was TD_VENDOR_CRATES + guix-build the 139 FODs).
4. [done] `check.sh` prelude: `sh tools/warm-cargo-proxy.sh uu_cat 0.9.0 cat || true`
   (EXCLUSIVE spine landing).
5. [done] `tools/affected-checks.sh`: warm-cargo-proxy/crate-free-build block → also
   `rust-uutils`; self-test `assert_target tests/cat-uutils.lock rust-uutils`. self-test GREEN.
6. [in-progress] `./check.sh rust-uutils` GREEN.
7. [todo] Verified-red: perturb a crate sha / drop the vendor tree → gate goes red.
8. [todo] Full loop (affected-checks escalates on the check.sh edit) → land per protocol.

## Notes
- No `recipe-cat.ts` change: the corpus migration left `recipe-uutils.ts` mentioning
  TD_VENDOR_CRATES too — the recipe is vendoring-agnostic; minimal increment.
- Source is the upstream `uu_cat` crate tarball (a crates.io crate, unlike td-fetch's
  local `fetch/` dir), so the generic warm + crate-free-build.sh apply unchanged.
- After this, every corpus rust gate is guix-free. Remaining FOD-carrying locks are the
  seed/demo set: td-feed (builds itself), td-russh-demo (local src), td-ts-eval (seed),
  td-vendor-demo (keeps one TD_VENDOR_CRATES path alive).
