#!/bin/sh
# tests/reset-native.sh — td-NATIVE per-test CoW-reset ephemerality suite
# (move-off-Guile §5, lever 3): the SSH-driven reconstruction of tests/reset.scm's
# marionette test, with NO Guile in the guest and NO (gnu tests)/marionette.
# Three boots of a NON-volatile qcow2 ($1) on explicit CoW overlays over the
# read-only backing image (so /gnu/store is never written):
#   boot 1, overlay A : guest dirties state (/home/tester/td-dirt) and syncs it.
#   boot 2, overlay A : NO reset — the dirt MUST persist (committed negative
#                       control: proves writes genuinely land, so boot 3's
#                       cleanliness is a property of the RESET, not write-loss).
#   boot 3, overlay B : the reset under test — fresh overlay, same backing image,
#                       state MUST be pristine (dirt gone).
# Dirt lives in the test user's home (ssh logs in as the non-root test user); it
# is on the persistent root, so the overlay swap is what makes it vanish. All
# legs DURABLE (no Guix oracle). Verified-red: see plan/ts-frontend.md.
set -eu

IMAGE="${1:?usage: reset-native.sh NONVOLATILE-QCOW2}"
DIRT="/home/tester/td-dirt"

VM_MEM=512
VM_WORK="$(mktemp -d)"
. tests/vm-lib.sh
cleanup() { vm_shutdown; rm -rf "$VM_WORK"; }
trap cleanup EXIT INT TERM
vm_setup

A="$VM_WORK/a.qcow2"; B="$VM_WORK/b.qcow2"

# boot 1 (overlay A): dirty + sync.
vm_make_overlay "$A" "$IMAGE"
echo ">> reset-native: boot 1 (overlay A) — dirty state"
vm_start "$A"; vm_wait_ssh 240 || { echo "FAIL: boot 1 never reached ssh"; exit 1; }
vm_ssh "echo td-dirt > $DIRT && sync && test -f $DIRT" \
  || { echo "FAIL: boot 1 could not write/sync dirt"; exit 1; }
echo "  [boot1] guest wrote $DIRT and synced"
vm_shutdown

# boot 2 (overlay A reused, NO reset): dirt MUST persist — negative control.
echo ">> reset-native: boot 2 (overlay A reused) — dirt must persist"
vm_start "$A"; vm_wait_ssh 240 || { echo "FAIL: boot 2 never reached ssh"; exit 1; }
if vm_ssh "test -f $DIRT"; then echo "  [boot2] negative control OK: dirt persisted across reboot on the SAME overlay"
else echo "FAIL: [boot2] dirt vanished on a reused overlay — writes are not landing (control broken)"; exit 1; fi
vm_shutdown

# boot 3 (fresh overlay B): THE RESET — dirt MUST be gone.
vm_make_overlay "$B" "$IMAGE"
echo ">> reset-native: boot 3 (fresh overlay B) — the reset; dirt must be gone"
vm_start "$B"; vm_wait_ssh 240 || { echo "FAIL: boot 3 never reached ssh"; exit 1; }
if vm_ssh "test -f $DIRT"; then echo "FAIL: [boot3] dirt survived a FRESH overlay — the reset does not isolate state"; exit 1
else echo "  [boot3] the reset OK: fresh overlay over the same backing image is pristine (dirt gone)"; fi
vm_shutdown

echo "PASS: td-native CoW reset ephemerality — guest-dirtied state persists across a reboot on the SAME overlay (control) and is GONE on a fresh overlay over the same read-only backing image (the reset), asserted over SSH with NO marionette/Guile in the guest. td's own VM harness."
