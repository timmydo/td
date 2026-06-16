# guix-dependence — measure td's BUILD-TIME independence from guix
# (independence-metric track). The loop has many DIFFERENTIAL gates that test
# td's own code against guix as the oracle (td-build*, drv-*, store-*), and the
# `no-guix` gate (110) proves the shipped IMAGE carries no guix binary (RUNTIME
# independence). What was missing is a NUMBER for the other axis — BUILD-TIME
# independence: of everything needed to BUILD a target, how much does td build
# itself vs pull from guix? This gate records that ratio.
#
# A derivation is "td-reproducible" iff a non-perturbed tests/ts/recipe-<spec>.ts
# exists AND td BUILDS it with its own Rust builder (proven by a td-build-* gate in
# THIS ladder, so a green loop means recipe ⇒ proof). The byte-identity corpus-*
# gates that used to ground this were retired with system/td-recipe.scm; pkg-config
# is authored but not yet td-built (no own-builder gate) and is excluded.
# tests/guix-dependence.scm walks the full build closure (derivation prerequisite
# graph — lowering only, NO building) of two targets — the owned-recipe union and
# the shipped system/td.scm — and emits a DETERMINISTIC census, compared verbatim
# to tests/guix-dependence.expected.
#
# Snapshot, not threshold (the DIGESTS pattern): the number can only change by a
# deliberate re-baseline, so landing a recipe RAISES it and the PR shows the
# delta; a pin bump re-baselines it like DIGESTS. PURELY ADDITIVE — it removes,
# loosens, skips, and reorders NOTHING (CLAUDE.md directive 3); it records a
# ratio and fails closed on undocumented drift.
#
# What it does NOT claim: the denominators are guix's closure shape, and it does
# not re-prove reproducibility (the td-build-* gates do). It quantifies td's
# OWNERSHIP ratio and catches drift in it. Cheap (<2s; lowers derivations, no
# build; offline like `diff`) → cheap pool, fails fast. Re-baseline:
#   TD_DEPENDENCE_WRITE=1 ./check.sh -- and commit tests/guix-dependence.expected
# (or: TD_DEPENDENCE_WRITE=1 guix repl -L . tests/guix-dependence.scm inside the
# sandbox). Run as a repl SCRIPT so the script's (exit) is the gate's status.
CHEAP_GATES += guix-dependence
guix-dependence:
	@echo ">> guix-dependence: td's build-time independence from guix (snapshot census of td-reproducible vs guix-supplied derivations across the owned-corpus union + shipped system)"
	$(GUIX) repl $(LOAD) tests/guix-dependence.scm
