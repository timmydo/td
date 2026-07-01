# daemon-budget — the shared build daemon is the loop's SINGLE machine-wide build limiter:
# it realizes drvs CONCURRENTLY but never more than its global budget at once, ACROSS
# independent submitters (the N-agent property that stops N concurrent checks from
# oversubscribing/OOMing the box). Drives the REAL `td-builder daemon` subcommand over a
# real Unix socket with budget K=2 and TD_DAEMON_TEST_SLEEP_MS (a test-only slot hold, so
# the ceiling is observable deterministically without slow real builds), fires M=6 concurrent
# `daemon-request` submitters, and asserts the daemon's OWN concurrency log shows the peak
# reached EXACTLY K — it parallelized up to the budget AND never exceeded it. The requests
# use nonexistent drvs (they ERR fast); the build OUTCOME is irrelevant — the FEATURE under
# test is the concurrency cap, and each request still occupies a build slot for the hold.
#
# Verified-red: drop the semaphore in build_daemon::serve → the log shows "(6/2 active)",
# so the peak grep yields 6 != 2 and the gate reds; force it serial → peak 1 != 2. (The cap
# logic is also covered hermetically + deterministically by the build_daemon budget unit
# test, run in the check-engine cargo-test tier.)
HEAVY_GATES += daemon-budget
daemon-budget:
	@echo ">> daemon-budget: the shared build daemon caps concurrent builds at its global budget across independent submitters (the machine-wide limiter)"
	@set -euo pipefail; \
	tb=`ls "$(CURDIR)"/.td-build-cache/stage0/store/*/bin/td-builder 2>/dev/null | head -1 || true`; \
	if [ -z "$$tb" ] || [ ! -x "$$tb" ]; then ( cd builder && cargo build --release --quiet ) && tb="$(CURDIR)/builder/target/release/td-builder"; fi; \
	test -x "$$tb" || { echo "FAIL: no td-builder binary for the gate" >&2; exit 1; }; \
	scratch="$(CURDIR)/.daemon-budget-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch/d"; \
	sock="$$scratch/sock"; budget=2; \
	TD_BUILD_JOBS=$$budget TD_DAEMON_TEST_SLEEP_MS=400 "$$tb" daemon "$$sock" "$$scratch/unused-store-db" "$$scratch/d" > "$$scratch/daemon.log" 2>&1 & dpid=$$!; \
	trap 'kill $$dpid 2>/dev/null || true; rm -rf "$$scratch"' EXIT; \
	t=0; while [ ! -S "$$sock" ] && [ $$t -lt 50 ]; do sleep 0.2; t=$$((t+1)); done; \
	[ -S "$$sock" ] || { echo "FAIL: daemon socket never appeared" >&2; cat "$$scratch/daemon.log" >&2; exit 1; }; \
	grep -q "budget $$budget concurrent builds" "$$scratch/daemon.log" || { echo "FAIL: daemon did not honor TD_BUILD_JOBS=$$budget" >&2; cat "$$scratch/daemon.log" >&2; exit 1; }; \
	pids=""; for i in 1 2 3 4 5 6; do "$$tb" daemon-request "$$sock" "$$scratch/nonexistent-$$i.drv" >/dev/null 2>&1 & pids="$$pids $$!"; done; \
	for p in $$pids; do wait "$$p" || true; done; \
	peak=`grep -oE 'START \([0-9]+/'"$$budget"' active\)' "$$scratch/daemon.log" | grep -oE '\([0-9]+' | tr -d '(' | sort -n | tail -1 || true`; \
	starts=`grep -c 'daemon build START' "$$scratch/daemon.log" || true`; \
	test -n "$$peak" || { echo "FAIL: no matching build START lines (budget mislabelled?)" >&2; cat "$$scratch/daemon.log" >&2; exit 1; }; \
	test "$$starts" -ge 3 || { echo "FAIL: only $$starts submissions reached the daemon (expected 6)" >&2; exit 1; }; \
	test "$$peak" -eq "$$budget" || { echo "FAIL: peak concurrency $$peak != budget $$budget — the machine-wide cap did not hold (serial => <$$budget; no cap => >$$budget)" >&2; cat "$$scratch/daemon.log" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] $$starts independent submissions, peak concurrency $$peak == budget $$budget — the cap holds across submitters"; \
	"$$tb" daemon-request "$$sock" SHUTDOWN >/dev/null 2>&1 || true; \
	echo "PASS: daemon-budget — the shared build daemon caps concurrent builds at its global budget ($$budget) across independent submitters; N concurrent checks share ONE budget (machine-wide limiter)."
