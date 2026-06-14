# loop-gate (DESIGN §7.1; gate-2 "Loop tooling convergence", Step 1 — the full-gate
# differential). loop-sandbox (#30/#31) proved td's host-sandbox hosts a single guix
# operation; this proves it hosts a REAL loop gate. `td-builder host-sandbox
# --expose-cwd` adds guix shell -C's FULL loop env (the worktree/cwd bound like its
# shared cwd, the cgroup hierarchy + the guix cache, the caller's PATH — the toolchain,
# all /gnu/store — and TD_CHECK_*/USER preserved, chdir into the cwd). The differential:
# the `eval` gate's exact command (`$(GUIX) repl $(LOAD) tests/eval.scm` — loads every
# system/test module + prints "eval ok") produces BYTE-IDENTICAL combined output inside
# td's full-env sandbox as it does directly under check.sh's `guix shell -C` (the
# oracle). Proves a real gate runs identically in td's sandbox — the differential the
# wholesale check.sh swap (Step 2, deferred) needs. ADDITIVE: check.sh is UNCHANGED.
# Heavy (a td-builder compile + two guix repl evals), so it slots in the heavy pool by
# the other loop gates.
HEAVY_GATES += loop-rung
loop-rung:
	@echo ">> loop-rung: a REAL rung (eval) runs with IDENTICAL output + success inside td's full-env sandbox (--expose-cwd) as under guix shell -C"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	user="$${USER:-`id -un 2>/dev/null || echo nobody`}"; \
	scratch="$(CURDIR)/.loop-rung-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	echo ">> oracle: the eval rung's command directly under guix shell -C (stdout compared; the Guile auto-compile warnings on stderr are .go-cache-dependent and excluded)"; \
	oracle=`$(GUIX) repl $(LOAD) tests/eval.scm 2>"$$scratch/oracle.err"` \
	  || { echo "FAIL: the oracle eval failed under guix shell -C" >&2; cat "$$scratch/oracle.err" >&2; exit 1; }; \
	echo "   oracle stdout: [$$oracle]"; \
	echo ">> td: the SAME command inside td's host-sandbox --expose-cwd (worktree + toolchain + cache exposed, chdir'd in)"; \
	td=`USER="$$user" "$$tb" host-sandbox --expose-cwd -- $(GUIX) repl $(LOAD) tests/eval.scm 2>"$$scratch/td.err"` \
	  || { echo "FAIL: the eval rung FAILED inside td's sandbox (stderr below) — the full-env exposure is incomplete" >&2; cat "$$scratch/td.err" >&2; exit 1; }; \
	echo "   td stdout    : [$$td]"; \
	test "$$td" = "$$oracle" \
	  || { echo "FAIL: the eval rung produced DIFFERENT stdout inside td's sandbox ([$$td]) than under guix shell -C ([$$oracle])" >&2; exit 1; }; \
	test "$$td" = "eval ok" \
	  || { echo "FAIL: the eval rung did not print 'eval ok' inside td's sandbox ([$$td]) — it did not actually run" >&2; exit 1; }; \
	rm -rf "$$scratch"; \
	echo "PASS: a REAL loop rung (eval — loads every system/test module + prints 'eval ok', exit 0) ran with IDENTICAL stdout AND success inside td's OWN full-env sandbox (td-builder host-sandbox --expose-cwd: worktree + toolchain + cache + cgroups exposed) as directly under check.sh's guix shell -C; the Step-1 full-rung differential for the loop-tooling swap — check.sh's entry is still unchanged (Step 2 deferred)."
