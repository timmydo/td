# td — the single pass/fail entry point (CLAUDE.md "The loop").
#
# `make check` runs the rung ladder. The authoritative rung list is the
# CHEAP_RUNGS/HEAVY_RUNGS pools below, which the `check:` target expands
# (CLAUDE.md "The loop"); per-rung documentation lives as a comment on each
# rule. Cheap structural rungs run serial-first, heavy rungs two at a time
# (-j2, LPT order); a red stops new rungs from spawning.
#
# Every guix invocation is pinned to channels.scm via `guix time-machine`, so
# the reproducibility oracle is honest regardless of the ambient guix version.
# Run it via `./check.sh` (the hermetic, offline wrapper) — NOT a bare
# `guix shell -C --pure -- make check`, which lacks the store/daemon exposure,
# host-guix-pin guard, and substitute-disabling that keep the loop offline.

# Recipes use bash so multi-command recipes can run under `set -euo pipefail`
# (triage #1): a failure ANYWHERE in a `;`-chained recipe — notably a
# `guix build --check` reproducibility failure or an unreadable artifact — must
# abort the rung, never be swallowed so a later command's success greens it.
SHELL   := bash

GUIX    := guix time-machine -C channels.scm --
LOAD    := -L .
SYSTEM  := system/td.scm
IMGTYPE := qcow2

# Canned lower-then-realise for marionette system tests (the `test`,
# `boot-disk` and `reset` rungs; `container` lowers multiple artifacts and
# keeps its own block). Two steps on purpose: `guix repl` reading a script
# from STDIN always exits 0 (it swallows the script's exit code), so building
# the test there would make a FAILED test look green. Instead: (1) lower the
# monadic test value to a derivation file name via repl, then (2) realise it
# with `guix build`, whose exit status is honest and which streams the
# marionette log so failures are visible.
#   $(1) = test module, e.g. (tests boot)
#   $(2) = system-test variable, e.g. %test-td-boot
#   $(3) = label for messages, e.g. boot
define realise-system-test
	@drv=`printf '%s\n' \
	    '(use-modules (guix) (gnu tests) $(1))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (format #t "DRV=~a~%"' \
	    '          (derivation-file-name' \
	    '           (run-with-store store (system-test-value $(2))))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the $(3) test derivation" >&2; exit 1; }; \
	echo ">> realise $(3) test derivation: $$drv"; \
	$(GUIX) build "$$drv"
endef

# Bare `make` runs the in-sandbox loop, never the sandbox wrapper — guards
# against `container-check` (which calls ./check.sh) being the default goal and
# recursing into nested containers.
.DEFAULT_GOAL := check

# The 37 rungs, in the two pools the bounded-parallel loop schedules from.
# ADDING A RUNG: put it in exactly ONE pool below — .PHONY, the `check` target,
# the serial chain, and the heavy gate are all DERIVED from these two
# variables, so the lists cannot drift apart (review finding: they used to be
# three hand-kept copies).
#
# CHEAP_RUNGS are the sub-5s structural rungs; their list order IS their
# strict serial execution order (a generated order-only chain below), so a
# syntax error or differential regression reds the loop before any VM boots
# or tarball repacks.
#
# HEAVY_RUNGS run at most two at a time under check.sh's `make -j2` (DESIGN
# §7.3 resource note: more concurrent VMs may thrash; empirically the daemon
# overlaps two client builds, 17s->10s on the place trees). They are listed
# LONGEST-FIRST (LPT packing): under -j2 make starts them in list order, and
# seeding the slots with the longest rungs lets the short ones fill the gaps
# instead of leaving a long rung to run alone at the end (measured: the naive
# order left `container` solo for its full 71s). RE-MEASURE AND RE-SORT this
# list whenever a rung is added or the full-check wall time drifts well past
# the recorded 341s (-j2 floor with 18 rungs, 2026-06-10; per-rung numbers:
# plan/loop-latency.md "Measurement log"). `rollback` is seeded first on
# M10.3's judgment, not yet individually measured; `rootless` is slotted after
# `container` on its measured solo run (36s incl. sandbox setup,
# plan/rootless-builder.md); `oci-load` after `rootless` on its measured solo
# run (plan/oci-load.md — skopeo passes are seconds; the gunzip/regzip of the
# negative control dominates). `td-builder` (S1) is slotted late on judgment,
# not yet individually measured — its cost is a single warm-store Rust compile
# plus a --check rebuild; RE-MEASURE and RE-SORT once it has run. `registry`
# (M12 S3) is slotted after `oci-load` on judgment — the same skopeo passes
# plus a --check rebuild and signify signing, all seconds warm; RE-MEASURE
# once it has run. `verify-place` (M12 S4) sits right after `registry` (same
# inputs warm: registry + place trees; its own cost is the verified tree
# build + --check and shell-side rejection legs); RE-MEASURE once it has run.
# A stale order only costs latency, never correctness.
#
# NOTHING is removed, loosened, or skipped by the parallelism: all rungs must
# still pass, and make (run without -k) stops spawning new rungs after a
# failure — a red still short-circuits the loop. Order-only (|) prerequisites,
# so a plain serial `make -j1 check` behaves exactly as before.
CHEAP_RUNGS := eval diff typed-coverage oci-diff manifest-diff generation-diff
HEAVY_RUNGS := rollback generation-image no-guix manifest-check oci container rootless oci-load registry verify-place reset test place build boot-disk td-builder run offline memo ts ts-eval ts-diff corpus td-build drv-emit td-drv-build td-drv-add td-drv-assemble td-check loop-sandbox loop-rung

.PHONY: check check-fast container-check $(CHEAP_RUNGS) $(HEAVY_RUNGS)

# The hermetic, offline, self-contained entry point (DESIGN §1.1/§1.4). Plain
# `make check` assumes you are ALREADY inside the right `guix shell -C` sandbox;
# `make container-check` (or ./check.sh) sets that sandbox up for you. Prefer it.
container-check:
	@./check.sh

check: $(CHEAP_RUNGS) $(HEAVY_RUNGS)

# The fast tier — the rungs that test td's OWN surface (typed/TS front-end + the
# Rust builder/evaluator) and need only the toolchain: no `guix system image`,
# no marionette VM, no QEMU/kernel/bootloader closure. A STRICT SUBSET of
# `check`, for quick "is td's logic right" feedback and for a light CI job that
# need not import the full system/boot closure. PURELY ADDITIVE: `check` above
# is unchanged and remains the gate; nothing here removes, loosens, reorders, or
# skips a rung. FAST_RUNGS are a subset of HEAVY_RUNGS, so the ordering graph
# below already gates them on the last cheap rung.
FAST_RUNGS := ts ts-diff ts-eval corpus td-build drv-emit
check-fast: $(CHEAP_RUNGS) $(FAST_RUNGS)

# Generated ordering graph (do not hand-edit): chain each cheap rung
# order-only on its predecessor, and gate every heavy rung on the last cheap
# rung.
chain-prev :=
$(foreach r,$(CHEAP_RUNGS),$(eval $(if $(chain-prev),$(r): | $(chain-prev)))$(eval chain-prev := $(r)))
$(HEAVY_RUNGS): | $(lastword $(CHEAP_RUNGS))

# 1. Config eval — load every module; catches syntax/binding errors in well
#    under a second, before any expensive build. Run as a repl SCRIPT, NOT piped
#    via STDIN: `guix repl` reading from STDIN always exits 0 (swallows the
#    script's status), which made a broken module pass `eval` green. `guix repl
#    FILE` honors the exit code, so a load error reddens this rung honestly.
eval:
	@echo ">> eval: load (system td), (system td-typed), (tests boot) and (tests container)"
	$(GUIX) repl $(LOAD) tests/eval.scm

# M4 differential (DESIGN §2.4/§2.5). Cheap structural check — lowers systems to
# derivations, no building — so it runs right after eval and fails fast. Run as
# a repl SCRIPT (not piped via STDIN) so the script's `(exit)` is the rung's
# exit status; a piped script would always exit 0 and hide a red (see `test`).
diff:
	@echo ">> diff: typed front-end lowers to the same store path as the gexp"
	$(GUIX) repl $(LOAD) tests/typed-diff.scm

# M4 typed coverage (triage #4). Table-driven, derivation-level: every typed
# field must (A) change the lowered system when given a valid non-default value
# (proves it is wired, not ignored) and (B) reject an invalid value at
# construction (proves per-field validation). Where `diff` checks convergence +
# one perturbation, this sweeps all fields. Run as a repl SCRIPT for honest exit.
typed-coverage:
	@echo ">> typed-coverage: every typed field is wired and validated"
	$(GUIX) repl $(LOAD) tests/typed-coverage.scm

# M5 OCI differential (DESIGN §2.4 step 5/§2.5). Same cheap, derivation-level,
# self-discriminating shape as `diff`, but the artifact is the Docker/OCI image
# derivation: prove the typed front-end drives the OCI image too, and that a
# changed config diverges. No image is built here — the bit-for-bit repro check
# is the `oci` rung below. Run as a repl SCRIPT so `(exit)` is the rung's status.
oci-diff:
	@echo ">> oci-diff: typed front-end lowers to the same OCI image drv as the gexp"
	$(GUIX) repl $(LOAD) tests/oci-diff.scm

# M6 manifest-swap differential (DESIGN §6: manifest-driven, image-swap-only).
# Cheap, derivation-level, self-discriminating like `oci-diff`, but the lever is
# the typed config's `manifest` field: (a) the default manifest converges to the
# frozen OCI oracle; (b) a manifest that adds one package (hello) lowers to a
# DIFFERENT OCI image — a wholesale image swap; (c) the added package is in the
# swapped system's package set and absent from the default's. No image is built
# here — the bit-for-bit repro of a SWAPPED generation is the `manifest-check`
# rung below. Run as a repl SCRIPT so `(exit)` is the rung's status.
manifest-diff:
	@echo ">> manifest-diff: a changed manifest swaps the whole OCI image"
	$(GUIX) repl $(LOAD) tests/manifest-diff.scm

# M10.1 per-generation root (DESIGN §2.3 generations; M10-design.md P1). Cheap,
# derivation/record-level, self-discriminating like the diffs above: prove the
# typed `generation` field derives a DISTINCT, bootloader-selectable root per
# generation — (a) generation #f still converges to the shared-root oracle, (b)
# two generations get different root labels AND different system drvs, (c) a
# generation's root is not the shared td-root. Without this each generation would
# boot the same filesystem and rollback would be a no-op. The full boot+rollback
# is M10.3. Run as a repl SCRIPT so `(exit)` is the rung's status.
generation-diff:
	@echo ">> generation-diff: each generation gets a distinct, selectable root (M10.1)"
	$(GUIX) repl $(LOAD) tests/generation-diff.scm

# corpus-independence (DESIGN §7.1, Phase 2 of the §5 move-off-Guile goal). The
# CORPUS axis (where a package definition comes from), driven through the SAME
# TypeScript front-end as `ts-diff` — now declaring a PACKAGE instead of the
# system. A recipe AUTHORED in TypeScript (tests/ts/recipe-hello.ts — reconstructed
# from upstream coordinates, NOT looked up in the Guix corpus) is transpiled (tsc)
# + evaluated (boa td-ts-eval) to recipe JSON, lowered through the generic Guile
# recipe bridge (system td-recipe — the retire-last lowering target), and proven
# equal to the pinned corpus's own `hello` (the §2.5 oracle). Two legs:
#   (1) differential (tests/ts-recipe-diff.scm) — the TS recipe CONVERGES on the
#       oracle drv and a perturbed recipe (recipe-perturbed.ts, one wrong byte in
#       the source hash) DIVERGES; build-free (#:graft? #f), self-discriminating;
#   (2) build + --check (prime directive 1) — build the bridged recipe, --check it
#       reproducible (verdict-memoized — tests/check-memo.sh), assert the built
#       store object is path-identical AND NAR-hash-equal to the corpus oracle's.
# Heavy (TS toolchain + a warm hello compile + a --check), so it slots in the heavy
# pool next to the other ts rungs; RE-MEASURE and RE-SORT once it has run.
corpus:
	@echo ">> corpus: a TypeScript-authored recipe lowers (tsc->boa->bridge) to the corpus oracle's hello; build + --check NAR-hash-equal (corpus-independence Phase 2)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-hello.ts"`; \
	pj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-perturbed.ts"`; \
	test -n "$$rj" -a -n "$$pj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> hello recipe JSON     : $$rj"; \
	echo ">> perturbed recipe JSON : $$pj"; \
	echo ">> differential: TS recipe converges on the corpus oracle; perturbed diverges"; \
	TD_RECIPE_JSON="$$rj" TD_RECIPE_PERTURBED_JSON="$$pj" $(GUIX) repl $(LOAD) tests/ts-recipe-diff.scm; \
	echo ">> build leg: lower the bridged recipe, build, --check, NAR-equal"; \
	vars=`TD_RECIPE_JSON="$$rj" $(GUIX) repl $(LOAD) tests/ts-recipe-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_DRV=//p'`; \
	oracle_drv=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_DRV=//p'`; \
	oracle_out=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_OUT=//p'`; \
	test -n "$$td_drv" -a -n "$$oracle_drv" -a -n "$$oracle_out" \
	  || { echo "ERROR: could not lower the recipe derivations" >&2; exit 1; }; \
	echo ">> TS recipe drv     : $$td_drv"; \
	echo ">> corpus oracle drv : $$oracle_drv"; \
	test "$$td_drv" = "$$oracle_drv" \
	  || { echo "FAIL: TS recipe drv != corpus oracle drv — convergence lost at the build-derivation level." >&2; exit 1; }; \
	echo ">> build the bridged recipe"; \
	out=`$(GUIX) build "$$td_drv"`; \
	test -n "$$out" || { echo "ERROR: building the recipe produced no output path" >&2; exit 1; }; \
	echo ">> check: reproducibility (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$td_drv"; \
	test "$$out" = "$$oracle_out" \
	  || { echo "FAIL: built $$out but the corpus oracle is $$oracle_out — not the same store object." >&2; exit 1; }; \
	echo ">> NAR-hash-equal (§6 metric)"; \
	nar_td=`$(GUIX) hash -S nar "$$out"`; \
	nar_or=`$(GUIX) hash -S nar "$$oracle_out"`; \
	echo "   TS recipe NAR     : $$nar_td"; \
	echo "   corpus oracle NAR : $$nar_or"; \
	test -n "$$nar_td" -a "$$nar_td" = "$$nar_or" \
	  || { echo "FAIL: TS recipe NAR hash != corpus oracle NAR hash." >&2; exit 1; }; \
	echo "PASS: a TypeScript-authored recipe builds reproducibly to the corpus oracle's exact store object (NAR-hash-equal)."

