# store-gc (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td computes
# the GC-reachable CLOSURE of a path from its OWN store DB — the daemon's THIRD role
# (GC), in pure Rust. `td-builder store-closure DB ROOT` reads the DB with td's own
# SQLite reader (`store_db_read`) and walks the `Refs` graph from ROOT (the GC "mark"
# set), no daemon. The differential (daemon = oracle, prime directive 4): td WRITES
# hello's full-closure store DB (`store-register`, #34/#35), then computes the reachable
# set from hello's output over its OWN scanned `Refs` — and it equals `guix gc -R`
# (the daemon's own closure computation) exactly. This proves td's Refs graph + traversal
# reconstruct the daemon's GC liveness set; a missing edge would leave paths unreached
# (verified-red). The destructive SWEEP (deletion) is NOT done — only the boundary-safe
# liveness/mark phase, over td's OWN scratch DB; host infra stays immutable. Needs
# td-builder built, so it slots in the heavy pool.
HEAVY_GATES += store-gc
store-gc:
	@echo ">> store-gc: td computes the GC-reachable closure of hello from its OWN store DB (pure Rust, no daemon) == guix gc -R"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	out=`guix build hello`; drv=`guix build -d hello`; \
	test -n "$$out" -a -n "$$drv" || { echo "ERROR: could not realise hello" >&2; exit 1; }; \
	scratch="$(CURDIR)/.store-gc-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	guix gc -R "$$out" | sort -u > "$$scratch/closure.txt"; \
	"$$tb" store-register "$$out" "$$drv" "$$scratch/closure.txt" "$$scratch/td.db"; \
	td_reach=`"$$tb" store-closure "$$scratch/td.db" "$$out"`; \
	oracle=`sort -u "$$scratch/closure.txt"`; \
	test -n "$$oracle" || { echo "FAIL: guix gc -R returned nothing for $$out (oracle vacuous)" >&2; exit 1; }; \
	test "$$td_reach" = "$$oracle" || { echo "FAIL: td's GC-reachable closure != guix gc -R" >&2; echo "$$td_reach" | sed 's/^/  td:   /' >&2; echo "$$oracle" | sed 's/^/  guix: /' >&2; exit 1; }; \
	n=`echo "$$td_reach" | wc -l`; \
	echo "   td reached all $$n closure paths from $$out over its OWN Refs graph (== guix gc -R)"; \
	rm -rf "$$scratch"; \
	echo "PASS: td computed the GC-reachable CLOSURE of hello ($$n paths) from its OWN store DB, in pure Rust — reading the DB with td's own SQLite reader and walking the Refs graph from hello's output (no daemon) — and it equals guix gc -R EXACTLY (the daemon's own closure computation, the oracle). td's scanned Refs + traversal reconstruct the daemon's GC liveness/mark set; the destructive sweep is not done (boundary). A td store backend + the destructive GC sweep are later increments."
