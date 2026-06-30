#!/bin/sh
# tests/harness-loop.sh — the guix-free inner loop body, run INSIDE td's OWN
# /td/store harness by `./check.sh check-harness` (via mk/harness.mk). It proves
# td's loop SUBSTRATE — the busybox + GNU make userland interned at /td/store
# (gate 420, guix-byte-free) — drives a real build with NO guix and NO /gnu/store.
# This is the container ci/daily-full-suite.sh uses on a VM with no guix installed.
#
# This script may use ONLY the harness userland (the busybox applets + make on the
# /td/store PATH). No guix, no /gnu/store, no host tools — that is the whole point.
# (td-builder, the engine, joins the IN-harness pillars via rust-store-native rung
# 3 — today it runs host-side as the sandbox provider; see plan/host-sandbox-stage0.md.)
#
# Legs (DURABLE — no guix oracle in the room):
#   [structural]  inside, /gnu/store + /var/guix are ABSENT and guix is unresolvable.
#   [structural]  the store IS /td/store — the harness busybox lives there.
#   [behavioral]  the busybox userland performs a real, deterministic transform.
#   [behavioral]  the /td/store GNU make is the driver and is re-invokable.
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }
echo "HARNESS-LOOP-START"

# [structural] no guix substrate inside the container
[ -e /gnu/store ] && fail "/gnu/store is PRESENT inside the harness" || echo "  GNU-ABSENT"
[ -e /var/guix ] && fail "/var/guix is PRESENT inside the harness" || echo "  VARGUIX-ABSENT"
command -v guix >/dev/null 2>&1 && fail "guix is resolvable inside the harness" || echo "  GUIX-ABSENT"

# [structural] the store IS /td/store — the harness userland resolves from there
bb=`command -v busybox` || fail "busybox not resolvable on the harness PATH"
case "$bb" in /td/store/*) echo "  STORE-IS-TDSTORE ($bb)" ;; *) fail "busybox is not from /td/store: $bb" ;; esac

# [behavioral] the busybox userland does real, deterministic work (sort + sed)
got=`printf 'gamma\nalpha\nbeta\n' | sort | sed -n '1p'`
[ "$got" = alpha ] || fail "busybox sort/sed pipeline wrong: got '$got'"
echo "  BUSYBOX-PIPELINE-OK"

# [behavioral] the /td/store GNU make drove us and is re-invokable (no host make)
mk=`command -v make` || fail "make not resolvable on the harness PATH"
case "$mk" in /td/store/*) : ;; *) fail "make is not from /td/store: $mk" ;; esac
mv=`make --version 2>/dev/null | sed -n '1p'`
case "$mv" in "GNU Make 4."*) echo "  MAKE-OK ($mv)" ;; *) fail "harness make did not report GNU Make 4.x: '$mv'" ;; esac

echo "HARNESS-LOOP-OK"