# corpus-independence — own Rust builder (DESIGN §7.1, Phase 2; the §5 move-off-
# Guile goal, the "behaviorally equal where a recipe legitimately differs" case
# named in the §7.1 entry). Where `corpus` lowers the TS recipe through
# gnu-build-system (a Guile build-system + a `guile` builder), this lowers the SAME
# TS recipe through system/td-build — a raw `derivation` whose BUILDER is the
# td-builder Rust binary (`autotools-build`, builder/src/build.rs). So gnu-build-
# system and build-time Guile are GONE from the build (guix still constructs the
# .drv — the scope the human fixed 2026-06-13). The own-builder output has a
# DIFFERENT store path (own builder → own $out, which hello bakes in), so the
# differential is BEHAVIORAL, not NAR-equal:
#   • STRUCTURAL proof — the td-build derivation's builder basename is `td-builder`
#     (the Rust binary), while the corpus oracle's is `guile` (gnu-build-system);
#   • the artifact is reproducible (`guix build --check`, verdict-memoized — prime
#     directive 1);
#   • BEHAVIORAL equivalence — the td-built hello and the corpus oracle hello print
#     byte-identical output ("Hello, world!"), at DISTINCT store paths.
# Heavy (TS front-end + a hello compile + a --check + the oracle build), so it
# slots in the heavy pool next to `corpus`; RE-MEASURE and RE-SORT once it has run.
td-build:
	@echo ">> td-build: a TS recipe built by td's OWN Rust builder (no gnu-build-system) is reproducible and behaves identically to the corpus hello (corpus-independence)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-hello.ts"`; \
	test -n "$$rj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> recipe JSON (TS-authored): $$rj"; \
	vars=`TD_RECIPE_JSON="$$rj" $(GUIX) repl $(LOAD) tests/td-build-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_BUILD_DRV=//p'`; \
	td_builder=`printf '%s\n' "$$vars" | sed -n 's/^TD_BUILD_BUILDER=//p'`; \
	oracle_drv=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_DRV=//p'`; \
	oracle_builder=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_BUILDER=//p'`; \
	oracle_out=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_OUT=//p'`; \
	test -n "$$td_drv" -a -n "$$oracle_drv" -a -n "$$oracle_out" \
	  || { echo "ERROR: could not lower the td-build / oracle derivations" >&2; exit 1; }; \
	echo ">> td-build drv : $$td_drv (builder: $$td_builder)"; \
	echo ">> oracle   drv : $$oracle_drv (builder: $$oracle_builder)"; \
	echo ">> STRUCTURAL proof: the builder is the Rust binary, not gnu-build-system's guile"; \
	case "$$td_builder" in td-builder*) : ;; *) echo "FAIL: td-build builder is '$$td_builder', expected the td-builder Rust binary." >&2; exit 1;; esac; \
	case "$$oracle_builder" in guile*) : ;; *) echo "FAIL: oracle builder is '$$oracle_builder', expected guile (gnu-build-system) — the contrast is not meaningful." >&2; exit 1;; esac; \
	echo ">> build the TS recipe with td's OWN Rust builder"; \
	out=`$(GUIX) build "$$td_drv"`; \
	test -n "$$out" -a -x "$$out/bin/hello" || { echo "FAIL: the td build produced no bin/hello" >&2; exit 1; }; \
	echo ">> check: reproducibility of the td-built artifact (verdict-memoized — prime directive 1)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$td_drv"; \
	echo ">> build the corpus oracle hello (gnu-build-system)"; \
	oracle_out_built=`$(GUIX) build "$$oracle_drv"`; \
	echo ">> behavioral differential: run BOTH, stdout must be byte-identical"; \
	td_say=`"$$out/bin/hello"`; \
	oracle_say=`"$$oracle_out_built/bin/hello"`; \
	echo "   td     hello -> $$td_say"; \
	echo "   oracle hello -> $$oracle_say"; \
	test "$$td_say" = "Hello, world!" || { echo "FAIL: td-built hello printed '$$td_say', expected 'Hello, world!'." >&2; exit 1; }; \
	test "$$td_say" = "$$oracle_say" || { echo "FAIL: td-built hello output differs from the corpus oracle ('$$td_say' vs '$$oracle_say')." >&2; exit 1; }; \
	echo ">> independence: the own-builder artifact is a DISTINCT store object"; \
	test "$$out" != "$$oracle_out" || { echo "FAIL: the td-built path equals the corpus oracle path — not an independent build." >&2; exit 1; }; \
	echo "PASS: a TS recipe built by td's OWN Rust builder (builder=$$td_builder, no gnu-build-system) is reproducible and prints byte-identical output to the corpus hello (gnu-build-system), at a distinct store path ($$out != $$oracle_out)."

# evaluator-as-library (DESIGN §7.1; the §5 move-off-Guile goal). The last Guile in
# hello's build path is the `.drv` CONSTRUCTION — `system/td-build.scm` calls Guile's
# `derivation` to compute the output path, serialize the ATerm, and write the `.drv`.
# This rung proves td-builder (Rust) constructs that `.drv` itself, byte-identical to
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
drv-emit:
	@echo ">> drv-emit: td-builder constructs the td-build hello .drv byte-identical to guix's; a perturbed recipe is a distinct .drv it also matches (evaluator-as-library)"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	vars=`$(GUIX) repl $(LOAD) tests/drv-emit-drv.scm 2>/dev/null`; \
	drv=`printf '%s\n' "$$vars" | sed -n 's/^DRV=//p'`; \
	drv_pert=`printf '%s\n' "$$vars" | sed -n 's/^DRV_PERT=//p'`; \
	test -n "$$drv" -a -n "$$drv_pert" || { echo "ERROR: could not lower the td-build derivations" >&2; exit 1; }; \
	echo ">> oracle .drv (guix-constructed): $$drv"; \
	echo ">> td-builder re-constructs it (output paths + .drv path + ATerm) and verifies byte-identity:"; \
	"$$tb" drv-emit "$$drv"; \
	echo ">> discriminator: a perturbed recipe is a DIFFERENT .drv, also constructed byte-identical:"; \
	"$$tb" drv-emit "$$drv_pert"; \
	test "$$drv" != "$$drv_pert" || { echo "FAIL: the perturbed recipe did not change the .drv — the differential is vacuous." >&2; exit 1; }; \
	echo "PASS: td-builder constructs the .drv byte-identical to guix (path + content) for the td-build hello derivation, and a perturbed recipe yields a distinct .drv it also matches — no guile in the construction."

# td-drv-build (DESIGN §7.1; the capstone of the §5 move-off-Guile arc). Stitches
# #22 (emit) + #21 (the autotools-build builder) + td-builder S3/S4 (the executor):
# for the `td-build` hello subject, td-builder EMITS the `.drv` AND EXECUTES it in its
# own user-namespace sandbox, output NAR-equal to the daemon's build of the same
# recipe — so construct AND execute are td's Rust, the derivation's builder is
# `td-builder autotools-build` run by `td-builder build`, with NO guile in either; the
# daemon is ONLY the oracle (prime directive 4). The rung: lower + daemon-build the
# hello drv (the oracle facts via query-path-info); `drv-emit-to` writes the emitted
# `.drv` (asserted byte-identical to guix's); stage the input closure; `td-builder
# build` the EMITTED file; assert the registered output — path, NAR hash, size,
# deriver — equals the daemon's recorded facts. Reuses the td-builder S3/S4 harness.
# Scope boundary (honest): input resolution + the closure (`guix gc -R`) + the daemon
# BUILDING the inputs stay Guix's; only the TOP derivation is td-constructed +
# td-executed (toolchain retired last, §5). Heavy (a td-builder compile + two hello
# compiles — daemon oracle + td executor), so it slots in the heavy pool by
# td-builder; RE-MEASURE and RE-SORT once it has run. Scratch on disk (the staged
# closure + output), kept on red for triage, removed on green.
td-drv-build:
	@echo ">> td-drv-build: td-builder EMITS the td-build hello .drv AND EXECUTES it (userns sandbox), output NAR-equal to the daemon — no guile in construct or execute"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-drv-build-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	$(GUIX) repl $(LOAD) tests/td-drv-build-drv.scm 2>/dev/null > "$$scratch/facts.txt"; \
	drv=`sed -n 's/^HELLO_DRV=//p' "$$scratch/facts.txt"`; \
	out=`sed -n 's/^HELLO_OUT=//p' "$$scratch/facts.txt"`; \
	hash=`sed -n 's/^HELLO_HASH=//p' "$$scratch/facts.txt"`; \
	narsize=`sed -n 's/^HELLO_NARSIZE=//p' "$$scratch/facts.txt"`; \
	deriver=`sed -n 's/^HELLO_DERIVER=//p' "$$scratch/facts.txt"`; \
	test -n "$$drv" -a -n "$$out" -a -n "$$hash" -a -n "$$narsize" -a -n "$$deriver" \
	  || { echo "ERROR: could not lower/daemon-build the hello drv oracle facts" >&2; exit 1; }; \
	echo ">> oracle (daemon-built): $$out  nar-hash sha256:$$hash"; \
	echo ">> td-builder EMITS the .drv (construct, #22) — byte-identical to guix's:"; \
	"$$tb" drv-emit "$$drv" >/dev/null \
	  || { echo "FAIL: td's construction is not byte-identical to guix's .drv" >&2; exit 1; }; \
	computed=`"$$tb" drv-emit-to "$$drv" "$$scratch/emitted.drv" 2>"$$scratch/emit.err"` \
	  || { echo "FAIL: td-builder drv-emit-to failed:" >&2; cat "$$scratch/emit.err" >&2; exit 1; }; \
	test "$$computed" = "$$drv" \
	  || { echo "FAIL: td computed a different .drv store path ($$computed vs $$drv)" >&2; exit 1; }; \
	echo "   emitted .drv byte-identical to guix's (drv-emit verified), at $$computed"; \
	echo ">> stage the input closure"; \
	{ sed -n 's/^HELLO_INPUT=//p' "$$scratch/facts.txt"; echo "$$drv"; } | xargs $(GUIX) gc -R | sort -u > "$$scratch/paths.txt"; \
	echo "   staged closure: $$(wc -l < "$$scratch/paths.txt") store items"; \
	echo ">> td-builder EXECUTES the EMITTED .drv in its userns sandbox (builder=td-builder autotools-build, #21):"; \
	"$$tb" build "$$scratch/emitted.drv" "$$scratch/paths.txt" "$$scratch/b" > "$$scratch/buildout.txt" \
	  || { echo "FAIL: td-builder could not build the emitted hello .drv" >&2; cat "$$scratch/buildout.txt" >&2; exit 1; }; \
	grep -qx "OUT=out $$out" "$$scratch/buildout.txt" \
	  || { echo "FAIL: td-builder built a different output than the daemon's $$out" >&2; exit 1; }; \
	reg="$$scratch/b/registration"; \
	test -s "$$reg" || { echo "FAIL: td-builder wrote no registration record" >&2; exit 1; }; \
	echo ">> differential: td's registered output vs the daemon's recorded facts"; \
	grep -qx "path $$out" "$$reg" || { echo "FAIL: store-path mismatch (record below) vs $$out" >&2; cat "$$reg" >&2; exit 1; }; \
	grep -qx "nar-hash sha256:$$hash" "$$reg" || { echo "FAIL: NAR hash mismatch — td '$$(sed -n 's/^nar-hash //p' "$$reg")' vs daemon 'sha256:$$hash'" >&2; exit 1; }; \
	grep -qx "nar-size $$narsize" "$$reg" || { echo "FAIL: NAR size mismatch — td '$$(sed -n 's/^nar-size //p' "$$reg")' vs daemon '$$narsize'" >&2; exit 1; }; \
	grep -qx "deriver $$deriver" "$$reg" || { echo "FAIL: deriver mismatch — td '$$(sed -n 's/^deriver //p' "$$reg")' vs daemon '$$deriver'" >&2; exit 1; }; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo "PASS: td-builder EMITTED the td-build hello .drv (byte-identical to guix's) AND EXECUTED it in its own userns sandbox, registering the daemon's exact facts (path, NAR hash, size, deriver) — no guile in construct or execute; the daemon is only the oracle."

