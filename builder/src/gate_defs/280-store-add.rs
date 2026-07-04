//! store-add (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td PLACES
//! a path into its OWN store and REGISTERS it itself — the daemon's addToStore (the WRITE
//! side), in pure Rust, no daemon in td's write path. `td-builder store-add-text` computes
//! the addTextToStore path (`store::make_text_path`), WRITES the content into a td-owned
//! store dir as a canonical store file (regular, 0444), and registers it in a td store DB
//! (`store_db`). The differential (daemon = oracle, prime directive 4): the SAME bytes
//! added via the daemon's addTextToStore RPC (`store-add`, #27) — which writes the file to
//! /gnu/store and returns the path — give (a) the IDENTICAL store path, and (b) a store
//! file that is byte-identical (by NAR hash) to the one td wrote; and td's registration,
//! read back with TD'S OWN reader (`store-query`, the #36 increment), records that path +
//! the NAR hash of what td wrote. The daemon's OWN store file is the oracle (not the DB:
//! a freshly-added path sits in the daemon's WAL, invisible to an immutable db.sqlite
//! read; the on-disk store file is the direct, WAL-free oracle and the stronger claim —
//! td's store bytes == the daemon's). NAR ignores mtime + the read/write perm bits, so
//! store identity is metadata-independent. Boundary: td writes only its OWN scratch
//! store/DB and READS the daemon's store file; the daemon RPC adds a GC-able probe path
//! (as the existing store-add/drv-add gates do) purely as the oracle — host infra stays
//! immutable. Needs td-builder built, so it slots in the heavy pool.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "store-add",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> store-add: td PLACES a text path into its OWN store + registers it (pure Rust, no daemon in the write path) — differential vs the daemon's addToStore"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
scratch="$PWD/.store-add-scratch"; rm -rf "$scratch"; mkdir -p "$scratch/store"; \
printf 'td store-add test payload\n' > "$scratch/content"; \
name="td-store-add-probe"; \
daemon_path=`"$tb" store-add "$name" "$scratch/content"`; \
test -n "$daemon_path" || { echo "FAIL: the daemon (oracle) returned no path for addTextToStore" >&2; exit 1; }; \
test -f "$daemon_path" || { echo "FAIL: the daemon did not write its store file $daemon_path (oracle missing)" >&2; exit 1; }; \
echo ">> daemon (oracle) addTextToStore wrote: $daemon_path"; \
td_path=`"$tb" store-add-text "$name" "$scratch/content" "$scratch/store" "$scratch/td.db"`; \
test "$td_path" = "$daemon_path" || { echo "FAIL: td computed $td_path != the daemon's $daemon_path" >&2; exit 1; }; \
echo "   td computed the SAME store path as the daemon (no daemon in td's path computation)"; \
base=`basename "$td_path"`; \
test -f "$scratch/store/$base" || { echo "FAIL: td did not write the store file $base" >&2; exit 1; }; \
mode=`stat -c '%a' "$scratch/store/$base"`; \
test "$mode" = "444" || { echo "FAIL: td's store file mode $mode != 444 (canonical read-only)" >&2; exit 1; }; \
echo "   td WROTE the store file itself, canonical mode 0444 (no daemon in the write path)"; \
oracle_hash=`"$tb" nar-hash "$daemon_path"`; \
td_file_hash=`"$tb" nar-hash "$scratch/store/$base"`; \
test "$td_file_hash" = "$oracle_hash" || { echo "FAIL: td's store bytes NAR-hash $td_file_hash != the daemon's store file $oracle_hash" >&2; exit 1; }; \
echo "   td's store file is byte-identical (NAR) to the daemon's own: $oracle_hash"; \
td_reg=`"$tb" store-query "$scratch/td.db" info`; \
reg_path=`echo "$td_reg" | cut -d'|' -f1`; \
reg_hash=`echo "$td_reg" | cut -d'|' -f2`; \
test "$reg_path" = "$daemon_path" || { echo "FAIL: td registered path $reg_path != $daemon_path" >&2; exit 1; }; \
test "$reg_hash" = "$oracle_hash" || { echo "FAIL: td registered hash $reg_hash != the daemon-equivalent $oracle_hash" >&2; exit 1; }; \
echo "   td's registration (read back by TD'S OWN reader) records the path + the NAR hash of what td wrote"; \
rm -rf "$scratch"; \
echo "PASS: td PLACED a path into its OWN store and REGISTERED it ITSELF, in pure Rust with NO daemon in the write path — td computed the IDENTICAL store path to the daemon's addTextToStore, WROTE a store file (canonical mode 0444) BYTE-IDENTICAL (by NAR hash) to the daemon's own store file, and its registration (read back by TD'S OWN reader) records that path + the hash of what td wrote. The daemon is only the oracle (it adds the probe bytes + writes its own store file purely for the differential). Recursive directory adds (canonical tree restore), references, GC, and a td store backend are later increments."
"##,
    }
}
