//! store-add-tree (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td
//! CANONICALLY restores a DIRECTORY TREE into its OWN store and registers it — the
//! RECURSIVE addToStore (the general write side, after the flat `store-add`), in pure
//! Rust, no daemon. `td-builder store-add-recursive` computes the content-addressed
//! `source` path from the tree's recursive NAR sha256 (`make_store_path("source", …)` —
//! the daemon's makeFixedOutputPath for recursive-sha256, no references), restores the
//! tree with `copy_canonical` (structure + contents + the file EXECUTABLE bit + symlinks
//! — the properties NAR captures; dir perms / rw bits / mtimes are NAR-irrelevant), and
//! registers it in a td store DB. The differential (daemon = oracle, prime directive 4):
//! the daemon's OWN interned `td-builder` source tree (a real directory added via
//! addToStore recursive, no refs — lowered with `guix repl`) gives the IDENTICAL store
//! path, and a tree BYTE-IDENTICAL (by NAR hash) to the one td restored; td's registration
//! (read back by TD'S OWN reader) records that path + the tree's NAR hash. Boundary: td
//! writes only its OWN scratch store/DB and READS the daemon's interned tree; the source
//! is already in the store (no fresh add, no WAL). Needs td-builder built, so it slots in
//! the heavy pool.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "store-add-tree",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        script: r##"
echo ">> store-add-tree: td CANONICALLY restores a directory tree into its OWN store + registers it (recursive addToStore, pure Rust, no daemon) — differential vs the daemon's interned source tree"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
scratch="$PWD/.store-add-tree-scratch"; rm -rf "$scratch"; mkdir -p "$scratch/store"; \
printf '%s\n' \
  '(use-modules (guix) (guix monads))' \
  '(with-store s (display (run-with-store s (lower-object (@@ (system td-builder) %builder-source)))) (newline))' \
  > "$scratch/lower.scm"; \
src=`$TD_GUIX repl -L . "$scratch/lower.scm"`; \
test -n "$src" -a -d "$src" || { echo "FAIL: could not realise the daemon's interned source tree (oracle)" >&2; exit 1; }; \
echo ">> daemon (oracle) interned source tree: $src"; \
name=`basename "$src"`; name=${name:33}; \
td_path=`"$tb" store-add-recursive "$name" "$src" "$scratch/store" "$scratch/td.db"`; \
test "$td_path" = "$src" || { echo "FAIL: td computed $td_path != the daemon's $src" >&2; exit 1; }; \
echo "   td computed the IDENTICAL content-addressed source path as the daemon"; \
base=`basename "$td_path"`; \
test -d "$scratch/store/$base" || { echo "FAIL: td did not restore the tree $base" >&2; exit 1; }; \
oracle_hash=`"$tb" nar-hash "$src"`; \
td_tree_hash=`"$tb" nar-hash "$scratch/store/$base"`; \
test "$td_tree_hash" = "$oracle_hash" || { echo "FAIL: td's restored tree NAR-hash $td_tree_hash != the daemon's interned tree $oracle_hash" >&2; exit 1; }; \
echo "   td's restored tree is byte-identical (NAR) to the daemon's own interned tree: $oracle_hash"; \
td_reg=`"$tb" store-query "$scratch/td.db" info`; \
reg_path=`echo "$td_reg" | cut -d'|' -f1`; \
reg_hash=`echo "$td_reg" | cut -d'|' -f2`; \
test "$reg_path" = "$src" || { echo "FAIL: td registered path $reg_path != $src" >&2; exit 1; }; \
test "$reg_hash" = "$oracle_hash" || { echo "FAIL: td registered hash $reg_hash != the daemon's $oracle_hash" >&2; exit 1; }; \
echo "   td's registration (read back by TD'S OWN reader) records the path + the NAR hash of the tree td restored"; \
rm -rf "$scratch"; \
echo "PASS: td CANONICALLY RESTORED a directory tree into its OWN store and REGISTERED it ITSELF, in pure Rust with NO daemon — td computed the IDENTICAL content-addressed source path to the daemon (from the recursive NAR sha256), restored the tree (structure + contents + exec bits + symlinks) BYTE-IDENTICAL (by NAR hash) to the daemon's own interned tree, and its registration (read back by TD'S OWN reader) records that path + the tree's hash. The daemon is only the oracle (its interned source tree). td now owns the recursive addToStore write side for no-reference sources; referenced sources, the destructive GC sweep, and a td store backend are later increments."
"##,
    }
}