# td-drv-add (DESIGN §7.1; the §5 move-off-Guile arc). Wire td's constructed `.drv`
# INTO the loop: td-builder REGISTERS it in the store itself via the guix-daemon
# worker-protocol `addTextToStore` (a Rust client, builder/src/daemon.rs) — no guile
# `(derivation …)`/`add-text-to-store`. The daemon (C++) stays the store/build
# backend. The rung: (1) `drv-emit` — td constructs the hello `.drv` byte-identical to
# guix's (#22); (2) `drv-add` — register it via the daemon, which returns td's OWN
# computed path (== guix's, by content addressing); (3) `store-add` of a
# uniquely-named object — the daemon WRITES td's bytes at a NOVEL path (proves it is
# not idempotent reuse: the path did not exist, and the read-back content matches);
# (4) `guix build` the td-registered `.drv` — output runs (Hello, world!), i.e. the
# loop builds td's registration. Scope: input RESOLUTION (the skeleton `.drv`) stays
# Guix's; the daemon is the backend. Heavy (a td-builder compile + a warm hello
# realise), so it slots in the heavy pool by the other td rungs. Scratch on disk,
# removed on green.
td-drv-add:
	@echo ">> td-drv-add: td-builder REGISTERS its constructed .drv via the daemon (addTextToStore) — no guile (derivation …); the loop builds td's registration"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-drv-add-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	drv=`$(GUIX) repl $(LOAD) tests/td-drv-add-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the td-build hello derivation" >&2; exit 1; }; \
	echo ">> hello .drv (skeleton, guile-resolved inputs): $$drv"; \
	echo ">> (1) td constructs the .drv byte-identical to guix's (#22):"; \
	"$$tb" drv-emit "$$drv" >/dev/null \
	  || { echo "FAIL: td's construction is not byte-identical to guix's .drv" >&2; exit 1; }; \
	echo ">> (2) td REGISTERS it via the daemon addTextToStore — daemon returns td's computed path:"; \
	added=`"$$tb" drv-add "$$drv"` \
	  || { echo "FAIL: td-builder drv-add (daemon registration) failed" >&2; exit 1; }; \
	test "$$added" = "$$drv" \
	  || { echo "FAIL: the daemon registered $$added but the .drv is $$drv" >&2; exit 1; }; \
	echo "   registered at td's own computed path: $$added"; \
	echo ">> (3) NOVEL-write proof: a uniquely-named object the store did NOT have:"; \
	uniq="td-drv-add-probe-$$$$.txt"; \
	printf 'td novel write %s\n' "$$uniq" > "$$scratch/novel.txt"; \
	novel=`"$$tb" store-add "$$uniq" "$$scratch/novel.txt"` \
	  || { echo "FAIL: td-builder store-add (daemon write) failed" >&2; exit 1; }; \
	test -f "$$novel" \
	  || { echo "FAIL: the daemon did not write the novel path $$novel" >&2; exit 1; }; \
	test "`cat "$$novel"`" = "`cat "$$scratch/novel.txt"`" \
	  || { echo "FAIL: the daemon-stored content does not match what td sent" >&2; exit 1; }; \
	echo "   daemon wrote $$novel (content matches td's bytes)"; \
	echo ">> (4) the loop builds td's REGISTERED .drv:"; \
	out=`$(GUIX) build "$$added"`; \
	test -n "$$out" -a -x "$$out/bin/hello" \
	  || { echo "FAIL: guix build of the td-registered .drv produced no bin/hello" >&2; exit 1; }; \
	say=`"$$out/bin/hello"`; \
	test "$$say" = "Hello, world!" \
	  || { echo "FAIL: the built hello printed '$$say', expected 'Hello, world!'" >&2; exit 1; }; \
	rm -rf "$$scratch"; \
	echo "PASS: td-builder constructed the hello .drv AND registered it in the store via the daemon's addTextToStore (no guile (derivation …)); the daemon returned td's own computed path, wrote a novel object byte-for-byte, and the loop built td's registered .drv to a working hello."

# td-drv-assemble (DESIGN §7.1; the §5 move-off-Guile arc). Removes the LAST guile
# `(derivation …)` from the build path. Guile RESOLVES the inputs (toolchain + source
# → store paths — input resolution, stays Guix's, retired last) and emits a raw SPEC
# (system/td-build.scm `write-td-build-spec`: name/system/builder/arg/input-drv/env,
# NO output paths, NO `(derivation …)`); td-builder `drv-assemble` does the ASSEMBLY
# `(derivation …)` used to do — add the `out` output + env var, SORT env by key and
# inputs by path (the daemon's canonical order), compute the output path (#22's
# construct_drv), serialize — and REGISTERS it via the daemon (#27). The differential:
# td's assembled+registered `.drv` is byte-identical to the SAME recipe lowered through
# guix's `(derivation …)` (the oracle) — equal store path proves equal bytes (the
# daemon content-addresses td's bytes). Then `guix build` builds td's `.drv` to a
# working hello. So nothing guile constructs the build derivation anymore; guile only
# resolves which inputs it has (§5). Heavy (a td-builder compile + a warm hello
# realise). Scratch removed on green.
td-drv-assemble:
	@echo ">> td-drv-assemble: td ASSEMBLES the build .drv from a guile-resolved spec (no (derivation …)) and registers it — byte-identical to guix's (derivation …)"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-drv-assemble-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	oracle=`TD_SPEC_OUT="$$scratch/hello.spec" $(GUIX) repl $(LOAD) tests/td-drv-assemble-drv.scm 2>/dev/null | sed -n 's/^ORACLE=//p'`; \
	test -n "$$oracle" -a -s "$$scratch/hello.spec" \
	  || { echo "ERROR: could not emit the spec / lower the oracle derivation" >&2; exit 1; }; \
	echo ">> guile emitted the SPEC without (derivation …): $$(wc -l < "$$scratch/hello.spec") lines (input resolution only)"; \
	echo ">> oracle (guix (derivation …)): $$oracle"; \
	echo ">> td ASSEMBLES the .drv from the spec (ordering + output paths in Rust) and registers it via the daemon:"; \
	added=`"$$tb" drv-assemble "$$scratch/hello.spec"` \
	  || { echo "FAIL: td-builder drv-assemble failed" >&2; exit 1; }; \
	test "$$added" = "$$oracle" \
	  || { echo "FAIL: the td-assembled .drv $$added != guix's (derivation …) $$oracle — td's assembly/ordering diverged" >&2; exit 1; }; \
	echo "   td registered at guix's exact path: $$added (byte-identical — the daemon content-addresses td's bytes)"; \
	echo ">> the loop builds td's assembled+registered .drv:"; \
	out=`$(GUIX) build "$$added"`; \
	test -n "$$out" -a -x "$$out/bin/hello" \
	  || { echo "FAIL: guix build of the td-assembled .drv produced no bin/hello" >&2; exit 1; }; \
	say=`"$$out/bin/hello"`; \
	test "$$say" = "Hello, world!" \
	  || { echo "FAIL: the built hello printed '$$say', expected 'Hello, world!'" >&2; exit 1; }; \
	rm -rf "$$scratch"; \
	echo "PASS: td ASSEMBLED the build .drv from a guile-resolved spec (no (derivation …)) — byte-identical to guix's (derivation …), at the same store path — registered it via the daemon, and the loop built it to a working hello. Nothing guile constructs the build derivation; input resolution stays Guix's (§5)."

# td-check (DESIGN §7.1; gate-2 of the move-off-Guile arc — td OWNS the reproducibility
# oracle). Prime directive 1 says *reproducibility is a test*; today that verdict is
# `guix build --check` (the daemon builds twice and compares). This rung has td compute
# that verdict ITSELF: `td-builder check` executes the `td-build` hello `.drv` TWICE in
# two INDEPENDENT user-namespace sandbox runs (reusing the #25 executor) and compares
# the per-output NAR hashes (reusing the #21/S2 NAR serializer + SHA-256) — equal ⇒
# reproducible, with no daemon and no `guix build --check` in td's verdict. The rung
# then proves that verdict matches guix's: td's reproducible NAR hash equals the
# daemon's RECORDED hash, AND the differential oracle `guix build --check` agrees the
# SAME `.drv` is reproducible (prime directive 4 — proven equal before any later
# replacement; nothing existing is loosened, directive 3). Honest scope: input
# resolution + the closure (`guix gc -R`) + the daemon building the INPUTS stay Guix's;
# only the TOP derivation's reproducibility is td's double-build (toolchain retired
# last, §5). Heavy (a td-builder compile + a daemon hello build for the oracle + TWO td
# hello builds + a --check), so it slots in the heavy pool by the other td rungs.
# Scratch on disk (two staged build trees), kept on red for triage, removed on green.
td-check:
	@echo ">> td-check: td computes the reproducibility verdict ITSELF — builds the td-build hello .drv TWICE (independent userns sandboxes), NAR-equal, matching guix build --check"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-check-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	$(GUIX) repl $(LOAD) tests/td-drv-build-drv.scm 2>/dev/null > "$$scratch/facts.txt"; \
	drv=`sed -n 's/^HELLO_DRV=//p' "$$scratch/facts.txt"`; \
	out=`sed -n 's/^HELLO_OUT=//p' "$$scratch/facts.txt"`; \
	hash=`sed -n 's/^HELLO_HASH=//p' "$$scratch/facts.txt"`; \
	test -n "$$drv" -a -n "$$out" -a -n "$$hash" \
	  || { echo "ERROR: could not lower/daemon-build the hello drv oracle facts" >&2; exit 1; }; \
	echo ">> subject .drv: $$drv"; \
	echo ">> oracle (daemon-recorded): $$out  nar-hash sha256:$$hash"; \
	echo ">> stage the input closure"; \
	{ sed -n 's/^HELLO_INPUT=//p' "$$scratch/facts.txt"; echo "$$drv"; } | xargs $(GUIX) gc -R | sort -u > "$$scratch/paths.txt"; \
	echo "   staged closure: $$(wc -l < "$$scratch/paths.txt") store items"; \
	echo ">> td's OWN reproducibility verdict: build the .drv TWICE (two independent userns sandboxes) and compare per-output NAR hashes"; \
	"$$tb" check "$$drv" "$$scratch/paths.txt" "$$scratch/c" > "$$scratch/checkout.txt" 2>"$$scratch/check.err" \
	  || { echo "FAIL: td-builder check reported NON-reproducible (or errored):" >&2; cat "$$scratch/checkout.txt" "$$scratch/check.err" >&2; exit 1; }; \
	grep -qx "CHECK out $$out sha256:$$hash reproducible" "$$scratch/checkout.txt" \
	  || { echo "FAIL: td-builder check did not report 'out $$out sha256:$$hash reproducible' — its two builds disagree or differ from the daemon's recorded hash:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	echo "   td double-build agrees: $$out is reproducible, sha256:$$hash (== the daemon's recorded hash)"; \
	echo ">> differential ORACLE: guix build --check agrees the SAME .drv is reproducible"; \
	$(GUIX) build --check "$$drv" >/dev/null 2>&1 \
	  || { echo "FAIL: guix build --check disagreed — the oracle says the .drv is NON-reproducible" >&2; exit 1; }; \
	echo "   guix build --check agrees: reproducible"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo "PASS: td-builder computed the reproducibility verdict ITSELF — built the td-build hello .drv twice in independent userns sandboxes to a byte-identical NAR (== the daemon's recorded hash) — matching guix build --check on the same .drv; no daemon and no guix build --check in td's verdict, which are only the oracle (input resolution + the daemon building the inputs stay Guix's, §5)."

# loop-sandbox (DESIGN §7.1; gate-2 "Loop tooling convergence"). Toward replacing
# `guix shell -C` with td's OWN sandbox: `td-builder host-sandbox` is a DEV-SHELL (vs.
# the build jail) — it pivots into a fresh root exposing ONLY the WHOLE /gnu/store
# (read-only), the daemon socket /var/guix, /proc and /dev, with host-guix on PATH and
# the host filesystem otherwise GONE. This rung is the gate-2 OBSERVE step done
# additively (it does NOT touch check.sh's real `guix shell -C` entry — directive 3):
# (1) EXPOSURE EQUIVALENCE — plain `guix build -d hello` lowers to the SAME .drv path
# inside td's host-sandbox as it does directly under check.sh's `guix shell -C` (guix's
# container is the oracle, directive 4); equal path proves td's sandbox exposes the
# store + daemon socket + guix the same way. (2) ISOLATION — the host worktree
# ($(CURDIR)/Makefile, visible in `guix shell -C`'s shared cwd) is INVISIBLE inside
# td's sandbox, while /gnu/store + the socket remain exposed — proving it is a real
# container, not a bare userns. (3) NET-NAMESPACE PARITY — td's sandbox enters its OWN
# network namespace (its /proc/self/ns/net inode differs from the rung's), loopback-only
# (no host interface) with `lo` brought up, matching `guix shell -C`'s offline posture;
# the daemon stays reachable across it (the Unix socket on the bound /var/guix), proven
# by the exposure equivalence holding. Scope (honest, deferred follow-up like the build
# jail deferred NEWPID/chroot to S4): the wholesale check.sh swap is the remaining LATER
# increment; this rung still runs INSIDE check.sh's offline outer container. Heavy (a
# td-builder compile + a few nested-sandbox guix/bash probes), in the heavy pool.
loop-sandbox:
	@echo ">> loop-sandbox: td's OWN sandbox hosts a loop step (guix build -d) byte-identical to guix shell -C, and isolates the host filesystem"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	echo ">> exposure equivalence: plain 'guix build -d hello' under guix shell -C vs inside td's host-sandbox"; \
	oracle=`guix build -d hello`; \
	test -n "$$oracle" || { echo "ERROR: oracle 'guix build -d hello' produced nothing" >&2; exit 1; }; \
	tdout=`"$$tb" host-sandbox -- guix build -d hello`; \
	echo "   guix shell -C  : $$oracle"; \
	echo "   td host-sandbox: $$tdout"; \
	test "$$tdout" = "$$oracle" \
	  || { echo "FAIL: td's sandbox lowered a DIFFERENT .drv than guix shell -C ($$tdout vs $$oracle) — exposure diverged" >&2; exit 1; }; \
	echo ">> isolation: the host worktree is invisible inside td's sandbox, while the store + socket stay exposed"; \
	realbash=`readlink -f "$$(command -v bash)"`; \
	if "$$tb" host-sandbox -- "$$realbash" -c "test -e '$(CURDIR)/Makefile'"; then \
	  echo "FAIL: the host worktree ($(CURDIR)) leaked into td's sandbox — not isolated" >&2; exit 1; fi; \
	"$$tb" host-sandbox -- "$$realbash" -c "test -d /gnu/store && test -S /var/guix/daemon-socket/socket" \
	  || { echo "FAIL: td's sandbox did not expose /gnu/store + the daemon socket" >&2; exit 1; }; \
	echo "   worktree gone; /gnu/store + daemon socket exposed"; \
	echo ">> net-namespace parity: td's sandbox runs in its OWN netns (like guix shell -C), loopback-only, daemon reachable across it"; \
	realreadlink=`readlink -f "$$(command -v readlink)"`; \
	parent_ns=`readlink /proc/self/ns/net`; \
	td_ns=`"$$tb" host-sandbox -- "$$realbash" -c 'exec "$$0" /proc/self/ns/net' "$$realreadlink"`; \
	echo "   guix shell -C netns: $$parent_ns"; \
	echo "   td host-sandbox netns: $$td_ns"; \
	case "$$td_ns" in net:\[*\]) : ;; *) echo "FAIL: td's sandbox netns '$$td_ns' is not a net namespace link" >&2; exit 1;; esac; \
	test "$$td_ns" != "$$parent_ns" \
	  || { echo "FAIL: td's sandbox did not enter its OWN netns (same as guix shell -C's $$parent_ns) — no net isolation" >&2; exit 1; }; \
	"$$tb" host-sandbox -- "$$realbash" -c 'ifaces=""; while IFS= read -r l; do case "$$l" in *:*) n="$${l%%:*}"; ifaces="$$ifaces $${n// /}";; esac; done < /proc/net/dev; test "$$ifaces" = " lo"' \
	  || { echo "FAIL: td's sandbox netns is not loopback-only (a non-lo interface is present)" >&2; exit 1; }; \
	echo "   td entered its own loopback-only netns; the daemon stayed reachable (the equivalence above held across it)"; \
	echo "PASS: td's OWN sandbox (td-builder host-sandbox) hosted 'guix build -d hello' to the SAME .drv as check.sh's guix shell -C (store + daemon socket + guix exposed identically) while ISOLATING the host filesystem (worktree gone) AND running in its OWN loopback-only network namespace (net parity with guix shell -C; the daemon stays reachable over the Unix socket); the gate-2 OBSERVE step — check.sh's entry is unchanged, the wholesale swap is the remaining follow-up."

# loop-rung (DESIGN §7.1; gate-2 "Loop tooling convergence", Step 1 — the full-rung
# differential). loop-sandbox (#30/#31) proved td's host-sandbox hosts a single guix
# operation; this proves it hosts a REAL loop rung. `td-builder host-sandbox
# --expose-cwd` adds guix shell -C's FULL loop env (the worktree/cwd bound like its
# shared cwd, the cgroup hierarchy + the guix cache, the caller's PATH — the toolchain,
# all /gnu/store — and TD_CHECK_*/USER preserved, chdir into the cwd). The differential:
# the `eval` rung's exact command (`$(GUIX) repl $(LOAD) tests/eval.scm` — loads every
# system/test module + prints "eval ok") produces BYTE-IDENTICAL combined output inside
# td's full-env sandbox as it does directly under check.sh's `guix shell -C` (the
# oracle). Proves a real rung runs identically in td's sandbox — the differential the
# wholesale check.sh swap (Step 2, deferred) needs. ADDITIVE: check.sh is UNCHANGED.
# Heavy (a td-builder compile + two guix repl evals), so it slots in the heavy pool by
# the other loop rungs.
loop-rung:
	@echo ">> loop-rung: a REAL rung (eval) runs BYTE-IDENTICALLY inside td's full-env sandbox (--expose-cwd) as under guix shell -C"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	user=`id -un 2>/dev/null || echo nobody`; \
	echo ">> oracle: the eval rung's command directly under guix shell -C"; \
	oracle=`$(GUIX) repl $(LOAD) tests/eval.scm 2>&1`; \
	echo "$$oracle" | sed 's/^/   oracle| /'; \
	echo ">> td: the SAME command inside td's host-sandbox --expose-cwd (worktree + toolchain + cache exposed, chdir'd in)"; \
	td=`USER="$$user" "$$tb" host-sandbox --expose-cwd -- $(GUIX) repl $(LOAD) tests/eval.scm 2>&1`; \
	echo "$$td" | sed 's/^/   td    | /'; \
	test "$$td" = "$$oracle" \
	  || { echo "FAIL: the eval rung produced DIFFERENT output inside td's sandbox than under guix shell -C — the full-env exposure diverged" >&2; exit 1; }; \
	case "$$td" in *"eval ok"*) : ;; *) echo "FAIL: the eval rung did not print 'eval ok' inside td's sandbox (output above) — it did not actually run" >&2; exit 1;; esac; \
	echo "PASS: a REAL loop rung (eval — loads every system/test module + prints 'eval ok') ran BYTE-IDENTICALLY inside td's OWN full-env sandbox (td-builder host-sandbox --expose-cwd: worktree + toolchain + cache + cgroups exposed) as directly under check.sh's guix shell -C; the Step-1 full-rung differential for the loop-tooling swap — check.sh's entry is still unchanged (Step 2 deferred)."

# ts-frontend Phase 1 (DESIGN §7.1, sub-task 1) — the TypeScript spec front-end.
# `tsc` (the pinned td-typescript input, run under the packaged node) BOTH
# type-checks a td system spec and emits its type-stripped JS. Self-discriminating
# like the `diff`/`oci-diff` rungs (tests/ts-check.sh): the well-typed v0 spec
# checks clean AND emits a byte-identical golden, while an out-of-union
# rootFsType ("ext3") is REJECTED with a type error (TS2322) — the always-on
# negative control proving the types are load-bearing. No image/VM: it builds two
# warm packages and runs tsc on tiny files (seconds), so it slots late in the
# heavy LPT order. The pinned channel's swc CLI is a non-functional stub and tsc
# is unpackaged, so tsc does both jobs (human 2026-06-13; plan/ts-frontend.md).
ts:
	@echo ">> ts: TypeScript spec front-end — tsc type-checks + emits the v0 spec (ts-frontend Phase 1)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	test -n "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / td-typescript" >&2; exit 1; }; \
	TD_NODE="$$node" TD_TSC="$$tsc" TD_TSDIR="$(CURDIR)/tests/ts" \
	  sh tests/ts-check.sh

