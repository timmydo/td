# build-hermetic — td's build SANDBOX is SELF-hermetic (own-builder-daemon
# increment 2). A derivation realized by `td-builder realize` cannot reach the host
# filesystem the outer loop-sandbox exposes: the probe drv's builder ERRORS if
# /var/guix (the guix daemon db/socket/gc-roots, bound read-write into the loop
# container, never wanted in a build) is reachable, so realize succeeds ONLY because
# sandbox::build pivot_roots into a minimal root. DURABLE/behavioral — no guix
# oracle leg: the assertion holds with no daemon in the room (it asserts the daemon
# state is ABSENT from the build). Verified-red: drop the pivot_root in
# sandbox::build and realize fails (the build sees /var/guix).
HEAVY_GATES += build-hermetic
build-hermetic:
	@echo ">> build-hermetic: a td-realized build cannot see /var/guix (the daemon state the loop exposes) — sandbox::build pivot_roots into a minimal root"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: no td-builder" >&2; exit 1; }; \
	scratch="$(CURDIR)/.build-hermetic-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	$(GUIX) repl $(LOAD) tests/build-hermetic-drv.scm 2>"$$scratch/repl.err" > "$$scratch/facts.txt" \
	  || { echo "FAIL: could not emit/realize the probe drv" >&2; cat "$$scratch/repl.err" >&2; exit 1; }; \
	drv=`sed -n 's/^PROBE_DRV=//p' "$$scratch/facts.txt"`; \
	out=`sed -n 's/^PROBE_OUT=//p' "$$scratch/facts.txt"`; \
	test -n "$$drv" -a -n "$$out" || { echo "FAIL: missing probe facts" >&2; cat "$$scratch/facts.txt" >&2; exit 1; }; \
	"$$tb" drv-emit-to "$$drv" "$$scratch/emitted.drv" >/dev/null || { echo "FAIL: drv-emit-to" >&2; exit 1; }; \
	if "$$tb" realize "$$scratch/emitted.drv" /var/guix/db/db.sqlite "$$scratch/b" > "$$scratch/out.txt" 2> "$$scratch/realize.err"; then :; \
	else echo "FAIL: realize errored — the probe builder saw /var/guix (build-sandbox hermeticity regression: sandbox::build did not pivot the host fs away)" >&2; tail -8 "$$scratch/realize.err" >&2; exit 1; fi; \
	grep -qx "path $$out" "$$scratch/b/registration" || { echo "FAIL: probe output $$out not registered" >&2; cat "$$scratch/b/registration" >&2; exit 1; }; \
	echo ">> [DURABLE: behavioral] td realized the probe with NO /var/guix reachable in the build sandbox (no guix oracle — the assertion is that the daemon state is absent from the build)"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo "PASS: td's build sandbox is self-hermetic — a realized build cannot reach /var/guix (or the rest of the invoking filesystem); sandbox::build pivot_roots into a minimal root."
