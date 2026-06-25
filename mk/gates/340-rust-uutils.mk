# rust-uutils — td builds a REAL coreutils tool from source: the uutils `cat` (crate uu_cat
# 0.9.0, whose [[bin]] is named `cat`) with its WHOLE crate closure (source + 139 deps)
# provisioned GUIX-FREE through td's OWN cargo-proxy: cargo resolved + fetched it through
# `td-feed cargo-proxy` (tools/warm-cargo-proxy.sh uu_cat 0.9.0 cat, host PREP), the proxy
# verifying each `.crate` sha256 == the crates.io index cksum; source + deps interned by
# store-add-recursive, vendored via TD_VENDOR_DIR. No guix oracle: content-address
# (Cargo.lock pin == index cksum) is the oracle. Shared build+assert in tests/crate-free-
# build.sh. The rust/gcc toolchain seed stays guix-built (retired last).
#
#   [DURABLE supply-chain] every vendored crate's sha256 ∈ uu_cat's shipped Cargo.lock.
#   [DURABLE structural] the .drv sets TD_VENDOR_DIR + references NO /gnu/store crate path.
#   [DURABLE behavioral] the built `cat` round-trips a file AND a stdin pipe — it works as cat.
#   [DURABLE repro] td-builder check double-build agrees the 139-crate build is reproducible.
HEAVY_GATES += rust-uutils
BUILD_GATES += rust-uutils
rust-uutils:
	@echo ">> rust-uutils: td builds the uutils 'cat' (uu_cat 0.9.0, 139 deps) GUIX-FREE via the cargo-proxy (interned vendor tree, TD_VENDOR_DIR); it works as cat (file + stdin); reproducible; no guix build / no /gnu/store crate / no oracle"
	@set -euo pipefail; \
	tsgo=`sh tests/tsgo.sh`; test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: no tsgo" >&2; exit 1; }; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts" GUIX="$(GUIX)" ROOT="$(CURDIR)"; \
	nsout=`sh tests/crate-free-build.sh cat uu_cat-0.9.0 tests/cat-uutils.lock cat-source tests/ts/recipe-cat.ts` || exit 1; \
	eval "$$nsout"; ns="$$NS"; \
	bin="$$ns/bin/cat"; \
	test -x "$$bin" || { echo "FAIL: no uu_cat 'cat' binary at $$bin" >&2; exit 1; }; \
	w="$(CURDIR)/.td-build-cache/cat-crate-free/work"; rm -rf "$$w"; mkdir -p "$$w"; \
	printf 'hello from td-built cat\nline two\n' > "$$w/in.txt"; \
	got=`"$$bin" "$$w/in.txt"`; \
	test "$$got" = "$$(printf 'hello from td-built cat\nline two')" || { echo "FAIL: td-built cat did not round-trip the file (got: $$got)" >&2; exit 1; }; \
	piped=`printf 'piped-in\n' | "$$bin"`; \
	test "$$piped" = "piped-in" || { echo "FAIL: td-built cat did not round-trip stdin (got: $$piped)" >&2; exit 1; }; \
	rm -rf "$$w"; \
	echo "  [DURABLE behavioral] the td-built uutils 'cat' (guix-free crates) round-trips a file AND a stdin pipe — it works as cat"; \
	echo "PASS: rust-uutils — the uutils 'cat' (uu_cat 0.9.0) built with its 139-crate closure provisioned GUIX-FREE via td's cargo-proxy (Cargo.lock-pinned, sha == crates.io cksum, no guix build / no /gnu/store FOD), source+vendor interned by store-add-recursive, built via TD_VENDOR_DIR with guix off PATH; the binary works as cat (file + stdin round-trip); reproducible. NO oracle (content-address = the upstream pin). Toolchain seed retired last."
