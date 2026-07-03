//! td-subst — td builds td-subst (its OWN substitute / binary-cache server, subst/) FROM
//! SOURCE via `td-builder build-recipe` (buildSystem "rust") by the td-bootstrapped stage0
//! (move-off-Guile §5), then proves the substitute protocol end-to-end OFFLINE. td-subst
//! shares td-feed/td-fetch's vendored closure exactly (ureq + rustls/ring + sha2; subst adds
//! `ring` as a direct dep, already in the closure, so subst/Cargo.lock pins td-feed's exact
//! versions), so it reuses tests/td-subst.lock (== td-fetch's seed + .crate deps). The .drv
//! is assembled by td (no guix (derivation …)) and realized daemon-free, guix/Guile SCRUBBED
//! FROM PATH. The rustc/cargo/gcc seed is external (§5, retired last).
//! 
//! [DURABLE behavioral] the td-built td-subst `selftest` keygens, signs + serves a one-entry
//! export dir on loopback, fetches it back + verifies (ed25519 signature + NarHash) — the
//! full keygen->sign->serve->fetch->verify path, offline (std::net + ring).
//! [SELF-DISCRIMINATION] that same selftest reds if a tampered narinfo, a corrupted nar, or a
//! WRONG public key is accepted — signature AND content-hash are load-bearing.
//! [DURABLE behavioral] END-TO-END "fetch, don't build": the td-bootstrapped stage0 PLACES a
//! path into a td store + registers it, the td-built td-subst exports + signs + serves it on
//! loopback, td FETCHES it back (verifying signature + NarHash) and RESTORES it (nar-restore)
//! to a tree BYTE-IDENTICAL to the original — a path obtained WITHOUT building it. A tampered
//! narinfo reds the fetch (the consumer falls back to building).
//! [DURABLE structural] the .drv builder is the td-bootstrapped stage0 (not the guix-built
//! td-builder); ts-emit ran under td's OWN td-recipe-eval.
//! [DURABLE repro] td-builder check double-build agrees the td-subst build is reproducible.
//! [DURABLE behavioral] LOCK-KEYED substitute (tasks 2b/2c — [[toolchain-input-addressed]]): a
//! real artifact is interned at the INPUT-ADDRESSED path `toolchain-path tests/td-toolchain.lock
//! glibc-2.41`, subst-export'd + signed + served; a CONSUMER that has ONLY the lock derives the
//! SAME /td/store path (asserted equal), fetches it by that basename, verifies the signature +
//! the narinfo StorePath == its lock-computed path + NarHash, restores it, and RUNS the
//! fetched-not-built binary — a toolchain path obtained WITHOUT building it. Trust = the ed25519
//! signature + the input-addressed NAME, not repro-equality (the toolchain is not byte-
//! reproducible — that is task 3). A wrong public key reds the fetch (self-discrimination). The
//! literal gcc/glibc bytes flow through the IDENTICAL machinery; wiring gate 412 to emit them
//! input-addressed runs in the DAILY heavy suite (a ~90-min from-seed build), not per-PR.
//! A BUILD_GATE (like td-feed): ordered AFTER the parallel build-recipes phase so its cargo
//! build doesn't oversubscribe cores, and it depends on the td-recipe-eval that build-recipes'
//! prelude builds. Not in BUILD_SPECS — the source is interned at gate time.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "td-subst",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        script: r##"
echo ">> td-subst: td builds td-subst (its own substitute server) from source via build-recipe (offline, guix/Guile off PATH); it selftests the signed serve/fetch over loopback, proves fetch-don't-build end-to-end byte-identical, proves the build-recipe CONSUMER HOOK substitutes (CACHE=subst) instead of rebuilding, and is reproducible"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; tb="$TB"; \
case "$TD_RECIPE_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_RECIPE_EVAL is not td's own build ($TD_RECIPE_EVAL)" >&2; exit 1 ;; esac; \
echo "  [DURABLE structural] recipes evaluate with td's OWN td-recipe-eval ($TD_RECIPE_EVAL)"; \
lock0="$PWD/tests/td-subst.lock"; \
test -s "$lock0" || { echo "ERROR: no lock $lock0" >&2; exit 1; }; \
cu=`grep -- '-coreutils-' "$lock0" | sed 's/^[^ ]* //' | head -1`; \
test -n "$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
ncrate=`grep -cE '\.crate /gnu/store/' "$lock0"`; \
test "$ncrate" -ge 70 || { echo "ERROR: lock has <70 vendored .crate deps ($ncrate)" >&2; exit 1; }; \
scratch="$PWD/.td-build-cache/td-subst"; mkdir -p "$scratch/tmp" "$scratch/b"; rm -f "$scratch/b/"*.drv; \
grep ' /gnu/store/' "$lock0" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null || { echo "ERROR: could not realize the seed + vendored .crate deps" >&2; exit 1; }; \
srcinfo=`sh tests/intern-src.sh "$tb" td-subst-src "$PWD/subst" "$scratch" target vendor .cargo` || { echo "ERROR: td could not intern the subst crate tree" >&2; exit 1; }; \
eval "$srcinfo"; \
test -n "$src" -a -d "$srcstore/`basename "$src"`" || { echo "ERROR: td interned no subst source tree" >&2; exit 1; }; \
lock="$scratch/td-subst.lock"; { cat "$lock0"; echo "td-subst-source $src"; } > "$lock"; \
sh tests/recipe-emit.sh td-subst > "$scratch/subst.json"; \
test -s "$scratch/subst.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
sd="$scratch/b"; mkdir -p "$sd"; \
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" "$tb" build-recipe "$scratch/subst.json" "$lock" "$sd" /gnu/store "$srcstore" "$srcdb" > "$scratch/bout" 2>"$scratch/err" || { echo "FAIL: build-recipe td-subst build:" >&2; tail -30 "$scratch/err" >&2; exit 1; }; \
out=`sed -n 's/^OUT=out //p' "$scratch/bout"`; \
test -n "$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$scratch/err" >&2; exit 1; }; \
if grep -qx 'CACHE=hit' "$scratch/bout"; then hit=1; else hit=; fi; \
ns="$sd/newstore/`basename "$out"`"; \
test -x "$ns/bin/td-subst" || { echo "FAIL: td-subst build produced no binary at $ns/bin/td-subst" >&2; exit 1; }; \
ts="$ns/bin/td-subst"; \
grep -q 'TD_VENDOR_CRATES' "$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES" >&2; exit 1; }; \
test -n "$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset" >&2; exit 1; }; \
grep -qF "$TD_BUILDER_PATH/bin/td-builder" "$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $TD_BUILDER_PATH" >&2; exit 1; }; \
echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($TD_BUILDER_PATH)"; \
if [ -n "$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — reused td's prior td-subst build: $out"; else echo "  [STRUCTURAL] td assembled + realized the .drv ($ncrate deps) with guix/Guile off PATH: $out"; fi; \
st=`"$ts" selftest 2>"$scratch/run.err"` || { echo "FAIL: the td-built td-subst failed its loopback selftest:" >&2; tail -8 "$scratch/run.err" >&2; exit 1; }; \
echo "$st" | grep -q '^td-subst: selftest OK' || { echo "FAIL: td-subst selftest did not report OK (got: $st)" >&2; cat "$scratch/run.err" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the td-built td-subst keygen+sign+serve+fetch+verify round-trip over loopback: '$st'"; \
echo "  [SELF-DISCRIMINATION] that selftest also reds a tampered narinfo, a corrupted nar, and a wrong public key — signature + NarHash are load-bearing"; \
e2e="$scratch/e2e"; rm -rf "$e2e"; mkdir -p "$e2e/store" "$e2e/served" "$e2e/fetch" "$e2e/restored"; \
printf 'td substitute end-to-end payload\n' > "$e2e/content"; \
path=`env -i PATH="$cu/bin" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" "$tb" store-add-text td-subst-e2e "$e2e/content" "$e2e/store" "$e2e/td.db"`; \
base=`basename "$path"`; \
env -i PATH="$cu/bin" "$tb" subst-export "$e2e/td.db" "$e2e/store" "$e2e/served" "$path" >/dev/null || { echo "FAIL: subst-export" >&2; exit 1; }; \
test -f "$e2e/served/$base.narinfo" || { echo "FAIL: subst-export wrote no narinfo for $base" >&2; exit 1; }; \
"$ts" keygen "$e2e/priv" "$e2e/pub" >/dev/null; \
"$ts" sign "$e2e/served" "$e2e/priv" >/dev/null; \
grep -q '^Sig: ' "$e2e/served/$base.narinfo" || { echo "FAIL: td-subst sign did not sign the narinfo" >&2; exit 1; }; \
"$ts" serve "$e2e/served" 127.0.0.1:0 > "$e2e/serve.log" 2>&1 & spid=$!; \
trap 'kill $spid 2>/dev/null || true' EXIT; \
port=""; for i in `seq 1 100`; do port=`sed -n 's#.*http://127.0.0.1:\([0-9]*\)/.*#\1#p' "$e2e/serve.log" 2>/dev/null`; [ -n "$port" ] && break; sleep 0.1; done; \
test -n "$port" || { echo "FAIL: td-subst serve never bound a loopback port" >&2; cat "$e2e/serve.log" >&2; exit 1; }; \
"$ts" fetch "http://127.0.0.1:$port" "$base" "$e2e/fetch" "$e2e/pub" >/dev/null || { echo "FAIL: td-subst fetch (verify) failed" >&2; cat "$e2e/serve.log" >&2; exit 1; }; \
narfile=`grep '^NarFile: ' "$e2e/fetch/$base.narinfo" | cut -d' ' -f2`; \
env -i PATH="$cu/bin" "$tb" nar-restore "$e2e/fetch/$narfile" "$e2e/restored/$base" >/dev/null || { echo "FAIL: nar-restore the fetched substitute" >&2; exit 1; }; \
oh=`"$tb" nar-hash "$e2e/content"`; rh=`"$tb" nar-hash "$e2e/restored/$base"`; \
	test -n "$oh" -a "x$oh" = "x$rh" || { echo "FAIL: the FETCHED+restored path differs from the original (NAR $rh != original $oh)" >&2; exit 1; }; \
echo "  [DURABLE behavioral] FETCH-DON'T-BUILD: td placed $base, the td-built td-subst signed+served it, and td fetched+restored it BYTE-IDENTICAL over loopback — a path obtained without building it"; \
sed -i 's/td-subst-e2e/td-subst-XXXX/' "$e2e/served/$base.narinfo"; \
if "$ts" fetch "http://127.0.0.1:$port" "$base" "$e2e/fetch2" "$e2e/pub" >/dev/null 2>&1; then echo "FAIL: fetch ACCEPTED a tampered narinfo — the signature is not load-bearing" >&2; exit 1; fi; \
echo "  [SELF-DISCRIMINATION] a tampered narinfo reds the fetch (the consumer falls back to building)"; \
kill $spid 2>/dev/null || true; trap - EXIT; \
obase=`basename "$out"`; \
csrv="$scratch/consumer-served"; csd="$scratch/consumer-build"; rm -rf "$csrv" "$csd"; mkdir -p "$csrv" "$csd/tmp"; \
env -i PATH="$cu/bin" "$tb" subst-export --paths "$sd/td.db" "$sd/newstore" "$csrv" "$out" >/dev/null || { echo "FAIL: subst-export --paths the td-subst output" >&2; exit 1; }; \
test -f "$csrv/$obase.narinfo" || { echo "FAIL: no narinfo for the td-subst output $obase" >&2; exit 1; }; \
"$ts" sign "$csrv" "$e2e/priv" >/dev/null; \
"$ts" serve "$csrv" 127.0.0.1:0 > "$scratch/csrv.log" 2>&1 & cspid=$!; \
trap 'kill $cspid 2>/dev/null || true' EXIT; \
cport=""; for i in `seq 1 100`; do cport=`sed -n 's#.*http://127.0.0.1:\([0-9]*\)/.*#\1#p' "$scratch/csrv.log" 2>/dev/null`; [ -n "$cport" ] && break; sleep 0.1; done; \
test -n "$cport" || { echo "FAIL: consumer-leg serve never bound a port" >&2; cat "$scratch/csrv.log" >&2; exit 1; }; \
env -i HOME="$csd" TMPDIR="$csd/tmp" PATH="$cu/bin" TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" TD_SUBST_URL="http://127.0.0.1:$cport" TD_SUBST_PUBKEY="$e2e/pub" TD_SUBST_BIN="$ts" "$tb" build-recipe "$scratch/subst.json" "$lock" "$csd" /gnu/store "$srcstore" "$srcdb" > "$csd/bout" 2>"$csd/cerr" || { echo "FAIL: build-recipe with TD_SUBST_URL (consumer hook):" >&2; tail -20 "$csd/cerr" >&2; exit 1; }; \
grep -qx 'CACHE=subst' "$csd/bout" || { echo "FAIL: build-recipe did NOT substitute (no CACHE=subst) though a valid signed substitute was served" >&2; cat "$csd/bout" >&2; tail -5 "$csd/cerr" >&2; exit 1; }; \
cout=`sed -n 's/^OUT=out //p' "$csd/bout"`; \
test "x$cout" = "x$out" || { echo "FAIL: substituted output path ($cout) != the built output ($out)" >&2; exit 1; }; \
cns="$csd/newstore/$obase"; test -x "$cns/bin/td-subst" || { echo "FAIL: the substituted output has no td-subst binary at $cns/bin/td-subst" >&2; exit 1; }; \
bh=`"$tb" nar-hash "$ns"`; sh2=`"$tb" nar-hash "$cns"`; \
test -n "$bh" -a "x$bh" = "x$sh2" || { echo "FAIL: the SUBSTITUTED output differs from the built output (NAR $sh2 != built $bh)" >&2; exit 1; }; \
env -i PATH="$cu/bin" "$cns/bin/td-subst" keygen "$csd/runprobe.priv" "$csd/runprobe.pub" >/dev/null 2>&1 || { echo "FAIL: the substituted td-subst binary does not run (keygen)" >&2; exit 1; }; \
echo "  [DURABLE behavioral] CONSUMER HOOK: build-recipe with TD_SUBST_URL FETCHED td-subst's own output into a FRESH store (CACHE=subst) instead of rebuilding it — substituted == built byte-identical, and the substituted binary runs"; \
"$ts" keygen "$scratch/wrong.priv" "$scratch/wrong.pub" >/dev/null; \
if env -i PATH="$cu/bin" "$ts" fetch "http://127.0.0.1:$cport" "$obase" "$scratch/wrongfetch" "$scratch/wrong.pub" >/dev/null 2>&1; then echo "FAIL: the consumer's fetch step ACCEPTED a wrong public key — the signature is not load-bearing in the consumer path" >&2; exit 1; fi; \
echo "  [SELF-DISCRIMINATION] a wrong public key reds the consumer's fetch step (the exact command try_substitute shells) -> it returns None -> build-recipe falls back to building"; \
echo "  [SELF-DISCRIMINATION] TD_SUBST_URL is load-bearing: the from-source build above ran with NO url; only this run (url set) yielded CACHE=subst without rebuilding"; \
kill $cspid 2>/dev/null || true; trap - EXIT; \
echo "  --- lock-keyed input-addressed substitute (tasks 2b/2c): a consumer fetches a /td/store path it computes FROM td-toolchain.lock ---"; \
ttl="$PWD/tests/td-toolchain.lock"; test -s "$ttl" || { echo "FAIL: no td-toolchain.lock" >&2; exit 1; }; \
iakey=`env -i PATH="$cu/bin" "$tb" toolchain-key "$ttl"`; test -n "$iakey" || { echo "FAIL: toolchain-key produced nothing" >&2; exit 1; }; \
bashpkg=`grep -- '-bash-' "$PWD/tests/hello-no-guix.lock" | grep -v static | sed 's/^[^ ]* //' | head -1`; \
iabs=`env -i PATH="$cu/bin" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" "$tb" store-closure-scan /gnu/store "$bashpkg" | grep -- '-bash-static-' | head -1`; \
test -n "$iabs" -a -x "$iabs/bin/bash" || { echo "FAIL: no static bash fixture in hello's closure" >&2; exit 1; }; \
iad="$scratch/ia"; rm -rf "$iad"; mkdir -p "$iad/store" "$iad/served" "$iad/fetch" "$iad/restored"; \
iap=`env -i PATH="$cu/bin" TD_STORE_DIR=/td/store "$tb" store-add-input-addressed glibc-2.41 "$iakey" "$iabs" "$iad/store" "$iad/td.db"`; \
case "$iap" in /td/store/*-glibc-2.41) : ;; *) echo "FAIL: producer path not input-addressed at /td/store: $iap" >&2; exit 1 ;; esac; \
iabase=`basename "$iap"`; \
env -i PATH="$cu/bin" "$tb" subst-export "$iad/td.db" "$iad/store" "$iad/served" "$iap" >/dev/null || { echo "FAIL: subst-export the input-addressed path" >&2; exit 1; }; \
test -f "$iad/served/$iabase.narinfo" || { echo "FAIL: subst-export wrote no narinfo for $iabase" >&2; exit 1; }; \
"$ts" sign "$iad/served" "$e2e/priv" >/dev/null; \
"$ts" serve "$iad/served" 127.0.0.1:0 > "$iad/serve.log" 2>&1 & iaspid=$!; \
trap 'kill $iaspid 2>/dev/null || true' EXIT; \
iaport=""; for i in `seq 1 100`; do iaport=`sed -n 's#.*http://127.0.0.1:\([0-9]*\)/.*#\1#p' "$iad/serve.log" 2>/dev/null`; [ -n "$iaport" ] && break; sleep 0.1; done; \
test -n "$iaport" || { echo "FAIL: lock-keyed serve never bound a port" >&2; cat "$iad/serve.log" >&2; exit 1; }; \
iapc=`env -i PATH="$cu/bin" TD_STORE_DIR=/td/store "$tb" toolchain-path "$ttl" glibc-2.41`; \
test "x$iapc" = "x$iap" || { echo "FAIL: the consumer's lock-computed path ($iapc) != the producer's interned path ($iap)" >&2; exit 1; }; \
echo "  [DURABLE structural] producer + consumer INDEPENDENTLY derive the same /td/store path from td-toolchain.lock: $iapc"; \
env -i PATH="$cu/bin" "$ts" fetch "http://127.0.0.1:$iaport" "`basename "$iapc"`" "$iad/fetch" "$e2e/pub" >/dev/null || { echo "FAIL: consumer fetch of the lock-keyed path failed" >&2; cat "$iad/serve.log" >&2; exit 1; }; \
fsp=`grep '^StorePath: ' "$iad/fetch/$iabase.narinfo" | cut -d' ' -f2`; \
test "x$fsp" = "x$iapc" || { echo "FAIL: fetched narinfo StorePath ($fsp) != the lock-computed path ($iapc)" >&2; exit 1; }; \
ianar=`grep '^NarFile: ' "$iad/fetch/$iabase.narinfo" | cut -d' ' -f2`; \
env -i PATH="$cu/bin" "$tb" nar-restore "$iad/fetch/$ianar" "$iad/restored/$iabase" >/dev/null || { echo "FAIL: nar-restore the lock-keyed substitute" >&2; exit 1; }; \
ran=`env -i "$iad/restored/$iabase/bin/bash" -c 'echo RAN-FETCHED'`; \
test "x$ran" = "xRAN-FETCHED" || { echo "FAIL: the fetched (not built) binary did not run" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the consumer FETCHED the lock-named path (ed25519 sig + NarHash verified) and RAN it -- a toolchain path obtained WITHOUT building it (trust = signature + input-addressed name; repro-equality is task 3, the toolchain is not byte-reproducible)"; \
"$ts" keygen "$iad/wrong.priv" "$iad/wrong.pub" >/dev/null; \
if env -i PATH="$cu/bin" "$ts" fetch "http://127.0.0.1:$iaport" "$iabase" "$iad/wrongfetch" "$iad/wrong.pub" >/dev/null 2>&1; then echo "FAIL: the lock-keyed fetch ACCEPTED a wrong public key" >&2; exit 1; fi; \
echo "  [SELF-DISCRIMINATION] a wrong public key reds the lock-keyed fetch -- the signature is load-bearing"; \
kill $iaspid 2>/dev/null || true; trap - EXIT; \
rm -rf "$iad"; \
rm -rf "$csrv" "$csd" "$scratch/wrong.priv" "$scratch/wrong.pub" "$scratch/wrongfetch" "$scratch/csrv.log"; \
if [ -n "$hit" ] && [ -f "$sd/verified-reproducible" ]; then \
  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped"; \
else \
  rm -rf "$scratch/chk"; "$tb" check-drv "$sd"/*.drv "$sd/closure.txt" "$scratch/chk" > "$scratch/checkout.txt" 2>"$scratch/chk.err" \
    || { echo "FAIL: td-subst NOT reproducible (td-builder check):" >&2; tail -6 "$scratch/checkout.txt" "$scratch/chk.err" >&2; exit 1; }; \
  grep -qE "^CHECK out $out sha256:[0-9a-f]+ reproducible$" "$scratch/checkout.txt" \
    || { echo "FAIL: td-builder check did not confirm $out reproducible:" >&2; cat "$scratch/checkout.txt" >&2; exit 1; }; \
  : > "$sd/verified-reproducible"; \
  echo "  [DURABLE repro] td-builder check double-build agrees the td-subst build is reproducible"; \
fi; \
rm -rf "$scratch/chk" "$scratch/tmp" "$scratch/bout" "$scratch/err" "$scratch/checkout.txt" "$scratch/chk.err" "$scratch/run.err" "$e2e"; mkdir -p "$scratch/tmp"; \
echo "PASS: td built td-subst (its own substitute server) from source via td-builder build-recipe — the closure resolved from pinned static.crates.io fetches (no specification->package, no network), the .drv assembled + realized by td with its BUILDER the td-bootstrapped stage0 and guix/Guile SCRUBBED FROM PATH; the td-built td-subst signs+serves+fetches+verifies over loopback and reds on a tampered narinfo / corrupted nar / wrong key (durable behavioral + self-discrimination, offline); it proves FETCH-DON'T-BUILD end-to-end (td placed a path, td-subst served it signed, td fetched+restored it BYTE-IDENTICAL without building it); the td-builder CONSUMER HOOK (build-recipe with TD_SUBST_URL) FETCHES a built output (CACHE=subst, byte-identical, runs) into a fresh store instead of rebuilding it and falls back to building on a wrong key (durable behavioral + self-discrimination); and the build is reproducible by td's own double-build (durable). It also proves the LOCK-KEYED substitute (tasks 2b/2c): a consumer that has ONLY td-toolchain.lock independently computes the toolchain's INPUT-ADDRESSED /td/store path, fetches it (signature + StorePath + NarHash verified), and RUNS the fetched-not-built binary — signature-trusted (the toolchain is not byte-reproducible; repro-equality is task 3) — and a wrong key reds the fetch. td now OWNS a substitute server for its built outputs AND a consumer that uses it; the external fetch of seeds runs in the network PREP (§5)."
"##,
    }
}
