# tests/chain-cache-lib.sh — the machine-wide, content-keyed bootstrap chain-brick cache
# (#317: the FLIPPED gate-state default — gates share warm builder state unless a gate
# declares a private store). Sourced by tests/bootstrap-chain.sh; exercised directly by the
# chain-cache gate.
#
# Contract (env):
#   TD_CHAIN_CACHE — the cache home. The gate runner wires it per gate (gates.rs run_gate):
#       Shared gates (the default) get ~/.td/build-daemon/chain — a path host-sandbox binds
#       RW into every check sandbox at the SAME absolute path, so runs, worktrees, and
#       agents share ONE cache; Private gates get "" (force-cleared). Empty/unset here ⇒
#       COLD: chain_hit always misses, chain_save is a no-op — byte-for-byte the pre-#317
#       from-scratch behavior. (Sourcing scripts outside gate-run get the warm default via
#       bootstrap-chain.sh, which resolves the same ~/.td path when the var is unset.)
#   TB             — the stage0 td-builder (load_stage0): `$TB nar-hash` is the verifier.
#
# Mechanism (the cache-lib pattern applied to chain bricks — sharing never weakens the
# gates: every behavioral/repro assertion still runs per gate; only redundant REBUILDS go):
#   * CHAIN KEY: sha256 over the chain recipe + every pinned input (locks, patches, seed
#     tree). ANY change re-keys the whole chain — a stale brick can never be served across
#     a recipe or pin change.
#   * BRICKS build ONCE, at stable paths under $TD_CHAIN_CACHE/<key>/ (paths are baked
#     into later bricks' binaries — interp, symlinks — so bricks must never move; that is
#     why the cache reuses IN PLACE and never renames).
#   * SENTINEL per brick records the brick dir + the nar-hash of every IMMUTABLE product
#     (recorded at build time). chain_hit re-hashes each product on EVERY reuse: a
#     tampered/poisoned/truncated product mismatches ⇒ the brick is torn down and rebuilt
#     (NAR-verified reuse, never trust-on-presence).
#   * ONE exclusive flock per key serializes build-or-reuse across agents: the first
#     check builds, concurrent checks block, then cache-hit. The lock dies with its
#     holder (flock semantics), so a SIGKILLed gate never wedges the cache.
#
# API:
#   chain_cache_init NAME FILE...  — compute the key from FILEs, set CHAIN_WARM/CHAIN_DIR,
#                                    take the lock. NAME namespaces the lock+dir (the
#                                    modern chain uses "chain").
#   chain_hit NAME                 — 0 on verified reuse; sets CHAIN_PATH to the brick dir.
#   chain_path NAME                — echo the recorded brick dir.
#   chain_save NAME DIR PRODUCT... — record the sentinel after a successful build.
#   chain_done                     — release the lock (also released on process exit).

# chain_cache_init NAMESPACE FILE... — returns 0 with CHAIN_WARM=1 when the warm cache is
# usable (TD_CHAIN_CACHE non-empty, flock + TB available); else CHAIN_WARM=0 (cold).
chain_cache_init() {
  CHAIN_NS="$1"; shift
  CHAIN_WARM=0; CHAIN_DIR=""; CHAIN_KEY=""
  test -n "${TD_CHAIN_CACHE:-}" || return 0
  if [ -z "${TB:-}" ] || ! command -v flock >/dev/null 2>&1; then
    echo "chain-cache: WARNING: warm cache requested ($TD_CHAIN_CACHE) but \$TB or flock is unavailable — running COLD" >&2
    return 0
  fi
  CHAIN_KEY=`cat "$@" 2>/dev/null | sha256sum | cut -c1-16` || return 0
  test -n "$CHAIN_KEY" || return 0
  CHAIN_DIR="$TD_CHAIN_CACHE/$CHAIN_NS-$CHAIN_KEY"
  mkdir -p "$CHAIN_DIR" || { echo "chain-cache: WARNING: cannot create $CHAIN_DIR — running COLD" >&2; CHAIN_DIR=""; return 0; }
  # The per-key exclusive lock (fd 9), held for the whole build-or-reuse section. A
  # concurrent agent building the same key blocks here, then hits the finished bricks.
  exec 9>>"$CHAIN_DIR/.lock" || { echo "chain-cache: WARNING: cannot open lock — running COLD" >&2; CHAIN_DIR=""; return 0; }
  if ! flock -n 9; then
    echo "chain-cache: waiting for $CHAIN_DIR/.lock (another agent is building this chain key)..." >&2
    flock 9 || { echo "chain-cache: WARNING: flock failed — running COLD" >&2; exec 9>&-; CHAIN_DIR=""; return 0; }
  fi
  CHAIN_WARM=1
}

# chain_path NAME — the recorded brick dir (empty when absent).
chain_path() {
  sed -n 's/^dir //p' "$CHAIN_DIR/.brick-$1" 2>/dev/null | head -1
}

# chain_hit NAME — verified reuse. Every recorded product must exist AND nar-hash to its
# recorded value; any mismatch tears the brick down (dir + sentinel) so the caller rebuilds.
chain_hit() {
  test "$CHAIN_WARM" = 1 || return 1
  _s="$CHAIN_DIR/.brick-$1"
  test -f "$_s" || return 1
  CHAIN_PATH=`chain_path "$1"`
  test -n "$CHAIN_PATH" -a -d "$CHAIN_PATH" || { chain_evict "$1"; return 1; }
  while read -r _kind _want _p; do
    [ "$_kind" = prod ] || continue
    _got=`"$TB" nar-hash "$_p" 2>/dev/null` || _got=missing
    if [ "$_got" != "$_want" ]; then
      echo "chain-cache: REJECT $1: $_p nar-hash $_got != recorded $_want — tearing down + rebuilding" >&2
      chain_evict "$1"
      return 1
    fi
  done < "$_s"
  echo "   [chain-cache] HIT $1 (NAR-verified) at $CHAIN_PATH" >&2
  return 0
}

# chain_evict NAME — drop a brick (its dir + sentinel). Used on verify failure.
chain_evict() {
  _s="$CHAIN_DIR/.brick-$1"
  _d=`chain_path "$1"`
  rm -f "$_s"
  test -n "$_d" && rm -rf "$_d" 2>/dev/null || true
}

# chain_save NAME DIR PRODUCT... — record the sentinel (dir + per-product nar). Products
# must be IMMUTABLE post-build (later bricks may add files elsewhere in DIR, so callers
# list exactly what later consumers read, not whole mutated trees). No-op when cold.
chain_save() {
  test "$CHAIN_WARM" = 1 || return 0
  _n="$1"; _d="$2"; shift 2
  _s="$CHAIN_DIR/.brick-$_n"; _t="$_s.tmp.$$"
  printf 'dir %s\n' "$_d" > "$_t" || return 1
  for _p in "$@"; do
    _h=`"$TB" nar-hash "$_p"` || { echo "chain-cache: cannot nar-hash $_p — not caching $_n" >&2; rm -f "$_t"; return 0; }
    printf 'prod %s %s\n' "$_h" "$_p" >> "$_t"
  done
  mv "$_t" "$_s"
  echo "   [chain-cache] SAVED $_n at $_d" >&2
}

# chain_done — release the lock early (exit releases it anyway).
chain_done() {
  test "$CHAIN_WARM" = 1 && exec 9>&- || true
}
