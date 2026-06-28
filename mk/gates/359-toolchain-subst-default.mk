# toolchain-subst-default — the loop FETCHES the lock-keyed /td/store toolchain by DEFAULT
# (tools/resolve-toolchain.sh) instead of rebuilding the ~18-rung from-seed chain (~90 min).
# "Loop substitutes too" (human, 2026-06-28). Builds on the stable input-addressed key (#204)
# and the lock-keyed publish->fetch leg (#207); the new bits are a PERSISTENT signed
# substitute store keyed by tests/td-toolchain.lock + the consumer-DEFAULT resolver a real
# bootstrap gate sources (fetch-by-default, FALL BACK to from-seed on any miss).
#
# DELIBERATE directive-1 relaxation (human-approved, surfaced in the gate body + the PR): with
# the resolver the per-PR/local loop no longer builds the toolchain from source; the DAILY
# full suite (ci/daily-full-suite.sh, fresh main) is the SOLE remaining from-seed authoritative
# build AND the publisher of the signed substitute. Trust = ed25519 signature (pinned key) +
# the input-addressed NAME, NOT repro-equality (the toolchain is not byte-reproducible — task 3).
# Durable: DEFAULT-FETCH (a path obtained without building it, runs), FALL-BACK (cold store ->
# from-seed), self-discrimination (wrong pinned key -> reject), structural (the pinned anchor is
# well-formed). A BUILD_GATE like td-subst: builds td-subst from source, ordered after the
# build-recipes phase. See plan/toolchain-subst-default.md.
HEAVY_GATES += toolchain-subst-default
BUILD_GATES += toolchain-subst-default
toolchain-subst-default:
	@echo ">> toolchain-subst-default: the loop FETCHES the lock-keyed /td/store toolchain by DEFAULT (resolve-toolchain.sh) — sig+StorePath+NarHash verified, runs the fetched-not-built artifact, FALLS BACK to from-seed on a cold store / wrong key (deliberate directive-1 relaxation: the daily suite is the sole from-seed authoritative build + publisher)"
	sh tests/toolchain-subst-default.sh
