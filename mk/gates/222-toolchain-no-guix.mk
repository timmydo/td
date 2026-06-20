# toolchain-no-guix — td builds its toolchain's LEAF tools with its OWN builder
# (DESIGN §7.1 move-off-Guile §5, lever 4: retire the Guix toolchain
# package-by-package, leaves first). The build environment's non-compiler tools —
# make, sed, grep, xz, diffutils, patch, file, coreutils, gawk, tar, findutils,
# bash — come today from guix packages (specification->package); this
# reconstructs each as a td recipe (tests/ts/recipe-<t>.ts) built via `td-builder
# build-recipe`, so they are td-built, not guix-resolved. make is built guile-free
# (one fewer Guile dep); gawk/bash need a gcc-15/C23 CFLAGS workaround
# (-Wno-incompatible-pointer-types / -std=gnu17), carried as a whitespace-bearing
# CFLAGS through the recipe DSL's JSON-encoded configureFlags. tar/findutils/bash
# carry a guix source snippet (and bash's 37 patches), so their source is guix's
# patch-and-repacked .tar.zst, which the seed `tar` unpacks via the pinned `zstd`.
# The irreducible compiler seed (gcc-toolchain/glibc/binutils) stays external
# (§5, retired last). Per tool: STRUCTURAL (built with guix/Guile off PATH),
# DURABLE behavioral (the tool runs --version), DURABLE reproducibility (td-builder
# check double-build), MIGRATION ORACLE (distinct store path from guix's build —
# own, then diverge). Locks are the guix-built seed.
HEAVY_GATES += toolchain-no-guix
# Built up front by the parallel `build-recipes` phase (into the shared cache); this
# gate then cache-hits + memo-skips and only asserts behavior/oracle.
toolchain_SPECS := make sed grep xz diffutils patch file coreutils gawk tar findutils bash
BUILD_SPECS     += $(toolchain_SPECS)
BUILD_GATES     += toolchain-no-guix
toolchain-no-guix:
	@echo ">> toolchain-no-guix: td builds make + sed + grep + xz + diffutils + patch + file + coreutils + gawk + tar + findutils + bash via build-recipe (no guix/Guile in the build path); each runs, reproducible, distinct from guix; gcc/glibc/binutils seed stays external (§5)"
	@set -euo pipefail; \
	tgz=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-tsgo-tarball)'`; tsgo=`sh tests/tsgo.sh "$$tgz"`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	cu=`grep -- '-coreutils-' "$(CURDIR)/tests/make-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; CU="$$cu"; CACHE="$(CURDIR)/.td-build-cache/pkg"; mkdir -p "$$CACHE"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] recipes evaluate with td's OWN td-ts-eval ($$TD_TS_EVAL) — not the guix-built one (brick 4b)"; \
	for spec in $(toolchain_SPECS); do \
	  echo "================ $$spec ================"; \
	  lock="$(CURDIR)/tests/$$spec-no-guix.lock"; \
	  test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	  grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed for $$spec (regenerate locks on a channel bump)" >&2; exit 1; }; \
	  cached_build "$$spec" "$$lock" || exit 1; \
	  if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — drv unchanged, reused td's prior output (no rebuild): $$out"; else echo "  [STRUCTURAL] built with guix/Guile off PATH: $$out"; fi; \
	  L="$$ns/lib"; \
	  case "$$spec" in \
	    make)      bin=make;  nver="GNU Make 4.4.1" ;; \
	    sed)       bin=sed;   nver="(GNU sed) 4.9" ;; \
	    grep)      bin=grep;  nver="(GNU grep) 3.11" ;; \
	    xz)        bin=xz;    nver="xz (XZ Utils) 5.4.5" ;; \
	    diffutils) bin=diff;  nver="(GNU diffutils) 3.12" ;; \
	    patch)     bin=patch; nver="GNU patch 2.8" ;; \
	    file)      bin=file;  nver="file-5.46" ;; \
	    coreutils) bin=ls;    nver="(GNU coreutils) 9.1" ;; \
	    gawk)      bin=gawk;  nver="GNU Awk 5.3.0" ;; \
	    tar)       bin=tar;   nver="(GNU tar) 1.35" ;; \
	    findutils) bin=find;  nver="(GNU findutils) 4.10.0" ;; \
	    bash)      bin=bash;  nver="GNU bash, version 5.2.37" ;; \
	    *) echo "FAIL: no behavioral check defined for $$spec" >&2; exit 1 ;; \
	  esac; \
	  LD_LIBRARY_PATH="$$L" "$$ns/bin/$$bin" --version | grep -q "$$nver" || { echo "FAIL: $$spec ($$bin --version lacks '$$nver')" >&2; exit 1; }; \
	  echo "  [DURABLE behavioral] $$spec: $$bin runs --version ($$nver) from td's own store output"; \
	  cached_check "$$spec" || exit 1; \
	  g=`$(GUIX) build "$$spec" 2>/dev/null | grep -v -- '-debug' | head -1 || true`; \
	  if [ -n "$$g" ] && [ "$$out" = "$$g" ]; then echo "FAIL: td's $$spec path equals guix's — expected a distinct own-builder path" >&2; exit 1; fi; \
	  echo "  [MIGRATION ORACLE] distinct from guix's $$spec"; \
	  cached_clean; \
	done; \
	echo "PASS: td built its toolchain leaf tools — make (guile-free), sed, grep, xz, diffutils, patch, file, coreutils, gawk, tar, findutils, bash — via td-builder build-recipe, every input resolved from a pinned lock (no specification->package), the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon), with guix/Guile SCRUBBED FROM PATH; each runs --version (durable), is reproducible by td's own double-build (durable), and lands at a distinct store path from guix's build (own, then diverge). The compiler seed (gcc-toolchain/glibc/binutils) stays external (§5, retired last)."
