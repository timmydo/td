# tests/vm-lib.sh — td's OWN VM-boot test harness primitives (move-off-Guile §5,
# lever 3). The REPLACEMENT for guix's (gnu tests) + (gnu build marionette): boot
# a td-built qcow2 under qemu from the HOST and drive assertions over SSH, so NO
# Guile runs in the guest and no marionette REPL/backdoor is involved. qemu
# user-mode networking forwards a host loopback port to the guest's sshd (the
# guest brings up its NIC via dhcpcd — already in td-system); the host logs in
# with the committed test key. POSIX sh; sourced by tests/<test>-native.sh.
#
# Globals a caller may preset: VM_USER (default "tester"), VM_KEY (default the
# committed test private key), VM_MEM (default 1024). vm_setup resolves the
# qemu/ssh binaries from guix (the toolchain layer, retired last — not the test
# harness). vm_boot sets VM_PID / VM_PORT / VM_SERIAL.

: "${VM_USER:=tester}"
: "${VM_KEY:=tests/keys/td_test_ed25519}"
: "${VM_MEM:=1024}"

# Resolve the store output of a guix package that actually CONTAINS a given
# relative binary — `guix build` may print several outputs (e.g. qemu-minimal
# emits both `-doc` and the main output), so we cannot just take a line.
_vm_output_with() {  # $1=spec  $2=rel/path/to/bin
  for _p in $(${GUIX_BUILD:-guix build} "$1" 2>/dev/null); do
    [ -x "$_p/$2" ] && { printf '%s' "$_p"; return 0; }
  done
  return 1
}

vm_setup() {
  # The gate passes VM_QEMU/VM_QEMU_IMG/VM_SSH (resolved with the pinned $(GUIX));
  # only resolve here when run standalone (e.g. a de-risk).
  if [ -z "${VM_QEMU:-}" ] || [ -z "${VM_QEMU_IMG:-}" ] || [ -z "${VM_SSH:-}" ]; then
    _qd="$(_vm_output_with qemu-minimal bin/qemu-system-x86_64)" || true
    _od="$(_vm_output_with openssh bin/ssh)" || true
    VM_QEMU="$_qd/bin/qemu-system-x86_64"
    VM_QEMU_IMG="$_qd/bin/qemu-img"
    VM_SSH="$_od/bin/ssh"
  fi
  test -x "$VM_QEMU" -a -x "$VM_QEMU_IMG" -a -x "$VM_SSH" || {
    echo "vm-lib: could not resolve qemu/ssh (VM_QEMU=$VM_QEMU VM_SSH=$VM_SSH)" >&2; return 1; }
  # The test key must be 0600 for ssh to use it (git may store it group-readable).
  cp "$VM_KEY" "$VM_WORK/key" && chmod 600 "$VM_WORK/key"; VM_KEY="$VM_WORK/key"
  export HOME="$VM_WORK"
  # The ssh CLIENT calls getpwuid() for the invoking user before connecting; the
  # loop sandbox has no /etc/passwd entry for our uid, so ssh aborts with "No user
  # exists for uid N". nss_wrapper (LD_PRELOAD + a fake passwd/group for just our
  # uid) makes the lookup succeed. The gate passes VM_NSS_WRAPPER (resolved with
  # the pinned $(GUIX)); fall back to resolving it here when run standalone.
  _uid="$(id -u)"; _gid="$(id -g)"
  printf 'runner:x:%s:%s:runner:%s:/bin/sh\n' "$_uid" "$_gid" "$VM_WORK" > "$VM_WORK/passwd"
  printf 'runner:x:%s:\n' "$_gid" > "$VM_WORK/group"
  if [ -z "${VM_NSS_WRAPPER:-}" ]; then
    VM_NSS_WRAPPER="$(_vm_output_with nss-wrapper lib/libnss_wrapper.so)/lib/libnss_wrapper.so" || true
  fi
  if [ -f "${VM_NSS_WRAPPER:-/nonexistent}" ]; then
    export NSS_WRAPPER_PASSWD="$VM_WORK/passwd" NSS_WRAPPER_GROUP="$VM_WORK/group"
    export LD_PRELOAD="$VM_NSS_WRAPPER${LD_PRELOAD:+:$LD_PRELOAD}"
  else
    echo "vm-lib: WARNING: no nss_wrapper; ssh may fail if uid is unresolvable" >&2
  fi
  # A private host loopback port for hostfwd; $$ differs per gate shell so two
  # concurrent heavy gates (make -j2) in the shared netns do not collide.
  VM_PORT=$(( 20000 + ($$ % 20000) ))
}

