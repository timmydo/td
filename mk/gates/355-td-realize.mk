# td-realize — td REALIZES a derivation with NO guix-daemon in the path (DESIGN §7.1
# move-off-Guile §5; first own-builder-daemon increment). Where td-drv-build (235)
# still staged the input closure with `guix gc -R` (the daemon), `td-builder realize`
# computes that closure ITSELF — its own SQLite reader over the store db's Refs graph
# — then builds in its userns sandbox and registers the output. Subject: the td-build
# hello drv. Legs: DURABLE — td computed the closure itself, and the realized hello
# runs; MIGRATION ORACLE (removable when guix retires) — the output (path/NAR/size/
# deriver) is byte-identical to the daemon's build of the same drv.
HEAVY_GATES += td-realize
td-realize:
	@echo ">> td-realize: td realizes the hello drv with no guix-daemon — computes the input closure itself (its SQLite reader, no guix gc), builds in its userns sandbox, registers; output matches the daemon (oracle)"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: no td-builder" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-realize-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	$(GUIX) repl $(LOAD) tests/td-drv-build-drv.scm 2>/dev/null > "$$scratch/facts.txt"; \
	drv=`sed -n 's/^HELLO_DRV=//p' "$$scratch/facts.txt"`; \
	out=`sed -n 's/^HELLO_OUT=//p' "$$scratch/facts.txt"`; \
	hash=`sed -n 's/^HELLO_HASH=//p' "$$scratch/facts.txt"`; \
	narsize=`sed -n 's/^HELLO_NARSIZE=//p' "$$scratch/facts.txt"`; \
	deriver=`sed -n 's/^HELLO_DERIVER=//p' "$$scratch/facts.txt"`; \
	test -n "$$drv" -a -n "$$out" -a -n "$$hash" -a -n "$$narsize" -a -n "$$deriver" || { echo "ERROR: missing oracle facts" >&2; exit 1; }; \
	"$$tb" drv-emit-to "$$drv" "$$scratch/emitted.drv" >/dev/null || { echo "FAIL: drv-emit-to" >&2; exit 1; }; \
	"$$tb" realize "$$scratch/emitted.drv" /var/guix/db/db.sqlite "$$scratch/b" > "$$scratch/out.txt" 2> "$$scratch/realize.err" || { echo "FAIL: realize errored" >&2; cat "$$scratch/realize.err" >&2; exit 1; }; \
	sed 's/^/   /' "$$scratch/realize.err"; \
	cl=`grep -c . "$$scratch/b/closure.txt"`; test "$$cl" -gt 0 || { echo "FAIL: td computed an empty closure" >&2; exit 1; }; \
	echo ">> [DURABLE] td computed the input closure itself ($$cl paths, no guix gc / no daemon)"; \
	say=`"$$out/bin/hello"`; test "$$say" = "Hello, world!" || { echo "FAIL: realized hello did not greet (got '$$say')" >&2; exit 1; }; \
	echo ">> [DURABLE: behavioral] the realized hello runs: $$say"; \
	reg="$$scratch/b/registration"; \
	grep -qx "path $$out" "$$reg" || { echo "FAIL: path mismatch vs daemon $$out" >&2; cat "$$reg" >&2; exit 1; }; \
	grep -qx "nar-hash sha256:$$hash" "$$reg" || { echo "FAIL: NAR-hash mismatch vs daemon" >&2; exit 1; }; \
	grep -qx "nar-size $$narsize" "$$reg" || { echo "FAIL: NAR-size mismatch vs daemon" >&2; exit 1; }; \
	grep -qx "deriver $$deriver" "$$reg" || { echo "FAIL: deriver mismatch vs daemon" >&2; exit 1; }; \
	echo ">> [MIGRATION ORACLE — removable when guix retires] realize output == the daemon's build of the same drv (path/NAR/size/deriver)"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo "PASS: td-builder REALIZED the hello drv with NO guix-daemon in the path — it computed the $$cl-path input closure itself (its own SQLite reader over the store db's Refs graph, not guix gc), built in its userns sandbox, and registered the output; the realized hello runs (durable), and (oracle) the output is byte-identical to the daemon's build of the same drv."
