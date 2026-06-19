# retire-lowering-bridges — move-off-Guile §5 (drive the .scm count down)

**Goal.** The gates resolve td's seed packages through small Guile "lowering bridge"
scripts (`tests/<x>-drv.scm`) run with `guix repl`. Each exists only to print a
package's derivation file name (`DRV=…`) because a script piped into `guix repl`
always exits 0 (it would hide a red), so the honest build/`--check` happens in a
separate `guix build`. But `guix build` already does both jobs without a script:

- `guix build -d -e '(@ (system M) pkg)'` prints the **derivation file name** (verified
  byte-identical to the bridge's `DRV=` output), and
- `guix build -e '(@ (system M) pkg)'` prints the **output path** —

both with the honest exit status the bridges were working around, and using the exact
`-e '(@ (system M) pkg)'` form the gates ALREADY use for `td-typescript`.

**This increment.** Retire the two most-used package-lowering bridges:

- `tests/ts-eval-drv.scm` (9 uses) → resolve `td-ts-eval` directly:
  - binary-only sites (corpus/toolchain/corpus-deps/rust-build/rust-vendor/rust-uutils/
    ts-diff gates + the `build-recipes` phase): `guix build -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval.
  - the `ts-eval` gate (needs the .drv for `--check`): `guix build -d -e '…'`.
- `tests/td-builder-drv.scm` (1 use, the `td-builder` gate's `--check`): `guix build -d -e '(@ (system td-builder) td-builder)'`.

Net: −2 `.scm` files (52 → 50), ~10 fewer `guix repl` invocations, one uniform
resolution form. No behavior change — same .drv, same output path, same offline
`guix build`/`--check` in the sandbox (the bridges' `#:use-substitutes? #f` was
belt-and-suspenders for `guix repl`, which does not read GUIX_BUILD_OPTIONS; the gates'
`guix build` honors it via the loop sandbox).

Pure refactor (resolution-equivalent), so the test is that every touched gate stays
green; no new assertion ⇒ no verified-red beyond green. Future: the same `-d -e`
pattern retires the remaining package-lowering bridges as they come off the
system/retire-last list.

## Status / evidence

- `guix build -d -e '(@ (system td-ts) td-ts-eval)'` == ts-eval-drv.scm `DRV=` output (verified).
- Per-gate spot checks (`./check.sh ts-eval`, `./check.sh ts-diff`): TODO.
- Full `./check.sh`: TODO.
