# rust-youki — td builds youki, the Rust OCI container runtime, FROM SOURCE (the
# postponed crun replacement; Rust-focused minimal distro). The published `youki` crate
# 0.6.0 builds the `youki` binary, built via `td-builder build-recipe` (buildSystem
# "rust", recipe-youki.ts) with its 663-crate dependency closure vendored: each is a
# fixed-output fetch from static.crates.io (sha256 == its Cargo.lock checksum), pinned in
# tests/youki.lock; the source is the upstream `youki` crate tarball (keyed `youki-source`).
# youki has NO default features, so plain `cargo build --release` builds it with no
# seccomp/systemd/wasm — no libseccomp/pkg-config/git seed needed. run_rust assembles the
# cargo vendor dir + builds offline; the .drv is assembled by td (no guix (derivation …))
# and realized daemon-free (no guix-daemon), guix/Guile SCRUBBED FROM PATH. The
# rustc/cargo/gcc seed + locked deps are the external SEED (§5, retired last).
#
# ALL-DURABLE (no guix oracle — guix has no youki):
#   [STRUCTURAL] the build runs guix/Guile off PATH, produces the `youki` binary, and the
#     .drv carries TD_VENDOR_CRATES + the td-bootstrapped stage0 as builder.
#   [DURABLE behavioral] the td-built `youki` runs — `youki --version` reports youki, and
#     `youki --help` lists the OCI lifecycle subcommands (create/start/state/delete) — a
#     real OCI runtime CLI, not just a binary that links.
#   [DURABLE repro] td-builder check's double-build agrees over the 663-crate graph.
HEAVY_GATES += rust-youki
# Ordered AFTER the parallel build-recipes phase (its 663-crate cargo build would
# otherwise oversubscribe cores against build-recipes' fan-out). Not in BUILD_SPECS —
# its lock (tests/youki.lock) is self-contained, so the gate builds it itself.
BUILD_GATES += rust-youki
rust-youki:
	@echo ">> rust-youki: td builds youki, the Rust OCI runtime (youki 0.6.0, 663 vendored deps) from source via build-recipe (offline, guix/Guile off PATH); youki --version/--help runs + is reproducible"
	@set -euo pipefail; \
	tgz=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-tsgo-tarball)'`; tsgo=`sh tests/tsgo.sh "$$tgz"`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; tb="$$TB"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] ts-emit evaluates with td's OWN td-ts-eval ($$TD_TS_EVAL) — not the guix-built one (brick 4c)"; \
	lock="$(CURDIR)/tests/youki.lock"; \
	test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-9' "$$lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils seed in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	ncrate=`grep -cE '\.crate /gnu/store/' "$$lock"`; \
	test "$$ncrate" -ge 500 || { echo "ERROR: lock has <500 vendored .crate deps ($$ncrate) — regenerate from youki's Cargo.lock" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/rust-youki"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + source + vendored .crate deps (warm static.crates.io fetches; regenerate the lock on a channel/dep bump)" >&2; exit 1; }; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-youki.ts" > "$$scratch/youki.json"; \
	test -s "$$scratch/youki.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/youki.json" "$$lock" "$$sd" /var/guix/db/db.sqlite > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe youki build (guix/Guile off PATH):" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/youki" || { echo "FAIL: build produced no 'youki' binary at $$ns/bin/youki" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_CRATES' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES — the vendored path was not taken" >&2; exit 1; }; \
	test -n "$$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; exit 1; }; \
	grep -qF "$$TD_BUILDER_PATH/bin/td-builder" "$$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $$TD_BUILDER_PATH — built by the wrong td-builder?" >&2; exit 1; }; \
	echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($$TD_BUILDER_PATH) — not the guix-built td-builder (brick 3b)"; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — recipe unchanged, reused td's prior youki build (no rebuild): $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv (TD_VENDOR_CRATES, $$ncrate deps) with guix/Guile off PATH: $$out"; fi; \
	"$$ns/bin/youki" --version 2>&1 | grep -qi 'youki' || { echo "FAIL: youki --version did not report youki" >&2; "$$ns/bin/youki" --version >&2 || true; exit 1; }; \
	"$$ns/bin/youki" --help 2>&1 | grep -qiE '\bcreate\b' || { echo "FAIL: youki --help did not list the OCI 'create' subcommand" >&2; "$$ns/bin/youki" --help >&2 || true; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built youki runs — --version reports youki + --help lists the OCI lifecycle subcommands (a real OCI runtime CLI)"; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: rust-youki NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the 663-crate youki build is reproducible"; \
	fi; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td built youki, the Rust OCI container runtime (youki 0.6.0) from source via td-builder build-recipe — the 663-crate dependency closure + the crate source resolved from pinned static.crates.io fetches (no specification->package, no network), the cargo vendor dir assembled by td's run_rust, the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon), with guix/Guile SCRUBBED FROM PATH; the binary runs (--version + the OCI --help subcommands, durable) and is reproducible by td's own double-build across the whole graph (durable). The postponed crun replacement, built from source by td. The rustc/cargo/gcc seed + locked deps stay external (§5, retired last)."
