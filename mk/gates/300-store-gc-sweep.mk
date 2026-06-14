# store-gc-sweep (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). The
# DESTRUCTIVE GC SWEEP — the other half of GC, after the mark/liveness `store-closure`
# (#39), in pure Rust, no daemon. `td-builder store-gc-sweep STORE-DIR DB ROOT` computes
# the live set (closure of ROOT over the Refs), DELETES every registered content path NOT
# reachable from ROOT from the td-owned STORE-DIR, and rewrites the DB to the live set
# (ValidPaths + Refs renumbered). The differential (daemon = oracle, prime directive 4):
# a td-owned store is built by copying hello's full closure (cp -a) and registering it
# (`store-register`); after sweeping with ROOT=glibc (whose closure is a PROPER subset),
# the surviving store entries AND the rewritten DB hold EXACTLY `guix gc -R glibc` — the
# daemon's own reachable set — and the dead paths' files are gone. Boundary: the sweep
# deletes ONLY from the td-owned scratch STORE-DIR (a cp -a copy, chmod'd writable so it
# is deletable) and rewrites only the scratch DB — the host /gnu/store is NEVER touched.
# Needs td-builder built, so it slots in the heavy pool.
HEAVY_GATES += store-gc-sweep
store-gc-sweep:
	@echo ">> store-gc-sweep: td DELETES the GC-dead paths from its OWN store + rewrites the DB to the live set (destructive GC sweep, pure Rust, no daemon) == guix gc -R"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	out=`guix build hello`; drv=`guix build -d hello`; \
	test -n "$$out" -a -n "$$drv" || { echo "ERROR: could not realise hello" >&2; exit 1; }; \
	scratch="$(CURDIR)/.store-gc-sweep-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch/store"; \
	guix gc -R "$$out" | sort -u > "$$scratch/closure.txt"; \
	n=`wc -l < "$$scratch/closure.txt"`; \
	while read p; do cp -a "$$p" "$$scratch/store/"; done < "$$scratch/closure.txt"; \
	chmod -R u+w "$$scratch/store"; \
	"$$tb" store-register "$$out" "$$drv" "$$scratch/closure.txt" "$$scratch/td.db" >/dev/null; \
	root=`grep -- '-glibc-' "$$scratch/closure.txt" | head -1`; \
	test -n "$$root" || { echo "FAIL: no glibc in hello's closure to use as a non-trivial GC root" >&2; exit 1; }; \
	live=`guix gc -R "$$root" | sed 's#.*/##' | sort`; \
	nlive=`echo "$$live" | wc -l`; \
	test "$$nlive" -lt "$$n" || { echo "FAIL: glibc's closure is not a PROPER subset of hello's ($$nlive vs $$n) — nothing would be swept" >&2; exit 1; }; \
	echo ">> td store holds hello's $$n-path closure; GC root glibc keeps $$nlive live, $$(($$n-$$nlive)) dead"; \
	"$$tb" store-gc-sweep "$$scratch/store" "$$scratch/td.db" "$$root"; \
	survivors=`ls "$$scratch/store" | sort`; \
	test "$$survivors" = "$$live" || { echo "FAIL: surviving store entries != guix gc -R glibc" >&2; echo "$$survivors" | sed 's/^/  surv: /' >&2; echo "$$live" | sed 's/^/  live: /' >&2; exit 1; }; \
	echo "   td DELETED the $$(($$n-$$nlive)) dead paths; the store now holds EXACTLY guix gc -R glibc's $$nlive live paths"; \
	db_paths=`"$$tb" store-query "$$scratch/td.db" info | sed 's#|.*##;s#.*/##' | sort`; \
	test "$$db_paths" = "$$live" || { echo "FAIL: the swept DB's ValidPaths != the live set" >&2; echo "$$db_paths" | sed 's/^/  db:   /' >&2; echo "$$live" | sed 's/^/  live: /' >&2; exit 1; }; \
	echo "   the rewritten DB records EXACTLY the live set (dead ValidPaths rows removed)"; \
	rm -rf "$$scratch"; \
	echo "PASS: td performed the DESTRUCTIVE GC SWEEP on its OWN store, in pure Rust with NO daemon — after copying hello's $$n-path closure into a td-owned store and registering it, td swept with GC root glibc: it DELETED the dead paths' files and rewrote the DB so BOTH the surviving store entries AND the ValidPaths records hold EXACTLY guix gc -R glibc's $$nlive-path reachable set (the daemon's own GC decision, the oracle). The host /gnu/store is never touched (the sweep operates only on the td-owned cp -a copy). td now owns BOTH halves of GC — mark (#39) and sweep. A td store backend is a later increment."
