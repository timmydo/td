# rust-procs — td builds `procs` (a modern `ps` replacement) FROM SOURCE, continuing the
# shipped Rust userland (procs/fd/ripgrep/sd/eza/bat, PR #80) from guix-packaged to td-built
# (the uutils-`cat` pattern; move-off-Guile §5). `procs` 0.14.10 is built via `td-builder
# build-recipe` (buildSystem "rust") with its FULL 297-crate closure vendored as fixed-output
# static.crates.io fetches (sha256 == each Cargo.lock checksum), pinned in tests/procs.lock.
# Pure Rust, no C build. The .drv is td-assembled (no guix (derivation …)) + realized
# daemon-free (no guix-daemon), guix/Guile SCRUBBED FROM PATH. The rustc/cargo/gcc seed is
# external (§5). Auto-classified self-hosted by buildSystem (no census enrollment).
#
# ALL-DURABLE (no guix oracle): each leg stands with no Guix in the room:
#   [STRUCTURAL] the build runs guix/Guile off PATH, the .drv carries TD_VENDOR_CRATES and its
#     builder is the td-bootstrapped stage0.
#   [DURABLE behavioral] the built `procs` enumerates /proc and lists running processes — real
#     procs behavior, not just --version.
#   [DURABLE repro] td-builder check's double-build agrees the build is reproducible across the
#     whole 297-crate graph.
HEAVY_GATES += rust-procs
# Self-contained lock (tests/procs.lock) — built by the gate itself; ordered after
# build-recipes (a BUILD_GATE) so its cargo build doesn't oversubscribe the corpus fan-out.
BUILD_GATES += rust-procs
rust-procs:
	@echo ">> rust-procs: td builds 'procs' (0.14.10, 297 vendored deps) from source via build-recipe (offline, guix/Guile off PATH); it lists processes + is reproducible"
	@set -euo pipefail; \
	tsgo=`sh tests/tsgo.sh`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; tb="$$TB"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] ts-emit evaluates with td's OWN td-ts-eval ($$TD_TS_EVAL) — not the guix-built one (brick 4c)"; \
	lock="$(CURDIR)/tests/procs.lock"; \
	test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	ncrate=`grep -cE '\.crate /gnu/store/' "$$lock"`; \
	test "$$ncrate" -ge 250 || { echo "ERROR: lock has <250 vendored .crate deps ($$ncrate) — regenerate from procs' Cargo.lock" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/rust-procs"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + source + vendored .crate deps (warm static.crates.io fetches; regenerate the lock on a channel/dep bump)" >&2; exit 1; }; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-procs.ts" > "$$scratch/procs.json"; \
	test -s "$$scratch/procs.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/procs.json" "$$lock" "$$sd" /var/guix/db/db.sqlite > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe procs build (guix/Guile off PATH):" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/procs" || { echo "FAIL: procs build produced no 'procs' binary at $$ns/bin/procs" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_CRATES' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES — the vendored path was not taken" >&2; exit 1; }; \
	test -n "$$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; exit 1; }; \
	grep -qF "$$TD_BUILDER_PATH/bin/td-builder" "$$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $$TD_BUILDER_PATH — built by the wrong td-builder?" >&2; exit 1; }; \
	echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($$TD_BUILDER_PATH) — not the guix-built td-builder (brick 3b)"; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — recipe unchanged, reused td's prior procs build (no rebuild): $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv (TD_VENDOR_CRATES, $$ncrate deps) with guix/Guile off PATH: $$out"; fi; \
	"$$ns/bin/procs" --version >/dev/null 2>&1 || { echo "FAIL: td-built procs --version failed — the binary does not run" >&2; exit 1; }; \
	ptab=`"$$ns/bin/procs" </dev/null 2>/dev/null || true`; \
	printf '%s\n' "$$ptab" | grep -qiE 'PID|Command' || { echo "FAIL: td-built procs produced no process-table header reading /proc (first line: $$(printf '%s\n' "$$ptab" | head -1))" >&2; exit 1; }; \
	nrows=`printf '%s\n' "$$ptab" | grep -cE '^[[:space:]]*[0-9]+' || true`; \
	echo "  [DURABLE behavioral] the td-built 'procs' ran (--version) and read /proc into a process table (PID/Command columns, $$nrows process rows) — it works as procs"; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: rust-procs NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the 297-crate procs build is reproducible"; \
	fi; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td built 'procs' (0.14.10) from source via td-builder build-recipe — the full 297-crate dependency closure + the crate source resolved from pinned static.crates.io fetches (no specification->package, no network), the cargo vendor dir assembled by td's run_rust, the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon), with guix/Guile SCRUBBED FROM PATH; the binary works as procs (enumerates /proc, lists processes, durable) and is reproducible by td's own double-build across the whole graph (durable). Another shipped Rust userland tool, built from source by td. The rustc/cargo/gcc seed + locked deps stay external (§5, retired last)."
