//! store-gc (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td computes
//! the GC-reachable CLOSURE of a td-BUILT hello — the daemon's THIRD role (GC) — TWO
//! daemon-free ways, in pure Rust, over a TD-OWNED store:
//! (1) DB walk: `td-builder store-closure DB ROOT` reads the DB with td's own SQLite
//! reader (`store_db_read`) and walks the `Refs` graph from ROOT (the GC "mark" set).
//! (2) Content scan: `td-builder store-closure-scan STORE ROOT` re-derives the SAME set by
//! NAR-scanning ROOT's bytes for store-path references, transitively (the daemon's
//! scanForReferences), with NO store DB — the closure query the loop's store-native
//! gates use to resolve a runtime closure without the guix daemon or its DB.
//! R3 (guix-retirement ladder → #261): the SUBJECT is now td-BUILT (tests/store-subject.sh —
//! `td-builder build-recipe` GNU hello, cache-hit off the build-recipes prelude) and its
//! closure is CONTENT-SCANNED, so this gate runs with guix OFF PATH — no `guix build`, no
//! `guix gc`. The removable guix-comparison oracle (== `guix gc -R`) is DROPPED per CLAUDE.md
//! directive 3 (called out in the PR): the two TD methods are asserted equal to EACH OTHER and
//! to the staged closure (td's own internal consistency), a STRONGER feature test over a
//! td-built artifact than equality-vs-guix. A missing Refs edge or a missed byte reference
//! would make the two disagree (verified-red). BOUNDARY: the scan of an OUTPUT root is the
//! runtime closure (`.drv`-free); the destructive SWEEP is store-gc-sweep, not here. Needs
//! td-builder + the corpus build, so it slots in the heavy pool and the build-recipes prelude.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "store-gc",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        script: r##"
echo ">> store-gc: td computes the GC-reachable closure of a TD-BUILT hello from its OWN store DB (pure Rust, no daemon) == td's own content scan (guix off PATH; no guix gc)"
set -euo pipefail; \
. tests/store-subject.sh; \
scratch="$PWD/.store-gc-scratch"; rm -rf "$scratch"; mkdir -p "$scratch"; \
td_store_subject "$scratch" || exit 1; \
"$TB" store-register "$SUBJ_ROOT" "$SUBJ_DRV" "$SUBJ_CLOSURE" "$scratch/td.db"; \
td_reach=`"$TB" store-closure "$scratch/td.db" "$SUBJ_ROOT" | sort -u`; \
scan_reach=`"$TB" store-closure-scan "$SUBJ_STORE" "$SUBJ_ROOT" | sort -u`; \
staged=`sort -u "$SUBJ_CLOSURE"`; \
n=`echo "$staged" | wc -l`; \
test "$td_reach" = "$scan_reach" || { echo "FAIL: td's DB-walk GC closure != td's content-scan closure" >&2; echo "$td_reach" | sed 's/^/  db:   /' >&2; echo "$scan_reach" | sed 's/^/  scan: /' >&2; exit 1; }; \
echo "   (1) td's DB-walk (Refs graph) and (2) content-scan closures of the td-built hello AGREE ($n paths)"; \
test "$td_reach" = "$staged" || { echo "FAIL: the reachable set != the staged closure (register/scan disagree with what was staged)" >&2; exit 1; }; \
echo "   both == the staged runtime closure — every staged member is reachable from hello's output"; \
if echo "$scan_reach" | grep -q '\.drv$'; then echo "FAIL: the content-scan runtime closure of an OUTPUT root unexpectedly contains a .drv — the output-root boundary is broken" >&2; exit 1; fi; \
echo "   (2b) the content-scan runtime closure is .drv-free (an OUTPUT root's runtime closure, distinct from the structural .drv-input graph)"; \
rm -rf "$scratch"; \
echo "PASS: td computed the GC-reachable CLOSURE of a TD-BUILT hello ($n paths) TWO daemon-free ways, in pure Rust, over its OWN store — (1) walking the Refs graph in a store DB it wrote (td's own SQLite reader) and (2) CONTENT-SCANNING the staged store from hello's output — and BOTH agree with each other AND with the staged closure. td's Refs traversal and its byte-level scan reconstruct the same GC mark set. The subject is td-built and the closure content-scanned (guix off PATH; no guix build, no guix gc); the removable == guix gc -R oracle was dropped (directive 3). The destructive sweep is store-gc-sweep."
"##,
    }
}
