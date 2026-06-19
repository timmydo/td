# rust-russh — td builds a Rust SSH from source (rust-build Inc.4; move-off-Guile
# §5). A NEW domain beyond userland text tools: crypto + networking. The
# td-russh-demo crate is a self-contained russh (0.61) client<->server loopback
# round-trip — it starts an SSH server on 127.0.0.1, connects a client,
# authenticates by public key, exec's a command, and reads the reply: a real SSH
# handshake (curve25519 kex + the aws-lc crypto backend), no external sshd. Built
# via `td-builder build-recipe` (buildSystem "rust") with its 188-crate dependency
# closure vendored (static.crates.io fixed-output fetches, Cargo.lock-pinned). The
# crypto backend (aws-lc-sys) has a C build script; run_rust's C set-paths provides
# CC/CXX + C_INCLUDE_PATH from gcc-toolchain/include, which already bundles the
# kernel headers (linux/*.h) aws-lc compiles against — so NO extra seed is needed
# (the base toolchain seed suffices; aws-lc-sys uses its cc build path, not cmake).
# The .drv is assembled by td (no guix (derivation …)) and realized daemon-free
# (no guix-daemon), with guix/Guile SCRUBBED FROM PATH. The rustc/cargo/gcc seed is
# external (§5, retired last).
#
# ALL-DURABLE (no guix oracle): no guix build of this crate to diff against —
#   [STRUCTURAL] the build runs guix/Guile off PATH, produces the binary, and the
#     .drv carries TD_VENDOR_CRATES.
#   [DURABLE behavioral] the binary runs the full SSH round-trip and prints
#     `td-russh-ok: ping` — the handshake/auth/channel/exec all work end to end.
#   [DURABLE repro] td-builder check's double-build agrees the build is reproducible
#     across the whole 188-crate graph incl. the aws-lc C crypto build.
HEAVY_GATES += rust-russh
# Ordered AFTER the parallel build-recipes phase (its 188-crate cargo build, incl. the
# aws-lc C crypto, would otherwise oversubscribe cores against build-recipes' fan-out).
# Not in BUILD_SPECS — the source is interned at gate time, so the gate is self-contained.
BUILD_GATES += rust-russh
rust-russh:
	@echo ">> rust-russh: td builds a russh client<->server SSH round-trip (188 vendored deps incl. aws-lc crypto) from source via build-recipe (offline, guix/Guile off PATH); the SSH session works + is reproducible"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	ev=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$ev" -a -x "$$tb" -a -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc / ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	lock0="$(CURDIR)/tests/td-russh-demo.lock"; \
	test -s "$$lock0" || { echo "ERROR: no lock $$lock0" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock0" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	ncrate=`grep -cE '\.crate /gnu/store/' "$$lock0"`; \
	test "$$ncrate" -ge 150 || { echo "ERROR: lock has <150 vendored .crate deps ($$ncrate) — regenerate from the demo's Cargo.lock" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/rust-russh"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock0" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + vendored .crate deps (warm static.crates.io fetches; regenerate the lock on a channel/dep bump)" >&2; exit 1; }; \
	src=`$(GUIX) repl $(LOAD) tests/td-russh-demo-source.scm 2>/dev/null | sed -n 's/^SRC=//p'`; \
	test -n "$$src" -a -d "$$src" || { echo "ERROR: could not intern the russh-demo crate tree" >&2; exit 1; }; \
	lock="$$scratch/td-russh-demo.lock"; { cat "$$lock0"; echo "td-russh-demo-source $$src"; } > "$$lock"; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-td-russh-demo.ts" > "$$scratch/russh.json"; \
	test -s "$$scratch/russh.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" "$$tb" build-recipe "$$scratch/russh.json" "$$lock" "$$sd" /var/guix/db/db.sqlite > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe russh build (guix/Guile off PATH):" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/td-russh-demo" || { echo "FAIL: russh build produced no binary at $$ns/bin/td-russh-demo" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_CRATES' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES — the vendored path was not taken" >&2; exit 1; }; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — recipe unchanged, reused td's prior russh build (no rebuild): $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv (TD_VENDOR_CRATES, $$ncrate deps) with guix/Guile off PATH: $$out"; fi; \
	got=`"$$ns/bin/td-russh-demo" 2>"$$scratch/run.err"` || { echo "FAIL: the td-built russh binary failed to run the SSH round-trip:" >&2; tail -5 "$$scratch/run.err" >&2; exit 1; }; \
	echo "$$got" | grep -q '^td-russh-ok: ping$$' || { echo "FAIL: russh round-trip did not return the expected reply (got: $$got)" >&2; cat "$$scratch/run.err" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built russh binary ran a full SSH round-trip (handshake + publickey auth + exec) over loopback: '$$got'"; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: rust-russh NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the 188-crate russh build (incl. aws-lc C crypto) is reproducible"; \
	fi; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err" "$$scratch/run.err"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td built a Rust SSH (russh 0.61 client<->server loopback round-trip) from source via td-builder build-recipe — the 188-crate dependency closure (incl. the aws-lc crypto backend with a C build script) resolved from pinned static.crates.io fetches (no specification->package, no network), the cargo vendor dir assembled by td's run_rust, the C build env (CC/CXX + C_INCLUDE_PATH from gcc-toolchain, which bundles the kernel headers) provided by run_rust — no extra seed, the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon), with guix/Guile SCRUBBED FROM PATH; the binary runs a full SSH handshake/auth/exec round-trip (durable) and is reproducible by td's own double-build (durable). A real Rust SSH, built from source by td — crypto + networking, a new domain. The rustc/cargo/gcc seed stays external (§5, retired last)."
