//! store-backend (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). A td
//! STORE BACKEND for a BUILD OUTPUT — the capstone that composes the store stack into a
//! working, daemon-free backend. `td-builder store-add-output` PLACES a built output's tree
//! into a td-owned store at its output path and FULLY REGISTERS it (hash + narSize +
//! deriver + the output's references + the drv->output mapping — the daemon's post-build
//! registration), and then td's OWN tools SERVE it: store-query (the registration + the
//! references), store-verify (integrity re-hashed against the PLACED files), all with NO
//! daemon in any store operation.
//! R3 (guix-retirement ladder → #261): the SUBJECT is now td-BUILT (tests/store-subject.sh —
//! hello via build-recipe, cache-hit) and its closure CONTENT-SCANNED, so this gate runs with
//! guix OFF PATH — no `guix build [-d]`, no `guix gc`, no /var/guix read. The removable oracle
//! (the placed tree == the DAEMON's built output; store-query == the live /var/guix/db record;
//! references == `guix gc --references`; deriver/drv->output == the daemon's) is DROPPED per
//! CLAUDE.md directive 3 (called out in the PR). In its place, td-INTERNAL consistency over a
//! td-built subject: (1) the placed tree is NAR-identical to the SOURCE staged tree; (2)
//! store-query info == the re-derived hash+narSize of that tree; (3) store-query references ==
//! hello's DIRECT references as INDEPENDENTLY computed by store-register over the closure (two
//! separate scan paths agree); (4) store-verify passes against td's own placed files; (5) the
//! deriver + drv->output mapping td records is exactly (td-assembled .drv) -> out -> the output.
//! Boundary: td writes only its OWN scratch store/DB; the host store is untouched. Needs
//! td-builder + the corpus build → heavy pool + the build-recipes prelude.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "store-backend",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> store-backend: a td store backend HOLDS + SERVES a TD-BUILT hello output (place + register + query + verify, pure Rust, no daemon; guix off PATH)"
set -euo pipefail; \
. tests/store-subject.sh; \
scratch="$PWD/.store-backend-scratch"; rm -rf "$scratch"; mkdir -p "$scratch/store"; \
td_store_subject "$scratch" || exit 1; \
"$TB" store-add-output "$SUBJ_ROOT" "$SUBJ_DRV" "$SUBJ_CLOSURE" "$scratch/store" "$scratch/td.db" >/dev/null; \
base=`basename "$SUBJ_ROOT"`; \
test -d "$scratch/store/$base" || { echo "FAIL: td did not place the output tree into its store" >&2; exit 1; }; \
placed_nar=`"$TB" nar-hash "$scratch/store/$base"`; src_nar=`"$TB" nar-hash "$SUBJ_ROOT"`; \
test "$placed_nar" = "$src_nar" || { echo "FAIL: the placed output NAR $placed_nar != the source staged tree $src_nar" >&2; exit 1; }; \
echo "   (1) td PLACED hello's output into its store, NAR-identical to the source staged tree: $src_nar"; \
td_info=`"$TB" store-query "$scratch/td.db" info`; \
test "`echo "$td_info" | cut -d'|' -f1`" = "$SUBJ_ROOT" || { echo "FAIL: store-query info path != $SUBJ_ROOT ($td_info)" >&2; exit 1; }; \
test "`echo "$td_info" | cut -d'|' -f2`" = "$src_nar" || { echo "FAIL: store-query info hash != the re-derived NAR hash ($td_info)" >&2; exit 1; }; \
echo "   (2) td's store SERVES the registration (store-query info) == the re-derived hash + narSize"; \
"$TB" store-register "$SUBJ_ROOT" "$SUBJ_DRV" "$SUBJ_CLOSURE" "$scratch/full.db" >/dev/null; \
direct_refs=`"$TB" store-query "$scratch/full.db" references | grep -F "$SUBJ_ROOT|" | sed 's#^[^|]*|##' | sort`; \
test -n "$direct_refs" || { echo "FAIL: hello's output has no direct references (the check would be vacuous)" >&2; exit 1; }; \
td_refs=`"$TB" store-query "$scratch/td.db" references | sed 's#^[^|]*|##' | sort`; \
test "$td_refs" = "$direct_refs" || { echo "FAIL: the backend's served references != store-register's independent direct-ref scan" >&2; echo "$td_refs" | sed 's/^/  backend:  /' >&2; echo "$direct_refs" | sed 's/^/  register: /' >&2; exit 1; }; \
echo "   (3) td's store SERVES the references (store-query references) == store-register's INDEPENDENT direct-ref scan of the closure ($(echo "$td_refs" | wc -l) refs)"; \
"$TB" store-verify "$scratch/td.db" "$scratch/store" || { echo "FAIL: store-verify flagged the placed output" >&2; exit 1; }; \
echo "   (4) td's store VERIFIES (store-verify) the placed output's integrity against its OWN files"; \
doutsql="SELECT (SELECT deriver FROM ValidPaths WHERE path='$SUBJ_ROOT')||' :: '||v.path||':'||d.id||':'||d.path FROM DerivationOutputs d JOIN ValidPaths v ON d.drv=v.id WHERE d.path='$SUBJ_ROOT'"; \
td_dout=`sqlite3 "$scratch/td.db" "$doutsql"`; \
test "$td_dout" = "$SUBJ_DRV :: $SUBJ_DRV:out:$SUBJ_ROOT" || { echo "FAIL: td's deriver/drv->output ($td_dout) != the expected (td-assembled .drv) -> out -> output" >&2; exit 1; }; \
echo "   (5) td's store records the deriver + drv->output mapping == (the td-assembled .drv) -> out -> the output"; \
rm -rf "$scratch"; \
echo "PASS: a td STORE BACKEND holds + serves a TD-BUILT hello output, in pure Rust with NO daemon in any store operation and guix OFF PATH (no guix build, no guix gc, no /var/guix read) — td PLACED hello's built output into a td-owned store (NAR-identical to the source staged tree), FULLY REGISTERED it (hash + narSize + deriver + references + drv->output), and td's OWN tools SERVE it: store-query returns the registration + references, cross-checked against store-register's INDEPENDENT direct-ref scan, and store-verify re-hashes the PLACED files and confirms integrity. The removable guix oracle (daemon-built output + /var/guix/db record + guix gc --references) was dropped (directive 3); the subject is td-built and every check is td-internal. td now owns the full store backend — write/read the DB, add (flat/recursive/referenced), GC (mark + sweep), verify, AND back a build output end to end."
"##,
    }
}
