//! store-gc-sweep (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). The
//! DESTRUCTIVE GC SWEEP — the other half of GC, after the mark/liveness `store-closure`
//! (#39), in pure Rust, no daemon. `td-builder store-gc-sweep STORE-DIR DB ROOT` computes
//! the live set (closure of ROOT over the Refs), DELETES every registered content path NOT
//! reachable from ROOT from the td-owned STORE-DIR, and rewrites the DB to the live set
//! (ValidPaths + Refs renumbered).
//! R3 (guix-retirement ladder → #261): the SUBJECT is now td-BUILT (tests/store-subject.sh —
//! hello via build-recipe, cache-hit) staged into a td-OWNED store and its closure
//! CONTENT-SCANNED, so this gate runs with guix OFF PATH — no `guix build`, no `guix gc`. The
//! removable guix-comparison oracle (survivors == `guix gc -R glibc`) is DROPPED per CLAUDE.md
//! directive 3 (called out in the PR): the expected live set is td's OWN mark phase
//! (`store-closure DB glibc` — the reachable set the sweep must keep), so the gate asserts the
//! sweep KEEPS exactly what td's own liveness walk marks and DELETES the rest. Sweeping with
//! ROOT=glibc (a PROPER subset of hello's closure), the surviving store entries AND the
//! rewritten DB hold EXACTLY that reachable set and the dead paths' files are gone. Boundary:
//! the sweep deletes ONLY from the td-owned staged STORE-DIR and rewrites only its DB — the
//! host /gnu/store is NEVER touched. Needs td-builder + the corpus build → heavy pool + the
//! build-recipes prelude.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "store-gc-sweep",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        store: StoreMode::Private, // cold by design (#317 audit): GC-sweep semantics assert exact contents of a fresh fixture store
        non_blocking: true,
        script: r##"
echo ">> store-gc-sweep: td DELETES the GC-dead paths from its OWN store + rewrites the DB to the live set (destructive GC sweep of a TD-BUILT closure, pure Rust, no daemon; guix off PATH) == td's own mark phase"
set -euo pipefail; \
. tests/store-subject.sh; \
scratch="$PWD/.store-gc-sweep-scratch"; rm -rf "$scratch"; mkdir -p "$scratch"; \
td_store_subject "$scratch" || exit 1; \
n="$SUBJ_N"; \
"$TB" store-register "$SUBJ_ROOT" "$SUBJ_DRV" "$SUBJ_CLOSURE" "$scratch/td.db" >/dev/null; \
root=`grep -- '-glibc-' "$SUBJ_CLOSURE" | head -1`; \
test -n "$root" || { echo "FAIL: no glibc in hello's closure to use as a non-trivial GC root" >&2; exit 1; }; \
live=`"$TB" store-closure "$scratch/td.db" "$root" | sed 's#.*/##' | sort`; \
nlive=`echo "$live" | wc -l`; \
test "$nlive" -lt "$n" || { echo "FAIL: glibc's closure is not a PROPER subset of hello's ($nlive vs $n) — nothing would be swept" >&2; exit 1; }; \
echo ">> td store holds hello's $n-path closure; GC root glibc marks $nlive live (td's own store-closure), $(($n-$nlive)) dead"; \
"$TB" store-gc-sweep "$SUBJ_STORE" "$scratch/td.db" "$root"; \
survivors=`ls "$SUBJ_STORE" | sort`; \
test "$survivors" = "$live" || { echo "FAIL: surviving store entries != td's marked live set" >&2; echo "$survivors" | sed 's/^/  surv: /' >&2; echo "$live" | sed 's/^/  live: /' >&2; exit 1; }; \
echo "   td DELETED the $(($n-$nlive)) dead paths; the store now holds EXACTLY the $nlive marked-live paths"; \
db_paths=`"$TB" store-query "$scratch/td.db" info | sed 's#|.*##;s#.*/##' | sort`; \
test "$db_paths" = "$live" || { echo "FAIL: the swept DB's ValidPaths != the live set" >&2; echo "$db_paths" | sed 's/^/  db:   /' >&2; echo "$live" | sed 's/^/  live: /' >&2; exit 1; }; \
echo "   the rewritten DB records EXACTLY the live set (dead ValidPaths rows removed)"; \
rm -rf "$scratch"; \
echo "PASS: td performed the DESTRUCTIVE GC SWEEP on its OWN store, in pure Rust with NO daemon — over a TD-BUILT hello's $n-path closure staged into a td-owned store (guix off PATH; no guix build, no guix gc). After registering it and marking the live set with td's own store-closure (GC root glibc), td swept: it DELETED the dead paths' files and rewrote the DB so BOTH the surviving store entries AND the ValidPaths records hold EXACTLY the $nlive-path marked-live set. The removable == guix gc -R glibc oracle was replaced by td's own mark phase (directive 3). The host /gnu/store is never touched (the sweep operates only on the td-owned staged store). td now owns BOTH halves of GC — mark (#39) and sweep."
"##,
    }
}
