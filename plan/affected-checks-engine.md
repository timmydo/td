# affected-checks-engine — engine edits escalate to the full loop

Handle: claude-opus-c30fea — claimed 2026-06-19.

## The false-green gap

`tools/affected-checks.sh` maps a changed path to a focused check set and decides
whether the full `./check.sh` is waived. The `builder/Cargo.toml|builder/src/*` arm
selected only:

```
add_preflight cargo-test ; add_target cargo-test ; add_target td-builder
```

and did NOT `require_full`. But `builder/src/*` is the **td-builder build engine**
(`realize_drv`, `build_recipe`, `sandbox`, `store`, `drv`, `nar`, `store_db*`,
`build`, `scan` …) — the spine of EVERY recipe-building gate: `corpus-no-guix`,
`corpus-deps-no-guix`, `toolchain-no-guix`, the source-interning + bootstrap gates,
`rust-build`/`vendor`/`russh`/`uutils`, `build-plan`, `td-check`. `cargo-test` (unit
tests) + the single `td-builder` gate cannot prove a closure / drv / NAR change is
safe across that whole set.

Concretely (the bug that surfaced this): the build-plan track refactored
`realize_drv`/`build_recipe` to route the closure through `closure_multi` and to
carry both `src_override` (PR #97) and a multi-db `store_dbs`. The `src_override` +
`closure_multi` interaction lives ONLY in the source-interning gate, which the
dispatcher did not select — so a regression there would have been **waived to green**.

## Fix

Treat `builder/src/*` / `builder/Cargo.*` like the loop spine (`check.sh`, `Makefile`,
`channels.scm`): keep the fast `cargo-test` preflight for quick feedback, but
`require_full` so an engine edit cannot be locally waived. This is **drift-proof** —
it auto-covers every current and future engine-consuming gate without maintaining a
gate list (the same reason the gates were split into drop-in `mk/gates/*.mk` files).

## Verified-red

Encode the policy in the self-test FIRST:

```
assert_branch_policy builder/src/main.rs  "full ./check.sh would be required"
assert_branch_policy builder/src/sandbox.rs "full ./check.sh would be required"
assert_branch_policy builder/Cargo.toml   "full ./check.sh would be required"
```

Against the pre-fix dispatcher the self-test reds:

```
FAIL: builder/src/main.rs: missing '… full ./check.sh would be required'
FAIL: builder/src/sandbox.rs: missing '… full ./check.sh would be required'
FAIL: builder/Cargo.toml: missing '… full ./check.sh would be required'
affected-checks self-test: 3 failure(s)
```

After adding `require_full` to the `builder/*` arm: `PASS: affected-checks self-test`.

## Durable assertion

The self-test asserts the engine paths are NOT waivable — it still holds with no Guix
oracle in the room (it is a property of the dispatcher's policy, exercised by
`tools/affected-checks.sh --self-test`, the dispatcher's own guard).

## Landing

Touches `tools/affected-checks.sh` (+ self-test) + this track's plan files only — no
build code, no gates. affected-checks classifies its own diff as waived: the local
readiness set is `affected-self-test` + `shell-syntax` + `plan-index`.
