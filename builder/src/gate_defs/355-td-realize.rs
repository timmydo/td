//! td-realize — td REALIZES a derivation with NO guix-daemon AND NO guix store DB in the
//! path (DESIGN §7.1 move-off-Guile §5; own-builder-daemon). Where td-drv-build (235)
//! still staged the input closure with `guix gc -R` (the daemon), `td-builder realize`
//! computes that closure ITSELF by CONTENT-SCANNING the seed store DIR (scanForReferences
//! — the daemon's own reference criterion, == `guix gc -R` for an output root, gate 290),
//! with NO read of guix's private /var/guix/db — then builds in its userns sandbox and
//! registers the output. Subject: the td-build hello drv. Legs: DURABLE — td computed the
//! closure itself by content-scan, and the realized hello runs; DURABLE (discriminator) —
//! realize against a store dir that LACKS the inputs FAILS, proving the closure is
//! content-scanned from the given dir, not a hidden /var/guix read; MIGRATION ORACLE
//! (removable when guix retires) — the output (path/NAR/size/deriver) is byte-identical to
//! the daemon's build of the same drv.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "td-realize",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> td-realize: td realizes the hello drv with no guix-daemon and no /var/guix/db — computes the input closure itself by CONTENT-SCANNING /gnu/store, builds in its userns sandbox, registers; output matches the daemon (oracle)"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: no td-builder" >&2; exit 1; }; \
scratch="$PWD/.td-realize-scratch"; chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; mkdir -p "$scratch"; \
$TD_GUIX repl -L . tests/td-drv-build-drv.scm 2>/dev/null > "$scratch/facts.txt"; \
drv=`sed -n 's/^HELLO_DRV=//p' "$scratch/facts.txt"`; \
out=`sed -n 's/^HELLO_OUT=//p' "$scratch/facts.txt"`; \
hash=`sed -n 's/^HELLO_HASH=//p' "$scratch/facts.txt"`; \
narsize=`sed -n 's/^HELLO_NARSIZE=//p' "$scratch/facts.txt"`; \
deriver=`sed -n 's/^HELLO_DERIVER=//p' "$scratch/facts.txt"`; \
test -n "$drv" -a -n "$out" -a -n "$hash" -a -n "$narsize" -a -n "$deriver" || { echo "ERROR: missing oracle facts" >&2; exit 1; }; \
"$tb" drv-emit-to "$drv" "$scratch/emitted.drv" >/dev/null || { echo "FAIL: drv-emit-to" >&2; exit 1; }; \
"$tb" realize "$scratch/emitted.drv" /gnu/store "$scratch/b" > "$scratch/out.txt" 2> "$scratch/realize.err" || { echo "FAIL: realize errored" >&2; cat "$scratch/realize.err" >&2; exit 1; }; \
sed 's/^/   /' "$scratch/realize.err"; \
cl=`grep -c . "$scratch/b/closure.txt"`; test "$cl" -gt 0 || { echo "FAIL: td computed an empty closure" >&2; exit 1; }; \
echo ">> [DURABLE] td computed the input closure itself by CONTENT-SCANNING /gnu/store ($cl paths, no /var/guix/db, no guix gc, no daemon)"; \
echo ">> [DURABLE: discriminator] realize the SAME drv against a store dir that LACKS the inputs — its content-scan must find an INCOMPLETE closure and the build must FAIL (proving the closure is scanned from the given dir, NOT a hidden /var/guix read)"; \
empty="$scratch/empty-store"; mkdir -p "$empty"; \
if "$tb" realize "$scratch/emitted.drv" "$empty" "$scratch/b-empty" > "$scratch/empty.out" 2> "$scratch/empty.err"; then \
  echo "FAIL: realize against an EMPTY store dir SUCCEEDED — the closure was not content-scanned from the given dir (a hidden /var/guix read?)" >&2; cat "$scratch/empty.err" >&2; exit 1; \
fi; \
echo ">> [DURABLE: discriminator] confirmed — realize against an inputs-less store dir fails; the seed content-scan of the given dir is load-bearing"; \
say=`"$out/bin/hello"`; test "$say" = "Hello, world!" || { echo "FAIL: realized hello did not greet (got '$say')" >&2; exit 1; }; \
echo ">> [DURABLE: behavioral] the realized hello runs: $say"; \
reg="$scratch/b/registration"; \
grep -qx "path $out" "$reg" || { echo "FAIL: path mismatch vs daemon $out" >&2; cat "$reg" >&2; exit 1; }; \
grep -qx "nar-hash sha256:$hash" "$reg" || { echo "FAIL: NAR-hash mismatch vs daemon" >&2; exit 1; }; \
grep -qx "nar-size $narsize" "$reg" || { echo "FAIL: NAR-size mismatch vs daemon" >&2; exit 1; }; \
grep -qx "deriver $deriver" "$reg" || { echo "FAIL: deriver mismatch vs daemon" >&2; exit 1; }; \
echo ">> [MIGRATION ORACLE — removable when guix retires] realize output == the daemon's build of the same drv (path/NAR/size/deriver)"; \
chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; \
echo "PASS: td-builder REALIZED the hello drv with NO guix-daemon AND NO /var/guix/db in the path — it computed the $cl-path input closure itself by CONTENT-SCANNING /gnu/store (scanForReferences, not guix gc / not a store-db read), built in its userns sandbox, and registered the output; the realized hello runs (durable), realize against an inputs-less store dir fails so the content-scan is load-bearing (durable discriminator), and (oracle) the output is byte-identical to the daemon's build of the same drv."
"##,
    }
}
