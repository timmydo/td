#!/usr/bin/env bash
# Behavioral self-tests of td's loop-sandbox HARDENING (mk/gates/272):
#
#  (A) MINIMAL /dev — the sandbox exposes only the standard char devices, built
#      as a fresh tmpfs, NOT a blanket rbind of the host /dev. So /dev/null &c.
#      work, but the host /dev/kmsg (kernel log), /dev/kvm, raw disks, /dev/mem
#      and input devices are NOT reachable. (builder/src/sandbox.rs host_shell.)
#
#  (B) ORPHAN REAPING — killing the top-level td-builder must tear the whole
#      inner sandbox tree down (PR_SET_PDEATHSIG=SIGKILL armed at both fork
#      levels): the PID-ns parent dies → PID 1 is SIGKILLed → the kernel reaps
#      the namespace. Without it a CI cancel/timeout orphans the build + mounts.
#
# Runs INSIDE the loop sandbox (the gate already nests one); the nested
# td-builder's processes are visible in this PID namespace, so a /proc cmdline
# scan can confirm they are gone after the kill. No pgrep (absent from the
# toolchain) — pure bash + coreutils + td text checks.
set -euo pipefail
tb=${1:?usage: sandbox-hardening.sh TD_BUILDER}
realbash=$(readlink -f "$(command -v bash)")
realsleep=$(readlink -f "$(command -v sleep)")

store_root_for() {
  case "$1" in
    /*/store/*)
      rest=${1#/}
      first=${rest%%/*}
      printf '/%s/store\n' "$first"
      ;;
    *)
      echo "FAIL: $1 is not under a store root" >&2
      exit 1
      ;;
  esac
}

sroot=$(store_root_for "$realbash")
sleep_root=$(store_root_for "$realsleep")
test "$sroot" = "$sleep_root" || {
  echo "FAIL: bash and sleep resolved under different store roots: $realbash / $realsleep" >&2
  exit 1
}

echo ">> (A) minimal /dev: standard nodes present, host kmsg/kvm/disks/mem/input absent"
"$tb" host-sandbox --store-from "$sroot" --store-at "$sroot" -- "$realbash" -c '
  [ -e /dev/null ] && [ -w /dev/null ]    || { echo "  no writable /dev/null";   exit 11; }
  [ -e /dev/zero ] && [ -e /dev/urandom ] || { echo "  missing /dev/zero|urandom"; exit 12; }
  for leak in kmsg kvm mem sda sdb nvme0n1 input/event0; do
    [ -e "/dev/$leak" ] && { echo "  LEAK: /dev/$leak is reachable"; exit 21; }
  done
  exit 0
' || { echo "FAIL: minimal-/dev assertion failed — the sandbox /dev is not minimal (host device leak)" >&2; exit 1; }
echo "   /dev exposes the standard nodes; kmsg/kvm/mem/disks/input are absent"

echo ">> (B) orphan reaping: killing td-builder reaps the whole inner sandbox tree"
marker=$(( 1000000 + RANDOM ))   # distinctive token carried in every inner cmdline
scan() {                          # count procs whose cmdline contains $marker
  local n=0 f
  for f in /proc/[0-9]*/cmdline; do
    [ -r "$f" ] || continue
    if tr '\0' ' ' < "$f" 2>/dev/null | "$tb" text contains "$marker" -; then n=$((n + 1)); fi
  done
  printf %s "$n"
}
sweep() {                         # SIGKILL any leftover marker-carrying procs
  local f p
  for f in /proc/[0-9]*/cmdline; do
    [ -r "$f" ] || continue
    if tr '\0' ' ' < "$f" 2>/dev/null | "$tb" text contains "$marker" -; then
      p=${f#/proc/}; p=${p%/cmdline}; kill -9 "$p" 2>/dev/null || true
    fi
  done
}

"$tb" host-sandbox --store-from "$sroot" --store-at "$sroot" -- "$realbash" -c "$realsleep $marker & $realsleep $marker & wait" &
top=$!
for _ in $(seq 1 100); do [ "$(scan)" -ge 2 ] && break; sleep 0.1; done
before=$(scan)
echo "   inner procs carrying the marker before kill: $before"
[ "$before" -ge 2 ] || { echo "FAIL: the inner sandbox tree never started (marker=$marker)" >&2; kill "$top" 2>/dev/null || true; sweep; exit 1; }

kill -TERM "$top" 2>/dev/null || true
for _ in $(seq 1 100); do [ "$(scan)" -eq 0 ] && break; sleep 0.1; done
after=$(scan)
echo "   inner procs carrying the marker after killing td-builder ($top): $after"
if [ "$after" -ne 0 ]; then
  sweep
  echo "FAIL: $after sandbox descendant(s) survived td-builder termination — orphaned (PR_SET_PDEATHSIG reaping broken)" >&2
  exit 1
fi
echo "PASS: minimal /dev (no host device leak) + the inner sandbox tree is fully reaped when td-builder is killed."