# ts-frontend Phase 1 (DESIGN §7.1, sub-task 2) — the boa evaluator + curated
# global. Builds td-ts-eval (pure-Rust boa, crates from the hash-pinned
# %ts-eval-vendor fixed-output, compiled offline) and `--check`s it reproducible
# (prime directive 1 — it IS a new built artifact, like td-builder), then asserts
# the hermetic eval via tests/ts-eval-check.sh: a trivial expression evaluates to
# a known value, `typeof Date === "undefined"` (clock removed), and
# `Math.random()` is DENIED (the always-on negative control), while Math is
# otherwise intact. Heavy (a warm-store Rust build + a --check), so it slots late
# in the LPT order alongside td-builder.
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
ts-diff:
	@echo ">> ts-diff: TS v0 spec lowers (tsc->boa->config) to the oracle's system drv; a perturbed spec diverges (ts-frontend acceptance #1/#2)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	dj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/spec-v0.ts"`; \
	pj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/spec-perturbed.ts"`; \
	test -n "$$dj" -a -n "$$pj" || { echo "ERROR: ts-emit produced no config JSON" >&2; exit 1; }; \
	echo ">> v0 config        : $$dj"; \
	echo ">> perturbed config : $$pj"; \
	TD_TS_DEFAULT_JSON="$$dj" TD_TS_PERTURBED_JSON="$$pj" $(GUIX) repl $(LOAD) tests/ts-diff.scm

# 2. Reproducibility oracle — build the image, then rebuild its derivation with
#    --check (bit-for-bit identical or it is a FAILING test).
build:
	@echo ">> build: $(SYSTEM) image ($(IMGTYPE))"
	$(GUIX) system image $(LOAD) -t $(IMGTYPE) $(SYSTEM)
	@echo ">> check: reproducibility of the image derivation (verdict-memoized — tests/check-memo.sh)"
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh \
	  $$($(GUIX) system image $(LOAD) -t $(IMGTYPE) -d $(SYSTEM))

# 3. Boot + behavioral — realise the marionette test derivation. Its builder
#    runs the SRFI-64 assertions in/against a booted VM and exits non-zero if any
#    fail, so a failed assertion makes this rung go red (see the two-step note in
#    the recipe for why we must NOT pipe the build into `guix repl`).
test:
	@echo ">> test: boot marionette + assert behaviors"
	$(call realise-system-test,(tests boot),%test-td-boot,boot)

# 3b. Disk-image boot (triage #2) — boot the qcow2 through its GRUB bootloader
#     (not the direct-kernel VM the `test` rung uses), so the bootloader,
#     partition table and disk image are actually exercised. Same honest two-step
#     lower-then-realise as `test`. Heavier (builds a second full image + boots
#     it), so it runs after the cheap rungs.
boot-disk:
	@echo ">> boot-disk: boot the qcow2 disk through GRUB + assert kernel"
	$(call realise-system-test,(tests boot),%test-td-disk-boot,disk-boot)

# 3c. Ephemerality of the CoW reset (loop-latency; DESIGN §1.5). Boots the SAME
#     instrumented qcow2 derivation as boot-disk (cache hit, no extra image
#     build) three times on explicit qcow2 overlays: dirt written on overlay A,
#     dirt STILL THERE on reused overlay A (negative control — writes really
#     persist without a reset), dirt GONE on fresh overlay B (the reset). Makes
#     the loop's fresh-state-per-test guarantee an assertion instead of an
#     implicit property of qemu flags, so any future cycle-time change that
#     leaks guest state across boots goes red here. Same honest two-step
#     lower-then-realise as `test`/`boot-disk`.
reset:
	@echo ">> reset: CoW overlay reset discards dirtied guest state (ephemerality)"
	$(call realise-system-test,(tests reset),%test-td-reset,reset)

# 4. OCI reproducibility oracle (M5) — same shape as `build`, but for the
#    Docker/OCI image: build it, then rebuild its derivation with --check
#    (bit-for-bit identical or it is a FAILING test, prime directive 1). The
#    OS closure is shared with `build`, so --check mostly re-runs the cheap
#    docker-packing step. The matching declaration also boots as a VM (M1–M4),
#    closing the north-star "one declaration, store-based + OCI" loop (DESIGN §0).
oci:
	@echo ">> oci: $(SYSTEM) image (docker)"
	$(GUIX) system image $(LOAD) -t docker $(SYSTEM)
	@echo ">> check: reproducibility of the OCI image derivation (verdict-memoized — tests/check-memo.sh)"
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh \
	  $$($(GUIX) system image $(LOAD) -t docker -d $(SYSTEM))

# 5. M6 manifest-swap reproducibility — build a SWAPPED-manifest OCI image
#    generation (default manifest + GNU hello) and `--check` it bit-for-bit.
#    `manifest-diff` proves a changed manifest is a DIFFERENT image; this proves
#    that swapped generation is itself reproducible (DESIGN §6 image-swap-only;
#    prime directive 1 — a non-reproducible swapped image is a FAILING test).
#    The less-frequent/heavier rung (§1.3): it repacks a second docker tarball,
#    but hello's closure is tiny and the OS closure is shared with `oci`, so it
#    stays in budget. Two-step (lower to a drv via repl, then realise+check via
#    `guix build`) for the same honest-exit-status reason as the `test` rung.
#    It ALSO inspects the realized tarball (triage #5): manifest-diff only proves
#    the package is in the declaration (operating-system-packages) — an exporter
#    bug could change the image derivation yet omit the files. So here we crack
#    open the built layer.tar and assert hello/bin/hello is actually present in
#    the SWAPPED image and ABSENT from the default image — artifact contents, not
#    just the declaration.
manifest-check:
	@echo ">> manifest-check: build a SWAPPED-manifest OCI image and --check it"
	@set -euo pipefail; \
	drv=`$(GUIX) repl $(LOAD) tests/manifest-image-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the swapped OCI image derivation" >&2; exit 1; }; \
	echo ">> swapped OCI image derivation: $$drv"; \
	swapped_img=`$(GUIX) build "$$drv"`; \
	echo ">> check: reproducibility of the SWAPPED OCI image derivation (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$drv"; \
	echo ">> artifact check: the declared package is actually IN the built tarball"; \
	default_img=`$(GUIX) system image $(LOAD) -t docker $(SYSTEM)`; \
	probe() { \
	  listing=`tar xzOf "$$1" --wildcards '*/layer.tar' | tar tf -` \
	    || { echo "FAIL: could not read OCI archive $$1 (artifact missing or corrupt)" >&2; exit 1; }; \
	  printf '%s\n' "$$listing" | grep -c 'hello-2.12.2/bin/hello' || true; \
	}; \
	in_swapped=`probe "$$swapped_img"`; \
	in_default=`probe "$$default_img"`; \
	echo "   hello/bin/hello entries — swapped image: $$in_swapped   default image: $$in_default"; \
	test "$$in_swapped" -ge 1 || { echo "FAIL: the declared package is NOT in the built swapped tarball — the manifest reached the derivation but the exporter dropped it." >&2; exit 1; }; \
	test "$$in_default" -eq 0 || { echo "FAIL: the default image's tarball unexpectedly contains the swap package." >&2; exit 1; }; \
	echo "PASS: the declared package is present in the realized swapped image (not just the declaration) and absent from the default image."

# M10.1 bootc generation image (M10-design.md "What a generation bundle is").
# td's OCI lowering emits userspace ONLY; this builds the bootc-style image that
# makes it bootable by APPENDING a /boot layer (kernel + initrd) — see
# (system td-generation). Heavier rung (builds a system docker image + repacks),
# so it runs with the other image rungs. Validator scratch lives in
# $(CURDIR)/.genimg-scratch — disk, not the sandbox /tmp (the oci-load lesson:
# that tmpfs is a small RAM fraction and the extraction is multi-GB; it
# ENOSPC'd on a 16G-RAM CI host); kept on red for triage, removed on green.
# Self-discriminating at the artifact
# level (like manifest-check/no-guix), and it --checks reproducibility (prime
# directive 1 — this IS the new artifact):
#   • build the gen-1 + gen-2 bootc images, and `--check` BOTH bit-for-bit;
#   • crack each image's layers and assert /boot/bzImage AND /boot/initrd.cpio.gz
#     are PRESENT in the bootc image and ABSENT from the plain userspace image
#     (DRV_BASE) — the discriminator for the "made bootable" claim;
#   • assert gen-1 and gen-2 lower to DIFFERENT image derivations — each carries
#     its own generation's initrd (which mounts that generation's distinct root),
#     so the bundle is genuinely per-generation, not a shared artifact.
generation-image:
	@echo ">> generation-image: build a bootc-style generation image, --check it, crack /boot (M10.1)"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/generation-image-drv.scm 2>/dev/null`; \
	gen1=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_GEN1=//p'`; \
	gen2=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_GEN2=//p'`; \
	base=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_BASE=//p'`; \
	nogen=`printf '%s\n' "$$drvs" | sed -n 's/^REJECTS_NO_GEN=//p'`; \
	test -n "$$gen1" -a -n "$$gen2" -a -n "$$base" || { echo "ERROR: could not lower the generation image derivations" >&2; exit 1; }; \
	echo ">> gen1 image drv: $$gen1"; \
	echo ">> gen2 image drv: $$gen2"; \
	echo ">> base userspace drv: $$base"; \
	echo ">> P1: td-generation-image rejects a config with NO generation id"; \
	test "$$nogen" = "yes" || { echo "FAIL: td-generation-image ACCEPTED a config without a generation id — it would mount the shared td-root, not a per-generation root." >&2; exit 1; }; \
	gen1_img=`$(GUIX) build "$$gen1"`; \
	gen2_img=`$(GUIX) build "$$gen2"`; \
	base_img=`$(GUIX) build "$$base"`; \
	echo ">> check: reproducibility of BOTH bootc generation images (verdict-memoized — tests/check-memo.sh; a miss runs the real --check unchanged)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$gen1" "$$gen2"; \
	echo ">> validate artifacts (structured: guile-json metadata + guile-zlib initrd)"; \
	scratch="$(CURDIR)/.genimg-scratch"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	TD_GEN1_IMG="$$gen1_img" TD_GEN2_IMG="$$gen2_img" TD_BASE_IMG="$$base_img" \
	  TMPDIR="$$scratch" $(GUIX) repl $(LOAD) tests/generation-image-check.scm; \
	rm -rf "$$scratch"

# oci-load (side-track, deferred from M10.1; plan/oci-load.md). The shipped
# images must be consumable by an INDEPENDENT OCI implementation, not just our
# own placer (`place`) and runtime (`run`). Vehicle: skopeo, chosen by the M8
# probe discipline — 0 drvs to build on the warm store vs umoci 113 and podman
# 1238 + 290 cold fetches (rejected at M8); resolved via `$(GUIX) build` so
# check.sh's package list is untouched. For BOTH the plain td image and the
# gen-1 bootc generation image (drvs shared with `oci`/`generation-image`, so
# the marginal cost is the skopeo pass, not a rebuild):
#   • `skopeo copy docker-archive:… oci:…` — the foreign stack parses the
#     archive and verifies every blob digest while writing the CANONICAL OCI
#     LAYOUT, the §2.7 identity carrier;
#   • assert `skopeo inspect` yields a `sha256:` manifest digest from that
#     layout (the registry-addressable identity M12 signs).
# NEGATIVE CONTROL, in-rung: the gen-1 archive with ONE byte incremented inside
# the inner layer.tar must be REJECTED with a digest mismatch — proves the
# green leg is a real integrity check, not mere unpacking. The corruptor
# increments (mod 256) the byte at the midpoint, so the write can never be a
# no-op, and the midpoint of the outer tar lies inside the dominant layer.tar
# blob. `--insecure-policy` disables only signature *trust policy* (M12's
# territory, no keys exist yet); blob-digest integrity stays enforced — which
# is exactly what the control proves. Scratch lives in
# $(CURDIR)/.oci-load-scratch (disk, not the sandbox tmpfs — the rootless
# lesson: layouts + the decompressed archive are several GB); kept on red for
# triage, removed on green.
oci-load:
	@echo ">> oci-load: foreign OCI implementation (skopeo) loads the shipped images"
	@set -euo pipefail; \
	skopeo=`$(GUIX) build skopeo`/bin/skopeo; \
	plain_img=`$(GUIX) system image $(LOAD) -t docker $(SYSTEM)`; \
	gen1=`$(GUIX) repl $(LOAD) tests/generation-image-drv.scm 2>/dev/null | sed -n 's/^DRV_GEN1=//p'`; \
	test -n "$$gen1" || { echo "ERROR: could not lower the gen-1 bootc image derivation" >&2; exit 1; }; \
	gen1_img=`$(GUIX) build "$$gen1"`; \
	work="$(CURDIR)/.oci-load-scratch"; rm -rf "$$work"; mkdir -p "$$work"; \
	for leg in plain:$$plain_img gen1:$$gen1_img; do \
	  name=$${leg%%:*}; img=$${leg#*:}; \
	  echo ">> skopeo copy docker-archive -> oci layout ($$name): $$img"; \
	  "$$skopeo" --tmpdir "$$work" copy --insecure-policy "docker-archive:$$img" "oci:$$work/layout-$$name:td" >/dev/null; \
	  digest=`"$$skopeo" --tmpdir "$$work" inspect --format '{{.Digest}}' "oci:$$work/layout-$$name:td"`; \
	  case "$$digest" in \
	    sha256:*) echo "   manifest digest ($$name): $$digest";; \
	    *) echo "FAIL: no manifest digest from the $$name OCI layout (got: '$$digest')" >&2; exit 1;; \
	  esac; \
	done; \
	echo ">> negative control: a corrupted layer must be REJECTED"; \
	gunzip -c "$$gen1_img" > "$$work/bad.tar"; \
	off=$$(( `stat -c %s "$$work/bad.tar"` / 2 )); \
	b=`od -An -tu1 -j $$off -N1 "$$work/bad.tar" | tr -d ' '`; \
	printf "\\$$(printf '%03o' $$(( (b + 1) % 256 )))" \
	  | dd of="$$work/bad.tar" bs=1 seek=$$off count=1 conv=notrunc status=none; \
	gzip -1 "$$work/bad.tar"; \
	if "$$skopeo" --tmpdir "$$work" copy --insecure-policy "docker-archive:$$work/bad.tar.gz" \
	     "oci:$$work/layout-bad:bad" >/dev/null 2>"$$work/bad.err"; then \
	  echo "FAIL: skopeo ACCEPTED a deliberately corrupted image — the load is not an integrity check." >&2; \
	  cat "$$work/bad.err" >&2; \
	  exit 1; \
	fi; \
	grep -qi 'digest did not match' "$$work/bad.err" \
	  || { echo "FAIL: corrupted image was rejected, but NOT with a digest mismatch:" >&2; \
	       cat "$$work/bad.err" >&2; exit 1; }; \
	rm -rf "$$work"; \
	echo "PASS: foreign load green for plain + gen-1 images; corrupted layer rejected (digest mismatch)."

