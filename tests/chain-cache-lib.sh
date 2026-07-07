# tests/chain-cache-lib.sh — the machine-wide, content-keyed bootstrap chain-brick cache
# (#317: the FLIPPED gate-state default — gates share warm builder state unless a gate
# declares a private store). Sourced by tests/bootstrap-chain.sh; exercised directly by the
# chain-cache gate.
#
# Contract (env):
#   TD_CHECK_CHAIN_CACHE — the cache home. The gate runner wires it per gate (gates.rs
#       run_gate): Shared gates (the default) get ~/.td/build-daemon/chain — a path
#       host-sandbox binds RW into every check sandbox at the SAME absolute path, so runs,
#       worktrees, and agents share ONE cache; Private gates get "" (force-cleared).
#       Empty/unset here ⇒ COLD: chain_hit always misses, chain_save is a no-op —
#       byte-for-byte the pre-#317 from-scratch behavior. (Sourcing scripts outside
#       gate-run get the warm default via bootstrap-chain.sh, which resolves the same
#       ~/.td path when the var is unset.) The TD_CHECK_ prefix rides the existing
#       host-sandbox env passthrough, and `TD_CHECK_CHAIN_CACHE= ./check.sh`
#       (set-and-empty) is the operator's force-cold switch (the daily backstop pins it).
#   TB                   — the stage0 td-builder (load_stage0): `$TB nar-hash` verifies.
#
# FAIL-CLOSED: when a warm cache is requested but cannot be used SAFELY (no $TB, no
# lock helper, unreadable key inputs, unwritable cache dir), chain_cache_init returns nonzero —
# under the callers' `set -eu` that reds the gate with an actionable message. Silently
# degrading to cold is how a dead warm path hides (it did, in review); a deliberate cold
# run is always available by setting the var empty.
#
# Mechanism (the cache-lib pattern applied to chain bricks — sharing never weakens the
# gates: every behavioral/repro assertion still runs per gate; only redundant REBUILDS go):
#   * CHAIN KEY: sha256 over the chain recipe + every pinned input (locks, patches, seed
#     tree — the pinned inputs). ANY change re-keys the whole chain —
#     a stale brick can never be served across a recipe, lock, or seed change.
#     `sha256sum --` fails on any unreadable/missing input (never a bogus empty-input key).
#   * BRICKS build ONCE, at stable paths under $TD_CHECK_CHAIN_CACHE/<key>/ (paths are
#     baked into later bricks' binaries — interp, symlinks — so bricks must never move;
#     that is why the cache reuses IN PLACE and never renames).
#   * SENTINEL per brick records the brick dir + the nar-hash of every IMMUTABLE product
#     (recorded at build time). chain_hit re-hashes each product on EVERY reuse: a
#     tampered/poisoned/truncated product mismatches ⇒ the brick is torn down and rebuilt
#     (NAR-verified reuse, never trust-on-presence).
#   * ONE exclusive lock per key serializes build-or-reuse across agents: the first
#     check builds, concurrent checks block, then cache-hit. The lock dies with its
#     holder (kernel flock semantics), so a SIGKILLed gate never wedges the cache. (Pure hits
#     re-verify under the same exclusive lock — seconds against the ~90-min build they
#     replace; a shared-lock fast path is not worth the upgrade races.)
#   * WHOLE-KEY GC (#326): each key dir carries a `last-used` stamp, re-touched on every
#     chain_cache_init. A key not used for TD_CHECK_CHAIN_GC_DAYS (default 14) is a
#     multi-GB orphan from a superseded (recipe, lock, seed) tuple — nothing ever
#     prunes INSIDE a key (brick paths are baked into later bricks), so reclamation must be
#     whole-key. chain_cache_init sweeps stale keys AFTER taking its own lock+stamp; each
#     candidate is removed ONLY while its own exclusive .lock can be taken non-blocking, so
#     a key mid-build/mid-reuse (its holder still owns the lock) is NEVER swept. Confined:
#     only dirs directly under $TD_CHECK_CHAIN_CACHE matching "<namespace>-*" are eligible.
#
# API:
#   chain_cache_init NAME FILE...  — compute the key from FILEs, set CHAIN_WARM/CHAIN_DIR,
#                                    take the lock, stamp last-used, GC stale keys. NAME
#                                    namespaces the lock+dir+sweep (the modern chain uses
#                                    "chain"). Nonzero = requested warm cache unusable
#                                    (fail closed).
#   chain_hit NAME                 — 0 on verified reuse; sets CHAIN_PATH to the brick dir.
#   chain_path NAME                — echo the recorded brick dir.
#   chain_save NAME DIR PRODUCT... — record the sentinel after a successful build.
#   chain_gc NAMESPACE             — whole-key eviction of stale keys (called by init).
#   chain_done                     — release the lock (also released on process exit).

