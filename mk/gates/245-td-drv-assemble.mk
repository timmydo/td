# td-drv-assemble (DESIGN §7.1; the §5 move-off-Guile arc). Removes the LAST guile
# `(derivation …)` from the build path. Guile RESOLVES the inputs (toolchain + source
# → store paths — input resolution, stays Guix's, retired last) and emits a raw SPEC
# (system/td-build.scm `write-td-build-spec`: name/system/builder/arg/input-drv/env,
# NO output paths, NO `(derivation …)`); td-builder `drv-assemble` does the ASSEMBLY
# `(derivation …)` used to do — add the `out` output + env var, SORT env by key and
# inputs by path (the daemon's canonical order), compute the output path (#22's
# construct_drv), serialize — and REGISTERS it via the daemon (#27). The differential:
# td's assembled+registered `.drv` is byte-identical to the SAME recipe lowered through
# guix's `(derivation …)` (the oracle) — equal store path proves equal bytes (the
# daemon content-addresses td's bytes). Then `guix build` builds td's `.drv` to a
# working hello. So nothing guile constructs the build derivation anymore; guile only
# resolves which inputs it has (§5). Heavy (a td-builder compile + a warm hello
# realise). Scratch removed on green.
HEAVY_GATES += td-drv-assemble
td-drv-assemble:
	@echo ">> td-drv-assemble: td ASSEMBLES the build .drv from a guile-resolved spec (no (derivation …)) and registers it — byte-identical to guix's (derivation …)"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-drv-assemble-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	oracle=`TD_SPEC_OUT="$$scratch/hello.spec" $(GUIX) repl $(LOAD) tests/td-drv-assemble-drv.scm 2>/dev/null | sed -n 's/^ORACLE=//p'`; \
	test -n "$$oracle" -a -s "$$scratch/hello.spec" \
	  || { echo "ERROR: could not emit the spec / lower the oracle derivation" >&2; exit 1; }; \
	echo ">> guile emitted the SPEC without (derivation …): $$(wc -l < "$$scratch/hello.spec") lines (input resolution only)"; \
	echo ">> oracle (guix (derivation …)): $$oracle"; \
	echo ">> td ASSEMBLES the .drv from the spec (ordering + output paths in Rust) and registers it via the daemon:"; \
	added=`"$$tb" drv-assemble "$$scratch/hello.spec"` \
	  || { echo "FAIL: td-builder drv-assemble failed" >&2; exit 1; }; \
	test "$$added" = "$$oracle" \
	  || { echo "FAIL: the td-assembled .drv $$added != guix's (derivation …) $$oracle — td's assembly/ordering diverged" >&2; exit 1; }; \
	echo "   td registered at guix's exact path: $$added (byte-identical — the daemon content-addresses td's bytes)"; \
	echo ">> the loop builds td's assembled+registered .drv:"; \
	out=`$(GUIX) build "$$added"`; \
	test -n "$$out" -a -x "$$out/bin/hello" \
	  || { echo "FAIL: guix build of the td-assembled .drv produced no bin/hello" >&2; exit 1; }; \
	say=`"$$out/bin/hello"`; \
	test "$$say" = "Hello, world!" \
	  || { echo "FAIL: the built hello printed '$$say', expected 'Hello, world!'" >&2; exit 1; }; \
	rm -rf "$$scratch"; \
	echo "PASS: td ASSEMBLED the build .drv from a guile-resolved spec (no (derivation …)) — byte-identical to guix's (derivation …), at the same store path — registered it via the daemon, and the loop built it to a working hello. Nothing guile constructs the build derivation; input resolution stays Guix's (§5)."
