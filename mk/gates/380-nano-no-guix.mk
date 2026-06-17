# nano-no-guix — GNU nano builds with td's OWN tooling and NO guix/Guile in its build
# path (the move-off-Guile capstone, DESIGN §5). PREP (guix, the SEED — toolchain +
# deps + source + td's own tools, retired last): realize the pinned lock's inputs and
# resolve td-builder/ts-eval/tsc/node. BUILD (td only, with guix/Guile SCRUBBED FROM
# PATH): ts-eval (boa) lowers recipe-nano.ts -> JSON; `td-builder build-recipe` resolves
# every input from the pinned lock (no specification->package), assembles the .drv
# itself (no guix (derivation …)), and realizes it (no guix-daemon). The loop then runs
# nano FROM td's OWN store output.
#   STRUCTURAL: the build ran with guix/Guile absent from PATH — proof the path needs neither;
#   DURABLE: nano runs (--version 8.7.1) from td's own output;
#   MIGRATION ORACLE (removable when guix retires): guix's nano runs the same version
#     (td's nano is at a DISTINCT path — own, then diverge, since inputs are sources).
HEAVY_GATES += nano-no-guix
nano-no-guix:
	@echo ">> nano-no-guix: GNU nano builds with td's own tooling, NO guix/Guile in its build path — ts-eval -> build-recipe (resolve from pinned lock, assemble .drv, realize), guix/Guile scrubbed from PATH; runs from td's own output"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -x "$$tb" -a -x "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve td-builder / node / tsc / ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	scratch="$(CURDIR)/.nano-no-guix-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch/tmp"; \
	echo ">> PREP (guix realizes the SEED: toolchain + deps + source from the pinned lock, retired last §5)"; \
	grep ' /gnu/store/' "$(CURDIR)/tests/nano-no-guix.lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed (lock inputs); regenerate the lock on a channel bump" >&2; exit 1; }; \
	echo ">> ts-eval (boa) lowers recipe-nano.ts -> JSON (no Guile):"; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-nano.ts" > "$$scratch/nano.json"; \
	test -s "$$scratch/nano.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	echo ">> [STRUCTURAL] BUILD with guix/Guile SCRUBBED FROM PATH — proof nano's build path needs neither"; \
	cu=`sed -n 's/^coreutils //p' "$(CURDIR)/tests/nano-no-guix.lock"`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile present on the scrubbed PATH" >&2; exit 1; fi; \
	out=`env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" "$$tb" build-recipe "$$scratch/nano.json" "$(CURDIR)/tests/nano-no-guix.lock" "$$scratch/b" /var/guix/db/db.sqlite 2>"$$scratch/err" | sed -n 's/^OUT=out //p'` || { echo "FAIL: td-builder build-recipe (guix/Guile off PATH) failed:" >&2; tail -20 "$$scratch/err" >&2; exit 1; }; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output path" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	sed 's/^/   /' "$$scratch/err" | grep -E 'closure ITSELF|registered|assembled' || true; \
	echo "   td assembled + realized nano with NO guix/Guile on PATH: $$out"; \
	echo ">> [DURABLE: behavioral] run nano from td's OWN store output:"; \
	nsout="$$scratch/b/newstore/`basename "$$out"`"; \
	ver=`"$$nsout/bin/nano" --version | head -n1`; \
	echo "   $$ver"; \
	printf '%s' "$$ver" | grep -q "version 8.7.1" || { echo "FAIL: td-built nano did not report version 8.7.1 (got '$$ver')" >&2; exit 1; }; \
	echo ">> [MIGRATION ORACLE — removable when guix retires] guix's nano runs the same version, at a DISTINCT path (own, then diverge)"; \
	gnano=`$(GUIX) build nano | grep -- '-nano-' | grep -v -- '-debug' | head -n1`; \
	test -n "$$gnano" || { echo "ERROR: could not build the guix nano oracle" >&2; exit 1; }; \
	gver=`"$$gnano/bin/nano" --version | head -n1`; \
	test "$$out" != "$$gnano" || { echo "FAIL: td's nano path equals guix's — expected a distinct own-builder path" >&2; exit 1; }; \
	printf '%s' "$$gver" | grep -q "version 8.7.1" || { echo "FAIL: guix nano version mismatch (got '$$gver')" >&2; exit 1; }; \
	echo "   guix nano: $$gnano ($$gver) — distinct path, same version"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo "PASS: GNU nano built with td's OWN tooling and NO guix/Guile in its build path — ts-eval (boa) lowered recipe-nano.ts to JSON, td-builder build-recipe resolved every input from the pinned lock (no specification->package), assembled the .drv itself (no guix (derivation …)) and realized it (no guix-daemon), with guix/Guile SCRUBBED FROM PATH; nano runs from td's own store output (8.7.1, durable), at a distinct path from guix's nano (own, then diverge; same version, oracle). The toolchain + lock are the guix-built seed (§5, retired last)."
