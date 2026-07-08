//! stage0-cold-start — a COLD stage0 placement needs NO guix state (#313). Gate 170
//! proved td-builder needs no guix to be COMPILED; this gate proves the PLACEMENT half:
//! `tests/stage0-builder.sh` (the one entry point every stage0 consumer goes through —
//! cache-lib's load_stage0, the check prelude, the daemon-ensure probe) places a stage0
//! from a cold cache with guix's private state HIDDEN. Before #313, store-add-builder
//! hard-read /var/guix/db/db.sqlite as its reference-scan seed, so any cold start (or any
//! builder/ edit — a new fingerprint) FATALed on a guix-less host: exactly the machine
//! `check-harness` exists for. Now the reference-scan candidates come from a readdir of
//! the seed store DIRECTORY (scan_candidate_index, the #267 content-scan pattern), and an
//! absent dir contributes nothing.
//!
//! Per the differential+durable discipline:
//! [DURABLE behavioral] with /var/guix bind-mounted EMPTY in a private mount ns, a cold
//! `tests/stage0-builder.sh` places a stage0 that RUNS its sentinel.
//! [DURABLE no-drift] the cold placement is IDENTICAL to the warm guix-host placement:
//! same canonical path, same builder.db closure — and the closure is NON-VACUOUS (the
//! glibc + gcc-lib refs the stage0 links were found WITHOUT the guix db, from the
//! store-dir readdir), so the parity is not two empty sets agreeing.
//! [DURABLE guix-less arm] store-add-builder with an ABSENT seed dir (a truly guix-less
//! host: no /gnu/store at all) still places, recording a self-only closure — the arm the
//! rustup/system-cc cold start takes (its stage0 embeds no store paths).
//! [DURABLE fail-loud] a PRESENT-but-unreadable seed dir (a regular file, not a
//! directory) ERRORS instead of silently placing a refless builder — the absent-dir
//! tolerance must not degrade into swallowing a misconfigured seed (a refless placement
//! would poison the closure and surface only as an opaque build failure).
//! [DURABLE self-discrimination] the SAME probe tree embedding a real store path records
//! NO external ref with the seed dir absent and DOES record it with /gnu/store passed —
//! the readdir candidate source is load-bearing, not decorative.
//!
//! The full guix-less VM run (`td-builder check check-harness` on a host with no guix at
//! all) is the arc this unblocks; this gate pins its cold-start contract inside the loop,
//! where a guix host can still A/B the warm baseline.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "stage0-cold-start",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> stage0-cold-start: a COLD stage0 placement works with guix state HIDDEN — same path, same closure as the warm guix-host placement; an absent seed dir places with no refs, a broken seed dir errors loudly (#313)"
set -euo pipefail; \
scratch="$PWD/.td-build-cache/stage0-cold-start"; rm -rf "$scratch"; mkdir -p "$scratch/empty"; \
echo ">> warm leg (baseline): realize the pinned seed + place the shared stage0 via provision_stage0 (the SAME prelude every stage0 consumer uses — cache-lib.sh)"; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; \
provision_stage0 || { echo "FAIL: warm stage0 provisioning failed" >&2; exit 1; }; \
cbw="$TD_BUILDER_PATH"; tbw="$TB"; wdb="$TD_BUILDER_DB"; \
echo ">> cold leg (the feature): fresh cache, /var/guix bind-mounted EMPTY in a private mount ns — the placement must need no guix db"; \
printf '%s\n' 'mkdir -p /var/guix' \
              'mount --bind "$1" /var/guix || exit 9' \
              'test -z "$(ls -A /var/guix)" || { echo "cold leg: /var/guix not hidden" >&2; exit 9; }' \
              'exec sh tests/stage0-builder.sh "$2"' > "$scratch/cold.sh"; \
cbc=`"$tbw" userns-private -- sh "$scratch/cold.sh" "$scratch/empty" "$scratch/cold"` \
  || { echo "FAIL: cold stage0 placement with /var/guix hidden failed — the guix-less cold start is broken (#313)" >&2; exit 1; }; \
