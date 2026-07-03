//! td-feed — td builds td-feed (its OWN local HTTP mirror of every network-downloaded
//! artifact, feed/) FROM SOURCE via `td-builder build-recipe` (buildSystem "rust") by the
//! td-bootstrapped stage0 (move-off-Guile §5), then proves the mirror works offline — with
//! its crate closure provisioned GUIX-FREE. td-feed shares td-fetch's vendored closure
//! exactly (ureq + rustls/ring + sha2, 73 crates — only the bin name differs), so it reuses
//! td-fetch's td-fetched vendor tree (.td-build-cache/crate-vendor/td-fetch, warmed GUIX-FREE
//! by tools/warm-td-fetch-crates.sh in the check.sh prelude — NO guix build, NO /gnu/store
//! crate FOD), interned by td's OWN store-add-recursive and vendored via TD_VENDOR_DIR. The
//! crate correctness oracle is the UPSTREAM feed/Cargo.lock checksum, NOT a guix differential
//! (human 2026-06-23, "no new guix dependencies, even an oracle"). The .drv is assembled by td
//! (no guix (derivation …)) and realized daemon-free, guix/Guile SCRUBBED FROM PATH. The
//! rustc/cargo/gcc seed is external (§5, retired last).
//! 
//! [DURABLE supply-chain] every vendored crate's sha256 is a checksum pinned in
//! feed/Cargo.lock (the upstream crates.io hash) — the guix-free equivalence oracle.
//! [DURABLE structural] the .drv sets TD_VENDOR_DIR and references NO `/gnu/store` crate
//! path; the vendor tree is td-interned (store-add-recursive); guix/Guile off PATH.
//! [DURABLE behavioral] the td-built td-feed `selftest` warms a one-entry index from a
//! loopback ORIGIN, serves it on a 2nd loopback port, and fetches it back THROUGH the
//! feed + sha256-verifies — the full warm->serve->fetch path, offline (std::net).
//! [SELF-DISCRIMINATION] that same selftest reds if a wrong index hash is accepted on warm
//! or a corrupted store byte is served (verify-on-serve) — the content hash is
//! load-bearing on BOTH the warm and the serve side.
//! [DURABLE behavioral] the td-built td-feed `warm-selftest` exercises the consolidated
//! `td-feed warm <action>` host-PREP orchestration (the former warm-*.sh): lock parse,
//! linux/version.h codegen, cargo-config, and an IN-PROCESS cargo-proxy source-crate GET
//! round-trip — all offline (std::net). A malformed lock + a crate whose bytes mismatch
//! its index cksum red it [SELF-DISCRIMINATION] (the parse + verifying egress load-bearing).
//! [DURABLE structural] tests/td-feed.index is self-consistent: every line is
//! <path> <url> <sha256>, each sha256 is 64-hex, and no path repeats.
//! [DURABLE structural] the index is TRUTHFUL against the vendored closure: for every crate
//! td-feed vendors, the index's recorded sha256 equals the vendored .crate's content
//! sha256 — the mirror would serve the same content-addressed bytes td built against.
//! [DURABLE structural] the .drv builder is the td-bootstrapped stage0 (not the guix-built
//! td-builder); ts-emit ran under td's OWN td-recipe-eval.
//! [DURABLE repro] td-builder check double-build agrees the build is reproducible.
//! A BUILD_GATE (like rust-fetch): ordered AFTER the parallel build-recipes phase so its
//! 73-crate cargo build doesn't oversubscribe cores, and it depends on the td-recipe-eval that
//! build-recipes' prelude builds. Not in BUILD_SPECS — the source is interned at gate time.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "td-feed",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        script: r##"
echo ">> td-feed: td builds td-feed (its own local HTTP mirror, 73 vendored deps) from source via build-recipe with crates provisioned GUIX-FREE (td-fetched + interned vendor tree, TD_VENDOR_DIR); it warms+serves+fetches over loopback + is reproducible, and tests/td-feed.index is self-consistent + truthful; no guix build / no /gnu/store crate / no oracle"
set -euo pipefail; \
vendor="$PWD/.td-build-cache/crate-vendor/td-fetch"; \
ncrate=`ls "$vendor"/*.crate 2>/dev/null | wc -l`; \
test "$ncrate" -ge 70 || { echo "ERROR: vendor dir $vendor has <70 crates ($ncrate) — the HOST PREP tools/warm-td-fetch-crates.sh (check.sh prelude) must td-fetch them first (offline gate cannot egress); td-feed shares td-fetch's closure" >&2; exit 1; }; \
miss=0; for c in "$vendor"/*.crate; do sha=`sha256sum "$c" | cut -d' ' -f1`; grep -qF "$sha" "$PWD/feed/Cargo.lock" || { echo "FAIL: crate `basename $c` sha $sha is NOT pinned in feed/Cargo.lock" >&2; miss=$((miss + 1)); }; done; \
test "$miss" -eq 0 || { echo "FAIL: $miss vendored crate(s) not pinned by feed/Cargo.lock" >&2; exit 1; }; \
echo "  [DURABLE supply-chain] all $ncrate vendored crates' sha256 are checksums pinned in feed/Cargo.lock (upstream crates.io hash — the guix-free oracle; td-feed shares td-fetch's closure exactly)"; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; tb="$TB"; \
case "$TD_RECIPE_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_RECIPE_EVAL is not td's own build ($TD_RECIPE_EVAL)" >&2; exit 1 ;; esac; \
echo "  [DURABLE structural] recipes evaluate with td's OWN td-recipe-eval ($TD_RECIPE_EVAL)"; \
lock0="$PWD/tests/td-feed.lock"; \
test -s "$lock0" || { echo "ERROR: no lock $lock0" >&2; exit 1; }; \
cu=`grep -- '-coreutils-' "$lock0" | sed 's/^[^ ]* //' | head -1`; \
test -n "$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
scratch="$PWD/.td-build-cache/td-feed"; rm -rf "$scratch"; mkdir -p "$scratch/tmp" "$scratch/sd"; \
grep -v '\.crate ' "$lock0" | grep ' /gnu/store/' | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null || { echo "ERROR: could not realize the toolchain seed" >&2; exit 1; }; \
srcinfo=`sh tests/intern-src.sh "$tb" td-feed-src "$PWD/feed" "$scratch/src" target vendor .cargo` || { echo "ERROR: td could not intern the feed crate tree" >&2; exit 1; }; \
eval "$srcinfo"; \
test -n "$src" -a -d "$srcstore/`basename "$src"`" || { echo "ERROR: td interned no feed source tree" >&2; exit 1; }; \
vinfo=`sh tests/intern-src.sh "$tb" td-feed-vendor "$vendor" "$scratch/vendor"` || { echo "ERROR: intern vendor tree failed" >&2; exit 1; }; \
vsrc=`echo "$vinfo" | sed -n "s/^src='\(.*\)'/\1/p"`; \
vstore=`echo "$vinfo" | sed -n "s/^srcstore='\(.*\)'/\1/p"`; \
vdb=`echo "$vinfo" | sed -n "s/^srcdb='\(.*\)'/\1/p"`; \
test -n "$vsrc" -a -n "$vstore" -a -n "$vdb" || { echo "ERROR: vendor intern produced no path" >&2; exit 1; }; \
echo "  [DURABLE structural] td interned the feed source + the crate set as content-addressed trees (store-add-recursive, no daemon): vendor $vsrc"; \
lock="$scratch/seed.lock"; { grep -v '\.crate ' "$lock0" | grep ' /gnu/store/'; echo "td-feed-source $src"; } > "$lock"; \
sh tests/recipe-emit.sh td-feed > "$scratch/feed.json"; \
test -s "$scratch/feed.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
sd="$scratch/sd"; \
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" "$tb" build-recipe "$scratch/feed.json" "$lock" "$sd" /gnu/store "$srcstore" "$srcdb" "$vsrc" "$vstore" "$vdb" > "$scratch/bout" 2>"$scratch/err" || { echo "FAIL: build-recipe td-feed build (guix-free crates):" >&2; tail -40 "$scratch/err" >&2; exit 1; }; \
out=`sed -n 's/^OUT=out //p' "$scratch/bout"`; \
test -n "$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$scratch/err" >&2; exit 1; }; \
ns="$sd/newstore/`basename "$out"`"; \
test -x "$ns/bin/td-feed" || { echo "FAIL: td-feed build produced no binary at $ns/bin/td-feed" >&2; exit 1; }; \
grep -q 'TD_VENDOR_DIR' "$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_DIR" >&2; exit 1; }; \
if grep -oqE '/gnu/store/[a-z0-9]+-[^ /]+\.crate' "$sd"/*.drv; then echo "FAIL: the .drv references a /gnu/store crate path (not guix-free)" >&2; exit 1; fi; \
test -n "$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset" >&2; exit 1; }; \
grep -qF "$TD_BUILDER_PATH/bin/td-builder" "$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $TD_BUILDER_PATH" >&2; exit 1; }; \
echo "  [DURABLE structural] the .drv sets TD_VENDOR_DIR + NO /gnu/store crate path, and its builder is the td-bootstrapped stage0 ($TD_BUILDER_PATH): $out"; \
st=`"$ns/bin/td-feed" selftest 2>"$scratch/run.err"` || { echo "FAIL: the td-built td-feed failed its loopback warm->serve->fetch selftest:" >&2; tail -8 "$scratch/run.err" >&2; exit 1; }; \
echo "$st" | grep -q '^td-feed: selftest OK' || { echo "FAIL: td-feed selftest did not report OK (got: $st)" >&2; cat "$scratch/run.err" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the td-built td-feed warmed + served + fetched a blob over loopback (verify-on-warm + verify-on-serve): '$st'"; \
echo "  [SELF-DISCRIMINATION] that selftest also reds a wrong index hash (warm) and a corrupted store byte (serve) — verification is load-bearing on both sides"; \
cps=`"$ns/bin/td-feed" cargo-proxy-selftest 2>"$scratch/cps.err"` || { echo "FAIL: the td-built td-feed cargo-proxy selftest failed:" >&2; tail -8 "$scratch/cps.err" >&2; exit 1; }; \
echo "$cps" | grep -q '^td-feed: cargo-proxy selftest OK' || { echo "FAIL: cargo-proxy selftest did not report OK (got: $cps)" >&2; cat "$scratch/cps.err" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the td-built td-feed cargo-proxy fetched + verified a crate THROUGH the proxy over loopback (cargo's sparse protocol): '$cps'"; \
echo "  [SELF-DISCRIMINATION] the cargo-proxy refuses a crate whose bytes mismatch its index cksum — the verifying egress is load-bearing"; \
ws=`"$ns/bin/td-feed" warm-selftest 2>"$scratch/ws.err"` || { echo "FAIL: the td-built td-feed warm-selftest failed:" >&2; tail -8 "$scratch/ws.err" >&2; exit 1; }; \
echo "$ws" | grep -q '^td-feed: warm selftest OK' || { echo "FAIL: warm-selftest did not report OK (got: $ws)" >&2; cat "$scratch/ws.err" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the td-built td-feed warm-selftest exercised the consolidated 'td-feed warm <action>' orchestration's pure + IN-PROCESS legs (lock parse, linux/version.h codegen, cargo-config, an in-process cargo-proxy source-crate round-trip) over loopback: '$ws'"; \
echo "  [SELF-DISCRIMINATION] warm-selftest reds a malformed lock and a crate whose bytes mismatch its index cksum — the parse + verifying egress are load-bearing"; \
idx="$PWD/tests/td-feed.index"; \
test -s "$idx" || { echo "ERROR: no index $idx" >&2; exit 1; }; \
bad3=`grep -v '^#' "$idx" | grep -vcE '^[^ ]+ [^ ]+ [^ ]+$' || true`; \
test "$bad3" -eq 0 || { echo "FAIL: $bad3 index line(s) are not <path> <url> <sha256>" >&2; exit 1; }; \
badsha=`grep -v '^#' "$idx" | cut -d' ' -f3 | grep -vcE '^[0-9a-f]{64}$' || true`; \
test "$badsha" -eq 0 || { echo "FAIL: $badsha index sha256 field(s) are not 64-hex" >&2; exit 1; }; \
dup=`grep -v '^#' "$idx" | cut -d' ' -f1 | sort | uniq -d | wc -l`; \
test "$dup" -eq 0 || { echo "FAIL: $dup duplicate path(s) in the index" >&2; exit 1; }; \
nidx=`grep -cv '^#' "$idx"`; \
echo "  [DURABLE structural] tests/td-feed.index self-consistent: $nidx lines, all <path> <url> <sha256>, all sha256 64-hex, no duplicate path"; \
checked=0; \
for c in "$vendor"/*.crate; do \
  nv=`basename "$c" | sed -E 's/\.crate$//'`; \
  isha=`grep -F "/$nv.crate " "$idx" | head -1 | cut -d' ' -f3`; \
  test -n "$isha" || { echo "FAIL: vendored crate $nv is not in the index" >&2; exit 1; }; \
  csha=`sha256sum "$c" | cut -d' ' -f1`; \
  test "$isha" = "$csha" || { echo "FAIL: index sha256 for $nv ($isha) != vendored content ($csha)" >&2; exit 1; }; \
  checked=$((checked+1)); \
done; \
echo "  [DURABLE structural] index is TRUTHFUL: all $checked vendored crates' recorded sha256 == their td-fetched .crate content (the mirror serves the same content-addressed bytes td built against)"; \
rm -rf "$scratch/chk"; "$tb" check "$sd"/*.drv "$sd/closure.txt" "$scratch/chk" > "$scratch/checkout.txt" 2>"$scratch/chk.err" \
  || { echo "FAIL: td-feed NOT reproducible (td-builder check):" >&2; tail -6 "$scratch/checkout.txt" "$scratch/chk.err" >&2; exit 1; }; \
grep -qE "^CHECK out $out sha256:[0-9a-f]+ reproducible$" "$scratch/checkout.txt" \
  || { echo "FAIL: td-builder check did not confirm $out reproducible:" >&2; cat "$scratch/checkout.txt" >&2; exit 1; }; \
echo "  [DURABLE repro] td-builder check double-build agrees the guix-free-crate td-feed build is reproducible"; \
rm -rf "$scratch/chk" "$scratch/tmp" "$scratch/bout" "$scratch/err" "$scratch/checkout.txt" "$scratch/chk.err" "$scratch/run.err" "$scratch/cps.err" "$scratch/ws.err"; \
echo "PASS: td built td-feed (its own local HTTP mirror) from source via td-builder build-recipe with its 73-crate closure provisioned GUIX-FREE (td-fetched from static.crates.io, Cargo.lock-pinned, no guix build / no /gnu/store FOD), interned as a content-addressed vendor tree by store-add-recursive, vendored via TD_VENDOR_DIR, with its BUILDER the td-bootstrapped stage0 and guix/Guile SCRUBBED FROM PATH; the td-built td-feed warms+serves+fetches a blob over loopback and reds on a wrong/corrupted hash on BOTH the warm and serve side (durable behavioral + self-discrimination, offline); tests/td-feed.index is self-consistent and truthful against the vendored closure (durable structural); and the build is reproducible by td's own double-build (durable). td now OWNS the mirror of its seeds AND its crate path is guix-free with NO oracle (content-address = the upstream Cargo.lock pin). Toolchain seed retired last."
"##,
    }
}
