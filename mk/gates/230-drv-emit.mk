# evaluator-as-library (DESIGN §7.1; the §5 move-off-Guile goal). The last Guile in
# hello's build path is the `.drv` CONSTRUCTION — `system/td-build.scm` calls Guile's
# `derivation` to compute the output path, serialize the ATerm, and write the `.drv`.
# This gate proves td-builder (Rust) constructs that `.drv` itself, byte-identical to
# guix's — the §6-named differential, "identical `.drv` both ways", guix the oracle.
# The subject is the `td-build` hello derivation: guix lowers it (the oracle `.drv`),
# then `td-builder drv-emit` re-CONSTRUCTS it from its skeleton (recompute every
# output path via the recursive hashDerivationModulo, the `.drv`'s own store path via
# nix-base32/make-text-path, and the ATerm via the serializer) and verifies the store
# path AND the bytes match. Self-discriminating: a perturbed recipe (one wrong byte in
# the source hash) is a DIFFERENT `.drv` the emitter must also match, and the two must
# differ — so the differential can never go vacuous. Heavy (a warm-store Rust compile
# of td-builder + two cheap lowerings), so it slots in the heavy pool by td-builder;
# RE-MEASURE and RE-SORT once it has run. Input RESOLUTION (which toolchain/source
# paths are inputs) stays Guix's — the toolchain is retired last (§5); what moved to
# Rust is the construction.
HEAVY_GATES += drv-emit
# Not FAST_GATES: td-builder Rust build — too heavy for the fast CI tier
# (absent from the small td-ci-fast image). Full check / local ./check.sh.
drv-emit:
	@echo ">> drv-emit: td-builder constructs the td-build hello .drv byte-identical to guix's; a perturbed recipe is a distinct .drv it also matches (evaluator-as-library)"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	drv=`TD_GUIX="$(GUIX)" sh tools/guix-lower.sh '((@ (system td-build) td-rust-build-derivation) s (quote (("name" . "hello") ("version" . "2.12.2") ("source" . (("uri" . "mirror://gnu/hello/hello-2.12.2.tar.gz") ("sha256" . "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js"))) ("buildSystem" . "gnu"))))' 2>/dev/null`; \
	drv_pert=`TD_GUIX="$(GUIX)" sh tools/guix-lower.sh '((@ (system td-build) td-rust-build-derivation) s (quote (("name" . "hello") ("version" . "2.12.2") ("source" . (("uri" . "mirror://gnu/hello/hello-2.12.2.tar.gz") ("sha256" . "1bqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js"))) ("buildSystem" . "gnu"))))' 2>/dev/null`; \
	test -n "$$drv" -a -n "$$drv_pert" || { echo "ERROR: could not lower the td-build derivations" >&2; exit 1; }; \
	echo ">> oracle .drv (guix-constructed): $$drv"; \
	echo ">> td-builder re-constructs it (output paths + .drv path + ATerm) and verifies byte-identity:"; \
	"$$tb" drv-emit "$$drv"; \
	echo ">> discriminator: a perturbed recipe is a DIFFERENT .drv, also constructed byte-identical:"; \
	"$$tb" drv-emit "$$drv_pert"; \
	test "$$drv" != "$$drv_pert" || { echo "FAIL: the perturbed recipe did not change the .drv — the differential is vacuous." >&2; exit 1; }; \
	echo "PASS: td-builder constructs the .drv byte-identical to guix (path + content) for the td-build hello derivation, and a perturbed recipe yields a distinct .drv it also matches — no guile in the construction."
