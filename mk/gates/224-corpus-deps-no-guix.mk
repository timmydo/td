# corpus-deps-no-guix — td builds the corpus/toolchain's LIBRARY dependencies with
# its OWN builder (DESIGN §7.1 move-off-Guile §5, lever 4: reconstruct the
# shipped-system closure package-by-package so td's build-time independence from
# guix climbs). libsigsegv (→ gawk), libunistring (→ gettext-minimal), pcre2
# (→ grep), ncurses (→ bash/nano) and readline (→ bash/gawk) are guix-supplied
# build inputs today (specification->package); this reconstructs each as a td
# recipe (tests/ts/recipe-<l>.ts) built via `td-builder build-recipe`, so they are
# td-built, not guix-resolved. ncurses drops its optional C++ binding (incompatible
# with the seed gcc-15); readline links ncurses for termcap (an input). Per lib:
# STRUCTURAL (built with guix/Guile off PATH), DURABLE behavioral (a tiny C program
# LINKS against td's installed header+lib with -Wl,--no-as-needed and RUNS, exit 0,
# so the loader must load td's shared lib — pcre2 also runs pcre2test --version),
# DURABLE reproducibility (td-builder check double-build), MIGRATION ORACLE
# (distinct store path from guix's build — own, then diverge). The link-test gcc is
# guix's gcc-toolchain (the compiler seed, retired last §5). gmp deferred (its
# configure rejects the seed compiler at its long-long run-test).
#
# CONTENT-ADDRESSED CACHE: build-recipe's .drv path is deterministic, so a persistent
# cache (.td-build-cache/, gitignored) lets td SKIP the build when an unchanged recipe
# already has a NAR-verified output (build-recipe prints CACHE=hit). On a verified hit
# the reproducibility double-build is also skipped (verdict memoized, like check-memo)
# — so only the package whose recipe/inputs CHANGED rebuilds. A changed recipe ⇒
# different drv ⇒ cache miss ⇒ full build + check. Reproducibility/behavior unweakened:
# the first build still double-builds, and every run re-NAR-verifies the cached output.
HEAVY_GATES += corpus-deps-no-guix
# Built up front by the parallel `build-recipes` phase (into the shared cache); this
# gate then cache-hits + memo-skips and only asserts behavior/oracle.
deps_SPECS  := libsigsegv libunistring pcre2 ncurses readline
BUILD_SPECS += $(deps_SPECS)
BUILD_GATES += corpus-deps-no-guix
corpus-deps-no-guix:
	@echo ">> corpus-deps-no-guix: td builds libsigsegv + libunistring + pcre2 + ncurses + readline via build-recipe (no guix/Guile in the build path); each links+runs from td's own output, reproducible, distinct from guix"
	@set -euo pipefail; \
	tsgo=`sh tests/tsgo.sh`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	cu=`grep -- '-coreutils-' "$(CURDIR)/tests/pcre2-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	gtbin=`for p in $$($(GUIX) build gcc-toolchain 2>/dev/null); do [ -x "$$p/bin/gcc" ] && echo "$$p/bin" && break; done`; \
	test -n "$$gtbin" || { echo "ERROR: could not resolve gcc-toolchain for the link-test" >&2; exit 1; }; \
	lkh=`for p in $$($(GUIX) build linux-libre-headers 2>/dev/null); do [ -f "$$p/include/linux/limits.h" ] && echo "$$p/include" && break; done`; \
	test -n "$$lkh" || { echo "ERROR: could not resolve linux-libre-headers for the link-test" >&2; exit 1; }; \
	ncs=`for p in $$($(GUIX) build ncurses 2>/dev/null); do [ -f "$$p/lib/libncurses.so" ] && echo "$$p/lib" && break; done`; \
	test -n "$$ncs" || { echo "ERROR: could not resolve ncurses for readline's termcap link-test" >&2; exit 1; }; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; CU="$$cu"; CACHE="$(CURDIR)/.td-build-cache/pkg"; mkdir -p "$$CACHE"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] recipes evaluate with td's OWN td-ts-eval ($$TD_TS_EVAL) — not the guix-built one (brick 4b)"; \
	for spec in $(deps_SPECS); do \
	  echo "================ $$spec ================"; \
	  lock="$(CURDIR)/tests/$$spec-no-guix.lock"; \
	  test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	  grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed for $$spec" >&2; exit 1; }; \
	  cached_build "$$spec" "$$lock" || exit 1; \
	  if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — drv unchanged, reused td's prior output (no rebuild): $$out"; else echo "  [STRUCTURAL] built with guix/Guile off PATH: $$out"; fi; \
	  xtra=""; xtrun=""; \
	  case "$$spec" in \
	    libsigsegv)   hdr=sigsegv.h;  lib=sigsegv;    pre="" ;; \
	    libunistring) hdr=unistr.h;   lib=unistring;  pre="" ;; \
	    pcre2)        hdr=pcre2.h;    lib=pcre2-8;    pre="#define PCRE2_CODE_UNIT_WIDTH 8" ;; \
	    ncurses)      hdr=curses.h;   lib=ncurses;    pre="" ;; \
	    readline)     hdr=readline/readline.h; lib=readline; pre="#include <stdio.h>"; xtra="-L$$ncs -lncurses"; xtrun=":$$ncs" ;; \
	  esac; \
	  test -f "$$ns/include/$$hdr" || { echo "FAIL: $$spec header $$hdr missing from td output" >&2; exit 1; }; \
	  printf '%s\n#include <%s>\nint main(void){return 0;}\n' "$$pre" "$$hdr" > "$$sd/t.c"; \
	  PATH="$$gtbin:$$PATH" C_INCLUDE_PATH="$$lkh" "$$gtbin/gcc" "$$sd/t.c" -I"$$ns/include" -L"$$ns/lib" -Wl,--no-as-needed -l"$$lib" $$xtra -o "$$sd/t" 2>"$$sd/lk" || { echo "FAIL: $$spec link-test did not compile/link:" >&2; cat "$$sd/lk" >&2; exit 1; }; \
	  LD_LIBRARY_PATH="$$ns/lib$$xtrun" "$$sd/t" || { echo "FAIL: $$spec link-test binary did not run (td lib not loadable)" >&2; exit 1; }; \
	  echo "  [DURABLE behavioral] $$spec: a C program links td's $$hdr + lib$$lib and runs (lib loadable)"; \
	  if [ "$$spec" = pcre2 ]; then \
	    LD_LIBRARY_PATH="$$ns/lib" "$$ns/bin/pcre2test" --version | grep -q '10.42' || { echo "FAIL: pcre2test --version != 10.42" >&2; exit 1; }; \
	    echo "  [DURABLE behavioral] pcre2test --version reports 10.42"; \
	  fi; \
	  cached_check "$$spec" || exit 1; \
	  g=`$(GUIX) build "$$spec" 2>/dev/null | grep -v -- '-debug\|-doc\|-static' | head -1 || true`; \
	  if [ -n "$$g" ] && [ "$$out" = "$$g" ]; then echo "FAIL: td's $$spec path equals guix's — expected a distinct own-builder path" >&2; exit 1; fi; \
	  echo "  [MIGRATION ORACLE] distinct from guix's $$spec"; \
	  cached_clean; \
	done; \
	echo "PASS: td built corpus/toolchain library deps — libsigsegv, libunistring, pcre2, ncurses, readline — via td-builder build-recipe, every input resolved from a pinned lock (no specification->package), the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon), with guix/Guile SCRUBBED FROM PATH; each links+runs from td's own output (durable), is reproducible by td's own double-build (durable), and lands at a distinct store path from guix's build (own, then diverge). The compiler seed (gcc-toolchain) stays external (§5, retired last)."
