# bootstrap-build — the loop BUILDS a real package with the td-bootstrapped STAGE0 as
# the builder-of-record (move-off-Guile §5, bootstrap brick 2; follow-on to gate 170).
# Brick 1 (gate 170) proved td-builder needs no guix to be CREATED. This gate proves the
# loop can USE that stage0 to build: td places the cargo-built stage0 into its OWN
# content-addressed store (store-add-builder — restore the tree, scan its toolchain refs
# against the daemon db's ValidPaths, register path+refs), then `build-recipe` assembles
# hello's .drv with the stage0 path as `builder` and realizes it daemon-free, the
# builder STAGED from td's own store (canonical\ton-disk) and its closure self-contained
# by spanning the td builder db (the builder + its direct refs) ∪ the daemon seed db
# (those refs' transitive glibc/gcc-lib closures). So a real artifact is built by a binary
# guix NEVER produced. The toolchain seed is the guix-built pin (§5, retired last).
# (The seed-db span makes the builder's closure self-contained; for this subject it is
# defense-in-depth — hello's own toolchain inputs already supply glibc/gcc-lib — so it is
# correct but not independently load-bearing here, hence not a verified-red leg.)
#
# Per the differential+durable discipline:
#   [STRUCTURAL] hello's assembled .drv names the STAGE0 td path as its builder, NOT the
#     guix-built td-builder's store path; the build ran with guix/Guile off PATH.
#   [DURABLE behavioral] the loop RUNS hello from td's own store output → "Hello, world!".
#   [DURABLE intrinsic-reproducibility] `td-builder check` double-builds the stage0-built
#     .drv and agrees it is reproducible (no guix --check; the builder is staged from td's
#     own store on both runs via the canonical\ton-disk closure encoding).
#   [MIGRATION ORACLE, removable] the stage0-built hello is behaviorally == guix's hello
#     (same greeting) at a DISTINCT store path — own, then diverge (a different builder
#     path ⇒ a different drv ⇒ a different output path, identical behavior). Delete this
#     leg when guix retires; the durable legs above still stand.
#
# Self-discrimination (verified-red, recorded in plan/bootstrap-td-builder.md): (1)
# dropping the builder override makes build-recipe fall back to the guix-built td-builder
# and hello's .drv `builder` becomes guix's path — the STRUCTURAL assert flips (exit 2);
# (2) corrupting the builder's on-disk staging path (canonical\t<bogus>) makes the staged
# builder unreachable — `closure item … (on disk …): No such file`, the build fails —
# proving stage0 is genuinely fed into the build FROM td's own store, not /gnu/store.
HEAVY_GATES += bootstrap-build
bootstrap-build:
	@echo ">> bootstrap-build: td places its stage0 builder into its OWN store and the loop BUILDS hello with it (the builder-of-record is a binary guix never produced) — runs, reproducible, distinct from guix"
	@set -euo pipefail; \
	scratch="$(CURDIR)/.td-build-cache/bootstrap-build"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	TD_RECIPE_EVAL=`TD_GUIX="$(GUIX)" sh tests/recipe-eval-tool.sh "$(CURDIR)/.td-build-cache/recipe-eval"`; export TD_RECIPE_EVAL; \
	test -x "$$tb" -a -x "$$TD_RECIPE_EVAL" || { echo "ERROR: could not resolve td-builder / td-recipe-eval" >&2; exit 1; }; \
	lock="$(CURDIR)/tests/hello-no-guix.lock"; \
	test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize hello's seed (regenerate locks on a channel bump)" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	tblock="$(CURDIR)/tests/td-builder-rust.lock"; \
	test -s "$$tblock" || { echo "ERROR: no td-builder toolchain lock $$tblock" >&2; exit 1; }; \
	grep ' /gnu/store/' "$$tblock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the stage0 toolchain seed" >&2; exit 1; }; \
	echo ">> stage0: cargo build under env -i, pinned PATH only (guix/Guile scrubbed — gate 170's bootstrap)"; \
	s0dir="$$scratch/stage0"; \
	s0=`TD_LOCK="$$tblock" sh tools/bootstrap-td-builder.sh "$$s0dir"`; \
	test -x "$$s0" || { echo "FAIL: bootstrap produced no stage0 td-builder" >&2; exit 1; }; \
	test "`"$$s0"`" = "td-builder 0.1.0 ok" || { echo "FAIL: stage0 sentinel" >&2; exit 1; }; \
	echo ">> place stage0 into td's OWN content-addressed store (store-add-builder; refs scanned vs the daemon db)"; \
	tdstore="$$scratch/tdstore"; bdb="$$scratch/builder.db"; \
	Cb=`"$$tb" store-add-builder td-builder-0.1.0 "$$s0dir" "$$tdstore" "$$bdb" /var/guix/db/db.sqlite`; \
	case "$$Cb" in /gnu/store/*-td-builder-0.1.0) : ;; *) echo "FAIL: store-add-builder gave a malformed path '$$Cb'" >&2; exit 1 ;; esac; \
	test -x "$$tdstore/`basename "$$Cb"`/bin/td-builder" || { echo "FAIL: stage0 not restored under the td store dir" >&2; exit 1; }; \
	gtb=$${tb%/bin/td-builder}; \
	test "$$Cb" != "$$gtb" || { echo "FAIL: td's stage0 path equals the guix-built td-builder path — expected a distinct own-store path" >&2; exit 1; }; \
	echo "  td placed stage0 at $$Cb (distinct from guix-tb $$gtb)"; \
	echo ">> build hello with the STAGE0 builder override, guix/Guile scrubbed from PATH"; \
	b="$$scratch/b"; mkdir -p "$$b" "$$scratch/tmp"; \
	sh tests/recipe-emit.sh hello > "$$scratch/recipe.json" || { echo "FAIL: recipe-emit hello" >&2; exit 1; }; \
	test -s "$$scratch/recipe.json" || { echo "ERROR: recipe-emit produced no JSON" >&2; exit 1; }; \
	if env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" \
	     TD_BUILDER_PATH="$$Cb" TD_BUILDER_STORE="$$tdstore" TD_BUILDER_DB="$$bdb" \
	     "$$tb" build-recipe "$$scratch/recipe.json" "$$lock" "$$b" /var/guix/db/db.sqlite > "$$scratch/bout" 2>"$$scratch/err"; then :; \
	else echo "FAIL: build-recipe hello with stage0 (guix/Guile off PATH):" >&2; tail -20 "$$scratch/err" >&2; exit 1; fi; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	drv=`ls "$$b/"*.drv`; \
	grep -qF "$$Cb/bin/td-builder" "$$drv" || { echo "FAIL: hello's .drv does not name the stage0 builder $$Cb" >&2; exit 1; }; \
	if grep -qF "$$gtb/bin/td-builder" "$$drv"; then echo "FAIL: hello's .drv still references the guix-built td-builder $$gtb — the override did not take" >&2; exit 1; fi; \
	echo "  [STRUCTURAL] hello's .drv builder is the stage0 td path ($$Cb/bin/td-builder), NOT guix's td-builder; built with guix/Guile off PATH"; \
	ns="$$b/newstore/`basename "$$out"`"; \
	greet=`LD_LIBRARY_PATH="$$ns/lib" "$$ns/bin/hello"`; \
	test "$$greet" = "Hello, world!" || { echo "FAIL: stage0-built hello did not greet ('$$greet')" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the loop ran hello from td's own store output ($$ns/bin/hello) → '$$greet'"; \
	rm -rf "$$scratch/chk"; \
	"$$tb" check "$$drv" "$$b/closure.txt" "$$scratch/chk" >/dev/null 2>"$$scratch/chkerr" || { echo "FAIL: stage0-built hello NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/chkerr" >&2; exit 1; }; \
	echo "  [DURABLE intrinsic-reproducibility] td-builder check double-build agrees the stage0-built hello is reproducible (builder staged from td's own store both runs)"; \
	g=`$(GUIX) build hello 2>/dev/null | grep -v -- '-debug' | head -1 || true`; \
	test -n "$$g" || { echo "ERROR: could not resolve the guix hello oracle" >&2; exit 1; }; \
	test "$$out" != "$$g" || { echo "FAIL: td's hello path equals guix's — expected a distinct own-builder path" >&2; exit 1; }; \
	gg=`LD_LIBRARY_PATH="$$g/lib" "$$g/bin/hello"`; \
	test "$$gg" = "$$greet" || { echo "FAIL: guix's hello greeting ('$$gg') differs from td's ('$$greet')" >&2; exit 1; }; \
	echo "  [MIGRATION ORACLE] stage0-built hello is behaviorally == guix's hello (same greeting) at a DISTINCT path ($$out vs $$g)"; \
	rm -rf "$$scratch"; \
	echo "PASS: td placed its cargo-bootstrapped stage0 td-builder into its OWN content-addressed store (store-add-builder: tree restored, toolchain refs scanned against the daemon db, path+refs registered) and the loop BUILT hello with that stage0 as the drv's builder-of-record — assembled by td (no guix (derivation …)), realized daemon-free with the closure spanning td's builder db ∪ the seed db, guix/Guile SCRUBBED FROM PATH. The artifact greets (durable behavioral), is reproducible by td's own double-build (durable), and sits at a distinct store path from guix's hello while greeting identically (migration oracle, own-then-diverge). So the loop builds with a binary guix NEVER produced; the toolchain seed is the guix-built pin (§5, retired last)."