# M12 S3 signed distribution: the static registry (DESIGN §2.7). The registry
# is a derivation (system/td-registry.scm): both generation images pushed by
# skopeo into ONE canonical OCI layout (shared content-addressed blob store)
# plus, per image, a one-line manifest-digest STATEMENT and its detached
# signify (ed25519) signature by the committed TEST key (tests/keys/README) —
# sign the digest, never the install ordinal; no sigstore. The rung builds it,
# `--check`s it (deterministic skopeo conversion + RFC 8032 deterministic
# signatures), and runs tests/registry-check.sh: per generation the statement
# equals the manifest digest skopeo (the foreign implementation) re-derives,
# the signature verifies with the td test pubkey, and pull-by-digest works
# from the BYTES alone (manifest + every referenced blob re-hashes to its
# digest — content addressing IS the byte-identity between pushed and pulled);
# the whole blob store re-hashes honestly; and three negative controls run
# every loop on scratch copies: unsigned (sigs stripped), tampered (one layer
# byte flipped), forged (statement rewritten, signature kept) — each must be
# rejected for its own reason. The verifier exercised here (verify_pull) is
# the same contract the placer enforces before placing (M12 S4).
registry:
	@echo ">> registry: signed static OCI-layout distribution — push, verify statements/signatures/pull-by-digest (M12 S3)"
	@set -euo pipefail; \
	drv=`$(GUIX) repl $(LOAD) tests/registry-drv.scm 2>/dev/null | sed -n 's/^DRV_REGISTRY=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the registry derivation" >&2; exit 1; }; \
	echo ">> registry derivation: $$drv"; \
	reg=`$(GUIX) build "$$drv"`; \
	echo ">> check: reproducibility of the registry (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$drv"; \
	skopeo=`$(GUIX) build skopeo`/bin/skopeo; \
	signify=`$(GUIX) build signify`/bin/signify; \
	TD_REGISTRY="$$reg" SKOPEO="$$skopeo" SIGNIFY="$$signify" \
	TD_PUBKEY=tests/keys/td_m12_signify.pub TD_GENS="1 2" \
	  sh tests/registry-check.sh

# M12 S4 verify-then-place (the §7.1 acceptance). The placer's VERIFIED input
# mode (--registry/--digest/--pubkey) enforces the §2.7 pull contract BEFORE
# any staging — signed statement for the demanded digest, signify signature,
# statement states that digest, manifest blob re-hashes to it, every
# referenced blob re-hashes — then hands the decompressed layers to the
# existing placement path unchanged; the placed image-digest= records the
# VERIFIED manifest digest (the §2.7 representation move; legacy --image
# keeps the artifact sha256). Two-phase: build the registry, obtain each
# generation's manifest digest from skopeo (the FOREIGN oracle — the placer
# is told what to demand independently of the registry's own files), then
# build + `--check` the verified placed tree and validate it with
# tests/place-check.scm using digest-form TD_IMAGES. tests/verify-place-check.sh
# adds the S4 differential (verified tree == direct-placement oracle tree
# except the image-digest representation) and four negative controls every
# loop: unsigned / forged statement / tampered blob refused by the placer
# (each for its own §2.7 reason, placing nothing), and a crafted legacy image
# whose embedded identity states its own digest rejected by the
# self-reference guard.
verify-place:
	@echo ">> verify-place: placer verifies signature+digest before placing; rejects unsigned/tampered (M12 S4)"
	@set -euo pipefail; \
	reg_drv=`$(GUIX) repl $(LOAD) tests/registry-drv.scm 2>/dev/null | sed -n 's/^DRV_REGISTRY=//p'`; \
	test -n "$$reg_drv" || { echo "ERROR: could not lower the registry derivation" >&2; exit 1; }; \
	reg=`$(GUIX) build "$$reg_drv"`; \
	skopeo=`$(GUIX) build skopeo`/bin/skopeo; \
	signify_dir=`$(GUIX) build signify`/bin; \
	scratch="$(CURDIR)/.verify-place-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	d1=`"$$skopeo" --tmpdir "$$scratch" inspect --format '{{.Digest}}' "oci:$$reg/oci:gen-1"`; \
	d2=`"$$skopeo" --tmpdir "$$scratch" inspect --format '{{.Digest}}' "oci:$$reg/oci:gen-2"`; \
	rm -rf "$$scratch"; \
	case "$$d1$$d2" in *sha256:*) : ;; *) echo "ERROR: no manifest digests from skopeo" >&2; exit 1 ;; esac; \
	drvs=`TD_DIGEST_1="$$d1" TD_DIGEST_2="$$d2" $(GUIX) repl $(LOAD) tests/verify-place-drv.scm 2>/dev/null`; \
	vplace_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_VPLACE=//p'`; \
	direct_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_DIRECT=//p'`; \
	img1=`printf '%s\n' "$$drvs" | sed -n 's/^IMG_1=//p'`; \
	label1=`printf '%s\n' "$$drvs" | sed -n 's/^LABEL_1=//p'`; \
	test -n "$$vplace_drv" -a -n "$$direct_drv" -a -n "$$img1" -a -n "$$label1" \
	  || { echo "ERROR: could not lower the verify-place derivations" >&2; exit 1; }; \
	echo ">> verified placed tree derivation: $$vplace_drv"; \
	vplace=`$(GUIX) build "$$vplace_drv"`; \
	direct=`$(GUIX) build "$$direct_drv"`; \
	echo ">> check: reproducibility of the verified placed tree (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$vplace_drv"; \
	echo ">> validate the verified tree (digest-form TD_IMAGES: placed identity == verified manifest digest)"; \
	TD_PLACED="$$vplace" TD_PRESENT="1 2" TD_ABSENT="" \
	TD_IMAGES="1=$$d1 2=$$d2" \
	  $(GUIX) repl $(LOAD) tests/place-check.scm; \
	echo ">> differential + rejection legs"; \
	TD_REGISTRY="$$reg" TD_PLACER=system/td-place.sh \
	TD_PUBKEY=tests/keys/td_m12_signify.pub SIGNIFY_BIN="$$signify_dir" \
	TD_DIGEST_1="$$d1" TD_GEN1_IMG="$$img1" TD_GEN1_LABEL="$$label1" \
	TD_VPLACE="$$vplace" TD_DIRECT="$$direct" \
	  sh tests/verify-place-check.sh

# M10.2 guix-free placer (M10-design.md step 3, "Place"). The deployment side:
# a POSIX shell tool (system/td-place.sh) that runs ON THE TARGET — which has NO
# guix. Driven by the OCI manifest (not a blind layer scan), it: verifies the
# image's embedded identity (boot/td-identity) matches the --generation/--root-label
# it is placed as; APPLIES the userspace layers into that generation's own root,
# staged as roots/td/gen-N/root.tar (so the bare-label root=td-root-gen-N refers to a root
# that exists — M10.3 turns it into a labeled fs); extracts /boot per-generation;
# prunes to --keep (>=1); and regenerates a per-generation GRUB menu. Each
# generation is staged + validated then atomically swapped in, so a corrupt image
# never destroys the generation already installed. This rung exercises it
# hermetically (system/td-place.scm): it builds the per-generation bootc images
# with Guix (the M10.1 oracle) and runs the placer over them inside a derivation
# whose builder PATH is ONLY base tools, NO guix — so a successful build PROVES the
# placer is guix-free by construction (the same "absent → cannot be used" guarantee
# as `no-guix`), and `--check` proves the placed target tree reproducible. The
# deployment behavior is tested against the artifact (M10-design.md decision 2),
# not diffed against a Guix component it lacks: tests/place-check.scm cracks the
# tree and asserts each present generation is placed with its own kernel/initrd,
# an identity recording the artifact's sha256 (image-digest=, M12 §2.7 —
# value-checked against the real artifacts via TD_IMAGES),
# its applied root content, and a menuentry that selects its OWN root and no
# other's (per-entry, not block-wide), the user grub.cfg preamble survives, and
# (the prune scenario) the oldest generation's boot dir, root content AND menu
# entry are gone. Two scenarios: PLACE (gens 1,2 keep 10 — no prune) and PRUNE
# (gens 1,2,3 keep 2 — gen 1 dropped). Creating the labeled fs from the staged
# root.tar + the full boot+rollback is M10.3.
place:
	@echo ">> place: guix-free placer extracts /boot + writes a per-generation GRUB menu, prunes old generations (M10.2)"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/place-drv.scm 2>/dev/null`; \
	place_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_PLACE=//p'`; \
	prune_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_PRUNE=//p'`; \
	img1=`printf '%s\n' "$$drvs" | sed -n 's/^IMG_1=//p'`; \
	img2=`printf '%s\n' "$$drvs" | sed -n 's/^IMG_2=//p'`; \
	img3=`printf '%s\n' "$$drvs" | sed -n 's/^IMG_3=//p'`; \
	test -n "$$place_drv" -a -n "$$prune_drv" || { echo "ERROR: could not lower the placer tree derivations" >&2; exit 1; }; \
	test -n "$$img1" -a -n "$$img2" -a -n "$$img3" || { echo "ERROR: could not lower the generation image artifact paths" >&2; exit 1; }; \
	echo ">> place  tree derivation (gens 1,2 keep 10): $$place_drv"; \
	echo ">> prune  tree derivation (gens 1,2,3 keep 2): $$prune_drv"; \
	place_tree=`$(GUIX) build "$$place_drv"`; \
	prune_tree=`$(GUIX) build "$$prune_drv"`; \
	echo ">> check: reproducibility of BOTH placed target trees (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$place_drv" "$$prune_drv"; \
	echo ">> validate PLACE tree (gens 1,2 present, none pruned)"; \
	TD_PLACED="$$place_tree" TD_PRESENT="1 2" TD_ABSENT="" \
	TD_IMAGES="1=$$img1 2=$$img2" \
	  $(GUIX) repl $(LOAD) tests/place-check.scm; \
	echo ">> validate PRUNE tree (gens 2,3 present, gen 1 pruned)"; \
	TD_PLACED="$$prune_tree" TD_PRESENT="2 3" TD_ABSENT="1" \
	TD_IMAGES="2=$$img2 3=$$img3" \
	  $(GUIX) repl $(LOAD) tests/place-check.scm

# M10.3 manual rollback (M10-design.md step 5, "Roll back"; the DESIGN §7.1
# acceptance test). End-to-end: the guix-free placer's output — live labeled
# per-generation root filesystems (--mkfs) + the managed GRUB menu — is
# assembled into a real MBR/GRUB disk (system/td-disk.scm), and the marionette
# test (tests/rollback.scm) boots ONE persistent qcow2 overlay of it TWICE:
# generation 2 (the GRUB default) is asserted three independent ways (cmdline
# bare-label root=, mounted-root-IS-the-labeled-filesystem, /run/current-system ==
# gen-2's system path — the placer's gnu.system wiring), the manual rollback
# act writes `set default=td-gen-1` into the boot partition's td/default.cfg
# (the hook the managed block sources) plus a persistence sentinel, the guest
# reboots cleanly, and generation 1 is asserted the same three ways — with the
# sentinel, the selection, gen-2's placed files and BOTH menu entries proven to
# have survived the reboot (persistent placed state; rolling back never
# destroys the newer generation). Before booting: `--check` both new artifacts
# (the mkfs tree and the assembled disk — prime directive 1) and validate the
# tree with tests/place-check.scm in mkfs mode (superblock label/UUID, search
# line). Two-step lower-then-realise for the marionette derivation, as in
# `test`/`boot-disk` (honest exit status).
rollback:
	@echo ">> rollback: boot gen 2, roll back to gen 1 via the GRUB menu, assert identity + persistence (M10.3)"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/rollback-drv.scm 2>/dev/null`; \
	tree_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_TREE=//p'`; \
	disk_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_DISK=//p'`; \
	test -n "$$tree_drv" -a -n "$$disk_drv" || { echo "ERROR: could not lower the rollback derivations" >&2; exit 1; }; \
	echo ">> placed tree (mkfs) derivation: $$tree_drv"; \
	echo ">> rollback disk derivation:      $$disk_drv"; \
	tree=`$(GUIX) build "$$tree_drv"`; \
	disk=`$(GUIX) build "$$disk_drv"`; \
	echo ">> check: reproducibility of the mkfs placed tree AND the assembled disk (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$tree_drv" "$$disk_drv"; \
	echo ">> validate the mkfs tree (live labeled roots via superblock, boot wiring, search line)"; \
	TD_PLACED="$$tree" TD_PRESENT="1 2" TD_ABSENT="" TD_MKFS=1 TD_BOOT_LABEL=td-boot \
	  $(GUIX) repl $(LOAD) tests/place-check.scm; \
	drv=`printf '%s\n' \
	    '(use-modules (guix) (gnu tests) (tests rollback))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (format #t "DRV=~a~%"' \
	    '          (derivation-file-name' \
	    '           (run-with-store store (system-test-value %test-td-rollback)))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the rollback test derivation" >&2; exit 1; }; \
	echo ">> realise rollback test derivation: $$drv"; \
	$(GUIX) build "$$drv"

# 6. M7 imperative-surface removal — image-swap-only BY CONSTRUCTION (DESIGN §6).
#    M6 made image CONTENTS manifest-driven but left the imperative mutation
#    surface: the built image still ships `guix`/`guix-daemon`, so an in-image
#    `guix install` is physically possible. The typed `ship-guix?` field removes
#    it. Review showed (a) a NAME/PROPAGATION static check cannot guarantee a
#    guix-free image — guix can still arrive via a runtime reference or a renamed
#    inherited package — and (b) an OPT-IN gate is bypassable (the bare public
#    lowering stays ungated). So the real guarantee is now a CLOSURE-LEVEL gate
#    EMBEDDED in the hardened system's package set (system/td-hardening.scm
#    `guix-free-marker`, added by td-config->operating-system when ship-guix? is #f):
#    EVERY lowering builds the profile and therefore the marker, so a hardened image
#    is guix-free OR it does not build, for ANY manifest, with no opt-in to skip.
#    This rung proves that on the BARE public path, self-discriminating, against
#    explicit typed-config fixtures (triage F2 — NOT the shipped `$(SYSTEM)` target,
#    so promoting the shipped default to hardened never reddens this rung):
#      • HARDENED = bare docker image of (ship-guix? #f, base+hello): must BUILD
#        (the embedded marker certifies it guix-free); `--check` it reproducible
#        (prime directive 1 — this IS the gated artifact, so its --check covers the
#        gate too); crack its layer.tar — NO `bin/guix`/`bin/guix-daemon`.
#      • CONTROL = bare docker image of (ship-guix? #t): assert its tarball DOES
#        contain those binaries — the discriminator: if the probe stopped finding
#        guix, or the toggle stopped mattering, this reddens, so a green proves the
#        probe tells guix-ful from guix-free.
#      • ADVERSARIAL = bare docker image of (ship-guix? #f, manifest with a package
#        that keeps a RUNTIME REFERENCE to guix) — it BYPASSES the constructor's
#        name/propagation pre-filter, so guix enters the closure undetected by any
#        static check. Its BARE build MUST FAIL *at the embedded marker*
#        (verified-red half): this proves the guarantee is closure-level AND holds
#        on the ordinary public lowering, not via an opt-in. We assert both that the
#        build fails AND that it fails with the marker's own diagnostic (so an
#        unrelated build error cannot green it).
#    Artifact/closure-level (binary-absent) is STRONGER than the deferred docker-run
#    "guix install fails" runtime check (§2.3): a binary not in the image cannot run.
#    Heaviest rung → runs last (§1.3); closures are warm (base/hello/guix already built).
#    Two-step lower-then-realise (repl → guix build) for honest exit status.
no-guix:
	@echo ">> no-guix: prove ship-guix? #f is a closure-level, build-enforced guix-free guarantee (embedded, no opt-in)"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/imperative-surface.scm 2>/dev/null`; \
	hardened_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_HARDENED=//p'`; \
	control_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_CONTROL=//p'`; \
	adversarial_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_ADVERSARIAL=//p'`; \
	shipped_gate_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_SHIPPED_GATE=//p'`; \
	svcinj_gate_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_SVCINJ_GATE=//p'`; \
	test -n "$$hardened_drv" -a -n "$$control_drv" -a -n "$$adversarial_drv" \
	     -a -n "$$shipped_gate_drv" -a -n "$$svcinj_gate_drv" \
	  || { echo "ERROR: could not lower the no-guix derivations" >&2; exit 1; }; \
	echo ">> hardened (bare, embedded-gate) image derivation: $$hardened_drv"; \
	echo ">> control  image derivation: $$control_drv"; \
	echo ">> adversarial (manifest) derivation: $$adversarial_drv"; \
	echo ">> shipped whole-system gate derivation: $$shipped_gate_drv"; \
	echo ">> service-injection gate derivation: $$svcinj_gate_drv"; \
	echo ">> guarantee: the BARE hardened lowering must BUILD (the embedded marker certifies it guix-free)"; \
	hardened_img=`$(GUIX) build "$$hardened_drv"`; \
	control_img=`$(GUIX) build "$$control_drv"`; \
	echo ">> check: reproducibility of the HARDENED (gated) artifact (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$hardened_drv"; \
	echo ">> artifact check: the imperative guix surface is ABSENT from the hardened image and PRESENT in the control"; \
	probe() { \
	  listing=`tar xzOf "$$1" --wildcards '*/layer.tar' | tar tf -` \
	    || { echo "FAIL: could not read OCI archive $$1 (artifact missing or corrupt)" >&2; exit 1; }; \
	  printf '%s\n' "$$listing" | grep -Ec '/bin/guix(-daemon)?$$' || true; \
	}; \
	in_hardened=`probe "$$hardened_img"`; \
	in_control=`probe "$$control_img"`; \
	echo "   guix/guix-daemon executables — hardened image: $$in_hardened   control image: $$in_control"; \
	test "$$in_control" -ge 1 || { echo "FAIL: the ship-guix? #t control image has NO guix binary — the probe is broken or the toggle stopped mattering; the test cannot discriminate." >&2; exit 1; }; \
	test "$$in_hardened" -eq 0 || { echo "FAIL: the hardened (ship-guix? #f) image STILL contains a guix/guix-daemon binary — the imperative surface was not removed." >&2; exit 1; }; \
	echo ">> adversarial: the BARE hardened lowering of a manifest that smuggles guix past the pre-filter (runtime ref) must FAIL at the embedded marker"; \
	adv_log=`mktemp`; \
	if $(GUIX) build "$$adversarial_drv" >"$$adv_log" 2>&1; then \
	  echo "FAIL: the adversarial ship-guix? #f image BUILT on the bare public path — the embedded marker did NOT trip; guix entered the closure undetected by both the static pre-filter and the gate." >&2; \
	  tail -20 "$$adv_log" >&2; rm -f "$$adv_log"; exit 1; \
	fi; \
	if ! grep -q "STILL contains a guix" "$$adv_log"; then \
	  echo "FAIL: the adversarial build failed, but NOT at the guix-free marker (unexpected error) — cannot credit the gate:" >&2; \
	  tail -20 "$$adv_log" >&2; rm -f "$$adv_log"; exit 1; \
	fi; \
	rm -f "$$adv_log"; \
	echo "   ok: the adversarial hardened image was REJECTED at the embedded marker on the bare public path (guix-in-closure detected)"; \
	echo ">> whole-system gate: the SHIPPED system must pass the closure-level gate (it is guix-free)"; \
	$(GUIX) build "$$shipped_gate_drv" >/dev/null; \
	echo "   ok: the shipped td-system passes the whole-system guix-free gate (a guix-service regression in system/td.scm would redden this)"; \
	echo ">> service-injection: restoring guix-service-type to a hardened system must FAIL the whole-system gate (guix re-enters the SYSTEM closure, invisible to the manifest marker)"; \
	svc_log=`mktemp`; \
	if $(GUIX) build "$$svcinj_gate_drv" >"$$svc_log" 2>&1; then \
	  echo "FAIL: the service-injection system gate BUILT — guix-service-type re-introduced guix into the system closure but the whole-system gate did NOT trip. The gate does not actually scan the folded system closure." >&2; \
	  tail -20 "$$svc_log" >&2; rm -f "$$svc_log"; exit 1; \
	fi; \
	if ! grep -q "system closure STILL contains" "$$svc_log"; then \
	  echo "FAIL: the service-injection gate failed, but NOT at the whole-system guix-free gate (unexpected error) — cannot credit the gate:" >&2; \
	  tail -20 "$$svc_log" >&2; rm -f "$$svc_log"; exit 1; \
	fi; \
	rm -f "$$svc_log"; \
	echo "   ok: service-injected guix was REJECTED at the whole-system gate (the hole the manifest-only marker leaves open is closed)"; \
	echo "PASS: ship-guix? #f is a closure-level, build-enforced guarantee — (1) the embedded MARKER refuses any manifest-injected guix on every bare lowering; (2) the whole-system GATE certifies the shipped td-system guix-free and REJECTS service-injected guix (guix-service-type restored) that the marker cannot see; and the control ships the surface, proving the probes discriminate."