test "$cbw" = "$cbc" || { echo "FAIL: cold placement $cbc != warm placement $cbw — provenance drift" >&2; exit 1; }; \
tbc="$scratch/cold/store/`basename "$cbc"`/bin/td-builder"; \
sent=`"$tbc"`; test "$sent" = "td-builder 0.1.0 ok" || { echo "FAIL: cold-placed stage0 sentinel was '$sent'" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the cold-placed stage0 runs its sentinel ($cbc)"; \
"$tbc" store-closure "$scratch/cold/builder.db" "$cbc" | sort > "$scratch/cl.cold"; \
"$tbw" store-closure "$wdb" "$cbw" | sort > "$scratch/cl.warm"; \
if test "`sha256sum < "$scratch/cl.cold" | cut -d' ' -f1`" != "`sha256sum < "$scratch/cl.warm" | cut -d' ' -f1`"; then \
  echo "FAIL: cold closure differs from warm closure (provenance drift):" >&2; \
  comm -3 "$scratch/cl.warm" "$scratch/cl.cold" >&2; exit 1; \
fi; \
grep -q -- '-glibc-' "$scratch/cl.cold" || { echo "FAIL: no glibc ref in the cold closure — the seed-store readdir scan found nothing (vacuous parity)" >&2; exit 1; }; \
grep -qE -- '-gcc-[0-9.]+-lib' "$scratch/cl.cold" || { echo "FAIL: no gcc-lib ref in the cold closure — the seed-store readdir scan missed the link deps" >&2; exit 1; }; \
echo "  [DURABLE no-drift] cold (guix state hidden) and warm closures are IDENTICAL and non-vacuous (glibc + gcc-lib found from the store-dir readdir alone)"; \
echo ">> guix-less arm: an ABSENT seed dir (no /gnu/store at all) must still place, with a self-only closure"; \
mkdir -p "$scratch/probe/bin"; printf 'no store refs in this tree\n' > "$scratch/probe/bin/tool"; \
pa=`"$tbw" store-add-builder probe-0.1.0 "$scratch/probe" "$scratch/pstore-a" "$scratch/pa.db" "$scratch/ABSENT"` \
  || { echo "FAIL: store-add-builder with an absent seed dir failed — the truly-guix-less arm is broken (#313)" >&2; exit 1; }; \
pan=`"$tbw" store-closure "$scratch/pa.db" "$pa" | grep -c .`; \
test "$pan" = 1 || { echo "FAIL: absent-seed-dir placement recorded $pan closure paths, expected 1 (self only)" >&2; exit 1; }; \
echo "  [DURABLE guix-less arm] absent seed dir: placement succeeds, closure is self-only"; \
echo ">> fail-loud arm: a PRESENT-but-unreadable seed dir (a regular file, not a directory) must ERROR — a broken seed must not silently place a refless builder (#313 fail-open guard)"; \
printf 'not a store directory\n' > "$scratch/notadir"; \
if "$tbw" store-add-builder probe-0.1.0 "$scratch/probe" "$scratch/pstore-f" "$scratch/pf.db" "$scratch/notadir" 2>"$scratch/pf.err"; then \
  echo "FAIL: store-add-builder ACCEPTED a non-directory seed store — a broken seed silently placed a refless builder (fail-open)" >&2; exit 1; \
fi; \
grep -qF "$scratch/notadir" "$scratch/pf.err" || { echo "FAIL: store-add-builder errored but did not name the bad seed store:" >&2; cat "$scratch/pf.err" >&2; exit 1; }; \
echo "  [DURABLE fail-loud] a non-directory seed store errors loudly, naming the bad path (not a silent refless placement)"; \
echo ">> self-discrimination: the SAME probe embedding a real store path — no ref with the dir absent, the ref WITH /gnu/store"; \
g=`grep -- '-glibc-' "$scratch/cl.cold" | head -1`; \
mkdir -p "$scratch/probe2/bin"; printf '%s' "$g" > "$scratch/probe2/bin/tool"; \
pb0=`"$tbw" store-add-builder probe2-0.1.0 "$scratch/probe2" "$scratch/pstore-b0" "$scratch/pb0.db" "$scratch/ABSENT"`; \
pbn=`"$tbw" store-closure "$scratch/pb0.db" "$pb0" | grep -c .`; \
test "$pbn" = 1 || { echo "FAIL: absent-seed-dir scan found $pbn paths for the embedded-ref probe, expected 1 (no candidates, no refs)" >&2; exit 1; }; \
pb=`"$tbw" store-add-builder probe2-0.1.0 "$scratch/probe2" "$scratch/pstore-b" "$scratch/pb.db" /gnu/store`; \
"$tbw" store-closure "$scratch/pb.db" "$pb" | sort > "$scratch/cl.pb"; \
grep -qF "$g" "$scratch/cl.pb" \
  || { echo "FAIL: the embedded ref $g was NOT found by the /gnu/store readdir scan — the candidate source is broken" >&2; exit 1; }; \
echo "  [DURABLE self-discrimination] same probe bytes: absent dir → self-only; /gnu/store → the embedded glibc ref recorded"; \
rm -rf "$scratch"; \
echo "PASS: the stage0 placement no longer needs ANY guix state: with /var/guix bind-mounted empty, a cold tests/stage0-builder.sh run placed a stage0 that runs, at the SAME canonical path with the SAME (non-vacuous: glibc + gcc-lib) builder.db closure as the warm guix-host placement — the reference scan's candidates come from a readdir of the seed store dir, not guix's private db. An absent seed dir (a truly guix-less host) still places with a self-only closure; a non-directory seed dir errors loudly (no silent refless placement); and the same probe tree records an embedded store ref ONLY when the dir is passed — the readdir candidate source is load-bearing. The guix-less check-harness cold start (#313) is unblocked."
"##,
    }
}