chain_lock_fd() {
  if [ -n "${TD_LOCK_TOOL:-}" ]; then "$TD_LOCK_TOOL" lock-fd "$@"; else flock "$@"; fi
}

chain_lock_file_cmd() {
  if [ -n "${TD_LOCK_TOOL:-}" ]; then "$TD_LOCK_TOOL" lock-file "$@"; return $?; fi
  _clf_nonblock=0
  if [ "${1:-}" = -n ]; then _clf_nonblock=1; shift; fi
  _clf_path="${1:?chain_lock_file_cmd: missing lock path}"; shift
  [ "${1:-}" = "--" ] && shift
  if [ "$_clf_nonblock" = 1 ]; then flock -n "$_clf_path" "$@"; else flock "$_clf_path" "$@"; fi
}

# chain_cache_init NAMESPACE FILE... — CHAIN_WARM=1 + the key lock on success; CHAIN_WARM=0
# when the cache is deliberately off (var empty/unset); nonzero when warm was requested
# but cannot be established safely.
chain_cache_init() {
  CHAIN_NS="$1"; shift
  CHAIN_WARM=0; CHAIN_DIR=""; CHAIN_KEY=""
  test -n "${TD_CHECK_CHAIN_CACHE:-}" || return 0
  if [ -z "${TB:-}" ]; then
    echo "chain-cache: FAIL-CLOSED: warm cache requested ($TD_CHECK_CHAIN_CACHE) but \$TB is unset — load_stage0 first, or set TD_CHECK_CHAIN_CACHE= for a deliberate cold run" >&2
    return 1
  fi
  if [ -n "${TD_LOCK_TOOL:-}" ] && [ ! -x "$TD_LOCK_TOOL" ]; then
    echo "chain-cache: FAIL-CLOSED: warm cache requested but TD_LOCK_TOOL is not executable ($TD_LOCK_TOOL) — fix the environment, or set TD_CHECK_CHAIN_CACHE= for a deliberate cold run" >&2
    return 1
  fi
  if [ -z "${TD_LOCK_TOOL:-}" ] && ! command -v flock >/dev/null 2>&1; then
    echo "chain-cache: FAIL-CLOSED: warm cache requested but no TD_LOCK_TOOL is set and fallback flock(1) is not on PATH — fix the environment, or set TD_CHECK_CHAIN_CACHE= for a deliberate cold run" >&2
    return 1
  fi
  # sha256sum -- errors on ANY unreadable/missing input (an unexpanded glob, a bad cwd),
  # so a broken invocation can never silently key to the empty-input hash.
  _sums=`sha256sum -- "$@"` || {
    echo "chain-cache: FAIL-CLOSED: cannot read the chain key inputs (run from the repo root?)" >&2
    return 1
  }
  CHAIN_KEY=`printf '%s\n' "$_sums" | sha256sum | cut -c1-16`
  test -n "$CHAIN_KEY" || { echo "chain-cache: FAIL-CLOSED: empty chain key" >&2; return 1; }
  CHAIN_DIR="$TD_CHECK_CHAIN_CACHE/$CHAIN_NS-$CHAIN_KEY"
  mkdir -p "$CHAIN_DIR" || { echo "chain-cache: FAIL-CLOSED: cannot create $CHAIN_DIR" >&2; return 1; }
  # The per-key exclusive lock (fd 9), held for the whole build-or-reuse section. A
  # concurrent agent building the same key blocks here, then hits the finished bricks.
  exec 9>>"$CHAIN_DIR/.lock" || { echo "chain-cache: FAIL-CLOSED: cannot open $CHAIN_DIR/.lock" >&2; return 1; }
  if ! chain_lock_fd -n 9; then
    echo "chain-cache: waiting for $CHAIN_DIR/.lock (another agent is building this chain key)..." >&2
    chain_lock_fd 9 || { echo "chain-cache: FAIL-CLOSED: lock failed on $CHAIN_DIR/.lock" >&2; exec 9>&-; return 1; }
  fi
  CHAIN_WARM=1
  # Mark this key live (the GC freshness signal), then reclaim stale sibling keys.
  touch "$CHAIN_DIR/last-used" 2>/dev/null || true
  chain_gc "$CHAIN_NS"
}

