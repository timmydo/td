//! rust-fetch — td builds td-fetch with its crate closure provisioned GUIX-FREE:
//! the 73 `.crate` deps are td-fetched from static.crates.io (Cargo.lock-pinned, NO guix
//! build, NO /gnu/store FOD; tools/warm-td-fetch-crates.sh, host PREP), interned as ONE
//! content-addressed vendor TREE by td's OWN store-add-recursive, and build-recipe vendors
//! from it (TD_VENDOR_DIR) — so NOTHING in the crate path is guix. Per the human (2026-06-23,
//! "no new guix dependencies, even an oracle"): crates are content-addressed, so the
//! correctness oracle is the UPSTREAM Cargo.lock checksum, NOT a guix differential. The
//! rust/gcc toolchain seed stays guix-built (retired last).
//! 
//! [DURABLE supply-chain] every vendored crate's sha256 is a checksum pinned in
//! fetch/Cargo.lock (the upstream crates.io hash) — the guix-free equivalence oracle.
//! [DURABLE structural] the .drv sets TD_VENDOR_DIR and references NO `/gnu/store` crate
//! path; the vendor tree is td-interned (store-add-recursive); guix/Guile off PATH.
//! [DURABLE behavioral] td BUILDS td-fetch from the interned vendor tree and it runs.
//! [DURABLE repro] td-builder check double-build agrees the build is reproducible.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "rust-fetch",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        script: r##"
echo ">> rust-fetch: td builds td-fetch with crates provisioned GUIX-FREE (td-fetched + interned vendor tree, TD_VENDOR_DIR), no guix build / no /gnu/store crate / no oracle"
set -euo pipefail; \
vendor="$PWD/.td-build-cache/crate-vendor/td-fetch"; \
ncrate=`ls "$vendor"/*.crate 2>/dev/null | wc -l`; \
test "$ncrate" -ge 70 || { echo "ERROR: vendor dir $vendor has <70 crates ($ncrate) — the HOST PREP tools/warm-td-fetch-crates.sh (check.sh prelude) must td-fetch them first (offline gate cannot egress)" >&2; exit 1; }; \
miss=0; for c in "$vendor"/*.crate; do sha=`sha256sum "$c" | cut -d' ' -f1`; grep -qF "$sha" "$PWD/fetch/Cargo.lock" || { echo "FAIL: crate `basename $c` sha $sha is NOT pinned in fetch/Cargo.lock" >&2; miss=$((miss + 1)); }; done; \
test "$miss" -eq 0 || { echo "FAIL: $miss vendored crate(s) not pinned by fetch/Cargo.lock" >&2; exit 1; }; \
echo "  [DURABLE supply-chain] all $ncrate vendored crates' sha256 are checksums pinned in fetch/Cargo.lock (upstream crates.io hash — the guix-free oracle)"; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; tb="$TB"; \
lock0="$PWD/tests/td-fetch.lock"; \
cu=`grep -- '-coreutils-' "$lock0" | sed 's/^[^ ]* //' | head -1`; \
test -n "$cu" || { echo "ERROR: no coreutils in the lock" >&2; exit 1; }; \
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
scratch="$PWD/.td-build-cache/rust-fetch"; rm -rf "$scratch"; mkdir -p "$scratch/tmp" "$scratch/sd"; \
grep -v '\.crate ' "$lock0" | grep ' /gnu/store/' | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null || { echo "ERROR: could not realize the toolchain seed" >&2; exit 1; }; \
srcinfo=`sh tests/intern-src.sh "$tb" td-fetch-src "$PWD/fetch" "$scratch/src" target vendor .cargo` || { echo "ERROR: intern source failed" >&2; exit 1; }; \
eval "$srcinfo"; \
vinfo=`sh tests/intern-src.sh "$tb" td-fetch-vendor "$vendor" "$scratch/vendor"` || { echo "ERROR: intern vendor tree failed" >&2; exit 1; }; \
vsrc=`echo "$vinfo" | sed -n "s/^src='\(.*\)'/\1/p"`; \
vstore=`echo "$vinfo" | sed -n "s/^srcstore='\(.*\)'/\1/p"`; \
vdb=`echo "$vinfo" | sed -n "s/^srcdb='\(.*\)'/\1/p"`; \
test -n "$vsrc" -a -n "$vstore" -a -n "$vdb" || { echo "ERROR: vendor intern produced no path" >&2; exit 1; }; \
echo "  [DURABLE structural] td interned the crate set as one content-addressed vendor tree (store-add-recursive, no daemon): $vsrc"; \
seedlock="$scratch/seed.lock"; { grep -v '\.crate ' "$lock0"; echo "td-fetch-source $src"; } > "$seedlock"; \
sh tests/recipe-emit.sh td-fetch > "$scratch/fetch.json"; \
test -s "$scratch/fetch.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
sd="$scratch/sd"; \
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" "$tb" build-recipe "$scratch/fetch.json" "$seedlock" "$sd" /gnu/store "$srcstore" "$srcdb" "$vsrc" "$vstore" "$vdb" > "$scratch/bout" 2>"$scratch/err" || { echo "FAIL: build-recipe (guix-free crates):" >&2; tail -40 "$scratch/err" >&2; exit 1; }; \
out=`sed -n 's/^OUT=out //p' "$scratch/bout"`; \
test -n "$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$scratch/err" >&2; exit 1; }; \
ns="$sd/newstore/`basename "$out"`"; \
test -x "$ns/bin/td-fetch" || { echo "FAIL: no td-fetch binary at $ns/bin/td-fetch" >&2; exit 1; }; \
grep -q 'TD_VENDOR_DIR' "$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_DIR" >&2; exit 1; }; \
if grep -oqE '/gnu/store/[a-z0-9]+-[^ /]+\.crate' "$sd"/*.drv; then echo "FAIL: the .drv references a /gnu/store crate path (not guix-free)" >&2; exit 1; fi; \
echo "  [DURABLE structural] the .drv sets TD_VENDOR_DIR and references NO /gnu/store crate path — crates are guix-free: $out"; \
rc=0; "$ns/bin/td-fetch" >/dev/null 2>&1 || rc=$?; test "$rc" = 2 || { echo "FAIL: the td-built td-fetch usage exit != 2 (got $rc)" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the td-built td-fetch (guix-free crates) runs (usage exit 2)"; \
rm -rf "$scratch/chk"; "$tb" check "$sd"/*.drv "$sd/closure.txt" "$scratch/chk" > "$scratch/checkout.txt" 2>"$scratch/chk.err" \
  || { echo "FAIL: NOT reproducible (td-builder check):" >&2; tail -6 "$scratch/checkout.txt" "$scratch/chk.err" >&2; exit 1; }; \
grep -qE "^CHECK out $out sha256:[0-9a-f]+ reproducible$" "$scratch/checkout.txt" \
  || { echo "FAIL: td-builder check did not confirm $out reproducible:" >&2; cat "$scratch/checkout.txt" >&2; exit 1; }; \
echo "  [DURABLE repro] td-builder check double-build agrees the guix-free-crate td-fetch build is reproducible"; \
rm -rf "$scratch/chk" "$scratch/tmp" "$scratch/bout" "$scratch/err" "$scratch/checkout.txt" "$scratch/chk.err"; \
echo "PASS: rust-fetch — td built td-fetch with its 73-crate closure provisioned GUIX-FREE: td-fetched from static.crates.io (Cargo.lock-pinned, no guix build / no /gnu/store FOD), interned as one content-addressed vendor tree by store-add-recursive, vendored via TD_VENDOR_DIR, built by stage0 with guix off PATH; the .drv has no /gnu/store crate path; the binary runs; reproducible. The crate path is guix-free with NO oracle (content-address = the upstream Cargo.lock pin). Toolchain seed retired last."
"##,
    }
}