# vm_make_overlay OVERLAY BACKING   a fresh CoW qcow2 OVERLAY over (read-only)
# BACKING — the store image is never written, so /gnu/store immutability holds.
vm_make_overlay() {
  rm -f "$1"
  "$VM_QEMU_IMG" create -q -f qcow2 -b "$2" -F qcow2 "$1" >/dev/null
}

# vm_start DRIVEFILE   boot a qcow2 file read-write under qemu. We do NOT use
# qemu's -snapshot: it writes its ephemeral overlay to /var/tmp, absent in the
# loop sandbox. Instead callers boot an explicit overlay (ephemeral or persistent)
# that they manage. -no-reboot: each boot is one qemu lifecycle, we drive resets
# by swapping overlays, not in-guest reboots.
vm_start() {
  VM_SERIAL="$VM_WORK/serial.log"
  # KVM only when /dev/kvm is actually USABLE (writable) — mere existence is not
  # enough (a host/sandbox may expose the node without granting access), and
  # `-enable-kvm` then aborts qemu. Fall back to TCG (slower, but always works).
  _kvm=""; [ -w /dev/kvm ] && _kvm="-enable-kvm"
  echo "vm-lib: booting accel=$( [ -n "$_kvm" ] && echo KVM || echo TCG ) port=$VM_PORT serial=$VM_SERIAL" >&2
  # shellcheck disable=SC2086
  "$VM_QEMU" $_kvm -no-reboot -m "$VM_MEM" \
    -drive "file=$1,if=virtio,format=qcow2" \
    -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:${VM_PORT}-:22" -device virtio-net-pci,netdev=n0 \
    -display none -serial "file:$VM_SERIAL" &
  VM_PID=$!
}

# vm_boot IMAGE   ephemeral boot: a throwaway CoW overlay, discarded with VM_WORK.
vm_boot() {
  vm_make_overlay "$VM_WORK/eph.qcow2" "$1"
  vm_start "$VM_WORK/eph.qcow2"
}

_vm_ssh_opts() {
  printf '%s' "-i $VM_KEY -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
-o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o BatchMode=yes -p $VM_PORT"
}

# vm_ssh CMD...   run a command in the guest over ssh; stdout+stderr passed through.
vm_ssh() {
  # shellcheck disable=SC2046
  "$VM_SSH" $(_vm_ssh_opts) "${VM_USER}@127.0.0.1" "$@"
}

# vm_wait_ssh [timeout_s]   poll until a key login succeeds (guest booted + sshd up).
vm_wait_ssh() {
  _deadline=$(( $(date +%s) + ${1:-${VM_BOOT_TIMEOUT:-240}} ))
  while [ "$(date +%s)" -lt "$_deadline" ]; do
    if ! kill -0 "$VM_PID" 2>/dev/null; then
      echo "vm-lib: qemu exited before ssh came up; serial:" >&2
      cat "$VM_SERIAL" >&2 2>/dev/null || true
      return 1
    fi
    if vm_ssh true >/dev/null 2>&1; then return 0; fi
    sleep 3
  done
  echo "vm-lib: timed out waiting for ssh; verbose ssh probe + serial follow:" >&2
  # shellcheck disable=SC2046
  "$VM_SSH" -v $(_vm_ssh_opts) "${VM_USER}@127.0.0.1" true 2>&1 | head -25 >&2 || true
  echo "---- serial ($(wc -c <"$VM_SERIAL" 2>/dev/null || echo 0) bytes) ----" >&2
  cat "$VM_SERIAL" >&2 2>/dev/null || true
  echo "---- end serial ----" >&2
  return 1
}

# vm_ssh_auth_advert USER   the server's offered auth methods (default-deny probe):
# offer only the "none" method and capture ssh's verbose "can continue" advert.
vm_ssh_auth_advert() {
  # shellcheck disable=SC2046
  "$VM_SSH" -v -o PreferredAuthentications=none -o PubkeyAuthentication=no \
    -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o ConnectTimeout=10 -p "$VM_PORT" "${1:-probe}@127.0.0.1" true 2>&1 || true
}

vm_shutdown() {
  [ -n "${VM_PID:-}" ] || return 0
  kill "$VM_PID" 2>/dev/null || true
  for _ in 1 2 3 4 5; do kill -0 "$VM_PID" 2>/dev/null || return 0; sleep 1; done
  kill -9 "$VM_PID" 2>/dev/null || true
}
