# build-check-split — separate the parallel build phase from the check gates

**Goal.** The cold / builder-change loop was single-threaded where it hurts: each
package-build gate (corpus/toolchain/corpus-deps-no-guix) built its specs SERIALLY
inside the gate, and only two gates ran at once under `check.sh`'s `make -j2`. With
`td-builder check` doing a reproducibility DOUBLE-build, a cold package costs 3
single-threaded builds; 21 packages × 3 builds funnelled through 2 lanes is the
bottleneck the user hit ("why is this so slow … can we separate building everything
from the checks").

**Approach.** A `build-recipes` make phase that realizes + reproducibility-checks every
package recipe up front, fanned out across cores into the shared content-addressed
cache (`.td-build-cache/pkg`). Each build is single-threaded (the builder runs `make`
serially, `NIX_BUILD_CORES=1`), so the fan-out is ~nproc wide with no internal
oversubscription. The check gates then cache-HIT the build and memo-skip the repro
double-build (the cache from #90), so they only run their durable behavioral +
migration-oracle assertions.

Nothing is weakened (directive 3): the SAME `.drv` is assembled, realized and
double-built — just once, in parallel, instead of serial-within-gate. The gates remain
self-sufficient (a direct `make corpus-no-guix` still builds + checks via the cache).

## Pieces

- `tests/build-pkg.sh` — build + reproducibility-check ONE spec into `.td-build-cache/pkg`
  (cached_build + cached_check from cache-lib.sh); per-package log, one-line status, so
  the parallel fan-out stays readable.
- `Makefile` — `build-recipes` target (resolves the node/tsc/ts-eval/td-builder env +
  realizes all seeds ONCE, then `xargs -P $(nproc)` over `$(BUILD_SPECS)`); `BUILD_SPECS`
  / `BUILD_GATES` pools; `check: $(CHEAP) build-recipes $(HEAVY)` with order-only deps so
  build-recipes runs after the fail-fast cheap gates and the build gates wait on it.
  `TD_BUILD_JOBS` overrides the fan-out width. (Exclusive-spine change — Makefile.)
- Each package-build gate fragment now appends its own `<g>_SPECS` to `BUILD_SPECS`
  (no shared list to collide on), registers into `BUILD_GATES`, and reads the shared
  `.td-build-cache/pkg`. rust-build is ordered after build-recipes (its cargo build
  would otherwise use all cores during the fan-out); td-builder is NOT in BUILD_SPECS
  (its lock is extended with the freshly-interned source — self-contained).

## Status / evidence

- Serial single-package build (`build-pkg.sh hello`): green (built + repro).
- Clean parallel cold `./check.sh build-recipes`: fans all 21 out at once, all
  build + reproducible, exit 0 (~17 min, bounded by the long pole — coreutils' 3
  sequential single-threaded builds — not the serial SUM of all 21). The earlier
  "command not found" buildlogs were from OVERLAPPING runs colliding on `$sd/b`; a
  single clean run is correct.
- Verified-red: broke hello's lock and fanned out hello+gzip+sed — the xargs fan-out
  exits 123 while the others cache-hit, and the recipe's `set -o pipefail` turns that
  into a red phase. A single package's failure is NOT swallowed by `xargs -P`.
- Full warm `./check.sh`: GREEN (exit 0). build-recipes ran (21 recipes / 16 cores,
  all CACHE HIT); the build gates cache-hit + memo-skipped the double-build and ran
  only their behavioral/oracle assertions; rust-build cache-hit (builder unchanged).
  Warm overhead of the new phase is just parallel cache-hits + NAR re-verify (<1 min);
  the loop's wall time stays dominated by the heavy gates (no-guix, rust-uutils, …).
