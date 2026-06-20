# build-plan — td CHAINS its OWN build outputs into downstream builds: a downstream
# recipe links the dep td just built, not guix's (move-off-Guile §5). ONE gate, driven
# by tests/td-chained-edges.txt (the same manifest the guix-dependence census reads to
# credit EDGE-OWNED): for every `<subject> <dep>...` line it builds the deps with td,
# substitutes their outputs into the subject's inputs (a `td-recipe-output` lock entry,
# closure spanning td.db ∪ guix's db, deps staged from a td-store), and proves the chain.
# Deps cache across subjects (shared scratch), so bash<-readline<-ncurses builds each once.
#
# Per subject: DURABLE structural — the subject's .drv references td's dep output(s) AND
# NOT guix's (substitution load-bearing). DURABLE behavioral — the subject runs from td's
# output loading td's deps (a library subject: its .so is present). DURABLE repro —
# `td-builder check` double-builds it bit-identically. MIGRATION ORACLE — distinct path
# from guix's. guix/Guile SCRUBBED FROM PATH; the toolchain + locks are the seed (§5).
HEAVY_GATES += build-plan
build-plan:
	@echo ">> build-plan: chain td-built deps into downstream builds (every tests/td-chained-edges.txt edge) — subject .drv references td's deps (NOT guix's), runs, reproducibly, at distinct paths"
	@set -euo pipefail; \
	manifest="$(CURDIR)/tests/td-chained-edges.txt"; \
	subjects=`grep -vE '^[[:space:]]*#|^[[:space:]]*$$' "$$manifest" | sed 's/ .*//'`; \
	test -n "$$subjects" || { echo "ERROR: no chained edges in $$manifest" >&2; exit 1; }; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	ev=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$ev" -a -x "$$tb" -a -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc / ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	cu=`grep -- '-coreutils-' "$(CURDIR)/tests/grep-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	root="$(CURDIR)/.td-build-cache/build-plan"; mkdir -p "$$root/tmp"; \
	for S in $$subjects; do \
	  deps=`sed -n "s/^$$S //p" "$$manifest"`; \
	  test -n "$$deps" || { echo "ERROR: no deps for subject $$S" >&2; exit 1; }; \
	  slock="$(CURDIR)/tests/$$S-no-guix.lock"; \
	  test -f "$$slock" || { echo "ERROR: missing lock $$slock" >&2; exit 1; }; \
	  cp "$$slock" "$$root/$$S-chained.lock"; \
	  : > "$$root/plan-$$S"; \
	  for d in $$deps; do \
	    sh tests/ts-emit.sh "tests/ts/recipe-$$d.ts" > "$$root/$$d.json"; \
	    test -s "$$root/$$d.json" || { echo "ERROR: ts-emit produced no JSON for $$d" >&2; exit 1; }; \
	    if grep -q "^$$d /" "$$slock"; then gp=`sed -n "s/^$$d //p" "$$slock" | head -1`; \
	    else gp=`sed -n "s#^[^ ]*-$$d-[^ ]* \(/gnu/store/[^ ]*\)#\1#p" "$$slock" | head -1`; fi; \
	    test -n "$$gp" || { echo "ERROR: dep $$d not found in $$slock" >&2; exit 1; }; \
	    if grep -q "^$$d /" "$$root/$$S-chained.lock"; then sed -i "s#^$$d .*#$$d $$gp td-recipe-output#" "$$root/$$S-chained.lock"; \
	    else sed -i "s#^[^ ]*-$$d-[^ ]* .*#$$d $$gp td-recipe-output#" "$$root/$$S-chained.lock"; fi; \
	    grep -q "^$$d $$gp td-recipe-output" "$$root/$$S-chained.lock" || { echo "ERROR: failed to mark $$d as a td-recipe-output edge in $$S" >&2; exit 1; }; \
	    printf 'step %s %s\n' "$$root/$$d.json" "$(CURDIR)/tests/$$d-no-guix.lock" >> "$$root/plan-$$S"; \
	  done; \
	  sh tests/ts-emit.sh "tests/ts/recipe-$$S.ts" > "$$root/$$S.json"; \
	  test -s "$$root/$$S.json" || { echo "ERROR: ts-emit produced no JSON for $$S" >&2; exit 1; }; \
	  printf 'step %s %s\n' "$$root/$$S.json" "$$root/$$S-chained.lock" >> "$$root/plan-$$S"; \
	  { for d in $$deps; do grep ' /gnu/store/' "$(CURDIR)/tests/$$d-no-guix.lock"; done; grep ' /gnu/store/' "$$root/$$S-chained.lock" | grep -v 'td-recipe-output'; } | sed 's/^[^ ]* //' | sort -u | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize guix seeds for $$S" >&2; exit 1; }; \
	  env -i HOME="$$root" TMPDIR="$$root/tmp" PATH="$$cu/bin" "$$tb" build-plan "$$root/plan-$$S" /var/guix/db/db.sqlite "$$root" > "$$root/out-$$S" 2>"$$root/err-$$S" || { echo "FAIL: build-plan $$S (guix/Guile off PATH):" >&2; tail -30 "$$root/err-$$S" >&2; exit 1; }; \
	  td_S=`sed -n "s/^STEP $$S //p" "$$root/out-$$S"`; \
	  test -n "$$td_S" || { echo "FAIL: build-plan did not report the $$S step" >&2; cat "$$root/out-$$S" >&2; exit 1; }; \
	  sdrv=`ls "$$root/$$S"/*.drv 2>/dev/null | head -1`; \
	  test -s "$$sdrv" || { echo "FAIL: $$S's assembled .drv is missing" >&2; exit 1; }; \
	  ld=""; \
	  for d in $$deps; do \
	    td_d=`sed -n "s/^STEP $$d //p" "$$root/out-$$S"`; \
	    if grep -q "^$$d /" "$$slock"; then gp=`sed -n "s/^$$d //p" "$$slock" | head -1`; \
	    else gp=`sed -n "s#^[^ ]*-$$d-[^ ]* \(/gnu/store/[^ ]*\)#\1#p" "$$slock" | head -1`; fi; \
	    test -n "$$td_d" || { echo "FAIL: build-plan did not report the $$d step for $$S" >&2; exit 1; }; \
	    grep -q "$$td_d" "$$sdrv" || { echo "FAIL: $$S's .drv does NOT reference td's $$d ($$td_d)" >&2; exit 1; }; \
	    if grep -q "$$gp" "$$sdrv"; then echo "FAIL: $$S's .drv STILL references guix's $$d ($$gp) — substitution did not happen" >&2; exit 1; fi; \
	    ld="$$ld$${ld:+:}$$root/tdstore/`basename "$$td_d"`/lib"; \
	  done; \
	  echo "  [$$S DURABLE structural] .drv references td's $$deps and NOT guix's"; \
	  out="$$root/$$S/newstore/`basename "$$td_S"`"; \
	  bld="$$out/lib$${ld:+:}$$ld"; \
	  case "$$S" in \
	    grep) printf 'foobar\nbaz\n' | LD_LIBRARY_PATH="$$bld" "$$out/bin/grep" -P 'o{2}' | grep -qx foobar || { echo "FAIL: $$S -P PCRE match failed" >&2; exit 1; }; bh="grep -P matches via td's pcre2" ;; \
	    nano) LD_LIBRARY_PATH="$$bld" "$$out/bin/nano" --version | grep -q 'version 8.7.1' || { echo "FAIL: nano --version" >&2; exit 1; }; bh="nano --version 8.7.1 loads td's ncurses" ;; \
	    bash) LD_LIBRARY_PATH="$$bld" "$$out/bin/bash" -c 'echo $$BASH_VERSION' | grep -q '^5' || { echo "FAIL: bash run/version" >&2; exit 1; }; bh="bash runs loading td's readline + ncurses" ;; \
	    gettext-minimal) LD_LIBRARY_PATH="$$bld" "$$out/bin/msgfmt" --version | grep -qi 'gettext' || { echo "FAIL: msgfmt --version" >&2; exit 1; }; bh="msgfmt --version runs (libtextstyle loads td's shared ncurses)" ;; \
	    readline) ls "$$out"/lib/libreadline.so* >/dev/null 2>&1 || { echo "FAIL: libreadline.so missing from $$S output" >&2; exit 1; }; bh="libreadline.so present (library subject)" ;; \
	    *) echo "FAIL: no behavioral check defined for subject $$S — add one" >&2; exit 1 ;; \
	  esac; \
	  echo "  [$$S DURABLE behavioral] $$bh"; \
	  if [ -f "$$root/$$S/repro-ok" ] && [ "$$root/$$S/repro-ok" -nt "$$sdrv" ]; then \
	    echo "  [$$S DURABLE repro] CACHED: drv unchanged + previously reproducible"; \
	  else \
	    rm -rf "$$root/$$S/chk"; \
	    env -i HOME="$$root" TMPDIR="$$root/tmp" PATH="$$cu/bin" "$$tb" check "$$sdrv" "$$root/$$S/closure.txt" "$$root/$$S/chk" >/dev/null 2>"$$root/chkerr-$$S" || { echo "FAIL: chained $$S NOT reproducible:" >&2; tail -6 "$$root/chkerr-$$S" >&2; exit 1; }; \
	    touch "$$root/$$S/repro-ok"; \
	    echo "  [$$S DURABLE repro] td-builder check double-build agrees the chained $$S is reproducible"; \
	  fi; \
	  gs=`$(GUIX) build "$$S" 2>/dev/null | grep -v -- '-debug\|-doc\|-static\|-lib$$' | head -1 || true`; \
	  if [ -n "$$gs" ] && [ "$$td_S" = "$$gs" ]; then echo "FAIL: td's $$S path equals guix's" >&2; exit 1; fi; \
	  echo "  [$$S MIGRATION ORACLE] td's $$S lands at a distinct path from guix's"; \
	  echo "  ==> $$S edge-owned: built from td's $$deps ($$td_S)"; \
	done; \
	echo "PASS: build-plan chained EVERY edge in tests/td-chained-edges.txt — each subject's .drv references td's OWN dep outputs (not guix's), runs from td's output loading td's deps (durable; bash<-readline<-ncurses is a 2-level td DAG), is reproducible by td's own double-build (durable), and lands at a distinct store path from guix's (own, then diverge). Built with guix/Guile SCRUBBED FROM PATH; the toolchain + locks are the guix-built seed (§5, retired last)."
