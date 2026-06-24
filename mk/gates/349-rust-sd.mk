# rust-sd — td builds `sd` (the find-and-replace tool, 1.0.0) with its WHOLE crate closure
# (source + 111 deps) provisioned GUIX-FREE through td's OWN cargo-proxy: cargo resolved +
# fetched it through `td-feed cargo-proxy` (tools/warm-cargo-proxy.sh, host PREP), the proxy
# verifying each `.crate` sha256 == the crates.io index cksum (UPSTREAM pin, NOT a guix
# artifact); source + deps interned by store-add-recursive, vendored via TD_VENDOR_DIR.
# No guix oracle: the content-address (Cargo.lock pin == index cksum) is the oracle. Shared
# build+assert in tests/crate-free-build.sh. The rust/gcc toolchain seed stays guix-built
# (retired last).
#
#   [DURABLE supply-chain] every vendored crate's sha256 ∈ sd's shipped Cargo.lock.
#   [DURABLE structural] the .drv sets TD_VENDOR_DIR + references NO /gnu/store crate path.
#   [DURABLE behavioral] the td-built `sd` does a real find-and-replace (and leaves a non-match
#     unchanged) — real sd behavior, not just --version.
#   [DURABLE repro] td-builder check double-build agrees the 111-crate build is reproducible.
HEAVY_GATES += rust-sd
BUILD_GATES += rust-sd
rust-sd:
	@echo ">> rust-sd: td builds 'sd' (1.0.0, 111 deps) GUIX-FREE via the cargo-proxy (interned vendor tree, TD_VENDOR_DIR); sd find-and-replaces; reproducible; no guix build / no /gnu/store crate / no oracle"
	@set -euo pipefail; \
	tsgo=`sh tests/tsgo.sh`; test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: no tsgo" >&2; exit 1; }; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts" GUIX="$(GUIX)" ROOT="$(CURDIR)"; \
	nsout=`sh tests/crate-free-build.sh sd sd-1.0.0 tests/sd.lock sd-source tests/ts/recipe-sd.ts` || exit 1; \
	eval "$$nsout"; ns="$$NS"; \
	test -x "$$ns/bin/sd" || { echo "FAIL: no sd binary at $$ns/bin/sd" >&2; exit 1; }; \
	got=`printf 'hello world\n' | "$$ns/bin/sd" 'world' 'there'`; \
	test "$$got" = "hello there" || { echo "FAIL: td-built sd did not replace world->there (got: $$got)" >&2; exit 1; }; \
	unchanged=`printf 'hello world\n' | "$$ns/bin/sd" 'zzznomatch' 'X'`; \
	test "$$unchanged" = "hello world" || { echo "FAIL: sd altered input on a non-matching pattern (got: $$unchanged)" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built 'sd' (guix-free crates) replaced world->there (and left a non-match unchanged) — it works as sd"; \
	echo "PASS: rust-sd — sd (1.0.0) built with its 111-crate closure provisioned GUIX-FREE via td's cargo-proxy (Cargo.lock-pinned, sha == crates.io cksum, no guix build / no /gnu/store FOD), source+vendor interned by store-add-recursive, built via TD_VENDOR_DIR with guix off PATH; sd find-and-replaces; reproducible. NO oracle (content-address = the upstream pin). Toolchain seed retired last."
