//! stage0-cold-start — a COLD stage0 placement needs NO guix state (#313). Gate 170
//! proved td-builder needs no guix to be COMPILED; this gate proves the PLACEMENT half:
//! `td-builder stage0-place` (builder/src/stage0.rs — the one entry point every stage0
//! consumer goes through: cache-lib's load_stage0, the check prelude, the daemon-ensure
//! probe) places a stage0 from a cold cache with guix's private state HIDDEN. Before #313,
//! store-add-builder hard-read /var/guix/db/db.sqlite as its reference-scan seed, so any
//! cold start (or any builder/ edit — a new fingerprint) FATALed on a guix-less host. Now
//! the reference-scan candidates come from a readdir of the seed store DIRECTORY
//! (scan_candidate_index, the #267 content-scan pattern), and an absent dir contributes
//! nothing.
//!
//! The stage0 builder is now musl-STATIC (builder/src/stage0.rs), so it embeds NO external
//! store references at all — its recorded closure is SELF-ONLY by construction. stage0_place
//! therefore scans an EMPTY seed dir and records just the builder itself. That self-only
//! closure IS the #469 no-leak property: a static builder drags no host runtime lib dir
//! (nor the +x `libasan.la` libtool archive beside it, #468) into the sandbox.
//!
//! Per the differential+durable discipline:
//! [DURABLE behavioral] with /var/guix bind-mounted EMPTY in a private mount ns, a cold
//! `td-builder stage0-place` places a stage0 that RUNS its sentinel.
//! [DURABLE no-drift] the cold placement is IDENTICAL to the warm guix-host placement:
//! same canonical path, same builder.db closure — and that closure is SELF-ONLY (exactly
//! the one canonical builder path, no external ref), the musl-static no-leak invariant.
//! [DURABLE guix-less arm] store-add-builder with an ABSENT seed dir (a truly guix-less
//! host: no /gnu/store at all) still places, recording a self-only closure — the arm the
//! rustup/system-cc cold start takes.
//! [DURABLE fail-loud] a PRESENT-but-unreadable seed dir (a regular file, not a
//! directory) ERRORS instead of silently placing a refless builder — the absent-dir
//! tolerance must not degrade into swallowing a misconfigured seed (a refless placement
//! would poison the closure and surface only as an opaque build failure).
//! [DURABLE self-discrimination] the reference-scan mechanism is load-bearing, proven
//! independently of the (now refless) builder: a probe tree embedding a SYNTHETIC store
//! path records NO ref with the seed dir absent and DOES record it when a controlled seed
//! dir holding the matching entry is passed — the readdir candidate source drives the scan.
//!
//! The full guix-less VM run on a host with no guix at all is the arc this unblocks;
//! this gate pins its cold-start contract inside the loop,
//! where a guix host can still A/B the warm baseline.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "stage0-cold-start",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        non_blocking: true,
        script: r##"
echo ">> stage0-cold-start: a COLD stage0 placement works with guix state HIDDEN — same path, same SELF-ONLY closure as the warm guix-host placement; an absent seed dir places with no refs, a broken seed dir errors loudly (#313)"
set -euo pipefail; \
scratch="$PWD/.td-build-cache/stage0-cold-start"; rm -rf "$scratch"; mkdir -p "$scratch/empty"; \
echo ">> warm leg (baseline): realize the pinned seed + place the shared stage0 via provision_stage0 (the SAME prelude every stage0 consumer uses — cache-lib.sh)"; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; \
provision_stage0 || { rc=$?; echo "warm stage0 provisioning did not complete (exit $rc): 69 = no toolchain reachable in the jail (skipped); other = a real failure" >&2; exit $rc; }; \
cbw="$TD_BUILDER_PATH"; tbw="$TB"; wdb="$TD_BUILDER_DB"; \
echo ">> cold leg (the feature): fresh cache, /var/guix bind-mounted EMPTY in a private mount ns — the placement must need no guix db"; \
	printf '%s\n' 'mkdir -p /var/guix || exit 9' \
	              'mount --bind "$1" /var/guix || exit 9' \
              'test -z "$(ls -A /var/guix)" || { echo "cold leg: /var/guix not hidden" >&2; exit 9; }' \
              'exec "$3" stage0-place "$2"' > "$scratch/cold.sh"; \
	cbc=`"$tbw" userns-private -- sh "$scratch/cold.sh" "$scratch/empty" "$scratch/cold" "$tbw"` \
  || { rc=$?; echo "cold stage0 placement with /var/guix hidden did not complete (exit $rc): 69 = no toolchain reachable in the jail (skipped); other = the guix-less cold start is broken (#313)" >&2; exit $rc; }; \
