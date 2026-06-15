# store-add-referenced (DESIGN §7.1; td-store-db track — begin replacing guix-daemon).
# td ADDS a path WITH references to its OWN store — the daemon's addToStore/addTextToStore
# with a references set (after the no-reference flat #38 + recursive #41 adds), in pure
# Rust, no daemon. `td-builder store-add-referenced` computes the content-addressed path
# with the references FOLDED INTO THE TYPE (`make_text_path`: `text:<sorted refs>` — the
# daemon's makeTextPath/makeType), WRITES the content into a td-owned store (canonical 0444
# file), and REGISTERS the path with its `Refs` to the referenced paths. The canonical
# referenced content-addressed item is a `.drv` (referenced by its input drvs/srcs). The
# differential (daemon = oracle, prime directive 4): for hello's `.drv` and its references
# (`guix gc --references`), td computes the IDENTICAL store path (proving the references are
# folded into the path correctly — drop one and the path diverges), writes a `.drv`
# byte-identical (by NAR hash) to the daemon's own, and registers EXACTLY the daemon's
# recorded references (read back by td's own `store-query references`). Boundary: td writes
# only its OWN scratch store/DB and READS the daemon's `.drv`; the host store is untouched.
# Needs td-builder built, so it slots in the heavy pool.
HEAVY_GATES += store-add-referenced
store-add-referenced:
	@echo ">> store-add-referenced: td ADDS a path WITH references (hello's .drv) to its OWN store + registers the references (pure Rust, no daemon) — differential vs the daemon"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	drv=`guix build -d hello`; \
	test -n "$$drv" -a -f "$$drv" || { echo "ERROR: could not realise hello's .drv" >&2; exit 1; }; \
	name=`basename "$$drv"`; name=$${name:33}; \
	scratch="$(CURDIR)/.store-add-referenced-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch/store"; \
	guix gc --references "$$drv" | sort > "$$scratch/refs.txt"; \
	nref=`wc -l < "$$scratch/refs.txt"`; \
	test "$$nref" -gt 0 || { echo "FAIL: the .drv has no references (the differential would be vacuous)" >&2; exit 1; }; \
	echo ">> hello's .drv ($$name) has $$nref references (its input drvs/srcs) — the daemon's record"; \
	td_path=`"$$tb" store-add-referenced "$$name" "$$drv" "$$scratch/refs.txt" "$$scratch/store" "$$scratch/td.db"`; \
	test "$$td_path" = "$$drv" || { echo "FAIL: td computed $$td_path != the daemon's $$drv (references not folded into the path correctly)" >&2; exit 1; }; \
	echo "   td computed the IDENTICAL content-addressed path WITH the $$nref references folded into the type"; \
	base=`basename "$$td_path"`; \
	test -f "$$scratch/store/$$base" || { echo "FAIL: td did not write the .drv into its store" >&2; exit 1; }; \
	td_nar=`"$$tb" nar-hash "$$scratch/store/$$base"`; oracle_nar=`"$$tb" nar-hash "$$drv"`; \
	test "$$td_nar" = "$$oracle_nar" || { echo "FAIL: td's stored .drv NAR $$td_nar != the daemon's $$oracle_nar" >&2; exit 1; }; \
	echo "   td's stored .drv is byte-identical (NAR) to the daemon's own: $$oracle_nar"; \
	td_refs=`"$$tb" store-query "$$scratch/td.db" references | sed 's#^[^|]*|##' | sort`; \
	oracle_refs=`cat "$$scratch/refs.txt"`; \
	test "$$td_refs" = "$$oracle_refs" || { echo "FAIL: td's registered references (read by td's own reader) != the daemon's guix gc --references" >&2; echo "$$td_refs" | sed 's/^/  td:     /' >&2; echo "$$oracle_refs" | sed 's/^/  daemon: /' >&2; exit 1; }; \
	echo "   td REGISTERED all $$nref references (read back by TD'S OWN reader) == guix gc --references (the daemon's record)"; \
	rm -rf "$$scratch"; \
	echo "PASS: td ADDED a path WITH references to its OWN store, in pure Rust with NO daemon — for hello's .drv and its $$nref references, td computed the IDENTICAL content-addressed path (the references folded into the type, makeTextPath — drop one and it diverges), WROTE a .drv byte-identical (by NAR hash) to the daemon's own, and REGISTERED exactly the daemon's recorded references (read back by td's own store-query). The daemon is only the oracle. td now owns addToStore for flat (#38), recursive (#41), AND referenced paths. A td store backend is a later increment."
