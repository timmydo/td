# ts-frontend Phase 1 (DESIGN §7.1, acceptance #1/#2 — the capstone). The full
# pipeline end to end: the v0 TS spec is transpiled (tsc) and evaluated (boa
# td-ts-eval), its emitted system() config JSON is mapped to a td-config and
# lowered (td-config->operating-system), and the resulting system derivation is
# diffed against the frozen system/td.scm oracle — the SAME convergence
# tests/typed-diff.scm proves for the Guile typed front-end, now driven from the
# TypeScript surface. Self-discriminating (tests/ts-diff.scm): the v0 spec
# CONVERGES (== oracle) and a perturbed spec (sshPort 2222) DISCRIMINATES
# (!= oracle), so the differential can never rot vacuous. Derivation-level (no
# image build) but coupled to the td-ts-eval Rust binary, so it slots in the
# heavy pool next to ts-eval.
HEAVY_GATES += ts-diff
# Not FAST_GATES: needs the boa (td-ts-eval) Rust closure — too heavy for the
# fast CI tier (absent from the small td-ci-fast image). Full check / ./check.sh.
ts-diff:
	@echo ">> ts-diff: TS v0 spec lowers (tsc->boa->config) to the oracle's system drv; a perturbed spec diverges (ts-frontend acceptance #1/#2)"
	@set -euo pipefail; \
	tsgo=`sh tests/tsgo.sh`; \
	ev=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" -a -x "$$ev" || { echo "ERROR: could not resolve td-tsgo / td-ts-eval" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	dj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/spec-v0.ts"`; \
	pj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/spec-perturbed.ts"`; \
	gj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/spec-gen1.ts"`; \
	test -n "$$dj" -a -n "$$pj" -a -n "$$gj" || { echo "ERROR: ts-emit produced no config JSON" >&2; exit 1; }; \
	echo ">> v0 config         : $$dj"; \
	echo ">> perturbed config  : $$pj"; \
	echo ">> generation config : $$gj"; \
	TD_TS_DEFAULT_JSON="$$dj" TD_TS_PERTURBED_JSON="$$pj" TD_TS_GEN_JSON="$$gj" $(GUIX) repl $(LOAD) tests/ts-diff.scm
