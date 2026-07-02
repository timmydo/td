# store-verify (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td
# VERIFIES a store's integrity ITSELF — the daemon's `guix gc --verify --check-contents`,
# in pure Rust, no daemon. `td-builder store-verify DB STORE-ROOT` reads the recorded
# registration from a td store DB (`store_db_read`, #36) and re-NAR-hashes each registered
# path at STORE-ROOT/<basename>, flagging (exit 1) any path whose content no longer matches
# its recorded `hash`.
# R3 (guix-retirement ladder → #261): the SUBJECT is now td-BUILT (tests/store-subject.sh —
# hello via build-recipe, cache-hit) staged into a td-OWNED store and its closure
# CONTENT-SCANNED, so this gate runs with guix OFF PATH — no `guix build`, no `guix gc`, no
# /var/guix read. The removable DAEMON DIFFERENTIAL (leg A used to prove td.db records the
# live /var/guix/db hashes and then verify /gnu/store against them) is DROPPED per CLAUDE.md
# directive 3 (called out in the PR); in its place the CORRUPTION-DETECTION feature now runs
# against the REAL td-built closure, a stronger test than the old synthetic-only probe. Three
# legs: (A) td-verify PASSES over the intact td-built closure it registered; (B) a one-byte
# corruption of a closure member is DETECTED (verify exits nonzero); (C) an independent flat
# probe added by `store-add-text` verifies OK, then a one-byte corruption of it is DETECTED.
# Boundary: td READS + writes only its OWN scratch store/DB/probe — host infra stays
# immutable. Needs td-builder + the corpus build → heavy pool + the build-recipes prelude.
HEAVY_GATES += store-verify
BUILD_GATES += store-verify
store-verify:
	@echo ">> store-verify: td VERIFIES store integrity of a TD-BUILT closure (re-hash vs the recorded registration) + DETECTS a one-byte corruption — the daemon's guix gc --verify --check-contents, pure Rust, no daemon (guix off PATH)"
	@set -euo pipefail; \
	. tests/store-subject.sh; \
	scratch="$(CURDIR)/.store-verify-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch/pstore"; \
	td_store_subject "$$scratch" || exit 1; \
	n="$$SUBJ_N"; \
	"$$TB" store-register "$$SUBJ_ROOT" "$$SUBJ_DRV" "$$SUBJ_CLOSURE" "$$scratch/td.db" >/dev/null; \
	"$$TB" store-verify "$$scratch/td.db" "$$SUBJ_STORE" || { echo "FAIL: td-verify flagged the intact td-built closure" >&2; exit 1; }; \
	echo "   (A) td-verify: hello's intact $$n-path closure in the td-owned store matches its recorded hashes (--check-contents)"; \
	victim=`find "$$SUBJ_STORE" -type f | head -1 || true`; \
	test -n "$$victim" || { echo "FAIL: no regular file in the staged closure to corrupt" >&2; exit 1; }; \
	chmod u+w "$$victim"; printf 'X' >> "$$victim"; \
	if "$$TB" store-verify "$$scratch/td.db" "$$SUBJ_STORE" >/dev/null 2>&1; then echo "FAIL: td-verify did NOT detect the corrupted closure member ($$victim)" >&2; exit 1; fi; \
	echo "   (B) td-verify: a one-byte corruption of a REAL closure member is DETECTED (verify exits nonzero)"; \
	printf 'td store-verify probe payload\n' > "$$scratch/content"; \
	"$$TB" store-add-text verify-probe "$$scratch/content" "$$scratch/pstore" "$$scratch/probe.db" >/dev/null; \
	"$$TB" store-verify "$$scratch/probe.db" "$$scratch/pstore" || { echo "FAIL: td-verify flagged an intact probe" >&2; exit 1; }; \
	echo "   (C) td-verify: an intact td-authored probe (store-add-text) verifies OK"; \
	pbase=`basename "$$("$$TB" store-query "$$scratch/probe.db" info | cut -d'|' -f1)"`; \
	chmod u+w "$$scratch/pstore/$$pbase"; printf 'X' >> "$$scratch/pstore/$$pbase"; \
	if "$$TB" store-verify "$$scratch/probe.db" "$$scratch/pstore" >/dev/null 2>&1; then echo "FAIL: td-verify did NOT detect the corrupted probe" >&2; exit 1; fi; \
	echo "   (C) td-verify: a one-byte corruption of the probe is DETECTED (verify exits nonzero)"; \
	rm -rf "$$scratch"; \
	echo "PASS: td VERIFIED store integrity ITSELF, in pure Rust with NO daemon — the daemon's guix gc --verify --check-contents. Over a TD-BUILT hello's $$n-path closure staged into a td-owned store (guix off PATH; no guix build, no guix gc, no /var/guix read): (A) td-verify re-NAR-hashed each registered path and confirmed it matches td's recorded hash; (B) a one-byte corruption of a real closure member is DETECTED (exit nonzero); (C) an independent flat probe (store-add-text) verifies OK and its corruption is DETECTED. The removable daemon differential (leg A's /var/guix/db hash comparison) was dropped (directive 3); corruption detection now runs against the real td-built subject. Boundary: td reads + writes only its own scratch store. The destructive GC sweep is store-gc-sweep."
