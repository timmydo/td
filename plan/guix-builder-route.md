# guix-builder-route — working notes

Handle: claude-fable-a94246 · base: `#114` (3e6ab5c, guix-surface on main) · 2026-06-20

## Goal

Lower the `guix-surface` packager count (#114) by routing the loop's td-builder
TOOL-USE sites off `guix build -e '(@ (system td-builder) td-builder)'` onto the
td-bootstrapped **stage0** builder (`cache-lib.sh load_stage0` → `$TB`), the mechanism
the gnu+rust gates already use ([[td-bootstrap-stage0-arc]]). The guix-built td-builder
stays ONLY at the genuine oracle legs.

## Route pattern (per gate)

Replace:
```
tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
```
with:
```
. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
```
The added `case` is a DURABLE structural leg: it asserts the gate runs on td's own
bootstrapped stage0, not a guix `/gnu/store` td-builder. The gate's existing behavioral
assertions then prove stage0's td-builder does the op correctly.

## Keep (oracle legs — NOT routed)

- `170-bootstrap.mk:86` (`gtb=`, stage0-vs-guix bootstrap oracle)
- `330-rust-build.mk:40` + `:89` (the self-host gate already builds via stage0; both
  guix-td-builder refs are the "agrees with / distinct from the guix-built builder"
  behavioral + migration oracle — own-then-diverge)
- `175-td-builder.mk:49` (the td-builder PACKAGE gate — its subject IS the guix package)

## PR plan (step 2 = several PRs)

- **PR 1 (DONE, #117): store-backend family** — 275/280/285/290/295/300/305/310 (8
  gates). Count 34→26. Each store gate uses `$$tb` for td's own store op AND the
  daemon-oracle RPC (store-add); stage0's td-builder is the same binary, so the
  differential is unchanged.
- **PR 2 (DONE, #118): drv-* family** — 230-drv-emit / 235-td-drv-build / 240-td-drv-add /
  245-td-drv-assemble (4 gates). Count 26→22. Their oracle is the guix DAEMON +
  `guix repl`/`guix build`, NOT the td-builder binary — stage0 is the same source so the
  emitted/assembled .drv is byte-identical.
- **PR 3 (this): loop-* family** — 265-loop-sandbox / 270-loop-rung (2 gates). Count
  22→20. These INTRINSIC self-tests run `$$tb host-sandbox` to nest td's own sandbox;
  stage0 provides the same host-sandbox, so routing them validates the stage0 sandbox
  (the one the loop actually runs on). Re-baselined `tests/guix-surface.expected` (20).
- Follow-on: td-check 250, resolve 255, rootless 130, sandbox-hardening 272,
  td-realize 355, td-offline 360, build-hermetic 356; then the td-ts-eval routing
  (step 3) → toward the ~11 oracle floor.

## Verified-red

- `./check.sh store-add` GREEN on stage0 (REALEXIT=0): td computed the identical store
  path, wrote a byte-identical (NAR) store file, registered it — all via the stage0
  td-builder, with the new `case "$$tb"` stage0 assertion present (so it passed because
  tb WAS stage0). The guix-surface gate also passed at the new 26 baseline.
- The `case "$$tb"` leg rejects a guix `/gnu/store/...-td-builder/...` path (exit 1) and
  accepts the `.td-build-cache/stage0/...` path — the durable proof each gate runs on
  td's own bootstrapped builder, not a guix-built one.

## Landing

- Gate-file edits only (no Makefile/spine) → parallel-safe.
- Strict ruleset: rebase-onto-tip + check-fast on BEHIND. Heal-check main on each fetch.
