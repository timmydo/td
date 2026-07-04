//! store-add-tree (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td
//! CANONICALLY restores a DIRECTORY TREE into its OWN store and registers it — the
//! RECURSIVE addToStore (the general write side, after the flat `store-add`), in pure
//! Rust, no daemon, NO guix. `td-builder store-add-recursive` computes the
//! content-addressed `source` path from the tree's recursive NAR sha256
//! (`make_store_path("source", …)` — the daemon's makeFixedOutputPath for
//! recursive-sha256, no references), restores the tree with `copy_canonical` (structure
//! + contents + the file EXECUTABLE bit + symlinks — the properties NAR captures; dir
//! perms / rw bits / mtimes are NAR-irrelevant), and registers it in a td store DB.
//!
//! Subject: a self-contained FIXTURE tree assembled in scratch — a nested dir, a plain
//! file, an executable file, and a symlink — so the gate controls every NAR-captured
//! property directly and needs no external tree. All-td-native / all-durable:
//! [DETERMINISM] re-interning the identical tree yields the IDENTICAL path (the path is
//! a pure function of the content). [ROUND-TRIP] the restored tree is NAR-byte-identical
//! to the source (exec bits + symlinks + nesting survive copy_canonical), cross-checked
//! by concrete restored-tree probes. [REGISTRATION] td's OWN reader reads back the path
//! + the tree's NAR hash. [DISCRIMINATION, load-bearing] a single-byte append AND an
//! exec-bit flip each MOVE the content-addressed path (and the append moves the registered
//! NAR hash) — the addressing is a real function of the bytes, not a constant. Needs
//! td-builder built, so it slots in the heavy pool.
//!
//! History: the guix-daemon differential this gate began as — interning the daemon's own
//! `%builder-source` tree (lowered with `guix repl … lower-object`) and asserting td's
//! path/NAR equals the daemon's — was the `lowering` guix-surface site retired here
//! (#310 / directive 6). The daemon-equality ORACLE is dropped; the DETERMINISM +
//! ROUND-TRIP + DISCRIMINATION assertions below cover the same property (a stable
//! content address that faithfully captures the tree) td-native — trading a REMOVABLE
//! guix-canonical cross-check (the byte-hash-vs-Guix oracle, removable per directive 4)
//! for a DURABLE discrimination the old gate never proved (it only matched the daemon for
//! one tree, never showed the address changes when the tree does). Called out in the PR
//! per directive 3.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "store-add-tree",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> store-add-tree: td CANONICALLY restores a directory tree into its OWN store + registers it (recursive addToStore, pure Rust, no daemon, no guix) — content-addressed round-trip + a perturbation control proving the addressing is load-bearing"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
scratch="$PWD/.store-add-tree-scratch"; rm -rf "$scratch"; mkdir -p "$scratch"; \
fx="$scratch/tree"; mkdir -p "$fx/sub"; \
printf 'hello from the td store-add-recursive fixture\n' > "$fx/file.txt"; \
printf '#!/bin/sh\necho hi\n' > "$fx/run.sh"; chmod +x "$fx/run.sh"; \
printf 'nested payload\n' > "$fx/sub/nested.txt"; \
ln -s file.txt "$fx/link"; \
name="td-store-add-fixture"; \
srcnar=`"$tb" nar-hash "$fx"`; \
echo ">> fixture tree (nested dir + file + exec file + symlink) NAR: $srcnar"; \
p1=`"$tb" store-add-recursive "$name" "$fx" "$scratch/store" "$scratch/td.db"`; \
case "$p1" in /gnu/store/*-"$name") : ;; *) echo "FAIL: store-add-recursive did not return a content-addressed source path (got '$p1')" >&2; exit 1 ;; esac; \
base=`basename "$p1"`; \
echo "   td interned the fixture at $p1"; \
p1b=`"$tb" store-add-recursive "$name" "$fx" "$scratch/store_b" "$scratch/td_b.db"`; \
test "$p1b" = "$p1" || { echo "FAIL: re-interning the same tree moved the path ($p1b != $p1) — not content-addressed" >&2; exit 1; }; \
echo "   [DETERMINISM] re-interning the same tree yields the identical path"; \
test -d "$scratch/store/$base" || { echo "FAIL: td did not restore the tree at $scratch/store/$base" >&2; exit 1; }; \
rnar=`"$tb" nar-hash "$scratch/store/$base"`; \
test "$rnar" = "$srcnar" || { echo "FAIL: restored tree NAR $rnar != source $srcnar — the round-trip is not byte-identical" >&2; exit 1; }; \
echo "   [ROUND-TRIP] the restored tree is NAR-byte-identical to the source: $srcnar"; \
test -x "$scratch/store/$base/run.sh" || { echo "FAIL: the executable bit was not restored on run.sh" >&2; exit 1; }; \
{ test -L "$scratch/store/$base/link" && [ "`readlink "$scratch/store/$base/link"`" = file.txt ]; } || { echo "FAIL: the symlink was not restored (link -> file.txt)" >&2; exit 1; }; \
test -f "$scratch/store/$base/sub/nested.txt" || { echo "FAIL: the nested file was not restored" >&2; exit 1; }; \
echo "   restored tree keeps the exec bit (run.sh), the symlink (link -> file.txt), and the nested file (sub/nested.txt)"; \
reg=`"$tb" store-query "$scratch/td.db" info`; \
test "`echo "$reg" | cut -d'|' -f1`" = "$p1" || { echo "FAIL: registered path != $p1 ($reg)" >&2; exit 1; }; \
test "`echo "$reg" | cut -d'|' -f2`" = "$srcnar" || { echo "FAIL: registered NAR hash != $srcnar ($reg)" >&2; exit 1; }; \
echo "   [REGISTRATION] td's own reader reads back the interned path + the tree's NAR hash"; \
cp -a "$fx" "$scratch/tree_c"; printf 'x' >> "$scratch/tree_c/file.txt"; \
pc=`"$tb" store-add-recursive "$name" "$scratch/tree_c" "$scratch/store_c" "$scratch/td_c.db"`; \
test "$pc" != "$p1" || { echo "FAIL: appending a single byte did NOT move the path — the store path is not a function of the content" >&2; exit 1; }; \
cnar=`"$tb" store-query "$scratch/td_c.db" info | cut -d'|' -f2`; \
test -n "$cnar" -a "$cnar" != "$srcnar" || { echo "FAIL: the single-byte edit did not change the registered NAR hash (got '$cnar')" >&2; exit 1; }; \
cp -a "$fx" "$scratch/tree_x"; chmod -x "$scratch/tree_x/run.sh"; \
px=`"$tb" store-add-recursive "$name" "$scratch/tree_x" "$scratch/store_x" "$scratch/td_x.db"`; \
test "$px" != "$p1" || { echo "FAIL: flipping the executable bit did NOT move the path — the exec bit is not captured in the content address" >&2; exit 1; }; \
echo "   [DISCRIMINATION] a single-byte append and an exec-bit flip each move the content-addressed path + registered NAR hash (contents + exec bits are load-bearing)"; \
rm -rf "$scratch"; \
echo "PASS: td CANONICALLY RESTORED a directory tree into its OWN store and REGISTERED it ITSELF, in pure Rust with NO daemon and NO guix — the content-addressed source path is a deterministic function of the tree's recursive NAR sha256 (re-interning is identical), the restored tree is NAR-byte-identical to the source (structure + contents + exec bits + symlinks), td's own reader reads back the path + hash, and a single-byte append or an exec-bit flip each move the path (the addressing is load-bearing). td owns the recursive addToStore write side for no-reference sources; referenced sources, the destructive GC sweep, and a td store backend are later increments."
"##,
    }
}
