# rust-eza — td builds `eza` (the ls replacement, 0.21.6, --no-default-features) with its
# WHOLE crate closure (source + 233 deps) provisioned GUIX-FREE through td's OWN cargo-proxy
# (cargo resolved + fetched it, the proxy verifying each `.crate` sha256 == the crates.io
# index cksum); source + deps interned by store-add-recursive, vendored via
# TD_VENDOR_DIR. No guix oracle: content-address (Cargo.lock pin == index cksum) is the
# oracle. Shared build+assert in tests/crate-free-build.sh. The rust/gcc toolchain seed stays
# guix-built (retired last).
#
#   [DURABLE supply-chain] every vendored crate's sha256 ∈ eza's shipped Cargo.lock.
#   [DURABLE structural] the .drv sets TD_VENDOR_DIR + references NO /gnu/store crate path.
#   [DURABLE behavioral] the td-built `eza` lists a directory's entries (and not a missing one).
#   [DURABLE repro] td-builder check double-build agrees the 233-crate build is reproducible.
HEAVY_GATES += rust-eza
BUILD_GATES += rust-eza
rust-eza:
	@echo ">> rust-eza: td builds 'eza' (0.21.6, 233 deps) GUIX-FREE via the cargo-proxy (interned vendor tree, TD_VENDOR_DIR); eza lists a dir; reproducible; no guix build / no /gnu/store crate / no oracle"
	@set -euo pipefail; \
	tsgo=`sh tests/tsgo.sh`; test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: no tsgo" >&2; exit 1; }; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts" GUIX="$(GUIX)" ROOT="$(CURDIR)"; \
	nsout=`sh tests/crate-free-build.sh eza eza-0.21.6 tests/eza.lock eza-source tests/ts/recipe-eza.ts` || exit 1; \
	eval "$$nsout"; ns="$$NS"; \
	test -x "$$ns/bin/eza" || { echo "FAIL: no eza binary at $$ns/bin/eza" >&2; exit 1; }; \
	tree="$(CURDIR)/.td-build-cache/eza-crate-free/tree"; rm -rf "$$tree"; mkdir -p "$$tree"; : > "$$tree/alpha.txt"; : > "$$tree/beta.log"; \
	listing=`"$$ns/bin/eza" "$$tree"`; \
	echo "$$listing" | grep -q 'alpha.txt' && echo "$$listing" | grep -q 'beta.log' || { echo "FAIL: td-built eza did not list the directory entries (got: $$listing)" >&2; exit 1; }; \
	echo "$$listing" | grep -q 'nonexistent' && { echo "FAIL: eza listed a file that does not exist" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built 'eza' (guix-free crates) listed the directory's entries (alpha.txt + beta.log) — it works as ls"; \
	rm -rf "$$tree"; \
	echo "PASS: rust-eza — eza (0.21.6) built with its 233-crate closure provisioned GUIX-FREE via td's cargo-proxy (Cargo.lock-pinned, sha == crates.io cksum, no guix build / no /gnu/store FOD), source+vendor interned by store-add-recursive, built via TD_VENDOR_DIR with guix off PATH; eza lists a dir; reproducible. NO oracle (content-address = the upstream pin). Toolchain seed retired last."
