#!/bin/sh
# build-daemon-ensure.sh — ensure ONE shared, persistent td build daemon is running for
# this host, and print its Unix-socket PATH on stdout. Idempotent + concurrency-safe
# (flock): the FIRST caller starts the daemon; every later caller (any worktree, any agent)
# reuses it. This is how N agents on N worktrees SHARE one builder with ONE global budget
# — the machine-wide build limiter. The daemon realizes drvs submitted over the socket
# (td-builder daemon), bounded to TD_BUILD_JOBS concurrent builds; the per-drv builder
# override travels with each request, so one shared daemon serves every worktree.
#
# The shared state lives under TD_DAEMON_DIR (default ~/.td/build-daemon): socket (the
# listening socket, bound into every check sandbox by host-sandbox), store/ (the shared
# CONTENT-ADDRESSED output store, read back by the submitter), daemon.pid, daemon.log,
# daemon.lock. STORE-DB (the seed reference graph for input-closure staging) is
# /var/guix/db/db.sqlite (TD_DAEMON_STORE_DB to override).
#
# Env: TD_DAEMON_DIR (shared dir), TD_DAEMON_BUILDER (a specific td-builder binary; else a
# td-bootstrapped stage0 placement, else a host `cargo build` of builder/), TD_BUILD_JOBS
# (the global concurrent-build budget; the daemon defaults it from cores + RAM), TD_NICE
# (nice level for the daemon + its build children, default 10 — so builds yield to
# interactive work; the budget bounds HOW MANY run, nice bounds their priority).
set -eu

daemon_dir=${TD_DAEMON_DIR:-$HOME/.td/build-daemon}
store="$daemon_dir/store"
lock_f="$daemon_dir/daemon.lock"
store_db=${TD_DAEMON_STORE_DB:-/var/guix/db/db.sqlite}
mkdir -p "$store"

root=$(cd "$(dirname "$0")/.." && pwd)
# Resolve the td-builder that RUNS the daemon (the orchestrator that stages inputs + forks
# the per-drv builder; guix-free, never the guix-built one — move-off-Guile §5). It MUST be
# the CURRENT code and MUST speak the same request grammar cache-lib sends, so resolve it the
# SAME way the loop's client does — the deterministic current stage0 from stage0-builder.sh —
# NOT an arbitrary placed stage0 (a leftover from an older build would speak an older grammar
# → protocol skew). Fallbacks: an explicit override, then a host cargo build of the tree.
tb=${TD_DAEMON_BUILDER:-}
if [ -z "$tb" ] || [ ! -x "$tb" ]; then
  cb=$(sh "$root/tests/stage0-builder.sh" "$root/.td-build-cache/stage0" 2>/dev/null || true)
  [ -n "$cb" ] && tb="$root/.td-build-cache/stage0/store/$(basename "$cb")/bin/td-builder"
fi
if { [ -z "$tb" ] || [ ! -x "$tb" ]; } && command -v cargo >/dev/null 2>&1; then
  ( cd "$root/builder" && cargo build --release --quiet ) >&2 && tb="$root/builder/target/release/td-builder"
fi
[ -n "$tb" ] && [ -x "$tb" ] || { echo "build-daemon-ensure: no td-builder binary (set TD_DAEMON_BUILDER or build builder/)" >&2; exit 1; }

# Key the socket/pid/log by the daemon binary's CONTENT hash. A daemon started by a DIFFERENT
# (e.g. older-grammar) td-builder lives on a different socket, so a current ensure never
# reuses a stale-grammar daemon — the skew a plain shared socket suffered. cache-lib receives
# this socket via TD_DAEMON_SOCKET, so the client and the serving daemon are always the same
# build. (Old-binary daemons idle out on their own sockets.)
key=$(sha256sum "$tb" | cut -c1-16)
sock="$daemon_dir/socket.$key"
pid_f="$daemon_dir/daemon.$key.pid"
log_f="$daemon_dir/daemon.$key.log"

# Serialize concurrent ensures so two agents never both start a daemon.
exec 9>"$lock_f"
flock 9

# Reuse a live daemon.
if [ -f "$pid_f" ] && [ -S "$sock" ] && kill -0 "$(cat "$pid_f" 2>/dev/null)" 2>/dev/null; then
  echo "$sock"
  exit 0
fi

# Start a fresh daemon, detached so it outlives this script AND the check that starts it
# (the machine-wide limiter must persist across checks). Close the lock fd (9>&-) in the
# child so a later ensure does not block forever on the inherited flock.
: > "$log_f"
rm -f "$sock"
# nice/ionice the daemon so its build children (the corpus builds — the real CPU/IO) yield
# to interactive work; the global budget bounds how MANY run at once.
nice_wrap="nice -n ${TD_NICE:-10}"
command -v ionice >/dev/null 2>&1 && nice_wrap="$nice_wrap ionice -c2 -n7"
nohup $nice_wrap env ${TD_BUILD_JOBS:+TD_BUILD_JOBS="$TD_BUILD_JOBS"} \
  "$tb" daemon "$sock" "$store_db" "$store" >"$log_f" 2>&1 9>&- &
pid=$!
echo "$pid" > "$pid_f"

# Wait for it to bind the socket.
i=0
while [ "$i" -lt 100 ]; do
  [ -S "$sock" ] && break
  kill -0 "$pid" 2>/dev/null || { echo "build-daemon-ensure: daemon exited before binding:" >&2; cat "$log_f" >&2; exit 1; }
  sleep 0.1
  i=$((i + 1))
done
[ -S "$sock" ] || { echo "build-daemon-ensure: daemon did not bind $sock" >&2; cat "$log_f" >&2; exit 1; }
echo "$sock"