# td-builder S1 toolchain probe + S2 NAR differential (DESIGN §7.1 side-track;
# plan/td-builder.md). The growing rung of the first Guix-component replacement
# (§2.5 discipline) — each sub-task adds a leg, none is ever removed:
#   • S1: lower the td-builder package to a drv (tests/td-builder-drv.scm),
#     build it offline, `guix build --check` it bit-for-bit (prime directive 1;
#     --check re-runs the compile, so a toolchain regression reds the loop),
#     RUN the binary and assert its sentinel (the toolchain produced a WORKING
#     executable — stronger than "cargo build exited 0"), and record closure
#     size + compile wall-clock (§1.3). The crate's unit tests (FIPS SHA-256
#     vectors, NAR framing/sort) also run inside the build (#:tests? #t).
#   • S2: NAR DIFFERENTIAL — td-builder's own NAR serializer + SHA-256
#     (`nar-hash`) must agree with the hash the DAEMON recorded in its DB
#     (query-path-info via tests/td-builder-nar.scm, printing NAR=<path> <hash>
#     pairs) for (1) a constructed fixture covering every node type and
#     framing edge (executable bit, dangling symlink, empty file/dir,
#     codepoint-order sort stress, pad-to-8 content lengths) and (2)
#     td-builder's own output. This is open question 2 settled by test: the
#     serialization the eventual builder registers outputs with is bit-for-bit
#     the daemon's. Verified-red (driven before this leg may land):
#     ordering/padding defects in nar.rs each red it — evidence in
#     plan/td-builder.md.
#   • S3: BUILD DIFFERENTIAL — td-builder parses the ATerm drv, executes its
#     builder in a fresh user namespace (uid 30001, staged store rbind, the
#     daemon's env contract — plan/td-builder.md Q4) and registers the output
#     (v1 record — Q3). Asserted against the daemon, which builds the SAME
#     deterministic drv (tests/td-builder-s3-drvs.scm): same store path,
#     NAR hash equal to the daemon's RECORDED hash, NAR size, references set
#     (an input ref + a self-ref — the scan must find both) and deriver all
#     equal; plus the rootless rung's isolation assert on a separate
#     namespace-sensitive probe drv (built td-side only — its output records
#     uid_map and can never be a differential subject).
#   • S4: SYSTEM-IMAGE DIFFERENTIAL — the §7.1 acceptance subject: td-builder
#     rebuilds the `build` rung's qcow2 image drv itself
#     (tests/td-builder-s4-drv.scm prints the oracle facts the root daemon
#     recorded when it built the SAME drv) and must register equal fields at
#     the same path — store path, NAR hash (recorded AND independently
#     re-hashed), NAR size, references set (compared even if empty) and
#     deriver. This is what forces the sandbox past S3's minimum: the image
#     builder is a real multi-process Guile build (mke2fs/genimage tree) that
#     honestly reds on any missing piece of the daemon's chroot contract.
# OFFLINE PRECONDITION (DESIGN §5): the pinned Rust closure must be warm in the
# host store — the loop fetches nothing. Two-step lower-then-realise (repl ->
# guix build) for an honest exit status, as in the other rungs.
td-builder:
	@echo ">> td-builder: reproducible offline build (S1) + NAR differential (S2) + build differential (S3) + system-image differential (S4)"
	@set -euo pipefail; \
	drv=`$(GUIX) repl $(LOAD) tests/td-builder-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the td-builder derivation" >&2; exit 1; }; \
	echo ">> td-builder derivation: $$drv"; \
	start=`date +%s`; \
	out=`$(GUIX) build "$$drv"`; \
	elapsed=$$(( `date +%s` - start )); \
	test -n "$$out" || { echo "ERROR: the td-builder build produced no output path" >&2; exit 1; }; \
	echo ">> check: reproducibility of the td-builder binary (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$drv"; \
	echo ">> run: the compiled binary must print its sentinel"; \
	"$$out/bin/td-builder" | grep -Eq '^td-builder [0-9.]+ ok$$' \
	  || { echo "FAIL: the compiled td-builder did not print its sentinel (or exited nonzero) — the toolchain did not produce a working binary." >&2; exit 1; }; \
	echo ">> S2: NAR differential — td-builder nar-hash vs the daemon's recorded hash"; \
	pairs=`$(GUIX) repl $(LOAD) tests/td-builder-nar.scm 2>/dev/null | sed -n 's/^NAR=//p'`; \
	test -n "$$pairs" || { echo "ERROR: could not compute the oracle NAR pairs (tests/td-builder-nar.scm)" >&2; exit 1; }; \
	n=0; \
	while read -r p expect; do \
	  test -n "$$p" -a -n "$$expect" || { echo "ERROR: malformed oracle pair: '$$p $$expect'" >&2; exit 1; }; \
	  have=`"$$out/bin/td-builder" nar-hash "$$p"` \
	    || { echo "FAIL: td-builder nar-hash failed on $$p" >&2; exit 1; }; \
	  test "$$have" = "sha256:$$expect" \
	    || { echo "FAIL: NAR hash mismatch for $$p" >&2; \
	         echo "      td-builder: $$have" >&2; \
	         echo "      daemon    : sha256:$$expect" >&2; exit 1; }; \
	  echo "   nar ok ($$have): $$p"; \
	  n=$$((n + 1)); \
	done <<< "$$pairs"; \
	test "$$n" -ge 2 || { echo "FAIL: expected at least 2 oracle NAR pairs (fixture + td-builder output), got $$n" >&2; exit 1; }; \
	echo ">> S3: drv parse + sandboxed userns build differential vs the daemon"; \
	scratch="$(CURDIR)/.td-builder-scratch"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	$(GUIX) repl $(LOAD) tests/td-builder-s3-drvs.scm 2>/dev/null > "$$scratch/s3.txt"; \
	diff_drv=`sed -n 's/^DIFF_DRV=//p' "$$scratch/s3.txt"`; \
	diff_out=`sed -n 's/^DIFF_OUT=//p' "$$scratch/s3.txt"`; \
	diff_hash=`sed -n 's/^DIFF_HASH=//p' "$$scratch/s3.txt"`; \
	diff_narsize=`sed -n 's/^DIFF_NARSIZE=//p' "$$scratch/s3.txt"`; \
	diff_deriver=`sed -n 's/^DIFF_DERIVER=//p' "$$scratch/s3.txt"`; \
	probe_drv=`sed -n 's/^PROBE_DRV=//p' "$$scratch/s3.txt"`; \
	probe_out=`sed -n 's/^PROBE_OUT=//p' "$$scratch/s3.txt"`; \
	test -n "$$diff_drv" -a -n "$$diff_out" -a -n "$$diff_hash" -a -n "$$diff_narsize" \
	     -a -n "$$diff_deriver" -a -n "$$probe_drv" -a -n "$$probe_out" \
	  || { echo "ERROR: could not lower the S3 drvs (tests/td-builder-s3-drvs.scm)" >&2; exit 1; }; \
	{ sed -n 's/^DIFF_INPUT=//p;s/^PROBE_INPUT=//p' "$$scratch/s3.txt"; \
	  printf '%s\n' "$$diff_drv" "$$probe_drv"; } \
	  | xargs $(GUIX) gc -R | sort -u > "$$scratch/paths.txt"; \
	echo "   staged closure: $$(wc -l < "$$scratch/paths.txt") store items"; \
	"$$out/bin/td-builder" drv-parse "$$diff_drv" > /dev/null \
	  || { echo "FAIL: td-builder drv-parse rejected the diff drv $$diff_drv" >&2; exit 1; }; \
	echo "   isolation probe: the build must run in a fresh user namespace"; \
	"$$out/bin/td-builder" build "$$probe_drv" "$$scratch/paths.txt" "$$scratch/probe" > /dev/null \
	  || { echo "FAIL: td-builder could not build the isolation probe drv" >&2; exit 1; }; \
	map="$$scratch/probe/newstore/$${probe_out#/gnu/store/}/uid_map"; \
	test -s "$$map" || { echo "FAIL: the isolation probe recorded an empty uid_map" >&2; exit 1; }; \
	echo "   uid_map seen by the td-builder sandbox:"; sed 's/^/     /' "$$map"; \
	map_lines=`wc -l < "$$map"`; read -r map_first map_rest < "$$map"; \
	if [ "$$map_lines" -ne 1 ] || [ "$$map_first" != "30001" ]; then \
	  echo "FAIL: the td-builder build's uid_map is not a fresh per-build user" >&2; \
	  echo "      namespace mapping with the daemon's guest uid (expected the" >&2; \
	  echo "      single entry '30001 <host> 1' — build.cc defaultGuestUID; a" >&2; \
	  echo "      leading 0 means no/inherited namespace, any other uid breaks" >&2; \
	  echo "      the Q4 contract)." >&2; exit 1; \
	fi; \
	echo "   differential: td-builder rebuild vs the daemon's recorded facts"; \
	"$$out/bin/td-builder" build "$$diff_drv" "$$scratch/paths.txt" "$$scratch/diff" > "$$scratch/diff-build.txt" \
	  || { echo "FAIL: td-builder could not build the diff drv $$diff_drv" >&2; exit 1; }; \
	grep -qx "OUT=out $$diff_out" "$$scratch/diff-build.txt" \
	  || { echo "FAIL: store-path mismatch: td-builder reported '$$(cat "$$scratch/diff-build.txt")', the daemon built $$diff_out" >&2; exit 1; }; \
	reg="$$scratch/diff/registration"; \
	test -s "$$reg" || { echo "FAIL: td-builder wrote no registration record" >&2; exit 1; }; \
	grep -qx "path $$diff_out" "$$reg" \
	  || { echo "FAIL: registration path mismatch (see record below) vs $$diff_out" >&2; cat "$$reg" >&2; exit 1; }; \
	grep -qx "nar-hash sha256:$$diff_hash" "$$reg" \
	  || { echo "FAIL: NAR hash mismatch — registration '$$(sed -n 's/^nar-hash //p' "$$reg")' vs daemon 'sha256:$$diff_hash'" >&2; exit 1; }; \
	grep -qx "nar-size $$diff_narsize" "$$reg" \
	  || { echo "FAIL: NAR size mismatch — registration '$$(sed -n 's/^nar-size //p' "$$reg")' vs daemon '$$diff_narsize'" >&2; exit 1; }; \
	grep -qx "deriver $$diff_deriver" "$$reg" \
	  || { echo "FAIL: deriver mismatch — registration '$$(sed -n 's/^deriver //p' "$$reg")' vs daemon '$$diff_deriver'" >&2; exit 1; }; \
	sed -n 's/^DIFF_REF=//p' "$$scratch/s3.txt" > "$$scratch/refs.oracle"; \
	sed -n 's/^reference //p' "$$reg" > "$$scratch/refs.td"; \
	test -s "$$scratch/refs.oracle" \
	  || { echo "ERROR: the oracle recorded NO references for the diff drv — the fixture lost its discriminating refs" >&2; exit 1; }; \
	test "$$(cat "$$scratch/refs.oracle")" = "$$(cat "$$scratch/refs.td")" \
	  || { echo "FAIL: references set mismatch:" >&2; \
	       echo "      daemon recorded:" >&2; sed 's/^/        /' "$$scratch/refs.oracle" >&2; \
	       echo "      td-builder registered:" >&2; sed 's/^/        /' "$$scratch/refs.td" >&2; exit 1; }; \
	rehash=`"$$out/bin/td-builder" nar-hash "$$scratch/diff/newstore/$${diff_out#/gnu/store/}"`; \
	test "$$rehash" = "sha256:$$diff_hash" \
	  || { echo "FAIL: independent re-hash of the on-disk rebuild gives $$rehash, the daemon recorded sha256:$$diff_hash" >&2; exit 1; }; \
	echo "   rebuild equal: store path, NAR hash (registered + re-hashed), size, references (input + self), deriver"; \
	echo ">> S4: system-image differential — td-builder rebuilds the build rung's qcow2 drv"; \
	img_drv=`$(GUIX) system image $(LOAD) -t $(IMGTYPE) -d $(SYSTEM)`; \
	test -n "$$img_drv" || { echo "ERROR: could not lower the image derivation" >&2; exit 1; }; \
	echo "   target image drv: $$img_drv"; \
	img_oracle=`$(GUIX) build "$$img_drv"`; \
	test -n "$$img_oracle" || { echo "ERROR: the oracle image build produced no output path" >&2; exit 1; }; \
	TD_IMAGE_DRV="$$img_drv" $(GUIX) repl $(LOAD) tests/td-builder-s4-drv.scm 2>/dev/null > "$$scratch/s4.txt"; \
	img_out=`sed -n 's/^IMG_OUT=//p' "$$scratch/s4.txt"`; \
	img_hash=`sed -n 's/^IMG_HASH=//p' "$$scratch/s4.txt"`; \
	img_narsize=`sed -n 's/^IMG_NARSIZE=//p' "$$scratch/s4.txt"`; \
	img_deriver=`sed -n 's/^IMG_DERIVER=//p' "$$scratch/s4.txt"`; \
	test -n "$$img_out" -a -n "$$img_hash" -a -n "$$img_narsize" -a -n "$$img_deriver" \
	  || { echo "ERROR: could not read the S4 oracle facts (tests/td-builder-s4-drv.scm)" >&2; exit 1; }; \
	test "$$img_out" = "$$img_oracle" \
	  || { echo "ERROR: lowered image output ($$img_out) != realized oracle output ($$img_oracle)" >&2; exit 1; }; \
	{ sed -n 's/^IMG_INPUT=//p' "$$scratch/s4.txt"; printf '%s\n' "$$img_drv"; } \
	  | xargs $(GUIX) gc -R | sort -u > "$$scratch/s4-paths.txt"; \
	echo "   staged closure: $$(wc -l < "$$scratch/s4-paths.txt") store items"; \
	"$$out/bin/td-builder" build "$$img_drv" "$$scratch/s4-paths.txt" "$$scratch/s4" > "$$scratch/s4-build.txt" \
	  || { echo "FAIL: td-builder could not build the image drv $$img_drv" >&2; exit 1; }; \
	grep -qx "OUT=out $$img_out" "$$scratch/s4-build.txt" \
	  || { echo "FAIL: store-path mismatch: td-builder reported '$$(cat "$$scratch/s4-build.txt")', the daemon built $$img_out" >&2; exit 1; }; \
	s4reg="$$scratch/s4/registration"; \
	test -s "$$s4reg" || { echo "FAIL: td-builder wrote no registration record for the image" >&2; exit 1; }; \
	grep -qx "path $$img_out" "$$s4reg" \
	  || { echo "FAIL: image registration path mismatch (see record below) vs $$img_out" >&2; cat "$$s4reg" >&2; exit 1; }; \
	grep -qx "nar-hash sha256:$$img_hash" "$$s4reg" \
	  || { echo "FAIL: image NAR hash mismatch — registration '$$(sed -n 's/^nar-hash //p' "$$s4reg")' vs daemon 'sha256:$$img_hash'" >&2; exit 1; }; \
	grep -qx "nar-size $$img_narsize" "$$s4reg" \
	  || { echo "FAIL: image NAR size mismatch — registration '$$(sed -n 's/^nar-size //p' "$$s4reg")' vs daemon '$$img_narsize'" >&2; exit 1; }; \
	grep -qx "deriver $$img_deriver" "$$s4reg" \
	  || { echo "FAIL: image deriver mismatch — registration '$$(sed -n 's/^deriver //p' "$$s4reg")' vs daemon '$$img_deriver'" >&2; exit 1; }; \
	sed -n 's/^IMG_REF=//p' "$$scratch/s4.txt" > "$$scratch/s4-refs.oracle"; \
	sed -n 's/^reference //p' "$$s4reg" > "$$scratch/s4-refs.td"; \
	test "$$(cat "$$scratch/s4-refs.oracle")" = "$$(cat "$$scratch/s4-refs.td")" \
	  || { echo "FAIL: image references set mismatch:" >&2; \
	       echo "      daemon recorded:" >&2; sed 's/^/        /' "$$scratch/s4-refs.oracle" >&2; \
	       echo "      td-builder registered:" >&2; sed 's/^/        /' "$$scratch/s4-refs.td" >&2; exit 1; }; \
	img_rehash=`"$$out/bin/td-builder" nar-hash "$$scratch/s4/newstore/$${img_out#/gnu/store/}"`; \
	test "$$img_rehash" = "sha256:$$img_hash" \
	  || { echo "FAIL: independent re-hash of the on-disk image rebuild gives $$img_rehash, the daemon recorded sha256:$$img_hash" >&2; exit 1; }; \
	echo "   image rebuild equal: store path, NAR hash (registered + re-hashed), size, references, deriver"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo ">> closure size:"; $(GUIX) size "$$out" | tail -n1; \
	echo "   compile wall-clock: $${elapsed}s (first run; warm store thereafter)"; \
	echo "PASS: reproducible offline build (S1); NAR serialization bit-for-bit equal to the daemon's recorded hashes across $$n items (S2); the userns sandbox rebuild registers the daemon's exact facts at the same store path and builds in a fresh user namespace (S3); td-builder rebuilds the SYSTEM IMAGE drv itself, daemon-equal on every recorded field (S4)."

# 7. M8 run rung — execute the SHIPPED OCI image as a real rootless OCI container
#    (crun) and assert its userspace runs. Every rung above proves a PROPERTY of
#    the artifact (reproducible, guix-free, manifest-driven) but none ever RAN it;
#    this closes that gap. crun is the low-level OCI runtime podman drives (podman
#    itself is a ~1238-derivation Go tree with cold fetches — it breaks the offline
#    loop; crun is 18 derivations, offline). NOT a derivation: running a container
#    needs a live user namespace, which the build daemon's sandbox forbids, so —
#    exactly like `docker run` — this runs in the loop shell against the freshly
#    built image (check.sh exposes the host cgroup2 so crun's startup probe passes;
#    the helper runs crun rootless, --cgroup-manager=disabled, single-uid map,
#    empty network ns → the container is offline by construction). The image
#    entrypoint is the system boot-program (the full boot is covered by the
#    marionette `test`/`boot-disk` rungs); here we OVERRIDE args like
#    `docker run IMG <cmd>` to drive /bin/sh. Self-discriminating: a positive run
#    (sentinel + exit 0) AND a negative control (a bogus exec must fail) — see
#    tests/run-image.sh. Heaviest behavioral rung (it unpacks the full image
#    rootfs) → runs last (§1.3). Its scratch (archive + unpacked rootfs) lives
#    in $(CURDIR)/.run-scratch — disk, not the sandbox /tmp (same tmpfs-size
#    lesson as .genimg-scratch above); run-image.sh's own EXIT trap cleans its
#    subdir either way, the recipe removes the parent on green.
run:
	@echo ">> run: execute the shipped OCI image as a real OCI container (crun)"
	@set -euo pipefail; \
	img=`$(GUIX) system image $(LOAD) -t docker $(SYSTEM)`; \
	test -n "$$img" || { echo "ERROR: could not build the shipped OCI image" >&2; exit 1; }; \
	echo ">> shipped OCI image: $$img"; \
	scratch="$(CURDIR)/.run-scratch"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	TMPDIR="$$scratch" sh tests/run-image.sh "$$img"; \
	rm -rf "$$scratch"

# 8. M9.2 container-HOST rung — boot the SHIPPED base and run a Guix-built OCI APP
#    image on it with the shipped crun, as root. Where `run` (M8) ran the shipped
#    SYSTEM image's userspace, this runs a SEPARATE app image ON the booted base —
#    the container-host relationship (DESIGN §2.3 OCI app model). The app is
#    `guix pack -f docker` of GNU hello (a store path → offline, no registry); it
#    is unpacked into a runtime-bundle rootfs at build time, then crun runs it AS
#    ROOT in the guest (no rootless/userns contortions — that was M8's sandbox-only
#    concern; M9.1 made the base a host: cgroup2 mounted + crun shipped). Marionette
#    rung, so it lowers-then-realises like `test`/`boot-disk` for an honest exit
#    status. The app runs via the IMAGE'S OWN declared entrypoint (read from its
#    archive — a bogus #:entry-point fails the positive, F1). First `--check`s ALL
#    FOUR app artifacts — the good image+bundle AND the bad-entrypoint image+bundle
#    used by the image-metadata negative — so every artifact is permanently proven
#    reproducible (CLAUDE.md), not just the good one. Then runs. Self-discriminating:
#    a POSITIVE run (app prints "Hello, world!", exit 0) and TWO negative controls (a
#    second image with a bogus DECLARED entrypoint, and a bogus runtime arg, both must
#    fail) — see tests/container.scm. M9.3 ADDS a managed-cgroups assertion: crun
#    (cgroupfs manager) applies a declared pids.max=73 to a coreutils container, which
#    reads its own /sys/fs/cgroup/pids.max back as 73 — resource-limit ENFORCEMENT, not
#    just that crun starts (self-discriminating: the cgroup2 default is "max"). The
#    cgroup app image+bundle are --checked for reproducibility alongside the others.
#    fhs-app-images ADDS an FHS-LAYOUT app image+bundle (hello with a /usr/bin/hello
#    symlink, also --checked): crun execs the explicit /usr/bin/hello against the FHS
#    rootfs (resolves, prints output) while the SAME arg fails on the plain
#    store-layout rootfs — proving the binary resolves at a traditional FHS path.
container:
	@echo ">> container: run an OCI app container on the booted td base (crun)"
	@set -euo pipefail; \
	arts=`printf '%s\n' \
	    '(use-modules (guix) (guix monads) (tests container))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (let ((img  (run-with-store store (td-app-image)))' \
	    '        (bun  (run-with-store store (td-app-bundle)))' \
	    '        (bimg (run-with-store store (td-app-badentry-image)))' \
	    '        (bbun (run-with-store store (td-app-badentry-bundle)))' \
	    '        (cimg (run-with-store store (td-app-cgroup-image)))' \
	    '        (cbun (run-with-store store (td-app-cgroup-bundle)))' \
	    '        (fimg (run-with-store store (td-app-fhs-image)))' \
	    '        (fbun (run-with-store store (td-app-fhs-bundle))))' \
	    '    (format #t "IMAGE=~a~%" (derivation-file-name img))' \
	    '    (format #t "BUNDLE=~a~%" (derivation-file-name bun))' \
	    '    (format #t "BADIMAGE=~a~%" (derivation-file-name bimg))' \
	    '    (format #t "BADBUNDLE=~a~%" (derivation-file-name bbun))' \
	    '    (format #t "CGIMAGE=~a~%" (derivation-file-name cimg))' \
	    '    (format #t "CGBUNDLE=~a~%" (derivation-file-name cbun))' \
	    '    (format #t "FHSIMAGE=~a~%" (derivation-file-name fimg))' \
	    '    (format #t "FHSBUNDLE=~a~%" (derivation-file-name fbun))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null`; \
	img=`printf '%s\n' "$$arts" | sed -n 's/^IMAGE=//p'`; \
	bun=`printf '%s\n' "$$arts" | sed -n 's/^BUNDLE=//p'`; \
	bimg=`printf '%s\n' "$$arts" | sed -n 's/^BADIMAGE=//p'`; \
	bbun=`printf '%s\n' "$$arts" | sed -n 's/^BADBUNDLE=//p'`; \
	cimg=`printf '%s\n' "$$arts" | sed -n 's/^CGIMAGE=//p'`; \
	cbun=`printf '%s\n' "$$arts" | sed -n 's/^CGBUNDLE=//p'`; \
	fimg=`printf '%s\n' "$$arts" | sed -n 's/^FHSIMAGE=//p'`; \
	fbun=`printf '%s\n' "$$arts" | sed -n 's/^FHSBUNDLE=//p'`; \
	test -n "$$img" -a -n "$$bun" -a -n "$$bimg" -a -n "$$bbun" -a -n "$$cimg" -a -n "$$cbun" -a -n "$$fimg" -a -n "$$fbun" || { echo "ERROR: could not lower the app artifacts" >&2; exit 1; }; \
	echo ">> app artifacts: image=$$img bundle=$$bun"; \
	echo ">> negative-control artifacts: badimage=$$bimg badbundle=$$bbun"; \
	echo ">> cgroup artifacts (M9.3): cgimage=$$cimg cgbundle=$$cbun"; \
	echo ">> fhs artifacts (fhs-app-images): fhsimage=$$fimg fhsbundle=$$fbun"; \
	$(GUIX) build "$$img" "$$bun" "$$bimg" "$$bbun" "$$cimg" "$$cbun" "$$fimg" "$$fbun" >/dev/null; \
	echo ">> reproducibility: guix build --check the app images + extracted bundles (good + negative + cgroup + fhs; verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$img" "$$bun" "$$bimg" "$$bbun" "$$cimg" "$$cbun" "$$fimg" "$$fbun"; \
	drv=`printf '%s\n' \
	    '(use-modules (guix) (gnu tests) (tests container))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (format #t "DRV=~a~%"' \
	    '          (derivation-file-name' \
	    '           (run-with-store store (system-test-value %test-td-container)))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the container test derivation" >&2; exit 1; }; \
	echo ">> realise container test derivation: $$drv"; \
	$(GUIX) build "$$drv"

# 9. offline-isolation sandbox probe (plan/offline-isolation.md S1). The
#    hermeticity clause says an UNDECLARED fetch — network access from a
#    non-fixed-output builder — must be impossible; until now that was an
#    assumed property of guix-daemon's sandbox, never an asserted rung. This
#    realises tests/offline-drv.scm's DRV_SANDBOX probe: a regular derivation
#    whose builder must see ONLY `lo` in /proc/net/dev and whose TCP egress
#    attempt must raise — i.e. the deliberate undeclared fetch demonstrably
#    fails. Then `guix build --check` re-runs the builder, so the assertions
#    RE-EXECUTE every loop (and the probe is proven reproducible, prime
#    directive 1) — a daemon regression (e.g. --disable-chroot) reds this rung
#    on the next check, not just on a cold store. Self-discriminating across
#    contexts: check.sh's host-side control proves the SAME /proc/net/dev
#    mechanism reports non-lo interfaces where network IS present, and the
#    fixed-output twin (DRV_DAEMON, wired in at S2) is the same builder body
#    failing red in a network-visible netns (verified-red evidence in
#    plan/offline-isolation.md). Cheapest heavy rung (one tiny local build) →
#    listed last (LPT).
offline:
	@echo ">> offline: an undeclared (non-fixed-output) network fetch must FAIL in the build sandbox"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/offline-drv.scm 2>/dev/null`; \
	sandbox_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_SANDBOX=//p'`; \
	test -n "$$sandbox_drv" || { echo "ERROR: could not lower the offline probe derivations" >&2; exit 1; }; \
	echo ">> sandbox probe derivation: $$sandbox_drv"; \
	$(GUIX) build "$$sandbox_drv"; \
	echo ">> re-run + reproducibility: --check forces the sandbox probe assertions to re-execute"; \
	$(GUIX) build --check "$$sandbox_drv"; \
	echo "PASS: a non-fixed-output builder has no network — loopback-only netns, egress raises (re-checked this run)."
