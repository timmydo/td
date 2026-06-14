# check-memo discipline gate (DESIGN §7.1 side-track; plan/check-memo.md — the
# §4.3 gate-2 charter with the BINDING constraints 1-6). Permanent,
# self-discriminating exercise of the verdict-memoization helper
# (tests/check-memo.sh) on TINY fixture drvs, so the charter's constraints are
# asserted EVERY loop, not only in one-off verified-red runs:
#   • wiring: TD_CHECK_ENV must be EXPORTED into the sandbox by check.sh
#     (possibly EMPTY — empty IS the CI gate, constraint 2). The helper is
#     then driven with SYNTHETIC identities + a scratch verdict dir + PINNED
#     knobs (every leg sets TD_CHECK_FULL/TD_CHECK_TTL_DAYS itself), so this
#     gate behaves identically on dev hosts, on CI, and under an ambient
#     force-full ladder run (TD_CHECK_FULL=1 ./check.sh — caught at S3: an
#     inherited knob turned the hit leg into a forced miss and red the gate),
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
# Cheap-side heavy gate (a handful of trivial local builds + repl calls) →
# listed last (LPT). Scratch lives in $(CURDIR)/.memo-scratch — kept on red
# for triage, removed on green.
HEAVY_GATES += memo
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
