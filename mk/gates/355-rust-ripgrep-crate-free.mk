# rust-ripgrep-crate-free — td builds `ripgrep` (rg 14.1.1) FROM SOURCE with its WHOLE crate
# closure (source crate + 57 deps) provisioned GUIX-FREE through td's OWN cargo-proxy: cargo
# resolved + fetched the closure through `td-feed cargo-proxy` (tools/warm-cargo-proxy.sh, host
# PREP), the proxy verifying each `.crate` sha256 == the crates.io sparse-index cksum (the
# UPSTREAM pin, NOT a guix artifact). The source + the dep set are interned by td's OWN
# store-add-recursive and build-recipe vendors from them (TD_VENDOR_DIR) — so NOTHING in the
# crate path is guix. This is the PoC that scales the guix-free crate path (#163: engine +
# cargo-proxy) to a real corpus rust package. Contrast rust-ripgrep (347), which realizes the
# source + 57 deps via `guix build /gnu/store/<hash>.crate` (the guix-daemon FOD).
#
# Per the human (2026-06-23, "no new guix dependencies, even an oracle"): crates are
# content-addressed, so the correctness oracle is the UPSTREAM Cargo.lock checksum (== the
# crates.io index cksum the proxy verified), NOT a guix differential. The rust/gcc toolchain
# seed stays guix-built (retired last by source-bootstrap).
#
#   [DURABLE supply-chain] every vendored crate's sha256 is a checksum pinned in ripgrep's OWN
#     shipped Cargo.lock (== the upstream crates.io cksum) — the guix-free equivalence oracle.
#   [DURABLE structural] the .drv sets TD_VENDOR_DIR and references NO `/gnu/store` crate path;
#     source + vendor tree are td-interned (store-add-recursive); guix/Guile off PATH.
#   [DURABLE behavioral] the td-built `rg` finds a pattern line in a tree (and not an unrelated
#     file) — real ripgrep behavior, not just --version.
#   [DURABLE repro] td-builder check double-build agrees the 57-crate build is reproducible.
HEAVY_GATES += rust-ripgrep-crate-free
BUILD_GATES += rust-ripgrep-crate-free
rust-ripgrep-crate-free:
	@echo ">> rust-ripgrep-crate-free: td builds 'ripgrep' (rg 14.1.1, 57 deps) with its crate closure provisioned GUIX-FREE (cargo-proxy + interned vendor tree, TD_VENDOR_DIR), no guix build / no /gnu/store crate / no oracle"
	@set -euo pipefail; \
	dest="$(CURDIR)/.td-build-cache/crate-vendor/ripgrep"; \
	srctree="$$dest/src/ripgrep-14.1.1"; vendor="$$dest/vendor"; cargolock="$$srctree/Cargo.lock"; \
	test -f "$$srctree/Cargo.toml" || { echo "ERROR: no ripgrep source tree at $$srctree — the HOST PREP tools/warm-cargo-proxy.sh (check.sh prelude) must provision it first (offline gate cannot egress)" >&2; exit 1; }; \
	test -f "$$cargolock" || { echo "ERROR: source $$srctree ships no Cargo.lock" >&2; exit 1; }; \
	ncrate=`ls "$$vendor"/*.crate 2>/dev/null | wc -l`; \
	test "$$ncrate" -ge 50 || { echo "ERROR: vendor dir $$vendor has <50 crates ($$ncrate) — re-run the HOST PREP tools/warm-cargo-proxy.sh ripgrep 14.1.1" >&2; exit 1; }; \
	miss=0; for c in "$$vendor"/*.crate; do sha=`sha256sum "$$c" | cut -d' ' -f1`; grep -qF "$$sha" "$$cargolock" || { echo "FAIL: crate `basename $$c` sha $$sha is NOT pinned in ripgrep's Cargo.lock" >&2; miss=$$((miss + 1)); }; done; \
	test "$$miss" -eq 0 || { echo "FAIL: $$miss vendored crate(s) not pinned by ripgrep's Cargo.lock" >&2; exit 1; }; \
	echo "  [DURABLE supply-chain] all $$ncrate vendored crates' sha256 are checksums pinned in ripgrep's shipped Cargo.lock (== upstream crates.io cksum the cargo-proxy verified — the guix-free oracle)"; \
	tsgo=`sh tests/tsgo.sh`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: no tsgo" >&2; exit 1; }; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; tb="$$TB"; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	lock="$(CURDIR)/tests/ripgrep.lock"; \
	test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	scratch="$(CURDIR)/.td-build-cache/rust-ripgrep-crate-free"; rm -rf "$$scratch"; mkdir -p "$$scratch/tmp" "$$scratch/sd"; \
	grep -v '\.crate ' "$$lock" | grep -v '^ripgrep-source ' | grep ' /gnu/store/' | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the toolchain seed" >&2; exit 1; }; \
	srcinfo=`sh tests/intern-src.sh "$$tb" ripgrep-src "$$srctree" "$$scratch/src" target vendor .cargo` || { echo "ERROR: intern source failed" >&2; exit 1; }; \
	eval "$$srcinfo"; \
	vinfo=`sh tests/intern-src.sh "$$tb" ripgrep-vendor "$$vendor" "$$scratch/vendor"` || { echo "ERROR: intern vendor tree failed" >&2; exit 1; }; \
	vsrc=`echo "$$vinfo" | sed -n "s/^src='\(.*\)'/\1/p"`; \
	vstore=`echo "$$vinfo" | sed -n "s/^srcstore='\(.*\)'/\1/p"`; \
	vdb=`echo "$$vinfo" | sed -n "s/^srcdb='\(.*\)'/\1/p"`; \
	test -n "$$vsrc" -a -n "$$vstore" -a -n "$$vdb" || { echo "ERROR: vendor intern produced no path" >&2; exit 1; }; \
	echo "  [DURABLE structural] td interned the source + the 57-crate set as content-addressed trees (store-add-recursive, no daemon): vendor $$vsrc"; \
	seedlock="$$scratch/seed.lock"; { grep -v '\.crate ' "$$lock" | grep -v '^ripgrep-source '; echo "ripgrep-source $$src"; } > "$$seedlock"; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-ripgrep.ts" > "$$scratch/rg.json"; \
	test -s "$$scratch/rg.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/rg.json" "$$seedlock" "$$sd" /var/guix/db/db.sqlite "$$srcstore" "$$srcdb" "$$vsrc" "$$vstore" "$$vdb" > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe (guix-free crates):" >&2; tail -40 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/rg" || { echo "FAIL: no rg binary at $$ns/bin/rg" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_DIR' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_DIR" >&2; exit 1; }; \
	if grep -oqE '/gnu/store/[a-z0-9]+-[^ /]+\.crate' "$$sd"/*.drv; then echo "FAIL: the .drv references a /gnu/store crate path (not guix-free)" >&2; exit 1; fi; \
	echo "  [DURABLE structural] the .drv sets TD_VENDOR_DIR and references NO /gnu/store crate path — crates are guix-free: $$out"; \
	tree="$$scratch/tree"; rm -rf "$$tree"; mkdir -p "$$tree/sub"; printf 'alpha line\nthe needle is here\nbeta line\n' > "$$tree/sub/hay.txt"; printf 'nothing to see\n' > "$$tree/other.txt"; \
	found=`"$$ns/bin/rg" needle "$$tree"`; \
	echo "$$found" | grep -q 'needle' || { echo "FAIL: td-built rg did not find the 'needle' line (got: $$found)" >&2; exit 1; }; \
	echo "$$found" | grep -q 'other.txt' && { echo "FAIL: td-built rg matched the unrelated file (over-match)" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built 'rg' (guix-free crates) found the 'needle' line (and not the unrelated file) — it works as ripgrep"; \
	rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	  || { echo "FAIL: NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	  || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	echo "  [DURABLE repro] td-builder check double-build agrees the guix-free-crate ripgrep build is reproducible"; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err" "$$tree"; \
	echo "PASS: rust-ripgrep-crate-free — td built 'ripgrep' (rg 14.1.1) with its 57-crate closure provisioned GUIX-FREE: cargo resolved + fetched it through td's cargo-proxy (sha == ripgrep's Cargo.lock pin == crates.io index cksum, no guix build / no /gnu/store FOD), the source + dep set interned as content-addressed trees by store-add-recursive, vendored via TD_VENDOR_DIR, built by stage0 with guix off PATH; the .drv has no /gnu/store crate path; rg greps a needle; reproducible. A real corpus rust package built guix-free with NO oracle (content-address = the upstream Cargo.lock pin). Toolchain seed retired last."
