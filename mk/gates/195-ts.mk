# ts-frontend Phase 1 (DESIGN §7.1, sub-task 1) — the TypeScript spec front-end.
# `tsc` (the pinned td-typescript input, run under the packaged node) BOTH
# type-checks a td system spec and emits its type-stripped JS. Self-discriminating
# like the `diff`/`oci-diff` gates (tests/ts-check.sh): the well-typed v0 spec
# checks clean AND emits a byte-identical golden, while an out-of-union
# rootFsType ("ext3") is REJECTED with a type error (TS2322) — the always-on
# negative control proving the types are load-bearing. No image/VM: it builds two
# warm packages and runs tsc on tiny files (seconds), so it slots late in the
# heavy LPT order. The pinned channel's swc CLI is a non-functional stub and tsc
# is unpackaged, so tsc does both jobs (human 2026-06-13; plan/ts-frontend.md).
HEAVY_GATES += ts
FAST_GATES += ts
ts:
	@echo ">> ts: TypeScript spec front-end — the NATIVE tsc (td-tsgo) type-checks + emits the v0 spec, NO node (ts-frontend Phase 1; tsgo migration)"
	@set -euo pipefail; \
	tgz=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-tsgo-tarball)'`; tsgo=`sh tests/tsgo.sh "$$tgz"`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo (native compiler)" >&2; exit 1; }; \
	TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts" \
	  sh tests/ts-check.sh
