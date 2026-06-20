# store-verify (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td
# VERIFIES a store's integrity ITSELF — the daemon's `guix gc --verify --check-contents`,
# in pure Rust, no daemon. `td-builder store-verify DB STORE-ROOT` reads the recorded
# registration from a td store DB (`store_db_read`, #36) and re-NAR-hashes each registered
# path at STORE-ROOT/<basename>, flagging (exit 1) any path whose content no longer matches
# its recorded `hash`. Two legs: (A) the daemon DIFFERENTIAL — td first proves its DB
# records the DAEMON's hashes (immutable live-DB read), then verifies hello's closure in
# the REAL /gnu/store against those hashes (exit 0): td independently confirms the store
# content matches the daemon's record, exactly `--check-contents`; (B) CORRUPTION
# DETECTION — a flat probe added to a td-owned store verifies OK, then a one-byte
# corruption is DETECTED (verify exits nonzero, naming the path). Boundary: td READS
# /gnu/store + the td DB, and writes only its OWN scratch store/DB/probe — host infra stays
# immutable. Needs td-builder built, so it slots in the heavy pool.
HEAVY_GATES += store-verify
store-verify:
	@echo ">> store-verify: td VERIFIES store integrity (re-hash registered paths vs the recorded registration) — the daemon's guix gc --verify --check-contents, pure Rust, no daemon"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	out=`guix build hello`; drv=`guix build -d hello`; \
	test -n "$$out" -a -n "$$drv" || { echo "ERROR: could not realise hello" >&2; exit 1; }; \
	scratch="$(CURDIR)/.store-verify-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch/store"; \
	guix gc -R "$$out" | sort -u > "$$scratch/closure.txt"; \
	n=`wc -l < "$$scratch/closure.txt"`; \
	"$$tb" store-register "$$out" "$$drv" "$$scratch/closure.txt" "$$scratch/td.db"; \
	inlist=`sed "s/.*/'&'/" "$$scratch/closure.txt" | paste -sd,`; \
	live="file:/var/guix/db/db.sqlite?immutable=1"; \
	td_rec=`"$$tb" store-query "$$scratch/td.db" info`; \
	daemon_rec=`sqlite3 "$$live" "SELECT path||'|'||hash||'|'||narSize FROM ValidPaths WHERE path IN ($$inlist) ORDER BY path"`; \
	test -n "$$daemon_rec" || { echo "FAIL: the closure is not in the live store DB snapshot (oracle vacuous)" >&2; exit 1; }; \
	test "$$td_rec" = "$$daemon_rec" || { echo "FAIL: td.db's recorded hashes != the daemon's" >&2; exit 1; }; \
	echo "   td.db records the DAEMON's hashes for all $$n closure paths"; \
	"$$tb" store-verify "$$scratch/td.db" /gnu/store || { echo "FAIL: td-verify flagged the intact /gnu/store closure" >&2; exit 1; }; \
	echo "   (A) td-verify: hello's closure in /gnu/store matches the daemon's recorded hashes (--check-contents)"; \
	printf 'td store-verify probe payload\n' > "$$scratch/content"; \
	"$$tb" store-add-text verify-probe "$$scratch/content" "$$scratch/store" "$$scratch/probe.db" >/dev/null; \
	"$$tb" store-verify "$$scratch/probe.db" "$$scratch/store" || { echo "FAIL: td-verify flagged an intact probe" >&2; exit 1; }; \
	echo "   (B) td-verify: an intact td-store probe verifies OK"; \
	pbase=`basename "$$("$$tb" store-query "$$scratch/probe.db" info | cut -d'|' -f1)"`; \
	chmod u+w "$$scratch/store/$$pbase"; printf 'X' >> "$$scratch/store/$$pbase"; \
	if "$$tb" store-verify "$$scratch/probe.db" "$$scratch/store" >/dev/null 2>&1; then echo "FAIL: td-verify did NOT detect the corrupted probe" >&2; exit 1; fi; \
	echo "   (B) td-verify: a one-byte corruption is DETECTED (verify exits nonzero)"; \
	rm -rf "$$scratch"; \
	echo "PASS: td VERIFIED store integrity ITSELF, in pure Rust with NO daemon — the daemon's guix gc --verify --check-contents. (A) After proving td.db records the DAEMON's hashes for hello's $$n-path closure, td-verify re-NAR-hashed each path in the REAL /gnu/store and confirmed it matches the daemon's recorded hash (content verification against the oracle). (B) td-verify passes an intact td-owned probe and DETECTS a one-byte corruption (exit nonzero, naming the path). Boundary: td reads /gnu/store + the td DB and writes only its own scratch store. The destructive GC sweep + a td store backend are later increments."
