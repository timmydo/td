# corpus-deps-no-guix — td builds the corpus/toolchain's LIBRARY dependencies with
# its OWN builder (DESIGN §7.1 move-off-Guile §5, lever 4: reconstruct the
# shipped-system closure package-by-package so td's build-time independence from
# guix climbs). libsigsegv (→ gawk), libunistring (→ gettext-minimal) and pcre2
# (→ grep) are guix-supplied build inputs today (specification->package); this
# reconstructs each as a td recipe (tests/ts/recipe-<l>.ts) built via `td-builder
# build-recipe`, so they are td-built, not guix-resolved. Per lib: STRUCTURAL
# (built with guix/Guile off PATH), DURABLE behavioral (a tiny C program LINKS
# against td's installed header+lib with -Wl,--no-as-needed and RUNS, exit 0, so
# the loader must load td's shared lib — pcre2 also runs pcre2test --version),
# DURABLE reproducibility (td-builder check double-build), MIGRATION ORACLE
# (distinct store path from guix's build — own, then diverge). The link-test gcc is
# guix's gcc-toolchain (the compiler seed, retired last §5). gmp/ncurses/readline
# deferred (gmp's configure rejects the seed compiler at its long-long run-test).
#
# CONTENT-ADDRESSED CACHE: build-recipe's .drv path is deterministic, so a persistent
# cache (.td-build-cache/, gitignored) lets td SKIP the build when an unchanged recipe
# already has a NAR-verified output (build-recipe prints CACHE=hit). On a verified hit
# the reproducibility double-build is also skipped (verdict memoized, like check-memo)
# — so only the package whose recipe/inputs CHANGED rebuilds. A changed recipe ⇒
# different drv ⇒ cache miss ⇒ full build + check. Reproducibility/behavior unweakened:
# the first build still double-builds, and every run re-NAR-verifies the cached output.
HEAVY_GATES += corpus-deps-no-guix
corpus-deps-no-guix:
	@echo ">> corpus-deps-no-guix: td builds libsigsegv + libunistring + pcre2 via build-recipe (no guix/Guile in the build path); each links+runs from td's own output, reproducible, distinct from guix"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$ev" -a -x "$$tb" -a -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc / ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	cu=`grep -- '-coreutils-' "$(CURDIR)/tests/pcre2-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	gtbin=`for p in $$($(GUIX) build gcc-toolchain 2>/dev/null); do [ -x "$$p/bin/gcc" ] && echo "$$p/bin" && break; done`; \
	test -n "$$gtbin" || { echo "ERROR: could not resolve gcc-toolchain for the link-test" >&2; exit 1; }; \
	lkh=`for p in $$($(GUIX) build linux-libre-headers 2>/dev/null); do [ -f "$$p/include/linux/limits.h" ] && echo "$$p/include" && break; done`; \
	test -n "$$lkh" || { echo "ERROR: could not resolve linux-libre-headers for the link-test" >&2; exit 1; }; \
	cache="$(CURDIR)/.td-build-cache/corpus-deps"; mkdir -p "$$cache"; \
	for spec in libsigsegv libunistring pcre2; do \
	  echo "================ $$spec ================"; \
	  lock="$(CURDIR)/tests/$$spec-no-guix.lock"; \
	  test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	  grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed for $$spec" >&2; exit 1; }; \
	  sd="$$cache/$$spec"; mkdir -p "$$sd/b" "$$sd/tmp"; \
	  sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-$$spec.ts" > "$$sd/recipe.json"; \
	  test -s "$$sd/recipe.json" || { echo "ERROR: ts-emit produced no JSON for $$spec" >&2; exit 1; }; \
	  rm -f "$$sd/b/"*.drv; \
	  env -i HOME="$$sd" TMPDIR="$$sd/tmp" PATH="$$cu/bin" "$$tb" build-recipe "$$sd/recipe.json" "$$lock" "$$sd/b" /var/guix/db/db.sqlite > "$$sd/bout" 2>"$$sd/err" || { echo "FAIL: build-recipe $$spec (guix/Guile off PATH):" >&2; tail -20 "$$sd/err" >&2; exit 1; }; \
	  out=`sed -n 's/^OUT=out //p' "$$sd/bout"`; \
	  test -n "$$out" || { echo "FAIL: build-recipe produced no output for $$spec" >&2; cat "$$sd/err" >&2; exit 1; }; \
	  if grep -qx 'CACHE=hit' "$$sd/bout"; then hit=1; echo "  [STRUCTURAL] CACHE HIT — drv unchanged, reused td's prior output (no rebuild): $$out"; else hit=; echo "  [STRUCTURAL] built with guix/Guile off PATH: $$out"; fi; \
	  ns="$$sd/b/newstore/`basename "$$out"`"; \
	  case "$$spec" in \
	    libsigsegv)   hdr=sigsegv.h;  lib=sigsegv;    pre="" ;; \
	    libunistring) hdr=unistr.h;   lib=unistring;  pre="" ;; \
	    pcre2)        hdr=pcre2.h;    lib=pcre2-8;    pre="#define PCRE2_CODE_UNIT_WIDTH 8" ;; \
	  esac; \
	  test -f "$$ns/include/$$hdr" || { echo "FAIL: $$spec header $$hdr missing from td output" >&2; exit 1; }; \
	  printf '%s\n#include <%s>\nint main(void){return 0;}\n' "$$pre" "$$hdr" > "$$sd/t.c"; \
	  PATH="$$gtbin:$$PATH" C_INCLUDE_PATH="$$lkh" "$$gtbin/gcc" "$$sd/t.c" -I"$$ns/include" -L"$$ns/lib" -Wl,--no-as-needed -l"$$lib" -o "$$sd/t" 2>"$$sd/lk" || { echo "FAIL: $$spec link-test did not compile/link:" >&2; cat "$$sd/lk" >&2; exit 1; }; \
	  LD_LIBRARY_PATH="$$ns/lib" "$$sd/t" || { echo "FAIL: $$spec link-test binary did not run (td lib not loadable)" >&2; exit 1; }; \
	  echo "  [DURABLE behavioral] $$spec: a C program links td's $$hdr + lib$$lib and runs (lib loadable)"; \
	  if [ "$$spec" = pcre2 ]; then \
	    LD_LIBRARY_PATH="$$ns/lib" "$$ns/bin/pcre2test" --version | grep -q '10.42' || { echo "FAIL: pcre2test --version != 10.42" >&2; exit 1; }; \
	    echo "  [DURABLE behavioral] pcre2test --version reports 10.42"; \
	  fi; \
	  if [ -n "$$hit" ] && [ -f "$$sd/b/verified-reproducible" ]; then \
	    echo "  [DURABLE repro] CACHED: drv unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
	  else \
	    rm -rf "$$sd/chk"; "$$tb" check "$$sd/b/"*.drv "$$sd/b/closure.txt" "$$sd/chk" >/dev/null 2>"$$sd/chkerr" || { echo "FAIL: $$spec NOT reproducible (td-builder check):" >&2; tail -6 "$$sd/chkerr" >&2; exit 1; }; \
	    : > "$$sd/b/verified-reproducible"; \
	    echo "  [DURABLE repro] td-builder check double-build agrees $$spec is reproducible"; \
	  fi; \
	  g=`$(GUIX) build "$$spec" 2>/dev/null | grep -v -- '-debug\|-doc\|-static' | head -1 || true`; \
	  if [ -n "$$g" ] && [ "$$out" = "$$g" ]; then echo "FAIL: td's $$spec path equals guix's — expected a distinct own-builder path" >&2; exit 1; fi; \
	  echo "  [MIGRATION ORACLE] distinct from guix's $$spec"; \
	  rm -rf "$$sd/chk" "$$sd/tmp" "$$sd/t" "$$sd/t.c" "$$sd/lk" "$$sd/bout" "$$sd/err" "$$sd/chkerr" "$$sd/recipe.json"; mkdir -p "$$sd/tmp"; \
	done; \
	echo "PASS: td built corpus/toolchain library deps — libsigsegv, libunistring, pcre2 — via td-builder build-recipe, every input resolved from a pinned lock (no specification->package), the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon), with guix/Guile SCRUBBED FROM PATH; each links+runs from td's own output (durable), is reproducible by td's own double-build (durable), and lands at a distinct store path from guix's build (own, then diverge). The compiler seed (gcc-toolchain) stays external (§5, retired last)."