# check-memo discipline rung (DESIGN §7.1 side-track; plan/check-memo.md — the
# §4.3 gate-2 charter with the BINDING constraints 1-6). Permanent,
# self-discriminating exercise of the verdict-memoization helper
# (tests/check-memo.sh) on TINY fixture drvs, so the charter's constraints are
# asserted EVERY loop, not only in one-off verified-red runs:
#   • wiring: TD_CHECK_ENV must be EXPORTED into the sandbox by check.sh
#     (possibly EMPTY — empty IS the CI gate, constraint 2). The helper is
#     then driven with SYNTHETIC identities + a scratch verdict dir + PINNED
#     knobs (every leg sets TD_CHECK_FULL/TD_CHECK_TTL_DAYS itself), so this
#     rung behaves identically on dev hosts, on CI, and under an ambient
#     force-full ladder run (TD_CHECK_FULL=1 ./check.sh — caught at S3: an
#     inherited knob turned the hit leg into a forced miss and red the rung),
#     and never touches the real .check-verdicts state.
#   • miss-then-record: first sight of the det fixture runs the real --check
#     and records a verdict;
#   • hit: the second run hits — including constraint 5's cheap assertion
#     (outputs valid in the store DB with the verdict's NAR hashes);
#   • changed drv (verified-red A's structural twin): a different fixture drv
#     can never hit the first one's verdict (key = drv store path,
#     constraint 1);
#   • expiry (B): a verdict aged past the TTL misses (constraint 3);
#   • future timestamp: a verdict "recorded in the future" (clock skew or a
#     hand-edited record) misses as malformed — the TTL bound cannot be
#     evaded by a timestamp the clock has not reached (constraint 3);
#   • foreign environment (C): another identity misses (constraint 2);
#   • tamper (constraint 5): a verdict whose recorded NAR hash is corrupted
#     misses — a vanished or tampered record cannot green a hit;
#   • force-full (constraint 4): a fresh valid verdict is BYPASSED;
#   • empty identity: never hits and never records, even over a fresh valid
#     verdict — the mechanism check.sh's CI gate relies on;
#   • TTL cap (constraint 3): a TTL above 14 days is REFUSED outright;
#   • nondet on a miss (D): a deliberately nondeterministic fixture with no
#     verdict runs the real --check and goes RED, and no verdict is recorded
#     — detection power is intact on every miss.
# Cheap-side heavy rung (a handful of trivial local builds + repl calls) →
# listed last (LPT). Scratch lives in $(CURDIR)/.memo-scratch — kept on red
# for triage, removed on green.
memo:
	@echo ">> memo: --check verdict memoization — miss/hit/changed-drv/expiry/foreign/tamper/force-full/TTL-cap/nondet discipline"
	@set -euo pipefail; \
	test "$${TD_CHECK_ENV+set}" = set \
	  || { echo "FAIL: TD_CHECK_ENV is not exported into the sandbox — check.sh's environment-identity computation or its --preserve wiring is broken (run via ./check.sh)." >&2; exit 1; }; \
	drvs=`$(GUIX) repl $(LOAD) tests/check-memo-drvs.scm 2>/dev/null`; \
	det=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_DET=//p'`; \
	det2=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_DET2=//p'`; \
	nondet=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_NONDET=//p'`; \
	test -n "$$det" -a -n "$$det2" -a -n "$$nondet" || { echo "ERROR: could not lower the memo fixture drvs" >&2; exit 1; }; \
	echo ">> fixture drvs: det=$$det det2=$$det2 nondet=$$nondet"; \
	$(GUIX) build "$$det" "$$det2" > /dev/null; \
	vd="$(CURDIR)/.memo-scratch"; rm -rf "$$vd"; mkdir -p "$$vd"; \
	vf="$$vd/`basename "$$det"`.verdict"; \
	echo ">> leg miss+record: first sight runs the real --check and records"; \
	out=`TD_GUIX="$(GUIX)" TD_CHECK_VERDICTS="$$vd" TD_CHECK_FULL= TD_CHECK_TTL_DAYS= TD_CHECK_ENV=td-memo-env-one sh tests/check-memo.sh "$$det" 2>&1`; \
	printf '%s\n' "$$out" | grep -q "MEMO MISS (no verdict)" || { echo "FAIL: first sight of the det fixture did not MISS with 'no verdict':" >&2; printf '%s\n' "$$out" >&2; exit 1; }; \
	printf '%s\n' "$$out" | grep -q "MEMO RECORD" || { echo "FAIL: the green --check did not record a verdict:" >&2; printf '%s\n' "$$out" >&2; exit 1; }; \
	test -s "$$vf" || { echo "FAIL: no verdict file was written at $$vf" >&2; exit 1; }; \
	echo ">> leg hit: a fresh same-env verdict skips the rebuild (constraint 5 DB assertion included)"; \
	out=`TD_GUIX="$(GUIX)" TD_CHECK_VERDICTS="$$vd" TD_CHECK_FULL= TD_CHECK_TTL_DAYS= TD_CHECK_ENV=td-memo-env-one sh tests/check-memo.sh "$$det" 2>&1`; \
	printf '%s\n' "$$out" | grep -q "MEMO HIT" || { echo "FAIL: the second sight did not HIT:" >&2; printf '%s\n' "$$out" >&2; exit 1; }; \
	if printf '%s\n' "$$out" | grep -q "MEMO MISS"; then echo "FAIL: the second sight MISSED despite a fresh valid verdict:" >&2; printf '%s\n' "$$out" >&2; exit 1; fi; \
	echo ">> leg changed-drv (A): a different drv can never hit the recorded verdict"; \
	out=`TD_GUIX="$(GUIX)" TD_CHECK_VERDICTS="$$vd" TD_CHECK_FULL= TD_CHECK_TTL_DAYS= TD_CHECK_ENV=td-memo-env-one sh tests/check-memo.sh "$$det2" 2>&1`; \
	printf '%s\n' "$$out" | grep -q "MEMO MISS (no verdict)" || { echo "FAIL: the CHANGED drv did not miss — verdicts are not keyed by drv store path:" >&2; printf '%s\n' "$$out" >&2; exit 1; }; \
	echo ">> leg expiry (B): a verdict aged past the TTL misses"; \
	sed -i 's/^recorded .*/recorded 1/' "$$vf"; \
	out=`TD_GUIX="$(GUIX)" TD_CHECK_VERDICTS="$$vd" TD_CHECK_FULL= TD_CHECK_TTL_DAYS= TD_CHECK_ENV=td-memo-env-one sh tests/check-memo.sh "$$det" 2>&1`; \
	printf '%s\n' "$$out" | grep -q "MEMO MISS (expired" || { echo "FAIL: an EXPIRED verdict did not miss:" >&2; printf '%s\n' "$$out" >&2; exit 1; }; \
	echo ">> leg future timestamp: a verdict recorded 'in the future' misses as malformed"; \
	sed -i 's/^recorded .*/recorded 99999999999/' "$$vf"; \
	out=`TD_GUIX="$(GUIX)" TD_CHECK_VERDICTS="$$vd" TD_CHECK_FULL= TD_CHECK_TTL_DAYS= TD_CHECK_ENV=td-memo-env-one sh tests/check-memo.sh "$$det" 2>&1`; \
	printf '%s\n' "$$out" | grep -q "MEMO MISS (malformed verdict (bad or future timestamp))" || { echo "FAIL: a FUTURE-dated verdict did not miss — the TTL bound can be evaded by clock skew:" >&2; printf '%s\n' "$$out" >&2; exit 1; }; \
	echo ">> leg foreign env (C): a verdict from another environment misses"; \
	out=`TD_GUIX="$(GUIX)" TD_CHECK_VERDICTS="$$vd" TD_CHECK_FULL= TD_CHECK_TTL_DAYS= TD_CHECK_ENV=td-memo-env-two sh tests/check-memo.sh "$$det" 2>&1`; \
	printf '%s\n' "$$out" | grep -q "MEMO MISS (foreign environment)" || { echo "FAIL: a FOREIGN verdict did not miss:" >&2; printf '%s\n' "$$out" >&2; exit 1; }; \
	echo ">> leg tamper (constraint 5): a corrupted recorded NAR hash misses"; \
	sed -i 's/^\(output out [^ ]* \)[0-9a-f]\{8\}/\1deadbeef/' "$$vf"; \
	out=`TD_GUIX="$(GUIX)" TD_CHECK_VERDICTS="$$vd" TD_CHECK_FULL= TD_CHECK_TTL_DAYS= TD_CHECK_ENV=td-memo-env-two sh tests/check-memo.sh "$$det" 2>&1`; \
	printf '%s\n' "$$out" | grep -q "MEMO MISS (verdict/DB mismatch" || { echo "FAIL: a TAMPERED verdict did not miss:" >&2; printf '%s\n' "$$out" >&2; exit 1; }; \
	echo ">> leg force-full (constraint 4): a fresh valid verdict is bypassed"; \
	out=`TD_GUIX="$(GUIX)" TD_CHECK_VERDICTS="$$vd" TD_CHECK_TTL_DAYS= TD_CHECK_ENV=td-memo-env-two TD_CHECK_FULL=1 sh tests/check-memo.sh "$$det" 2>&1`; \
	printf '%s\n' "$$out" | grep -q "MEMO MISS (forced full)" || { echo "FAIL: TD_CHECK_FULL=1 did not bypass a fresh valid verdict:" >&2; printf '%s\n' "$$out" >&2; exit 1; }; \
	echo ">> leg empty identity: no identity => never hit, never record (the CI gate's mechanism)"; \
	out=`TD_GUIX="$(GUIX)" TD_CHECK_VERDICTS="$$vd" TD_CHECK_FULL= TD_CHECK_TTL_DAYS= TD_CHECK_ENV= sh tests/check-memo.sh "$$det" 2>&1`; \
	printf '%s\n' "$$out" | grep -q "MEMO MISS (no environment identity)" || { echo "FAIL: an EMPTY identity did not force a miss:" >&2; printf '%s\n' "$$out" >&2; exit 1; }; \
	if printf '%s\n' "$$out" | grep -q "MEMO RECORD"; then echo "FAIL: a run with NO identity recorded a verdict:" >&2; printf '%s\n' "$$out" >&2; exit 1; fi; \
	echo ">> leg TTL cap (constraint 3): a TTL above 14 days is refused"; \
	if out=`TD_GUIX="$(GUIX)" TD_CHECK_VERDICTS="$$vd" TD_CHECK_FULL= TD_CHECK_ENV=td-memo-env-two TD_CHECK_TTL_DAYS=15 sh tests/check-memo.sh "$$det" 2>&1`; then \
	  echo "FAIL: TD_CHECK_TTL_DAYS=15 was ACCEPTED — the gate-2 TTL bound is not enforced:" >&2; printf '%s\n' "$$out" >&2; exit 1; \
	fi; \
	printf '%s\n' "$$out" | grep -q "re-opens gate 2" || { echo "FAIL: the TTL refusal did not state its gate-2 reason:" >&2; printf '%s\n' "$$out" >&2; exit 1; }; \
	echo ">> leg nondet on a miss (D): the real --check still reds, nothing recorded"; \
	$(GUIX) build "$$nondet" > /dev/null; \
	if TD_GUIX="$(GUIX)" TD_CHECK_VERDICTS="$$vd" TD_CHECK_FULL= TD_CHECK_TTL_DAYS= TD_CHECK_ENV=td-memo-env-one sh tests/check-memo.sh "$$nondet" > "$$vd/nondet.log" 2>&1; then \
	  echo "FAIL: the helper GREENED a deliberately nondeterministic drv on a miss — detection power lost:" >&2; cat "$$vd/nondet.log" >&2; exit 1; \
	fi; \
	grep -q "MEMO MISS (no verdict)" "$$vd/nondet.log" || { echo "FAIL: the nondet leg did not take the miss path:" >&2; cat "$$vd/nondet.log" >&2; exit 1; }; \
	test ! -f "$$vd/`basename "$$nondet"`.verdict" || { echo "FAIL: a verdict was recorded for a drv whose --check FAILED" >&2; exit 1; }; \
	rm -rf "$$vd"; \
	echo "PASS: memoization discipline holds — miss-then-record, hit with the constraint-5 DB assertion, changed-drv/expiry/future-timestamp/foreign/tamper all miss, force-full bypasses, empty identity never hits or records, TTL>14d refused, and a nondeterministic miss still reds."

