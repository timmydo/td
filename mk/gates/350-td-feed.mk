# td-feed — td builds td-feed (its OWN local HTTP mirror of every network-downloaded
# artifact, feed/) FROM SOURCE via `td-builder build-recipe` (buildSystem "rust") by the
# td-bootstrapped stage0 (move-off-Guile §5), then proves the mirror works offline. td-feed
# shares td-fetch's vendored closure exactly (ureq + rustls/ring + sha2, 73 crates — only
# the bin name differs), so it reuses tests/td-feed.lock (== td-fetch's seed + .crate deps).
# The .drv is assembled by td (no guix (derivation …)) and realized daemon-free, guix/Guile
# SCRUBBED FROM PATH. The rustc/cargo/gcc seed is external (§5, retired last).
#
#   [DURABLE behavioral] the td-built td-feed `selftest` warms a one-entry index from a
#     loopback ORIGIN, serves it on a 2nd loopback port, and fetches it back THROUGH the
#     feed + sha256-verifies — the full warm->serve->fetch path, offline (std::net).
#   [SELF-DISCRIMINATION] that same selftest reds if a wrong index hash is accepted on warm
#     or a corrupted store byte is served (verify-on-serve) — the content hash is
#     load-bearing on BOTH the warm and the serve side.
#   [DURABLE structural] tests/td-feed.index is self-consistent: every line is
#     <path> <url> <sha256>, each sha256 is 64-hex, and no path repeats.
#   [DURABLE structural] the index is TRUTHFUL against the realized closure: for every
#     crate td-feed itself vendors (tests/td-feed.lock), the index's recorded sha256 equals
#     the realized .crate's content sha256 — the mirror would serve the daemon-verified FOD
#     bytes.
#   [DURABLE structural] the .drv builder is the td-bootstrapped stage0 (not the guix-built
#     td-builder); ts-emit ran under td's OWN td-ts-eval.
#   [DURABLE repro] td-builder check double-build agrees the build is reproducible.
HEAVY_GATES += td-feed
# A BUILD_GATE (like rust-fetch): ordered AFTER the parallel build-recipes phase so its
# 73-crate cargo build doesn't oversubscribe cores, and it depends on the td-ts-eval that
# build-recipes' prelude builds. Not in BUILD_SPECS — the source is interned at gate time.
BUILD_GATES += td-feed
td-feed:
	@echo ">> td-feed: td builds td-feed (its own local HTTP mirror, 73 vendored deps) from source via build-recipe (offline, guix/Guile off PATH); it warms+serves+fetches over loopback + is reproducible, and tests/td-feed.index is self-consistent + truthful"
	@set -euo pipefail; \
	tsgo=`sh tests/tsgo.sh`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo (the TS front-end compiler)" >&2; exit 1; }; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; tb="$$TB"; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] ts-emit evaluates with td's OWN td-ts-eval ($$TD_TS_EVAL)"; \
	lock0="$(CURDIR)/tests/td-feed.lock"; \
	test -s "$$lock0" || { echo "ERROR: no lock $$lock0" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock0" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	ncrate=`grep -cE '\.crate /gnu/store/' "$$lock0"`; \
	test "$$ncrate" -ge 70 || { echo "ERROR: lock has <70 vendored .crate deps ($$ncrate)" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/td-feed"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock0" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + vendored .crate deps" >&2; exit 1; }; \
	srcinfo=`sh tests/intern-src.sh "$$tb" td-feed-src "$(CURDIR)/feed" "$$scratch" target vendor .cargo` || { echo "ERROR: td could not intern the feed crate tree" >&2; exit 1; }; \
	eval "$$srcinfo"; \
	test -n "$$src" -a -d "$$srcstore/`basename "$$src"`" || { echo "ERROR: td interned no feed source tree" >&2; exit 1; }; \
	lock="$$scratch/td-feed.lock"; { cat "$$lock0"; echo "td-feed-source $$src"; } > "$$lock"; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-td-feed.ts" > "$$scratch/feed.json"; \
	test -s "$$scratch/feed.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/feed.json" "$$lock" "$$sd" /var/guix/db/db.sqlite "$$srcstore" "$$srcdb" > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe td-feed build:" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/td-feed" || { echo "FAIL: td-feed build produced no binary at $$ns/bin/td-feed" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_CRATES' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES" >&2; exit 1; }; \
	test -n "$$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset" >&2; exit 1; }; \
	grep -qF "$$TD_BUILDER_PATH/bin/td-builder" "$$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $$TD_BUILDER_PATH" >&2; exit 1; }; \
	echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($$TD_BUILDER_PATH)"; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — reused td's prior td-feed build: $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv ($$ncrate deps) with guix/Guile off PATH: $$out"; fi; \
	st=`"$$ns/bin/td-feed" selftest 2>"$$scratch/run.err"` || { echo "FAIL: the td-built td-feed failed its loopback warm->serve->fetch selftest:" >&2; tail -8 "$$scratch/run.err" >&2; exit 1; }; \
	echo "$$st" | grep -q '^td-feed: selftest OK' || { echo "FAIL: td-feed selftest did not report OK (got: $$st)" >&2; cat "$$scratch/run.err" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built td-feed warmed + served + fetched a blob over loopback (verify-on-warm + verify-on-serve): '$$st'"; \
	echo "  [SELF-DISCRIMINATION] that selftest also reds a wrong index hash (warm) and a corrupted store byte (serve) — verification is load-bearing on both sides"; \
	cps=`"$$ns/bin/td-feed" cargo-proxy-selftest 2>"$$scratch/cps.err"` || { echo "FAIL: the td-built td-feed cargo-proxy selftest failed:" >&2; tail -8 "$$scratch/cps.err" >&2; exit 1; }; \
	echo "$$cps" | grep -q '^td-feed: cargo-proxy selftest OK' || { echo "FAIL: cargo-proxy selftest did not report OK (got: $$cps)" >&2; cat "$$scratch/cps.err" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built td-feed cargo-proxy fetched + verified a crate THROUGH the proxy over loopback (cargo's sparse protocol): '$$cps'"; \
	echo "  [SELF-DISCRIMINATION] the cargo-proxy refuses a crate whose bytes mismatch its index cksum — the verifying egress is load-bearing"; \
	idx="$(CURDIR)/tests/td-feed.index"; \
	test -s "$$idx" || { echo "ERROR: no index $$idx" >&2; exit 1; }; \
	bad3=`grep -v '^#' "$$idx" | grep -vcE '^[^ ]+ [^ ]+ [^ ]+$$' || true`; \
	test "$$bad3" -eq 0 || { echo "FAIL: $$bad3 index line(s) are not <path> <url> <sha256>" >&2; exit 1; }; \
	badsha=`grep -v '^#' "$$idx" | cut -d' ' -f3 | grep -vcE '^[0-9a-f]{64}$$' || true`; \
	test "$$badsha" -eq 0 || { echo "FAIL: $$badsha index sha256 field(s) are not 64-hex" >&2; exit 1; }; \
	dup=`grep -v '^#' "$$idx" | cut -d' ' -f1 | sort | uniq -d | wc -l`; \
	test "$$dup" -eq 0 || { echo "FAIL: $$dup duplicate path(s) in the index" >&2; exit 1; }; \
	nidx=`grep -cv '^#' "$$idx"`; \
	echo "  [DURABLE structural] tests/td-feed.index self-consistent: $$nidx lines, all <path> <url> <sha256>, all sha256 64-hex, no duplicate path"; \
	checked=0; \
	for p in `grep -E '\.crate /gnu/store/' "$$lock0" | sed 's/^[^ ]* //'`; do \
	  nv=`basename "$$p" | sed -E 's/^[a-z0-9]+-//; s/\.crate$$//'`; \
	  isha=`grep -F "/$$nv.crate " "$$idx" | head -1 | cut -d' ' -f3`; \
	  test -n "$$isha" || { echo "FAIL: vendored crate $$nv is not in the index" >&2; exit 1; }; \
	  csha=`sha256sum "$$p" | cut -d' ' -f1`; \
	  test "$$isha" = "$$csha" || { echo "FAIL: index sha256 for $$nv ($$isha) != realized content ($$csha)" >&2; exit 1; }; \
	  checked=$$((checked+1)); \
	done; \
	echo "  [DURABLE structural] index is TRUTHFUL: all $$checked vendored crates' recorded sha256 == their realized .crate content (the mirror serves the daemon-verified FOD bytes)"; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: td-feed NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the td-feed build is reproducible"; \
	fi; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err" "$$scratch/run.err"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td built td-feed (its own local HTTP mirror) from source via td-builder build-recipe — the closure resolved from pinned static.crates.io fetches (no specification->package, no network), the .drv assembled + realized by td with its BUILDER the td-bootstrapped stage0 and guix/Guile SCRUBBED FROM PATH; the td-built td-feed warms+serves+fetches a blob over loopback and reds on a wrong/corrupted hash on BOTH the warm and serve side (durable behavioral + self-discrimination, offline); tests/td-feed.index is self-consistent and truthful against the realized closure (durable structural); and the build is reproducible by td's own double-build (durable). td now OWNS the mirror of its seeds; the external fetch runs in the network PREP (§5 warm-store-in)."
