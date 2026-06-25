# td-subst — td builds td-subst (its OWN substitute / binary-cache server, subst/) FROM
# SOURCE via `td-builder build-recipe` (buildSystem "rust") by the td-bootstrapped stage0
# (move-off-Guile §5), then proves the substitute protocol end-to-end OFFLINE. td-subst
# shares td-feed/td-fetch's vendored closure exactly (ureq + rustls/ring + sha2; subst adds
# `ring` as a direct dep, already in the closure, so subst/Cargo.lock pins td-feed's exact
# versions), so it reuses tests/td-subst.lock (== td-fetch's seed + .crate deps). The .drv
# is assembled by td (no guix (derivation …)) and realized daemon-free, guix/Guile SCRUBBED
# FROM PATH. The rustc/cargo/gcc seed is external (§5, retired last).
#
#   [DURABLE behavioral] the td-built td-subst `selftest` keygens, signs + serves a one-entry
#     export dir on loopback, fetches it back + verifies (ed25519 signature + NarHash) — the
#     full keygen->sign->serve->fetch->verify path, offline (std::net + ring).
#   [SELF-DISCRIMINATION] that same selftest reds if a tampered narinfo, a corrupted nar, or a
#     WRONG public key is accepted — signature AND content-hash are load-bearing.
#   [DURABLE behavioral] END-TO-END "fetch, don't build": the td-bootstrapped stage0 PLACES a
#     path into a td store + registers it, the td-built td-subst exports + signs + serves it on
#     loopback, td FETCHES it back (verifying signature + NarHash) and RESTORES it (nar-restore)
#     to a tree BYTE-IDENTICAL to the original — a path obtained WITHOUT building it. A tampered
#     narinfo reds the fetch (the consumer falls back to building).
#   [DURABLE structural] the .drv builder is the td-bootstrapped stage0 (not the guix-built
#     td-builder); ts-emit ran under td's OWN td-ts-eval.
#   [DURABLE repro] td-builder check double-build agrees the td-subst build is reproducible.
HEAVY_GATES += td-subst
# A BUILD_GATE (like td-feed): ordered AFTER the parallel build-recipes phase so its cargo
# build doesn't oversubscribe cores, and it depends on the td-ts-eval that build-recipes'
# prelude builds. Not in BUILD_SPECS — the source is interned at gate time.
BUILD_GATES += td-subst
td-subst:
	@echo ">> td-subst: td builds td-subst (its own substitute server) from source via build-recipe (offline, guix/Guile off PATH); it selftests the signed serve/fetch over loopback, proves fetch-don't-build end-to-end byte-identical, and is reproducible"
	@set -euo pipefail; \
	tsgo=`sh tests/tsgo.sh`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo (the TS front-end compiler)" >&2; exit 1; }; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; tb="$$TB"; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] ts-emit evaluates with td's OWN td-ts-eval ($$TD_TS_EVAL)"; \
	lock0="$(CURDIR)/tests/td-subst.lock"; \
	test -s "$$lock0" || { echo "ERROR: no lock $$lock0" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock0" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	ncrate=`grep -cE '\.crate /gnu/store/' "$$lock0"`; \
	test "$$ncrate" -ge 70 || { echo "ERROR: lock has <70 vendored .crate deps ($$ncrate)" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/td-subst"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock0" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + vendored .crate deps" >&2; exit 1; }; \
	srcinfo=`sh tests/intern-src.sh "$$tb" td-subst-src "$(CURDIR)/subst" "$$scratch" target vendor .cargo` || { echo "ERROR: td could not intern the subst crate tree" >&2; exit 1; }; \
	eval "$$srcinfo"; \
	test -n "$$src" -a -d "$$srcstore/`basename "$$src"`" || { echo "ERROR: td interned no subst source tree" >&2; exit 1; }; \
	lock="$$scratch/td-subst.lock"; { cat "$$lock0"; echo "td-subst-source $$src"; } > "$$lock"; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-td-subst.ts" > "$$scratch/subst.json"; \
	test -s "$$scratch/subst.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/subst.json" "$$lock" "$$sd" /var/guix/db/db.sqlite "$$srcstore" "$$srcdb" > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe td-subst build:" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/td-subst" || { echo "FAIL: td-subst build produced no binary at $$ns/bin/td-subst" >&2; exit 1; }; \
	ts="$$ns/bin/td-subst"; \
	grep -q 'TD_VENDOR_CRATES' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES" >&2; exit 1; }; \
	test -n "$$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset" >&2; exit 1; }; \
	grep -qF "$$TD_BUILDER_PATH/bin/td-builder" "$$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $$TD_BUILDER_PATH" >&2; exit 1; }; \
	echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($$TD_BUILDER_PATH)"; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — reused td's prior td-subst build: $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv ($$ncrate deps) with guix/Guile off PATH: $$out"; fi; \
	st=`"$$ts" selftest 2>"$$scratch/run.err"` || { echo "FAIL: the td-built td-subst failed its loopback selftest:" >&2; tail -8 "$$scratch/run.err" >&2; exit 1; }; \
	echo "$$st" | grep -q '^td-subst: selftest OK' || { echo "FAIL: td-subst selftest did not report OK (got: $$st)" >&2; cat "$$scratch/run.err" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built td-subst keygen+sign+serve+fetch+verify round-trip over loopback: '$$st'"; \
	echo "  [SELF-DISCRIMINATION] that selftest also reds a tampered narinfo, a corrupted nar, and a wrong public key — signature + NarHash are load-bearing"; \
	e2e="$$scratch/e2e"; rm -rf "$$e2e"; mkdir -p "$$e2e/store" "$$e2e/served" "$$e2e/fetch" "$$e2e/restored"; \
	printf 'td substitute end-to-end payload\n' > "$$e2e/content"; \
	path=`env -i PATH="$$cu/bin" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" store-add-text td-subst-e2e "$$e2e/content" "$$e2e/store" "$$e2e/td.db"`; \
	base=`basename "$$path"`; \
	env -i PATH="$$cu/bin" "$$tb" subst-export "$$e2e/td.db" "$$e2e/store" "$$e2e/served" "$$path" >/dev/null || { echo "FAIL: subst-export" >&2; exit 1; }; \
	test -f "$$e2e/served/$$base.narinfo" || { echo "FAIL: subst-export wrote no narinfo for $$base" >&2; exit 1; }; \
	"$$ts" keygen "$$e2e/priv" "$$e2e/pub" >/dev/null; \
	"$$ts" sign "$$e2e/served" "$$e2e/priv" >/dev/null; \
	grep -q '^Sig: ' "$$e2e/served/$$base.narinfo" || { echo "FAIL: td-subst sign did not sign the narinfo" >&2; exit 1; }; \
	"$$ts" serve "$$e2e/served" 127.0.0.1:0 > "$$e2e/serve.log" 2>&1 & spid=$$!; \
	trap 'kill $$spid 2>/dev/null || true' EXIT; \
	port=""; for i in `seq 1 100`; do port=`sed -n 's#.*http://127.0.0.1:\([0-9]*\)/.*#\1#p' "$$e2e/serve.log" 2>/dev/null`; [ -n "$$port" ] && break; sleep 0.1; done; \
	test -n "$$port" || { echo "FAIL: td-subst serve never bound a loopback port" >&2; cat "$$e2e/serve.log" >&2; exit 1; }; \
	"$$ts" fetch "http://127.0.0.1:$$port" "$$base" "$$e2e/fetch" "$$e2e/pub" >/dev/null || { echo "FAIL: td-subst fetch (verify) failed" >&2; cat "$$e2e/serve.log" >&2; exit 1; }; \
	narfile=`grep '^NarFile: ' "$$e2e/fetch/$$base.narinfo" | cut -d' ' -f2`; \
	env -i PATH="$$cu/bin" "$$tb" nar-restore "$$e2e/fetch/$$narfile" "$$e2e/restored/$$base" >/dev/null || { echo "FAIL: nar-restore the fetched substitute" >&2; exit 1; }; \
	cmp -s "$$e2e/content" "$$e2e/restored/$$base" || { echo "FAIL: the FETCHED+restored path differs from the original (not byte-identical)" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] FETCH-DON'T-BUILD: td placed $$base, the td-built td-subst signed+served it, and td fetched+restored it BYTE-IDENTICAL over loopback — a path obtained without building it"; \
	sed -i 's/td-subst-e2e/td-subst-XXXX/' "$$e2e/served/$$base.narinfo"; \
	if "$$ts" fetch "http://127.0.0.1:$$port" "$$base" "$$e2e/fetch2" "$$e2e/pub" >/dev/null 2>&1; then echo "FAIL: fetch ACCEPTED a tampered narinfo — the signature is not load-bearing" >&2; exit 1; fi; \
	echo "  [SELF-DISCRIMINATION] a tampered narinfo reds the fetch (the consumer falls back to building)"; \
	kill $$spid 2>/dev/null || true; trap - EXIT; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: td-subst NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the td-subst build is reproducible"; \
	fi; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err" "$$scratch/run.err" "$$e2e"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td built td-subst (its own substitute server) from source via td-builder build-recipe — the closure resolved from pinned static.crates.io fetches (no specification->package, no network), the .drv assembled + realized by td with its BUILDER the td-bootstrapped stage0 and guix/Guile SCRUBBED FROM PATH; the td-built td-subst signs+serves+fetches+verifies over loopback and reds on a tampered narinfo / corrupted nar / wrong key (durable behavioral + self-discrimination, offline); it proves FETCH-DON'T-BUILD end-to-end (td placed a path, td-subst served it signed, td fetched+restored it BYTE-IDENTICAL without building it); and the build is reproducible by td's own double-build (durable). td now OWNS a substitute server for its built outputs; the external fetch of seeds runs in the network PREP (§5)."
