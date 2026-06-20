# rust-fetch — td builds td-fetch (its OWN seed fetcher, fetch/) from source via
# `td-builder build-recipe` (buildSystem "rust") by the td-bootstrapped stage0
# (move-off-Guile §5). td-fetch is a small vendored-Rust HTTP(S)+sha256 client (ureq +
# rustls/ring + sha2, 73 crates) that GETs a pinned blob and verifies its sha256 — the
# capability that replaces guix url-fetch as the FETCHER of the pinned fixed-output
# seeds (the tsgo tarball, the `.crate` deps, source tarballs). The closure is vendored
# as static.crates.io fixed-output fetches (tests/td-fetch.lock, Cargo.lock-pinned,
# TD_VENDOR_CRATES); ring's C build script is served by run_rust's C set-paths (the
# gcc-toolchain seed, as for russh's aws-lc). The .drv is assembled by td (no guix
# (derivation …)) and realized daemon-free (no guix-daemon), guix/Guile SCRUBBED FROM
# PATH. The rustc/cargo/gcc seed is external (§5, retired last).
#
# Proven on the REAL tsgo tarball (the warm guix `td-tsgo-tarball` origin):
#   [DURABLE structural] the .drv builder is the stage0 path; the .drv carries
#     TD_VENDOR_CRATES; ts-emit evaluates with td's OWN td-ts-eval (brick 4c).
#   [DURABLE behavioral] the td-built td-fetch round-trips the tsgo tarball over a
#     self-contained LOOPBACK HTTP server (127.0.0.1, std::net) and verifies its
#     sha256 — the full fetch+verify path works, offline (like rust-russh's loopback).
#   [SELF-DISCRIMINATION] a wrong sha256 reds the selftest — the verification is
#     load-bearing (the input hash is not decorative).
#   [MIGRATION ORACLE, removable] td-fetch's verified sha256 == guix's `td-tsgo-tarball`
#     origin pin (`guix hash` base32 == the pin in system/td-ts.scm) — own, then diverge.
#   [DURABLE repro] td-builder check double-build agrees the build is reproducible.
HEAVY_GATES += rust-fetch
# Ordered AFTER the parallel build-recipes phase (its 73-crate cargo build, incl. ring's
# C crypto, would otherwise oversubscribe cores). Not in BUILD_SPECS — the source is
# interned at gate time by td's OWN recursive addToStore (tests/intern-src.sh).
BUILD_GATES += rust-fetch
rust-fetch:
	@echo ">> rust-fetch: td builds td-fetch (its own seed fetcher, 73 vendored deps incl. ring TLS) from source via build-recipe (offline, guix/Guile off PATH); it round-trips + verifies the real tsgo tarball over loopback + is reproducible"
	@set -euo pipefail; \
	tgz=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-tsgo-tarball)'`; tsgo=`sh tests/tsgo.sh "$$tgz"`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" -a -s "$$tgz" || { echo "ERROR: could not resolve td-tsgo / the tsgo tarball" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; tb="$$TB"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] ts-emit evaluates with td's OWN td-ts-eval ($$TD_TS_EVAL) — not the guix-built one (brick 4c)"; \
	lock0="$(CURDIR)/tests/td-fetch.lock"; \
	test -s "$$lock0" || { echo "ERROR: no lock $$lock0" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock0" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	ncrate=`grep -cE '\.crate /gnu/store/' "$$lock0"`; \
	test "$$ncrate" -ge 70 || { echo "ERROR: lock has <70 vendored .crate deps ($$ncrate) — regenerate from fetch/Cargo.lock" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/rust-fetch"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock0" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + vendored .crate deps (warm static.crates.io fetches; regenerate the lock on a channel/dep bump)" >&2; exit 1; }; \
	srcinfo=`sh tests/intern-src.sh "$$tb" td-fetch-src "$(CURDIR)/fetch" "$$scratch" target vendor .cargo` || { echo "ERROR: td could not intern the fetch crate tree (store-add-recursive)" >&2; exit 1; }; \
	eval "$$srcinfo"; \
	test -n "$$src" -a -d "$$srcstore/`basename "$$src"`" || { echo "ERROR: td interned no fetch source tree (store-add-recursive)" >&2; exit 1; }; \
	lock="$$scratch/td-fetch.lock"; { cat "$$lock0"; echo "td-fetch-source $$src"; } > "$$lock"; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-td-fetch.ts" > "$$scratch/fetch.json"; \
	test -s "$$scratch/fetch.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/fetch.json" "$$lock" "$$sd" /var/guix/db/db.sqlite "$$srcstore" "$$srcdb" > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe td-fetch build (guix/Guile off PATH):" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/td-fetch" || { echo "FAIL: td-fetch build produced no binary at $$ns/bin/td-fetch" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_CRATES' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES — the vendored path was not taken" >&2; exit 1; }; \
	test -n "$$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; exit 1; }; \
	grep -qF "$$TD_BUILDER_PATH/bin/td-builder" "$$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $$TD_BUILDER_PATH — built by the wrong td-builder?" >&2; exit 1; }; \
	echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($$TD_BUILDER_PATH) — not the guix-built td-builder (cargo→stage0→td-fetch)"; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — recipe unchanged, reused td's prior td-fetch build (no rebuild): $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv (TD_VENDOR_CRATES, $$ncrate deps) with guix/Guile off PATH: $$out"; fi; \
	want=`sha256sum "$$tgz" | cut -d' ' -f1`; \
	test -n "$$want" || { echo "ERROR: could not compute the tsgo tarball sha256" >&2; exit 1; }; \
	got=`"$$ns/bin/td-fetch" selftest "$$tgz" "$$want" 2>"$$scratch/run.err"` || { echo "FAIL: the td-built td-fetch failed the loopback round-trip of the tsgo tarball:" >&2; tail -5 "$$scratch/run.err" >&2; exit 1; }; \
	echo "$$got" | grep -q '^td-fetch: loopback round-trip OK' || { echo "FAIL: td-fetch selftest did not report a round-trip (got: $$got)" >&2; cat "$$scratch/run.err" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built td-fetch served + fetched + sha256-verified the REAL tsgo tarball (sha256 $$want) over loopback HTTP: '$$got'"; \
	bad=0000000000000000000000000000000000000000000000000000000000000000; \
	if "$$ns/bin/td-fetch" selftest "$$tgz" "$$bad" >/dev/null 2>&1; then echo "FAIL: td-fetch selftest ACCEPTED a wrong sha256 ($$bad) — the verification is not load-bearing" >&2; exit 1; fi; \
	echo "  [SELF-DISCRIMINATION] a perturbed sha256 ($$bad) reds the selftest — the content hash is load-bearing"; \
	gh=`$(GUIX) hash "$$tgz"`; \
	grep -qF "$$gh" "$(CURDIR)/system/td-ts.scm" || { echo "FAIL: the tsgo tarball hash $$gh is not the pin in system/td-ts.scm" >&2; exit 1; }; \
	echo "  [MIGRATION ORACLE, removable] td-fetch verified the SAME content guix pins: guix-hash($$tgz)=$$gh == the td-tsgo-tarball origin pin in system/td-ts.scm"; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: rust-fetch NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the 73-crate td-fetch build (incl. ring's C TLS) is reproducible"; \
	fi; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err" "$$scratch/run.err"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td built td-fetch (its own seed fetcher: ureq + rustls/ring + sha2, 73 vendored crate deps) from source via td-builder build-recipe — the dependency closure resolved from pinned static.crates.io fetches (no specification->package, no network), the cargo vendor dir assembled by td's run_rust, ring's C TLS build served by run_rust's C set-paths (no extra seed), the fetch/ source interned by td's own recursive addToStore, the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon) with its BUILDER the td-bootstrapped stage0 and guix/Guile SCRUBBED FROM PATH; the td-built td-fetch round-trips + sha256-verifies the REAL tsgo tarball over a self-contained loopback HTTP server (durable behavioral, offline), reds on a perturbed hash (self-discrimination), verifies the SAME content guix's td-tsgo-tarball origin pins (migration oracle), and is reproducible by td's own double-build (durable). td now OWNS fetch+verify of its pinned seeds; the external TLS fetch runs in the network PREP (§5 warm-store-in), and the guix origin stays as the seed+oracle (own, then diverge)."
