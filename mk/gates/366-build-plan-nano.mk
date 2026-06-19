# build-plan-nano — td chains TWO td-built deps into a downstream build: nano links
# the ncurses + gettext-minimal td just built, not guix's (move-off-Guile §5). The
# wired edges come from tests/td-chained-edges.txt (the `nano` line), the same source
# the guix-dependence census reads to credit nano EDGE-OWNED. DURABLE: nano's .drv
# references td's ncurses + gettext-minimal AND NOT guix's (substitution load-bearing);
# nano runs (`nano --version` = 8.7.1) loading td's ncurses; `td-builder check`
# double-builds it reproducibly. ORACLE: td's nano + deps land at distinct paths.
# guix/Guile SCRUBBED FROM PATH; the toolchain + locks are the guix-built seed (§5).
HEAVY_GATES += build-plan-nano
build-plan-nano:
	@echo ">> build-plan-nano: ncurses + gettext-minimal -> nano — nano's .drv references td's deps (NOT guix's); nano runs reproducibly at a distinct path"
	@set -euo pipefail; \
	deps=`sed -n 's/^nano //p' "$(CURDIR)/tests/td-chained-edges.txt"`; \
	test -n "$$deps" || { echo "ERROR: no 'nano' edge line in tests/td-chained-edges.txt" >&2; exit 1; }; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	ev=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$ev" -a -x "$$tb" -a -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc / ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	cu=`grep -- '-coreutils-' "$(CURDIR)/tests/nano-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	root="$(CURDIR)/.td-build-cache/build-plan-nano"; mkdir -p "$$root/tmp"; \
	cp "$(CURDIR)/tests/nano-no-guix.lock" "$$root/nano-chained.lock"; \
	: > "$$root/plan"; \
	for d in $$deps; do \
	  sh tests/ts-emit.sh "tests/ts/recipe-$$d.ts" > "$$root/$$d.json"; \
	  test -s "$$root/$$d.json" || { echo "ERROR: ts-emit produced no JSON for $$d" >&2; exit 1; }; \
	  gp=`sed -n "s#^[^ ]*-$$d-[^ ]* \(/gnu/store/[^ ]*\)#\1#p" "$(CURDIR)/tests/nano-no-guix.lock" | head -1`; \
	  test -n "$$gp" || { echo "ERROR: dep $$d not found in nano-no-guix.lock" >&2; exit 1; }; \
	  sed -i "s#^[^ ]*-$$d-[^ ]* .*#$$d $$gp td-recipe-output#" "$$root/nano-chained.lock"; \
	  grep -q "^$$d $$gp td-recipe-output" "$$root/nano-chained.lock" || { echo "ERROR: failed to mark $$d as a td-recipe-output edge" >&2; exit 1; }; \
	  printf 'step %s %s\n' "$$root/$$d.json" "$(CURDIR)/tests/$$d-no-guix.lock" >> "$$root/plan"; \
	done; \
	sh tests/ts-emit.sh tests/ts/recipe-nano.ts > "$$root/nano.json"; \
	test -s "$$root/nano.json" || { echo "ERROR: ts-emit produced no JSON for nano" >&2; exit 1; }; \
	printf 'step %s %s\n' "$$root/nano.json" "$$root/nano-chained.lock" >> "$$root/plan"; \
	{ for d in $$deps; do grep ' /gnu/store/' "$(CURDIR)/tests/$$d-no-guix.lock"; done; grep ' /gnu/store/' "$$root/nano-chained.lock" | grep -v 'td-recipe-output'; } | sed 's/^[^ ]* //' | sort -u | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the guix-built seeds" >&2; exit 1; }; \
	env -i HOME="$$root" TMPDIR="$$root/tmp" PATH="$$cu/bin" "$$tb" build-plan "$$root/plan" /var/guix/db/db.sqlite "$$root" > "$$root/out" 2>"$$root/err" || { echo "FAIL: build-plan (guix/Guile off PATH):" >&2; tail -30 "$$root/err" >&2; exit 1; }; \
	td_nano=`sed -n 's/^STEP nano //p' "$$root/out"`; \
	test -n "$$td_nano" || { echo "FAIL: build-plan did not report the nano step" >&2; cat "$$root/out" >&2; exit 1; }; \
	ndrv="$$root/nano/nano-8.7.1.drv"; \
	test -s "$$ndrv" || { echo "FAIL: nano's assembled .drv is missing ($$ndrv)" >&2; exit 1; }; \
	for d in $$deps; do \
	  td_d=`sed -n "s/^STEP $$d //p" "$$root/out"`; \
	  gp=`sed -n "s#^[^ ]*-$$d-[^ ]* \(/gnu/store/[^ ]*\)#\1#p" "$(CURDIR)/tests/nano-no-guix.lock" | head -1`; \
	  test -n "$$td_d" || { echo "FAIL: build-plan did not report the $$d step" >&2; exit 1; }; \
	  echo "  built: $$d=$$td_d"; \
	  grep -q "$$td_d" "$$ndrv" || { echo "FAIL: nano's .drv does NOT reference td's $$d ($$td_d)" >&2; exit 1; }; \
	  if grep -q "$$gp" "$$ndrv"; then echo "FAIL: nano's .drv STILL references guix's $$d ($$gp) — substitution did not happen" >&2; exit 1; fi; \
	done; \
	echo "  built: nano=$$td_nano"; \
	echo "  [DURABLE structural] nano's .drv references td's $$deps and NOT guix's"; \
	nout="$$root/nano/newstore/`basename "$$td_nano"`"; \
	td_ncurses=`sed -n 's/^STEP ncurses //p' "$$root/out"`; \
	nl="$$root/tdstore/`basename "$$td_ncurses"`/lib"; \
	test -x "$$nout/bin/nano" || { echo "FAIL: nano binary missing from td's output" >&2; exit 1; }; \
	LD_LIBRARY_PATH="$$nl" "$$nout/bin/nano" --version | grep -q 'version 8.7.1' || { echo "FAIL: td's nano --version != 8.7.1" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] td's nano runs (--version 8.7.1) loading td's ncurses"; \
	if [ -f "$$root/nano/repro-ok" ] && [ "$$root/nano/repro-ok" -nt "$$ndrv" ]; then \
	  echo "  [DURABLE repro] CACHED: nano drv unchanged + previously verified reproducible — check skipped"; \
	else \
	  rm -rf "$$root/nano/chk"; \
	  env -i HOME="$$root" TMPDIR="$$root/tmp" PATH="$$cu/bin" "$$tb" check "$$ndrv" "$$root/nano/closure.txt" "$$root/nano/chk" >/dev/null 2>"$$root/chkerr" || { echo "FAIL: chained nano NOT reproducible (td-builder check):" >&2; tail -6 "$$root/chkerr" >&2; exit 1; }; \
	  touch "$$root/nano/repro-ok"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the chained nano is reproducible"; \
	fi; \
	gn=`$(GUIX) build nano 2>/dev/null | grep -v -- '-debug' | head -1 || true`; \
	if [ -n "$$gn" ] && [ "$$td_nano" = "$$gn" ]; then echo "FAIL: td's nano path equals guix's" >&2; exit 1; fi; \
	echo "  [MIGRATION ORACLE] td's nano lands at a distinct path from guix's"; \
	echo "PASS: build-plan chained ncurses + gettext-minimal -> nano — nano's .drv references td's OWN deps (not guix's), nano runs (--version 8.7.1) from td's output with td's ncurses loaded (durable), reproducible by td's own double-build (durable), at a distinct store path from guix's (own, then diverge). Built with guix/Guile SCRUBBED FROM PATH; the toolchain + locks are the guix-built seed (§5, retired last)."