test "$cbw" = "$cbc" || { echo "FAIL: cold placement $cbc != warm placement $cbw — provenance drift" >&2; exit 1; }; \
tbc="$scratch/cold/store/`basename "$cbc"`/bin/td-builder"; \
sent=`"$tbc"`; test "$sent" = "td-builder 0.1.0 ok" || { echo "FAIL: cold-placed stage0 sentinel was '$sent'" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the cold-placed stage0 runs its sentinel ($cbc)"; \
"$tbc" store-closure "$scratch/cold/builder.db" "$cbc" | sort > "$scratch/cl.cold"; \
"$tbw" store-closure "$wdb" "$cbw" | sort > "$scratch/cl.warm"; \
	if test "`"$tbw" text sha256 "$scratch/cl.cold"`" != "`"$tbw" text sha256 "$scratch/cl.warm"`"; then \
	  echo "FAIL: cold closure differs from warm closure (provenance drift):" >&2; \
	  comm -3 "$scratch/cl.warm" "$scratch/cl.cold" >&2; exit 1; \
	fi; \
	cn=`"$tbw" text count-nonempty "$scratch/cl.cold"`; \
	test "$cn" = 1 || { echo "FAIL: cold closure is not self-only ($cn paths) — the musl-static stage0 builder must record ONLY itself; an external ref means it linked dynamically and would leak a host runtime lib dir into the sandbox (re #469)" >&2; cat "$scratch/cl.cold" >&2; exit 1; }; \
	"$tbw" text line-exact "$cbc" "$scratch/cl.cold" || { echo "FAIL: the single cold-closure path is not the canonical builder $cbc" >&2; cat "$scratch/cl.cold" >&2; exit 1; }; \
echo "  [DURABLE no-drift] cold (guix state hidden) and warm closures are IDENTICAL and SELF-ONLY (exactly the canonical builder path, no external ref) — the musl-static link keeps every host runtime lib dir out of the sandbox (re #469)"; \
echo ">> guix-less arm: an ABSENT seed dir (no /gnu/store at all) must still place, with a self-only closure"; \
mkdir -p "$scratch/probe/bin"; printf 'no store refs in this tree\n' > "$scratch/probe/bin/tool"; \
pa=`"$tbw" store-add-builder probe-0.1.0 "$scratch/probe" "$scratch/pstore-a" "$scratch/pa.db" "$scratch/ABSENT"` \
  || { echo "FAIL: store-add-builder with an absent seed dir failed — the truly-guix-less arm is broken (#313)" >&2; exit 1; }; \
	pan=`"$tbw" store-closure "$scratch/pa.db" "$pa" | "$tbw" text count-nonempty -`; \
test "$pan" = 1 || { echo "FAIL: absent-seed-dir placement recorded $pan closure paths, expected 1 (self only)" >&2; exit 1; }; \
echo "  [DURABLE guix-less arm] absent seed dir: placement succeeds, closure is self-only"; \
echo ">> fail-loud arm: a PRESENT-but-unreadable seed dir (a regular file, not a directory) must ERROR — a broken seed must not silently place a refless builder (#313 fail-open guard)"; \
printf 'not a store directory\n' > "$scratch/notadir"; \
if "$tbw" store-add-builder probe-0.1.0 "$scratch/probe" "$scratch/pstore-f" "$scratch/pf.db" "$scratch/notadir" 2>"$scratch/pf.err"; then \
  echo "FAIL: store-add-builder ACCEPTED a non-directory seed store — a broken seed silently placed a refless builder (fail-open)" >&2; exit 1; \
fi; \
	"$tbw" text contains "$scratch/notadir" "$scratch/pf.err" || { echo "FAIL: store-add-builder errored but did not name the bad seed store:" >&2; cat "$scratch/pf.err" >&2; exit 1; }; \
echo "  [DURABLE fail-loud] a non-directory seed store errors loudly, naming the bad path (not a silent refless placement)"; \
echo ">> self-discrimination: the readdir candidate source is load-bearing — a probe embedding a SYNTHETIC store path records NO ref with the seed dir absent, and DOES record it when a controlled seed dir holding the matching entry is passed"; \
seedref="0123456789abcdef0123456789abcdef-fakeref-1.0"; \
mkdir -p "$scratch/seed/$seedref"; g="$scratch/seed/$seedref"; \
mkdir -p "$scratch/probe2/bin"; printf '%s' "$g" > "$scratch/probe2/bin/tool"; \
pb0=`"$tbw" store-add-builder probe2-0.1.0 "$scratch/probe2" "$scratch/pstore-b0" "$scratch/pb0.db" "$scratch/ABSENT"`; \
	pbn=`"$tbw" store-closure "$scratch/pb0.db" "$pb0" | "$tbw" text count-nonempty -`; \
test "$pbn" = 1 || { echo "FAIL: absent-seed-dir scan found $pbn paths for the embedded-ref probe, expected 1 (no candidates, no refs)" >&2; exit 1; }; \
pb=`"$tbw" store-add-builder probe2-0.1.0 "$scratch/probe2" "$scratch/pstore-b" "$scratch/pb.db" "$scratch/seed"`; \
"$tbw" store-closure "$scratch/pb.db" "$pb" | sort > "$scratch/cl.pb"; \
	"$tbw" text contains "$g" "$scratch/cl.pb" \
	  || { echo "FAIL: the embedded ref $g was NOT found by the controlled seed-dir readdir scan — the candidate source is broken" >&2; exit 1; }; \
echo "  [DURABLE self-discrimination] same probe bytes: absent dir → self-only; controlled seed dir → the embedded synthetic ref recorded"; \
rm -rf "$scratch"; \
echo "PASS: the stage0 placement no longer needs ANY guix state: with /var/guix bind-mounted empty, a cold td-builder stage0-place run placed a stage0 that runs, at the SAME canonical path with the SAME SELF-ONLY builder.db closure as the warm guix-host placement — the musl-static builder embeds no external store ref, so an empty seed scan is exactly right and no host runtime lib dir leaks into the sandbox (re #469). The reference-scan mechanism stays load-bearing (a probe with a synthetic store path records the ref ONLY when a controlled seed dir holding the matching entry is passed); an absent seed dir (a truly guix-less host) still places with a self-only closure; and a non-directory seed dir errors loudly (no silent refless placement). The guix-less cold start (#313) is unblocked."
"##,
    }
}
