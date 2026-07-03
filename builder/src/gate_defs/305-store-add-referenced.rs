//! store-add-referenced (DESIGN §7.1; td-store-db track — begin replacing guix-daemon).
//! td ADDS a path WITH references to its OWN store — the daemon's addToStore/addTextToStore
//! with a references set (after the no-reference flat #38 + recursive #41 adds), in pure
//! Rust, no daemon. `td-builder store-add-referenced` computes the content-addressed path
//! with the references FOLDED INTO THE TYPE (`make_text_path`: `text:<sorted refs>` — the
//! daemon's makeTextPath/makeType), WRITES the content into a td-owned store (canonical 0444
//! file), and REGISTERS the path with its `Refs` to the referenced paths. The canonical
//! referenced content-addressed item is a `.drv` (referenced by its input drvs/srcs).
//! R3 (guix-retirement ladder → #261): the subject `.drv` is now the one td ASSEMBLED
//! (tests/store-subject.sh — assemble-recipe, guix/Guile off PATH), NOT `guix build -d`, and
//! its references are read with `td-builder drv-refs` (parse inputDrvs ∪ inputSrcs), NOT `guix
//! gc --references`. So this gate runs with guix OFF PATH. The removable guix oracle (the
//! stored `.drv` byte-identical to the DAEMON's own + references == `guix gc --references`) is
//! DROPPED per CLAUDE.md directive 3 (called out in the PR); in its place a genuine ROUND-TRIP:
//! the references RECOVERED from the `.drv` bytes by `drv-refs` (parse — a DIFFERENT provenance
//! from the recipe inputs the ASSEMBLER folded in at build time) fold — through the shared
//! make_text_path — back to the SAME store path the assembler produced (drop a ref and the path
//! diverges, so this proves drv-refs recovers the exact folded set). Plus: the stored `.drv` is NAR-identical
//! to the source, and td registers EXACTLY the parsed references (read back by td's own
//! `store-query references`). Boundary: td writes only its OWN scratch store/DB. Needs
//! td-builder + the corpus build → heavy pool + the build-recipes prelude.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "store-add-referenced",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        script: r##"
echo ">> store-add-referenced: td ADDS a td-ASSEMBLED hello .drv WITH references to its OWN store + registers the references (pure Rust, no daemon; guix off PATH) — a round-trip of the folded references"
set -euo pipefail; \
. tests/store-subject.sh; \
scratch="$PWD/.store-add-referenced-scratch"; rm -rf "$scratch"; mkdir -p "$scratch/store"; \
td_store_subject "$scratch" || exit 1; \
drv="$SUBJ_LOCALDRV"; tddrv="$SUBJ_DRV"; \
name=`basename "$tddrv"`; name=${name:33}; \
"$TB" drv-refs "$drv" | sort > "$scratch/refs.txt"; \
nref=`wc -l < "$scratch/refs.txt"`; \
test "$nref" -gt 0 || { echo "FAIL: the .drv has no references (the round-trip would be vacuous)" >&2; exit 1; }; \
echo ">> hello's td-assembled .drv ($name) has $nref references (its input drvs/srcs, parsed by td-builder drv-refs)"; \
td_path=`"$TB" store-add-referenced "$name" "$drv" "$scratch/refs.txt" "$scratch/store" "$scratch/td.db"`; \
test "$td_path" = "$tddrv" || { echo "FAIL: td computed $td_path != the ASSEMBLER's $tddrv (references not folded into the path correctly)" >&2; exit 1; }; \
echo "   the $nref references PARSED from the .drv fold back to the SAME path the assembler computed from the recipe inputs (round-trip)"; \
base=`basename "$td_path"`; \
test -f "$scratch/store/$base" || { echo "FAIL: td did not write the .drv into its store" >&2; exit 1; }; \
td_nar=`"$TB" nar-hash "$scratch/store/$base"`; src_nar=`"$TB" nar-hash "$drv"`; \
test "$td_nar" = "$src_nar" || { echo "FAIL: td's stored .drv NAR $td_nar != the source .drv $src_nar" >&2; exit 1; }; \
echo "   td's stored .drv is byte-identical (NAR) to the source: $src_nar"; \
td_refs=`"$TB" store-query "$scratch/td.db" references | sed 's#^[^|]*|##' | sort`; \
parsed_refs=`cat "$scratch/refs.txt"`; \
test "$td_refs" = "$parsed_refs" || { echo "FAIL: td's registered references (read by td's own reader) != the parsed references" >&2; echo "$td_refs" | sed 's/^/  registered: /' >&2; echo "$parsed_refs" | sed 's/^/  parsed:     /' >&2; exit 1; }; \
echo "   td REGISTERED all $nref references (read back by TD'S OWN reader) == td-builder drv-refs (the parsed set)"; \
rm -rf "$scratch"; \
echo "PASS: td ADDED a path WITH references to its OWN store, in pure Rust with NO daemon — for hello's TD-ASSEMBLED .drv and its $nref references (guix off PATH; no guix build -d, no guix gc --references). td computed the content-addressed path with the references folded into the type (makeTextPath), and the references RECOVERED from the .drv bytes by drv-refs (parse) fold back through the shared make_text_path to the SAME path the ASSEMBLER produced from the recipe inputs — a round-trip that proves drv-refs recovers the exact folded set (drop one ref and it diverges). The stored .drv is NAR-identical to the source, and td registered exactly the parsed references (read back by td's own store-query). The removable guix oracle (== the daemon's .drv + == guix gc --references) was dropped (directive 3). td now owns addToStore for flat (#38), recursive (#41), AND referenced paths."
"##,
    }
}
