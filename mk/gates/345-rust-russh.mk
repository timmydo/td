# rust-russh — td builds a Rust SSH from source (rust-build Inc.4; move-off-Guile §5) with its
# crate closure provisioned GUIX-FREE. A NEW domain beyond userland text tools: crypto +
# networking. The td-russh-demo crate is a self-contained russh (0.61) client<->server loopback
# round-trip — it starts an SSH server on 127.0.0.1, connects a client, authenticates by public
# key, exec's a command, and reads the reply: a real SSH handshake (curve25519 kex + the aws-lc
# crypto backend), no external sshd. Its 188-crate closure is fetched GUIX-FREE through td's OWN
# cargo-proxy from the IN-TREE source's Cargo.lock — the SOURCE is the in-repo tests/russh-demo/
# tree (NOT a crates.io crate), so tools/warm-cargo-proxy-local.sh (host PREP) cargo-fetches only
# the deps through the proxy (each `.crate` sha256 == the crates.io index cksum). Source + the
# vendor tree are interned by td's OWN store-add-recursive and vendored via TD_VENDOR_DIR. No guix
# oracle: content-address (the in-tree Cargo.lock pin == index cksum) is the oracle. The crypto
# backend (aws-lc-sys) has a C build script; run_rust's C set-paths provide CC/CXX +
# C_INCLUDE_PATH from gcc-toolchain/include (which bundles the kernel headers aws-lc compiles
# against) — so NO extra seed. The .drv is assembled by td (no guix (derivation …)) and realized
# daemon-free, guix/Guile SCRUBBED FROM PATH. The rustc/cargo/gcc seed is external (§5, retired last).
#
#   [DURABLE supply-chain] every vendored crate's sha256 ∈ the in-tree tests/russh-demo/Cargo.lock.
#   [DURABLE structural] the .drv sets TD_VENDOR_DIR + references NO /gnu/store crate path; the
#     source + vendor trees are td-interned (store-add-recursive); builder is the stage0; guix off PATH.
#   [DURABLE behavioral] the binary runs the full SSH round-trip and prints `td-russh-ok: ping` —
#     the handshake/auth/channel/exec all work end to end over loopback.
#   [DURABLE repro] td-builder check double-build agrees the 188-crate build (incl. the aws-lc C
#     crypto build) is reproducible.
HEAVY_GATES += rust-russh
# Ordered AFTER the parallel build-recipes phase (its 188-crate cargo build, incl. the aws-lc C
# crypto, would otherwise oversubscribe cores against build-recipes' fan-out). Not in BUILD_SPECS —
# the source is interned at gate time.
BUILD_GATES += rust-russh
rust-russh:
	@echo ">> rust-russh: td builds a russh client<->server SSH round-trip (188 deps incl. aws-lc crypto) from source via build-recipe with crates provisioned GUIX-FREE (cargo-proxy from the in-tree Cargo.lock, interned vendor tree, TD_VENDOR_DIR); the SSH session works + is reproducible; no guix build / no /gnu/store crate / no oracle"
	@set -euo pipefail; \
	vendor="$(CURDIR)/.td-build-cache/crate-vendor/russh"; \
	ncrate=`ls "$$vendor"/*.crate 2>/dev/null | wc -l`; \
	test "$$ncrate" -ge 150 || { echo "ERROR: vendor dir $$vendor has <150 crates ($$ncrate) — the HOST PREP tools/warm-cargo-proxy-local.sh tests/russh-demo russh (check.sh prelude) must cargo-fetch them through the proxy first (offline gate cannot egress)" >&2; exit 1; }; \
	clock="$(CURDIR)/tests/russh-demo/Cargo.lock"; \
	test -f "$$clock" || { echo "ERROR: no in-tree Cargo.lock at $$clock" >&2; exit 1; }; \
	miss=0; for c in "$$vendor"/*.crate; do sha=`sha256sum "$$c" | cut -d' ' -f1`; grep -qF "$$sha" "$$clock" || { echo "FAIL: crate `basename $$c` sha $$sha is NOT pinned in tests/russh-demo/Cargo.lock" >&2; miss=$$((miss + 1)); }; done; \
	test "$$miss" -eq 0 || { echo "FAIL: $$miss vendored crate(s) not pinned by tests/russh-demo/Cargo.lock" >&2; exit 1; }; \
	echo "  [DURABLE supply-chain] all $$ncrate vendored crates' sha256 are checksums pinned in tests/russh-demo/Cargo.lock (upstream crates.io hash — the guix-free oracle)"; \
	tsgo=`sh tests/tsgo.sh`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; tb="$$TB"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] ts-emit evaluates with td's OWN td-ts-eval ($$TD_TS_EVAL) — not the guix-built one (brick 4c)"; \
	lock0="$(CURDIR)/tests/td-russh-demo.lock"; \
	test -s "$$lock0" || { echo "ERROR: no lock $$lock0" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock0" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	scratch="$(CURDIR)/.td-build-cache/rust-russh"; rm -rf "$$scratch"; mkdir -p "$$scratch/tmp" "$$scratch/sd"; \
	grep -v '\.crate ' "$$lock0" | grep ' /gnu/store/' | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the toolchain seed" >&2; exit 1; }; \
	srcinfo=`sh tests/intern-src.sh "$$tb" td-russh-demo-src "$(CURDIR)/tests/russh-demo" "$$scratch/src" target .cargo` || { echo "ERROR: td could not intern the russh-demo crate tree (store-add-recursive)" >&2; exit 1; }; \
	eval "$$srcinfo"; \
	test -n "$$src" -a -d "$$srcstore/`basename "$$src"`" || { echo "ERROR: td interned no russh-demo source tree (store-add-recursive)" >&2; exit 1; }; \
	vinfo=`sh tests/intern-src.sh "$$tb" td-russh-vendor "$$vendor" "$$scratch/vendor"` || { echo "ERROR: intern vendor tree failed" >&2; exit 1; }; \
	vsrc=`echo "$$vinfo" | sed -n "s/^src='\(.*\)'/\1/p"`; \
	vstore=`echo "$$vinfo" | sed -n "s/^srcstore='\(.*\)'/\1/p"`; \
	vdb=`echo "$$vinfo" | sed -n "s/^srcdb='\(.*\)'/\1/p"`; \
	test -n "$$vsrc" -a -n "$$vstore" -a -n "$$vdb" || { echo "ERROR: vendor intern produced no path" >&2; exit 1; }; \
	echo "  [DURABLE structural] td interned the russh-demo source + the $$ncrate-crate set as content-addressed trees (store-add-recursive, no daemon): vendor $$vsrc"; \
	lock="$$scratch/seed.lock"; { grep -v '\.crate ' "$$lock0" | grep ' /gnu/store/'; echo "td-russh-demo-source $$src"; } > "$$lock"; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-td-russh-demo.ts" > "$$scratch/russh.json"; \
	test -s "$$scratch/russh.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/russh.json" "$$lock" "$$sd" /var/guix/db/db.sqlite "$$srcstore" "$$srcdb" "$$vsrc" "$$vstore" "$$vdb" > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe russh build (guix-free crates):" >&2; tail -40 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/td-russh-demo" || { echo "FAIL: russh build produced no binary at $$ns/bin/td-russh-demo" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_DIR' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_DIR" >&2; exit 1; }; \
	if grep -oqE '/gnu/store/[a-z0-9]+-[^ /]+\.crate' "$$sd"/*.drv; then echo "FAIL: the .drv references a /gnu/store crate path (not guix-free)" >&2; exit 1; fi; \
	test -n "$$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; exit 1; }; \
	grep -qF "$$TD_BUILDER_PATH/bin/td-builder" "$$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $$TD_BUILDER_PATH — built by the wrong td-builder?" >&2; exit 1; }; \
	echo "  [DURABLE structural] the .drv sets TD_VENDOR_DIR + NO /gnu/store crate path, and its builder is the td-bootstrapped stage0 ($$TD_BUILDER_PATH): $$out"; \
	got=`"$$ns/bin/td-russh-demo" 2>"$$scratch/run.err"` || { echo "FAIL: the td-built russh binary failed to run the SSH round-trip:" >&2; tail -5 "$$scratch/run.err" >&2; exit 1; }; \
	echo "$$got" | grep -q '^td-russh-ok: ping$$' || { echo "FAIL: russh round-trip did not return the expected reply (got: $$got)" >&2; cat "$$scratch/run.err" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built russh binary (guix-free crates) ran a full SSH round-trip (handshake + publickey auth + exec) over loopback: '$$got'"; \
	rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	  || { echo "FAIL: rust-russh NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	  || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	echo "  [DURABLE repro] td-builder check double-build agrees the guix-free-crate 188-crate russh build (incl. aws-lc C crypto) is reproducible"; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err" "$$scratch/run.err"; \
	echo "PASS: rust-russh — td built a Rust SSH (russh 0.61 client<->server loopback round-trip) from source via td-builder build-recipe with its 188-crate closure (incl. the aws-lc crypto backend with a C build script) provisioned GUIX-FREE through td's cargo-proxy from the IN-TREE tests/russh-demo/Cargo.lock (no guix build / no /gnu/store FOD), source + vendor interned by store-add-recursive, vendored via TD_VENDOR_DIR, built by stage0 with guix/Guile SCRUBBED FROM PATH; the binary runs a full SSH handshake/auth/exec round-trip (durable) and is reproducible by td's own double-build (durable). A real Rust SSH, built from source by td, with NO oracle (content-address = the in-tree Cargo.lock pin). The rustc/cargo/gcc seed stays external (§5, retired last)."