# chain_gc NAMESPACE — whole-key eviction. A key is stale when its `last-used` stamp is
# older than TD_CHECK_CHAIN_GC_DAYS (default 14; non-numeric ⇒ GC off), or absent (a
# partial/legacy key). `find -mtime +N` buckets age into whole 24h periods, so the
# effective cutoff is the first N-day-plus boundary (a hair conservative — keeps a key
# up to a day longer, never sweeps too soon). Each stale candidate is removed ONLY while
# its own exclusive .lock is takeable non-blocking — a key another agent is mid-build or
# mid-reuse on holds that lock (kernel flock treats each open-file-description independently, so
# this is faithful even in-proc), so it is skipped, never swept. The rm -rf is CONFINED:
# only dirs directly under $TD_CHECK_CHAIN_CACHE whose basename matches "$NS-*" are
# eligible, and the current key ($CHAIN_DIR, just stamped + locked) is never a candidate.
# Best-effort: a broken threshold or a delete race skips quietly rather than reclaiming
# wrongly (a missed sweep just retries next init; a bad sweep would be data loss).
chain_gc() {
  _gcns="$1"
  _gcdays="${TD_CHECK_CHAIN_GC_DAYS:-14}"
  case "$_gcdays" in ''|*[!0-9]*) return 0 ;; esac   # non-numeric/empty ⇒ GC disabled
  test -n "${TD_CHECK_CHAIN_CACHE:-}" || return 0
  for _gcd in "$TD_CHECK_CHAIN_CACHE/$_gcns"-*; do
    [ -d "$_gcd" ] || continue                        # no glob match ⇒ literal pattern; skip
    [ "$_gcd" = "$CHAIN_DIR" ] && continue             # never sweep the key we just took
    # Keep a key whose stamp EXISTS and is fresh; everything else is a candidate.
    if [ -f "$_gcd/last-used" ] && \
       [ -z "`find "$_gcd/last-used" -mtime +"$_gcdays" -print 2>/dev/null`" ]; then
      continue
    fi
    # Confinement: only ever a dir the glob produced under the cache root (defensive —
    # $_gcd already comes from the "$NS-*" glob, never an untrusted sentinel).
    case "$_gcd" in "$TD_CHECK_CHAIN_CACHE/$_gcns"-*) : ;; *) continue ;; esac
    # Take the candidate's own exclusive lock and HOLD it across the delete.
    # A key another agent is mid-build/mid-reuse on holds that lock, so a nonblocking lock
    # can't take it ⇒ the key is skipped, never swept. rm unlinks the dir (incl .lock)
    # while the lock helper still owns the now-unlinked inode; it returns rm's status,
    # so 0 ⇒ swept.
    # (`2>/dev/null` is scoped to THIS command — a lock-busy diagnostic is expected noise,
    # not an error — and never leaks to the shell's stderr the way `exec N>f 2>/dev/null`
    # would.)
    if chain_lock_file_cmd -n "$_gcd/.lock" -- rm -rf "$_gcd" 2>/dev/null; then
      echo "   [chain-cache] GC swept stale key $_gcd (unused > ${_gcdays}d)" >&2
    fi
  done
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

# chain_evict NAME — drop a brick (its dir + sentinel). Used on verify failure. The
# recorded dir comes from an UNVERIFIED sentinel in a shared cache, so the rm -rf is
# CONFINED to this key's own dir — a corrupt/hostile `dir` line pointing elsewhere is
# dropped (sentinel removed), never deleted.
chain_evict() {
  _s="$CHAIN_DIR/.brick-$1"
  _d=`chain_path "$1"`
  rm -f "$_s"
  case "$_d" in
    "$CHAIN_DIR"/*) rm -rf "$_d" 2>/dev/null || true ;;
    "") : ;;
    *) echo "chain-cache: NOT deleting $_d (outside $CHAIN_DIR — corrupt sentinel?)" >&2 ;;
  esac
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
