# store-backend (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). A td
# STORE BACKEND for a BUILD OUTPUT — the capstone that composes the store stack into a
# working, daemon-free backend. `td-builder store-add-output` PLACES a built output's tree
# into a td-owned store at its output path and FULLY REGISTERS it (hash + narSize +
# deriver + the output's references + the drv->output mapping — the daemon's post-build
# registration), and then td's OWN tools SERVE it: store-query (the registration + the
# references), store-verify (integrity re-hashed against the PLACED files), all with NO
# daemon in any store operation. The differential (daemon = oracle, prime directive 4):
# for hello's output, (1) the placed tree is NAR-identical to the daemon's built output,
# (2) store-query info == the daemon's recorded hash + narSize, (3) store-query references
# == `guix gc --references`, (4) store-verify passes against td's own placed files, and
# (5) the deriver + drv->output mapping == the daemon's. Boundary: td writes only its OWN
# scratch store/DB and READS the daemon's built output + the immutable live DB; the host
# /gnu/store is untouched. Needs td-builder built, so it slots in the heavy pool.
HEAVY_GATES += store-backend
store-backend:
	@echo ">> store-backend: a td store backend HOLDS + SERVES a real build output (place + register + query + verify, pure Rust, no daemon)"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	out=`guix build hello`; drv=`guix build -d hello`; \
	test -n "$$out" -a -n "$$drv" || { echo "ERROR: could not realise hello" >&2; exit 1; }; \
	scratch="$(CURDIR)/.store-backend-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch/store"; \
	guix gc -R "$$out" | sort -u > "$$scratch/closure.txt"; \
	"$$tb" store-add-output "$$out" "$$drv" "$$scratch/closure.txt" "$$scratch/store" "$$scratch/td.db" >/dev/null; \
	base=`basename "$$out"`; \
	test -d "$$scratch/store/$$base" || { echo "FAIL: td did not place the output tree into its store" >&2; exit 1; }; \
	td_nar=`"$$tb" nar-hash "$$scratch/store/$$base"`; oracle_nar=`"$$tb" nar-hash "$$out"`; \
	test "$$td_nar" = "$$oracle_nar" || { echo "FAIL: the placed output NAR $$td_nar != the daemon's built output $$oracle_nar" >&2; exit 1; }; \
	echo "   (1) td PLACED hello's output into its store, NAR-identical to the daemon's built output: $$oracle_nar"; \
	live="file:/var/guix/db/db.sqlite?immutable=1"; \
	td_info=`"$$tb" store-query "$$scratch/td.db" info`; \
	oracle_row=`sqlite3 "$$live" "SELECT hash||'|'||narSize FROM ValidPaths WHERE path='$$out'"`; \
	test -n "$$oracle_row" || { echo "FAIL: the daemon has no record for $$out (oracle vacuous)" >&2; exit 1; }; \
	test "$$td_info" = "$$out|$$oracle_row" || { echo "FAIL: td's store-query info ($$td_info) != the daemon's record ($$out|$$oracle_row)" >&2; exit 1; }; \
	echo "   (2) td's store SERVES the registration (store-query info) == the daemon's recorded hash + narSize"; \
	td_refs=`"$$tb" store-query "$$scratch/td.db" references | sed 's#^[^|]*|##' | sort`; \
	oracle_refs=`guix gc --references "$$out" | sort`; \
	test "$$td_refs" = "$$oracle_refs" || { echo "FAIL: td's served references != the daemon's guix gc --references" >&2; echo "$$td_refs" | sed 's/^/  td:     /' >&2; echo "$$oracle_refs" | sed 's/^/  daemon: /' >&2; exit 1; }; \
	echo "   (3) td's store SERVES the references (store-query references) == guix gc --references"; \
	"$$tb" store-verify "$$scratch/td.db" "$$scratch/store" || { echo "FAIL: store-verify flagged the placed output" >&2; exit 1; }; \
	echo "   (4) td's store VERIFIES (store-verify) the placed output's integrity against its OWN files"; \
	doutsql="SELECT (SELECT deriver FROM ValidPaths WHERE path='$$out')||' :: '||v.path||':'||d.id||':'||d.path FROM DerivationOutputs d JOIN ValidPaths v ON d.drv=v.id WHERE d.path='$$out'"; \
	td_dout=`sqlite3 "$$scratch/td.db" "$$doutsql"`; \
	oracle_dout=`sqlite3 "$$live" "$$doutsql"`; \
	test -n "$$oracle_dout" || { echo "FAIL: the daemon has no deriver/drv->output for $$out (oracle vacuous)" >&2; exit 1; }; \
	test "$$td_dout" = "$$oracle_dout" || { echo "FAIL: td's deriver/drv->output ($$td_dout) != the daemon's ($$oracle_dout)" >&2; exit 1; }; \
	echo "   (5) td's store records the deriver + drv->output mapping == the daemon's"; \
	rm -rf "$$scratch"; \
	echo "PASS: a td STORE BACKEND holds + serves a real build output, in pure Rust with NO daemon in any store operation — td PLACED hello's built output into a td-owned store (NAR-identical to the daemon's), FULLY REGISTERED it (hash + narSize + deriver + references + drv->output), and td's OWN tools SERVE it: store-query returns the registration + references == the daemon's, and store-verify re-hashes the PLACED files and confirms integrity. The daemon is only the oracle (it built the output + holds the record). td now owns the full store backend — write/read the DB, add (flat/recursive/referenced), GC (mark + sweep), verify, AND back a build output end to end."
