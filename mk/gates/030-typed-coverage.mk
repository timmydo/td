# M4 typed coverage (triage #4). Table-driven, derivation-level: every typed
# field must (A) change the lowered system when given a valid non-default value
# (proves it is wired, not ignored) and (B) reject an invalid value at
# construction (proves per-field validation). Where `diff` checks convergence +
# one perturbation, this sweeps all fields. Run as a repl SCRIPT for honest exit.
CHEAP_GATES += typed-coverage
typed-coverage:
	@echo ">> typed-coverage: every typed field is wired and validated"
	$(GUIX) repl $(LOAD) tests/typed-coverage.scm
