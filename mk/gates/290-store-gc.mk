# store-gc (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td computes
# the GC-reachable CLOSURE of a path — the daemon's THIRD role (GC) — TWO daemon-free
# ways, in pure Rust: from a store DB, and by CONTENT-SCANNING the live store.
#   (1) DB walk: `td-builder store-closure DB ROOT` reads the DB with td's own SQLite
#       reader (`store_db_read`) and walks the `Refs` graph from ROOT (the GC "mark" set).
#   (2) Live-store scan: `td-builder store-closure-scan /gnu/store ROOT` re-derives the
#       SAME set by NAR-scanning ROOT's bytes for store-path references, transitively
#       (the daemon's scanForReferences), with NO store DB and NO /var/guix read at all —
#       the closure query the loop's store-native/bootstrap gates use to resolve a path's
#       runtime closure over the live /gnu/store without the guix daemon socket or its DB.
# The differential (daemon = oracle, prime directive 4): td WRITES hello's full-closure
# store DB (`store-register`), then BOTH the DB walk AND the live-store scan from hello's
# output equal `guix gc -R` (the daemon's own closure computation) exactly. A missing
# Refs edge or a missed byte reference would leave paths unreached (verified-red).
# BOUNDARY: the scan of an OUTPUT root is the RUNTIME closure (`.drv`-free) — content-
# scanning is valid for output roots but NOT for `.drv` roots, whose references are the
# structural derivation-input graph (input drvs/srcs), not the output paths embedded in
# the `.drv` bytes; that drv-graph query stays on the DB (e.g. ci/build-ci-image.sh). The
# destructive SWEEP (deletion) is NOT done here — only the boundary-safe liveness/mark
# phase, over td's OWN scratch DB; host infra stays immutable. Needs td-builder built, so
# it slots in the heavy pool.
HEAVY_GATES += store-gc
store-gc:
	@echo ">> store-gc: td computes the GC-reachable closure of hello from its OWN store DB (pure Rust, no daemon) == guix gc -R"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	out=`guix build hello`; drv=`guix build -d hello`; \
	test -n "$$out" -a -n "$$drv" || { echo "ERROR: could not realise hello" >&2; exit 1; }; \
	scratch="$(CURDIR)/.store-gc-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	guix gc -R "$$out" | sort -u > "$$scratch/closure.txt"; \
	"$$tb" store-register "$$out" "$$drv" "$$scratch/closure.txt" "$$scratch/td.db"; \
	td_reach=`"$$tb" store-closure "$$scratch/td.db" "$$out"`; \
	oracle=`sort -u "$$scratch/closure.txt"`; \
	test -n "$$oracle" || { echo "FAIL: guix gc -R returned nothing for $$out (oracle vacuous)" >&2; exit 1; }; \
	test "$$td_reach" = "$$oracle" || { echo "FAIL: td's DB-walk GC-reachable closure != guix gc -R" >&2; echo "$$td_reach" | sed 's/^/  td:   /' >&2; echo "$$oracle" | sed 's/^/  guix: /' >&2; exit 1; }; \
	n=`echo "$$td_reach" | wc -l`; \
	echo "   (1) td reached all $$n closure paths from $$out over its OWN Refs graph in the DB (== guix gc -R)"; \
	scan_reach=`"$$tb" store-closure-scan /gnu/store "$$out" | sort -u`; \
	test "$$scan_reach" = "$$oracle" || { echo "FAIL: td's CONTENT-SCAN closure over the live /gnu/store != guix gc -R" >&2; echo "$$scan_reach" | sed 's/^/  scan: /' >&2; echo "$$oracle" | sed 's/^/  guix: /' >&2; exit 1; }; \
	echo "   (2) td reconstructed the SAME $$n-path runtime closure by CONTENT-SCANNING the live /gnu/store from $$out (store-closure-scan — NO store DB, NO /var/guix read, no daemon) == guix gc -R"; \
	if echo "$$scan_reach" | grep -q '\.drv$$'; then echo "FAIL: the content-scan runtime closure of an OUTPUT root unexpectedly contains a .drv — the output-root boundary is broken" >&2; exit 1; fi; \
	echo "   (2b) the content-scan runtime closure is .drv-free (an OUTPUT root's runtime closure, distinct from the structural .drv-input graph — the boundary of what content-scan may replace)"; \
	rm -rf "$$scratch"; \
	echo "PASS: td computed the GC-reachable CLOSURE of hello ($$n paths) TWO daemon-free ways, in pure Rust — (1) walking the Refs graph in its OWN store DB (td's own SQLite reader) and (2) CONTENT-SCANNING the live /gnu/store from hello's output (store-closure-scan, no DB and no /var/guix read) — and BOTH equal guix gc -R EXACTLY (the daemon's own closure computation, the oracle). The scan of an output root is the .drv-free RUNTIME closure (the boundary: the structural .drv-input graph is not content-scannable and stays on the DB). td's Refs traversal AND its byte-level scan each reconstruct the daemon's GC liveness/mark set; the destructive sweep is not done (boundary). A td store backend + the destructive GC sweep are later increments."
