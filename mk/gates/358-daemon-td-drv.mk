# daemon-td-drv — the DAEMON realizes a td-ASSEMBLED .drv (the td-artifact bridge,
# plan/daemon-td-drv.md; the Rust-focused distro). td builds its artifacts in its OWN
# store (daemon-free); the system IMAGE is daemon-built — so a td-built artifact must
# become daemon-VALID to be referenced by the image. This gate proves the bridge: td
# assembles a recipe's .drv with the GUIX-built td-builder as builder (build-recipe
# WITHOUT the stage0 override, so every input-src — crates/source/builder — is
# daemon-valid), then tests/td-daemon-instantiate.scm puts the .drv into the daemon
# store (add-text-to-store) and the DAEMON realizes it. Proven on the uutils `cat`
# (a real Rust tool): the daemon runs td-builder in ITS sandbox and produces a working
# binary. The full coreutils gets daemon-built this same way during the system image
# (the shipping increment) — here the cheaper `cat` proves the mechanism.
#
# GUIX-SURFACE (directive 3, called out for sign-off): this gate adds ONE guix-as-packager
# site (move-off-Guile §5 ratchet) — `(system td-builder) td-builder`. The daemon needs a
# daemon-VALID builder to realize the .drv, and the td-placed stage0 is NOT daemon-valid;
# the guix-built td-builder is. So shipping a td-built artifact through the daemon-built
# image inherently re-uses the guix-built td-builder as the BUILDER SEED (retired when td
# has its own builder daemon). ts-eval uses load_ts_eval (td's own — no packager site).
#
# DURABLE (no guix oracle — uu_cat has none):
#   [STRUCTURAL] the assembled .drv's builder is the guix-built td-builder (daemon-valid)
#     and carries TD_VENDOR_CRATES — the daemon, not td's realize, produces the output.
#   [DURABLE behavioral] the DAEMON-built `cat` round-trips a file + a stdin pipe.
#   [DURABLE bridge] the daemon-built output is daemon-VALID and sits at the SAME store
#     path td's own daemon-free realize produced — the .drv is realizer-independent, so a
#     td-built artifact and its daemon realization are the same store object (this is what
#     lets the daemon-built system image reference td's build).
HEAVY_GATES += daemon-td-drv
# Self-contained (cat-uutils.lock); not in BUILD_SPECS.
BUILD_GATES += daemon-td-drv
daemon-td-drv:
	@echo ">> daemon-td-drv: the daemon realizes a td-assembled .drv (uu_cat, guix-built builder) — td-built artifact becomes daemon-valid; same store path as td's own realize"
	@set -euo pipefail; \
	tgz=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-tsgo-tarball)'`; tsgo=`sh tests/tsgo.sh "$$tgz"`; \
	. tests/cache-lib.sh; load_ts_eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$TD_TS_EVAL" -a -x "$$tb" -a -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo / ts-eval / td-builder" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	tbpkg=`dirname "$$(dirname "$$tb")"`; \
	$(GUIX) gc --references "$$tbpkg" >/dev/null 2>&1 || { echo "FAIL: the guix-built td-builder is not daemon-valid ($$tbpkg)" >&2; exit 1; }; \
	echo "  [STRUCTURAL] the builder is the daemon-valid guix-built td-builder: $$tbpkg"; \
	lock="$(CURDIR)/tests/cat-uutils.lock"; \
	test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/daemon-td-drv"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + source + vendored deps" >&2; exit 1; }; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-cat.ts" > "$$scratch/cat.json"; \
	test -s "$$scratch/cat.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" "$$tb" build-recipe "$$scratch/cat.json" "$$lock" "$$scratch/b" /var/guix/db/db.sqlite > "$$scratch/bout" 2>"$$scratch/berr" || { echo "FAIL: build-recipe (guix builder, daemon-free realize):" >&2; tail -20 "$$scratch/berr" >&2; exit 1; }; \
	td_out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$td_out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/berr" >&2; exit 1; }; \
	drv=`ls "$$scratch/b"/*.drv | head -1`; \
	grep -qF "$$tbpkg/bin/td-builder" "$$drv" || { echo "FAIL: the .drv builder is not the guix-built td-builder ($$tbpkg)" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_CRATES' "$$drv" || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES" >&2; exit 1; }; \
	echo "  [STRUCTURAL] td assembled the .drv (TD_VENDOR_CRATES) + realized it daemon-free at $$td_out"; \
	dm_out=`$(GUIX) repl -- "$(CURDIR)/tests/td-daemon-instantiate.scm" "$$drv" 2>"$$scratch/insterr"`; \
	test -n "$$dm_out" || { echo "FAIL: the daemon did not realize the td-assembled .drv:" >&2; tail -15 "$$scratch/insterr" >&2; exit 1; }; \
	test "$$dm_out" = "$$td_out" || { echo "FAIL: daemon-built path ($$dm_out) != td's daemon-free path ($$td_out) — .drv not realizer-independent" >&2; exit 1; }; \
	$(GUIX) gc --references "$$dm_out" >/dev/null 2>&1 || { echo "FAIL: the daemon-built output is not daemon-valid ($$dm_out)" >&2; exit 1; }; \
	echo "  [DURABLE bridge] the daemon realized td's .drv → daemon-VALID output at the SAME path as td's own realize: $$dm_out"; \
	printf 'bridge line one\nbridge line two\n' > "$$scratch/in.txt"; \
	got=`"$$dm_out/bin/cat" "$$scratch/in.txt"`; \
	test "$$got" = "$$(printf 'bridge line one\nbridge line two')" || { echo "FAIL: daemon-built cat did not round-trip the file (got: $$got)" >&2; exit 1; }; \
	piped=`printf 'piped\n' | "$$dm_out/bin/cat"`; \
	test "$$piped" = "piped" || { echo "FAIL: daemon-built cat did not round-trip stdin (got: $$piped)" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the DAEMON-built uutils 'cat' round-trips a file AND a stdin pipe"; \
	rm -rf "$$scratch/tmp" "$$scratch/bout" "$$scratch/berr" "$$scratch/insterr" "$$scratch/in.txt"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: the daemon REALIZED a td-assembled .drv — td assembled uu_cat's .drv with the daemon-valid guix-built td-builder (TD_VENDOR_CRATES, no guix (derivation …)), tests/td-daemon-instantiate.scm put it in the daemon store, and the DAEMON built it (ran td-builder in its sandbox) → a daemon-VALID 'cat' that round-trips file + stdin (durable), at the SAME store path td's own daemon-free realize produced (the .drv is realizer-independent). This is the td-artifact bridge: a td-built artifact the daemon-built system image can reference. The rustc/cargo/gcc seed stays external (§5, retired last)."
