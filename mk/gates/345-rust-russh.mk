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
# Not in BUILD_SPECS — the source is interned at gate time by td's OWN recursive
# addToStore (tests/intern-src.sh → store-add-recursive, no `guix repl`; move-off-Guile
# §5), so the gate is self-contained.
BUILD_GATES += rust-russh
rust-russh:
	@echo ">> rust-russh: td builds a russh client<->server SSH round-trip (188 vendored deps incl. aws-lc crypto) from source via build-recipe (offline, guix/Guile off PATH); the SSH session works + is reproducible"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	test -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TSDIR="$(CURDIR)/tests/ts"; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; tb="$$TB"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] ts-emit evaluates with td's OWN td-ts-eval ($$TD_TS_EVAL) — not the guix-built one (brick 4c)"; \
	lock0="$(CURDIR)/tests/td-russh-demo.lock"; \
	test -s "$$lock0" || { echo "ERROR: no lock $$lock0" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock0" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	ncrate=`grep -cE '\.crate /gnu/store/' "$$lock0"`; \
	test "$$ncrate" -ge 150 || { echo "ERROR: lock has <150 vendored .crate deps ($$ncrate) — regenerate from the demo's Cargo.lock" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/rust-russh"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock0" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + vendored .crate deps (warm static.crates.io fetches; regenerate the lock on a channel/dep bump)" >&2; exit 1; }; \
	srcinfo=`sh tests/intern-src.sh "$$tb" td-russh-demo-src "$(CURDIR)/tests/russh-demo" "$$scratch" target .cargo` || { echo "ERROR: td could not intern the russh-demo crate tree (store-add-recursive)" >&2; exit 1; }; \
	eval "$$srcinfo"; \
	test -n "$$src" -a -d "$$srcstore/`basename "$$src"`" || { echo "ERROR: td interned no russh-demo source tree (store-add-recursive)" >&2; exit 1; }; \
	lock="$$scratch/td-russh-demo.lock"; { cat "$$lock0"; echo "td-russh-demo-source $$src"; } > "$$lock"; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-td-russh-demo.ts" > "$$scratch/russh.json"; \
	test -s "$$scratch/russh.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/russh.json" "$$lock" "$$sd" /var/guix/db/db.sqlite "$$srcstore" "$$srcdb" > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe russh build (guix/Guile off PATH):" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/td-russh-demo" || { echo "FAIL: russh build produced no binary at $$ns/bin/td-russh-demo" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_CRATES' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES — the vendored path was not taken" >&2; exit 1; }; \
	test -n "$$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; exit 1; }; \
	grep -qF "$$TD_BUILDER_PATH/bin/td-builder" "$$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $$TD_BUILDER_PATH — built by the wrong td-builder?" >&2; exit 1; }; \
	echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($$TD_BUILDER_PATH) — not the guix-built td-builder (brick 3b)"; \
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
