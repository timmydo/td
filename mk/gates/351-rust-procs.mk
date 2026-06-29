# rust-procs — td builds `procs` (the process viewer, 0.14.10) with its WHOLE crate closure
# (source + 297 deps) provisioned GUIX-FREE through td's OWN cargo-proxy (cargo resolved +
# fetched it, the proxy verifying each `.crate` sha256 == the crates.io index cksum); source +
# deps interned by store-add-recursive, vendored via TD_VENDOR_DIR. No guix oracle:
# content-address (Cargo.lock pin == index cksum) is the oracle. Shared build+assert in
# tests/crate-free-build.sh. The rust/gcc toolchain seed stays guix-built (retired last).
#
#   [DURABLE supply-chain] every vendored crate's sha256 ∈ procs's shipped Cargo.lock.
#   [DURABLE structural] the .drv sets TD_VENDOR_DIR + references NO /gnu/store crate path.
#   [DURABLE behavioral] the td-built `procs` runs (--version) and reads /proc into a process table.
#   [DURABLE repro] td-builder check double-build agrees the 297-crate build is reproducible.
HEAVY_GATES += rust-procs
BUILD_GATES += rust-procs
rust-procs:
	@echo ">> rust-procs: td builds 'procs' (0.14.10, 297 deps) GUIX-FREE via the cargo-proxy (interned vendor tree, TD_VENDOR_DIR); procs reads /proc; reproducible; no guix build / no /gnu/store crate / no oracle"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_recipe_eval; \
	export GUIX="$(GUIX)" ROOT="$(CURDIR)"; \
	nsout=`sh tests/crate-free-build.sh procs procs-0.14.10 tests/procs.lock procs-source procs` || exit 1; \
	eval "$$nsout"; ns="$$NS"; \
	test -x "$$ns/bin/procs" || { echo "FAIL: no procs binary at $$ns/bin/procs" >&2; exit 1; }; \
	"$$ns/bin/procs" --version >/dev/null 2>&1 || { echo "FAIL: td-built procs --version failed — the binary does not run" >&2; exit 1; }; \
	ptab=`"$$ns/bin/procs" </dev/null 2>/dev/null || true`; \
	printf '%s\n' "$$ptab" | grep -qiE 'PID|Command' || { echo "FAIL: td-built procs produced no process-table header reading /proc (first line: $$(printf '%s\n' "$$ptab" | head -1))" >&2; exit 1; }; \
	nrows=`printf '%s\n' "$$ptab" | grep -cE '^[[:space:]]*[0-9]+' || true`; \
	echo "  [DURABLE behavioral] the td-built 'procs' (guix-free crates) ran (--version) and read /proc into a process table (PID/Command columns, $$nrows process rows) — it works as procs"; \
	echo "PASS: rust-procs — procs (0.14.10) built with its 297-crate closure provisioned GUIX-FREE via td's cargo-proxy (Cargo.lock-pinned, sha == crates.io cksum, no guix build / no /gnu/store FOD), source+vendor interned by store-add-recursive, built via TD_VENDOR_DIR with guix off PATH; procs reads /proc; reproducible. NO oracle (content-address = the upstream pin). Toolchain seed retired last."
