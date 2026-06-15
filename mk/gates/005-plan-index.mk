# PLAN.md's track table is GENERATED from the per-track records in
# plan/tracks/*.md by tools/plan-index.sh (the PLAN.md-management mechanism —
# CLAUDE.md "Parallel work"). This gate fails the loop if the committed PLAN.md
# drifts from the records: a merged track that was flipped to `status: done` but
# whose table line was not regenerated reds here, so PLAN.md can no longer carry
# a stale "claimed"/[ ] for landed work (the drift this gate exists to kill).
# Pure bash + sed + sort, no guix/store — also runs in the CI fast tier.
CHEAP_GATES += plan-index
plan-index:
	@echo ">> plan-index: PLAN.md matches plan/tracks/*.md (no track-status drift)"
	@bash tools/plan-index.sh --check