# 10. rootless-builder differential (DESIGN §7.1 side-track; prime directive 4).
#    Build the target with a ROOTLESS USER-NAMESPACE builder and prove
#    daemon-vs-rootless store-path equality — the root guix-daemon is the
#    oracle. The rootless builder is the SAME pinned daemon binary run
#    UNPRIVILEGED in a nested userns (at this pin a daemon without
#    --build-users-group gives every chroot build CLONE_NEWUSER), so privilege +
#    namespace is the ONLY variable in the experiment. tests/rootless.sh:
#      • stages a writable /gnu/store view (per-item binds + rbind; overlayfs
#        is impossible here — the sandbox's per-item profile binds are
#        MNT_LOCKED in the nested userns and overlay rejects such a lowerdir),
#        snapshots the host DB via sqlite's backup API, covers /var/guix with
#        tmpfs (the host daemon is unreachable by construction), and starts the
#        unprivileged daemon offline (--no-substitutes --no-offload);
#      • validity guard: the oracle output must be valid in the snapshot —
#        otherwise `--check` would BUILD instead of COMPARE (false green);
#      • isolation probe: a deliberately environment-sensitive drv records
#        /proc/self/uid_map from inside the build; an identity map (no userns)
#        reds the rung. The probe is an instrument, never `--check`ed, and its
#        output exists only in the discarded scratch store (it must stay
#        INVALID in the real store — the guard reds if it ever becomes valid);
#      • the differential: rootless `guix build --check` of the SAME image drv
#        the `build` rung oracles — same drv ⇒ same store path by construction
#        (asserted explicitly), and --check makes the rootless daemon rebuild
#        it and compare bit-for-bit against the root daemon's artifact. On
#        mismatch the divergent rebuild is kept (--keep-failed) and the rung
#        prints the exact diffoscope command to run OUTSIDE the loop
#        (diffoscope is a cold Python closure the offline sandbox cannot
#        build).
#    The recipe does the pinned-guix work ($(GUIX): lower, oracle-build,
#    closure via gc -R); the script does the namespace work with the
#    pin-guarded host guix (time-machine cannot re-resolve channels once
#    /gnu/store is covered). Scratch lives in $(CURDIR)/.rootless-scratch
#    (disk, not the sandbox tmpfs); kept on red for diffing, removed on green.
rootless:
	@echo ">> rootless: unprivileged userns builder vs root daemon — store-path differential"
	@set -euo pipefail; \
	test -n "$${GUIX_ENVIRONMENT-}" || { echo "ERROR: GUIX_ENVIRONMENT is unset — run via ./check.sh (the sandbox profile must be bound into the staged store)" >&2; exit 1; }; \
	img_drv=`$(GUIX) system image $(LOAD) -t $(IMGTYPE) -d $(SYSTEM)`; \
	test -n "$$img_drv" || { echo "ERROR: could not lower the image derivation" >&2; exit 1; }; \
	echo ">> target image drv: $$img_drv"; \
	echo ">> oracle build via the ROOT daemon"; \
	img_out=`$(GUIX) build "$$img_drv"`; \
	test -n "$$img_out" || { echo "ERROR: oracle build produced no output path" >&2; exit 1; }; \
	scratch="$(CURDIR)/.rootless-scratch"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	TD_IMAGE_DRV="$$img_drv" $(GUIX) repl $(LOAD) tests/rootless-drvs.scm 2>/dev/null > "$$scratch/drvs.txt"; \
	img_out_lowered=`sed -n 's/^IMG_OUT=//p' "$$scratch/drvs.txt" | head -n1`; \
	probe_drv=`sed -n 's/^PROBE_DRV=//p' "$$scratch/drvs.txt"`; \
	probe_out=`sed -n 's/^PROBE_OUT=//p' "$$scratch/drvs.txt"`; \
	test -n "$$img_out_lowered" -a -n "$$probe_drv" -a -n "$$probe_out" || { echo "ERROR: could not lower the rootless rung derivations" >&2; exit 1; }; \
	test "$$img_out_lowered" = "$$img_out" || { echo "ERROR: lowered image output ($$img_out_lowered) != realized oracle output ($$img_out)" >&2; exit 1; }; \
	guix_pkg=`dirname "$$(dirname "$$(readlink -f "$$(command -v guix)")")"`; \
	guix_daemon_pkg=`dirname "$$(dirname "$$(readlink -f "$$(command -v guix-daemon)")")"`; \
	{ sed -n 's/^IMG_INPUT=//p;s/^PROBE_INPUT=//p' "$$scratch/drvs.txt"; \
	  printf '%s\n' "$$img_drv" "$$img_out" "$$probe_drv" "$$guix_pkg" "$$guix_daemon_pkg" "$$GUIX_ENVIRONMENT"; } \
	  | xargs $(GUIX) gc -R | sort -u > "$$scratch/paths.txt"; \
	echo ">> bind closure: $$(wc -l < "$$scratch/paths.txt") store items"; \
	bash tests/rootless.sh "$$scratch" "$$img_drv" "$$img_out" "$$probe_drv" "$$probe_out"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"
