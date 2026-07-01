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

# PROTO — the daemon request-protocol version. It is part of the socket/pid/log names, so
# bumping it (whenever the request grammar or daemon behavior changes) makes a new check
# start a FRESH daemon on a new socket instead of reusing a stale daemon from an older
# td-builder (which would reject the new grammar). Old-proto daemons idle out on their old
# socket; the current one is always version-matched.
proto=2
daemon_dir=${TD_DAEMON_DIR:-$HOME/.td/build-daemon}
store="$daemon_dir/store"
sock="$daemon_dir/socket.v$proto"
pid_f="$daemon_dir/daemon.v$proto.pid"
log_f="$daemon_dir/daemon.v$proto.log"
lock_f="$daemon_dir/daemon.lock"
store_db=${TD_DAEMON_STORE_DB:-/var/guix/db/db.sqlite}
mkdir -p "$store"

root=$(cd "$(dirname "$0")/.." && pwd)
# Locate a td-builder binary to RUN the daemon (the orchestrator that stages inputs + forks
# the per-drv builder). It must be a td-built/cargo-built td-builder, never the guix-built
# one (move-off-Guile §5): explicit override, else any placed stage0, else host cargo.
tb=${TD_DAEMON_BUILDER:-}
if [ -z "$tb" ] || [ ! -x "$tb" ]; then
  tb=$(ls "$root"/.td-build-cache/stage0/store/*/bin/td-builder 2>/dev/null | head -1 || true)
fi
if { [ -z "$tb" ] || [ ! -x "$tb" ]; } && command -v cargo >/dev/null 2>&1; then
  ( cd "$root/builder" && cargo build --release --quiet ) >&2 && tb="$root/builder/target/release/td-builder"
fi
[ -n "$tb" ] && [ -x "$tb" ] || { echo "build-daemon-ensure: no td-builder binary (set TD_DAEMON_BUILDER or build builder/)" >&2; exit 1; }

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
