//! feed-shared — a persistent, SHARED td-feed daemon (`td-feed ensure-serve`) serves a warmed
//! artifact to multiple consumers on DIFFERENT worktrees, OFFLINE over loopback. This is how
//! a bunch of agents on different worktrees share ONE feed + its store (the follow-on to the
//! td-feed mirror, #157). Uses the td-built td-feed binary (depends on the `td-feed` gate);
//! everything is loopback (127.0.0.1) so the gate is offline/hermetic.
//!
//! [DURABLE behavioral] `ensure-serve` starts ONE shared daemon; two consumers in DIFFERENT
//! cwds (simulating two worktrees) each `td-feed warm` the SAME artifact THROUGH that one
//! shared feed and get byte-identical, sha256-verified content — no per-worktree refetch.
//! [DURABLE structural] a 2nd `ensure-serve` REUSES the running daemon (same addr + pid), so
//! the feed is persistent + shared, not started per run (the human's "persistent daemon").
//! [SELF-DISCRIMINATION] corrupting the shared store reds the consumer (serve sidecar
//! integrity); a cold (un-warmed) path 404s the consumer — the feed only serves what was
//! warmed + verified.
//! Needs the td-built td-feed binary (the td-feed gate builds + caches it from source).

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "feed-shared",
        pools: &[Pool::Heavy],
        needs: &["td-feed"],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> feed-shared: a persistent SHARED td-feed daemon serves a warmed artifact to consumers on different worktrees, offline over loopback; reused not restarted; reds on corruption/cold"
set -euo pipefail; \
tdfeed=`ls $PWD/.td-build-cache/td-feed/sd/newstore/*/bin/td-feed 2>/dev/null | head -1 || true`; \
test -x "$tdfeed" || { echo "ERROR: no td-built td-feed binary (the td-feed gate must build it first)" >&2; exit 1; }; \
echo "  [DURABLE structural] using the td-built td-feed: $tdfeed"; \
base="$PWD/.td-build-cache/feed-shared"; \
if [ -f "$base/feed/feed.pid" ]; then kill `cat "$base/feed/feed.pid"` 2>/dev/null || true; fi; \
rm -rf "$base"; mkdir -p "$base/origin/art" "$base/wtA" "$base/wtB"; \
export TD_FEED_DIR="$base/feed" TD_FEED_BIN="$tdfeed"; \
opid=""; \
trap 'kill $opid `cat "$TD_FEED_DIR/feed.pid" 2>/dev/null` 2>/dev/null || true' EXIT; \
head -c 4000 /dev/urandom > "$base/origin/art/blob"; \
sha=`sha256sum "$base/origin/art/blob" | cut -d' ' -f1`; printf '%s\n' "$sha" > "$base/origin/art/blob.sha256"; \
"$tdfeed" serve "$base/origin" 127.0.0.1:0 > "$base/origin.log" 2>&1 & opid=$!; \
for i in `seq 1 100`; do grep -q 'on http://' "$base/origin.log" && break; sleep 0.1; done; \
oaddr=`sed -n 's,.*on http://\([0-9.:]*\)/.*,\1,p' "$base/origin.log"`; \
test -n "$oaddr" || { echo "ERROR: the loopback origin did not bind" >&2; cat "$base/origin.log" >&2; exit 1; }; \
faddr=`"$tdfeed" ensure-serve`; fpid=`cat "$TD_FEED_DIR/feed.pid"`; \
test -n "$faddr" -a -n "$fpid" || { echo "ERROR: ensure-serve did not start a shared daemon" >&2; exit 1; }; \
echo "  [DURABLE behavioral] ensure-serve started ONE shared daemon at $faddr (pid $fpid)"; \
printf 'art/blob http://%s/art/blob %s\n' "$oaddr" "$sha" > "$base/warm.index"; \
"$tdfeed" warm "$base/warm.index" "$TD_FEED_DIR/store" >/dev/null || { echo "ERROR: could not warm the shared feed from the origin" >&2; exit 1; }; \
printf 'art/blob http://%s/art/blob %s\n' "$faddr" "$sha" > "$base/consume.index"; \
( cd "$base/wtA" && "$tdfeed" warm "$base/consume.index" "$base/cA" >/dev/null ) || { echo "FAIL: consumer A could not fetch via the shared feed" >&2; exit 1; }; \
( cd "$base/wtB" && "$tdfeed" warm "$base/consume.index" "$base/cB" >/dev/null ) || { echo "FAIL: consumer B (other worktree) could not fetch via the shared feed" >&2; exit 1; }; \
a=`sha256sum "$base/cA/art/blob" | cut -d' ' -f1`; b=`sha256sum "$base/cB/art/blob" | cut -d' ' -f1`; \
test "$a" = "$sha" -a "$b" = "$sha" || { echo "FAIL: consumer bytes differ from the pin ($a / $b vs $sha)" >&2; exit 1; }; \
echo "  [DURABLE behavioral] two consumers in different cwds (worktrees) fetched the SAME blob (sha $sha) through the ONE shared feed — offline, loopback only"; \
faddr2=`"$tdfeed" ensure-serve`; fpid2=`cat "$TD_FEED_DIR/feed.pid"`; \
test "$faddr" = "$faddr2" -a "$fpid" = "$fpid2" || { echo "FAIL: ensure-serve restarted the daemon ($faddr/$fpid -> $faddr2/$fpid2) instead of reusing it" >&2; exit 1; }; \
echo "  [DURABLE structural] a 2nd ensure-serve REUSED the same daemon ($faddr2 pid $fpid2) — persistent + shared, not per-run"; \
printf 'CORRUPT' | dd of="$TD_FEED_DIR/store/art/blob" bs=1 seek=0 conv=notrunc 2>/dev/null; \
if ( cd "$base/wtB" && "$tdfeed" warm "$base/consume.index" "$base/cBad" ) >/dev/null 2>&1; then echo "FAIL: a corrupted shared-store artifact was served (verify-on-serve not load-bearing)" >&2; exit 1; fi; \
echo "  [SELF-DISCRIMINATION] corrupting the shared store reds the consumer (serve sidecar integrity)"; \
printf 'art/missing http://%s/art/missing %s\n' "$faddr" "$sha" > "$base/cold.index"; \
if ( cd "$base/wtB" && "$tdfeed" warm "$base/cold.index" "$base/cCold" ) >/dev/null 2>&1; then echo "FAIL: an un-warmed (cold) path was served" >&2; exit 1; fi; \
echo "  [SELF-DISCRIMINATION] a cold (un-warmed) path 404s the consumer"; \
echo "PASS: feed-shared — \`td-feed ensure-serve\` runs ONE persistent shared td-feed daemon + store; multiple consumers on different worktrees fetch the same warmed artifact through it OFFLINE (loopback), a 2nd ensure reuses the daemon (same addr+pid), and a consumer reds on a corrupted shared store (serve sidecar integrity) or a cold path. This is how agents on different worktrees share one feed."
"##,
    }
}
