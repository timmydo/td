# ts-frontend Phase 1 (DESIGN §7.1, sub-task 2) — the boa evaluator + curated
# global. Builds td-ts-eval (pure-Rust boa, crates from the hash-pinned
# %ts-eval-vendor fixed-output, compiled offline) and `--check`s it reproducible
# (prime directive 1 — it IS a new built artifact, like td-builder), then asserts
# the hermetic eval via tests/ts-eval-check.sh: a trivial expression evaluates to
# a known value, `typeof Date === "undefined"` (clock removed), and
# `Math.random()` is DENIED (the always-on negative control), while Math is
# otherwise intact. Heavy (a warm-store Rust build + a --check), so it slots late
# in the LPT order alongside td-builder.
HEAVY_GATES += ts-eval
FAST_GATES += ts-eval
ts-eval:
	@echo ">> ts-eval: boa evaluator + curated global (ts-frontend Phase 1, sub-task 2)"
	@set -euo pipefail; \
	drv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	echo ">> td-ts-eval derivation: $$drv"; \
	out=`$(GUIX) build "$$drv"`; \
	test -n "$$out" || { echo "ERROR: the td-ts-eval build produced no output path" >&2; exit 1; }; \
	echo ">> check: reproducibility of td-ts-eval (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$drv"; \
	TD_TS_EVAL="$$out/bin/td-ts-eval" sh tests/ts-eval-check.sh
