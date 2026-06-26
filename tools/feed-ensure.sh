#!/bin/sh
# feed-ensure.sh — ensure ONE shared, persistent td-feed serve daemon is running for this
# host, and print its loopback address (HOST:PORT) on stdout. Idempotent + concurrency-safe
# (flock): the FIRST caller starts the daemon; every later caller (any worktree, any agent)
# reuses it. This is how a bunch of agents on different worktrees SHARE one feed + its store.
#
# The shared state lives under TD_FEED_DIR (default ~/.td/feed): store/ (the artifacts +
# .sha256 sidecars), feed.addr, feed.pid, feed.lock, feed.log. The daemon is index-free
# (serve verifies each file against its sidecar), so it serves whatever any worktree has
# `td-feed warm`ed into the shared store — no restart needed when the warmed set grows.
#
# Env: TD_FEED_DIR (shared dir), TD_FEED_BIN (a specific td-feed binary; else the td-feed
# gate's stage0 build, else a host `cargo build` of feed/).
set -eu

feed_dir=${TD_FEED_DIR:-$HOME/.td/feed}
store="$feed_dir/store"
addr_f="$feed_dir/feed.addr"
pid_f="$feed_dir/feed.pid"
log_f="$feed_dir/feed.log"
lock_f="$feed_dir/feed.lock"
mkdir -p "$store"

root=$(cd "$(dirname "$0")/.." && pwd)
# Locate a td-feed binary: explicit override, else the gate's td-built one, else host cargo.
tdfeed=${TD_FEED_BIN:-}
if [ -z "$tdfeed" ] || [ ! -x "$tdfeed" ]; then
  tdfeed=$(ls "$root"/.td-build-cache/td-feed/sd/newstore/*/bin/td-feed 2>/dev/null | head -1 || true)
fi
if { [ -z "$tdfeed" ] || [ ! -x "$tdfeed" ]; } && command -v cargo >/dev/null 2>&1; then
  ( cd "$root/feed" && cargo build --release --quiet ) >&2 && tdfeed="$root/feed/target/release/td-feed"
fi
[ -n "$tdfeed" ] && [ -x "$tdfeed" ] || { echo "feed-ensure: no td-feed binary (set TD_FEED_BIN or build feed/)" >&2; exit 1; }

# Serialize concurrent ensures so two agents never both start a daemon.
exec 9>"$lock_f"
flock 9

# Reuse a live daemon.
if [ -f "$pid_f" ] && [ -f "$addr_f" ] && kill -0 "$(cat "$pid_f" 2>/dev/null)" 2>/dev/null; then
  cat "$addr_f"
  exit 0
fi

# Start a fresh daemon on an ephemeral loopback port, detached so it outlives this script.
# Close the lock fd (9>&-) in the child: a detached daemon must NOT inherit-and-hold the
# flock, or a later feed-ensure would block forever waiting for the lock.
: > "$log_f"
nohup "$tdfeed" serve "$store" 127.0.0.1:0 >"$log_f" 2>&1 9>&- &
pid=$!
echo "$pid" > "$pid_f"

# Wait for it to announce its bound address ("... on http://HOST:PORT/").
addr=""
i=0
while [ "$i" -lt 100 ]; do
  addr=$(sed -n 's,.*on http://\([0-9.]*:[0-9]*\)/.*,\1,p' "$log_f" 2>/dev/null | head -1)
  [ -n "$addr" ] && break
  kill -0 "$pid" 2>/dev/null || { echo "feed-ensure: daemon exited before binding:" >&2; cat "$log_f" >&2; exit 1; }
  sleep 0.1
  i=$((i + 1))
done
[ -n "$addr" ] || { echo "feed-ensure: daemon did not report an address" >&2; cat "$log_f" >&2; exit 1; }
echo "$addr" > "$addr_f"
echo "$addr"
