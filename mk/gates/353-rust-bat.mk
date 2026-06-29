# rust-bat — td builds `bat` (the cat replacement, 0.25.0, no git/onig C) with its WHOLE crate
# closure (source + 207 deps) provisioned GUIX-FREE through td's OWN cargo-proxy (cargo
# resolved + fetched it, the proxy verifying each `.crate` sha256 == the crates.io index
# cksum); source + deps interned by store-add-recursive, vendored via TD_VENDOR_DIR. No
# guix oracle: content-address (Cargo.lock pin == index cksum) is the oracle. Shared
# build+assert in tests/crate-free-build.sh. The rust/gcc toolchain seed stays guix-built
# (retired last). This completes the shipped Rust userland (procs/fd/ripgrep/sd/eza/bat) built
# guix-free.
#
#   [DURABLE supply-chain] every vendored crate's sha256 ∈ bat's shipped Cargo.lock.
#   [DURABLE structural] the .drv sets TD_VENDOR_DIR + references NO /gnu/store crate path.
#   [DURABLE behavioral] the td-built `bat` prints a file's contents (plain style).
#   [DURABLE repro] td-builder check double-build agrees the 207-crate build is reproducible.
HEAVY_GATES += rust-bat
BUILD_GATES += rust-bat
rust-bat:
	@echo ">> rust-bat: td builds 'bat' (0.25.0, 207 deps) GUIX-FREE via the cargo-proxy (interned vendor tree, TD_VENDOR_DIR); bat prints a file; reproducible; no guix build / no /gnu/store crate / no oracle"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_recipe_eval; \
	export GUIX="$(GUIX)" ROOT="$(CURDIR)"; \
	nsout=`sh tests/crate-free-build.sh bat bat-0.25.0 tests/bat.lock bat-source bat` || exit 1; \
	eval "$$nsout"; ns="$$NS"; \
	test -x "$$ns/bin/bat" || { echo "FAIL: no bat binary at $$ns/bin/bat" >&2; exit 1; }; \
	btmp="$(CURDIR)/.td-build-cache/bat-crate-free/btmp"; rm -rf "$$btmp"; mkdir -p "$$btmp"; \
	printf 'hello from td-built bat\nsecond line\n' > "$$btmp/sample.txt"; \
	got=`"$$ns/bin/bat" --style=plain --paging=never --color=never "$$btmp/sample.txt"`; \
	echo "$$got" | grep -q 'hello from td-built bat' && echo "$$got" | grep -q 'second line' || { echo "FAIL: td-built bat did not print the file contents (got: $$got)" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built 'bat' (guix-free crates) printed the file's contents (plain style) — it works as cat"; \
	rm -rf "$$btmp"; \
	echo "PASS: rust-bat — bat (0.25.0) built with its 207-crate closure provisioned GUIX-FREE via td's cargo-proxy (Cargo.lock-pinned, sha == crates.io cksum, no guix build / no /gnu/store FOD), source+vendor interned by store-add-recursive, built via TD_VENDOR_DIR with guix off PATH; bat prints a file; reproducible — the shipped Rust userland (procs/fd/ripgrep/sd/eza/bat) now builds guix-free. NO oracle (content-address = the upstream pin). Toolchain seed retired last."
