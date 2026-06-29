# spec-diff (rust-recipe-surface; was ts-diff). The system-spec front-end end to
# end: the v0 spec — declared in Rust (recipes/src/specs.rs) and emitted by
# `td-recipe-eval emit-spec` — is mapped to a td-config and lowered
# (td-config->operating-system), and the resulting system derivation is diffed
# against the frozen system/td.scm oracle. Self-discriminating (tests/spec-diff.scm):
# the v0 spec CONVERGES (== oracle) and a perturbed spec (sshPort 2222)
# DISCRIMINATES (!= oracle), so the differential can never rot vacuous; a gen1 spec
# exercises the generation field. Derivation-level (no image build). The Guile
# lowering (td-config->operating-system) is the retire-last bridge; only the SPEC
# DATA moved to Rust.
HEAVY_GATES += spec-diff
# Not FAST_GATES: builds td-recipe-eval (rust) — absent from the small td-ci-fast image.
spec-diff:
	@echo ">> spec-diff: the Rust v0 spec lowers (td-recipe-eval -> config -> system drv) to the oracle; a perturbed spec diverges (rust-recipe-surface)"
	@set -euo pipefail; \
	TD_RECIPE_EVAL=`TD_GUIX="$(GUIX)" sh tests/recipe-eval-tool.sh "$(CURDIR)/.td-build-cache/recipe-eval"`; export TD_RECIPE_EVAL; \
	test -x "$$TD_RECIPE_EVAL" || { echo "ERROR: could not build td-recipe-eval" >&2; exit 1; }; \
	dj=`sh tests/recipe-emit.sh --spec v0`; \
	pj=`sh tests/recipe-emit.sh --spec perturbed`; \
	gj=`sh tests/recipe-emit.sh --spec gen1`; \
	test -n "$$dj" -a -n "$$pj" -a -n "$$gj" || { echo "ERROR: recipe-emit produced no config JSON" >&2; exit 1; }; \
	echo ">> v0 config         : $$dj"; \
	echo ">> perturbed config  : $$pj"; \
	echo ">> generation config : $$gj"; \
	TD_SPEC_DEFAULT_JSON="$$dj" TD_SPEC_PERTURBED_JSON="$$pj" TD_SPEC_GEN_JSON="$$gj" $(GUIX) repl $(LOAD) tests/spec-diff.scm
