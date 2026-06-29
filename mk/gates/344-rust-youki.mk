# rust-youki — td builds `youki` (the Rust OCI container runtime, 0.6.0) with its WHOLE crate
# closure (source + 663 deps) provisioned GUIX-FREE through td's OWN cargo-proxy: cargo
# resolved + fetched it through `td-feed cargo-proxy` (tools/warm-cargo-proxy.sh youki 0.6.0,
# host PREP), the proxy verifying each `.crate` sha256 == the crates.io index cksum; source +
# deps interned by store-add-recursive, vendored via TD_VENDOR_DIR. No guix oracle:
# content-address (Cargo.lock pin == index cksum) is the oracle. Shared build+assert in
# tests/crate-free-build.sh. The rust/gcc toolchain seed stays guix-built (retired last). With
# uutils this finishes OWNING the crates.io corpus rust packages guix-free (russh = local demo
# source, separate; then Phase 2b drops the /gnu/store crate strings).
#
#   [DURABLE supply-chain] every vendored crate's sha256 ∈ youki's shipped Cargo.lock.
#   [DURABLE structural] the .drv sets TD_VENDOR_DIR + references NO /gnu/store crate path.
#   [DURABLE behavioral] the td-built `youki` runs — --version reports youki + --help lists the
#     OCI `create` subcommand (a real OCI runtime CLI).
#   [DURABLE repro] td-builder check double-build agrees the 663-crate build is reproducible.
HEAVY_GATES += rust-youki
BUILD_GATES += rust-youki
rust-youki:
	@echo ">> rust-youki: td builds 'youki' (0.6.0, 663 deps) GUIX-FREE via the cargo-proxy (interned vendor tree, TD_VENDOR_DIR); youki runs as an OCI runtime CLI; reproducible; no guix build / no /gnu/store crate / no oracle"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_recipe_eval; \
	export GUIX="$(GUIX)" ROOT="$(CURDIR)"; \
	nsout=`sh tests/crate-free-build.sh youki youki-0.6.0 tests/youki.lock youki-source youki` || exit 1; \
	eval "$$nsout"; ns="$$NS"; \
	test -x "$$ns/bin/youki" || { echo "FAIL: no youki binary at $$ns/bin/youki" >&2; exit 1; }; \
	"$$ns/bin/youki" --version 2>&1 | grep -qi 'youki' || { echo "FAIL: youki --version did not report youki" >&2; "$$ns/bin/youki" --version >&2 || true; exit 1; }; \
	"$$ns/bin/youki" --help 2>&1 | grep -qiE '\bcreate\b' || { echo "FAIL: youki --help did not list the OCI 'create' subcommand" >&2; "$$ns/bin/youki" --help >&2 || true; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built youki (guix-free crates) runs — --version reports youki + --help lists the OCI lifecycle subcommands (a real OCI runtime CLI)"; \
	echo "PASS: rust-youki — youki (0.6.0) built with its 663-crate closure provisioned GUIX-FREE via td's cargo-proxy (Cargo.lock-pinned, sha == crates.io cksum, no guix build / no /gnu/store FOD), source+vendor interned by store-add-recursive, built via TD_VENDOR_DIR with guix off PATH; youki runs as an OCI runtime CLI; reproducible. The crates.io corpus rust packages now build guix-free. NO oracle (content-address = the upstream pin). Toolchain seed retired last."
