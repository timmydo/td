#!/bin/sh
# tests/boot-native.sh — the td-NATIVE disk-boot behavioral suite (move-off-Guile
# §5, lever 3): the SSH-driven reconstruction of tests/boot.scm's marionette
# assertions. Boots the un-instrumented td qcow2 ($1) under qemu and asserts, over
# SSH from the host (no Guile in the guest, no (gnu tests)/marionette):
#   M1  running kernel == the declaration ($2)
#   M2  sshd is up and answering on its port (the login itself proves it)
#   M3  default-deny: the daemon offers publickey only, never password
#   M3+ a key-based login as the non-root test user succeeds and returns output
#   M9  container host: cgroup2 mounted + crun shipped
#   rust-userland: procs/fd/rg/sd/eza/bat ship AND run (--version exits 0)
# All assertions are DURABLE (they hold with no Guix oracle: the system boots and
# does its job). Booting the qcow2 disk (firmware->GRUB->kernel->init->sshd) is
# itself the bootloader/partition/image exercise the marionette test guarded.
set -eu

IMAGE="${1:?usage: boot-native.sh QCOW2 EXPECTED-KERNEL}"
EXPECT_KERNEL="${2:?usage: boot-native.sh QCOW2 EXPECTED-KERNEL}"

VM_WORK="$(mktemp -d)"
. tests/vm-lib.sh
cleanup() { vm_shutdown; rm -rf "$VM_WORK"; }
trap cleanup EXIT INT TERM

vm_setup
echo ">> boot-native: booting $IMAGE (qemu disk boot, ssh on 127.0.0.1:$VM_PORT)"
vm_boot "$IMAGE"
vm_wait_ssh || { echo "FAIL: guest never reached ssh (boot failed)"; exit 1; }
echo "  [boot] firmware->GRUB->kernel->init->sshd reached over the qcow2 disk"

fail=0
note() { echo "  $1"; }

# M1: running kernel matches the declaration.
k="$(vm_ssh uname -r | tr -d '\r')"
if [ "$k" = "$EXPECT_KERNEL" ]; then note "[M1] kernel matches declaration: $k"
else echo "FAIL: [M1] kernel '$k' != declared '$EXPECT_KERNEL'"; fail=1; fi

# M2: sshd up + listening — the key login below already proves it; record it.
note "[M2] sshd up and answering (ssh login succeeded)"

# M3: default-deny — offered auth methods must be publickey only.
advert="$(vm_ssh_auth_advert probe)"
if printf '%s' "$advert" | grep -q "Authentications that can continue" \
   && printf '%s' "$advert" | grep -q "publickey" \
   && ! printf '%s' "$advert" | grep -q "password" \
   && ! printf '%s' "$advert" | grep -q "keyboard-interactive"; then
  note "[M3] daemon denies password auth (offers publickey only)"
else echo "FAIL: [M3] password-deny advert wrong:"; printf '%s\n' "$advert" | grep -i "can continue" >&2; fail=1; fi

# M3+: a key-based login as the non-root test user succeeds and returns output.
login="$(vm_ssh 'echo TD_LOGIN_OK; id -un' || true)"
if printf '%s' "$login" | grep -q "TD_LOGIN_OK" \
   && printf '%s' "$login" | grep -q "$VM_USER"; then
  note "[M3+] key login as $VM_USER works; command output captured"
else echo "FAIL: [M3+] key login/output wrong: $login"; fail=1; fi

# M9: container host — cgroup2 mounted + crun shipped.
cg="$(vm_ssh 'stat -f -c %T /sys/fs/cgroup' | tr -d '\r')"
crun="$(vm_ssh 'test -e /run/current-system/profile/bin/crun && echo yes || echo no' | tr -d '\r')"
if [ "$cg" = "cgroup2fs" ] && [ "$crun" = "yes" ]; then
  note "[M9] container host: cgroup2 mounted + crun shipped"
else echo "FAIL: [M9] cgroup='$cg' crun='$crun'"; fail=1; fi

# rust-userland: each tool present AND runs (--version exits 0 with output).
for t in procs fd rg sd eza bat; do
  out="$(vm_ssh "/run/current-system/profile/bin/$t --version" 2>/dev/null || true)"
  if [ -n "$out" ]; then note "[rust] $t runs ($(printf '%s' "$out" | head -1))"
  else echo "FAIL: [rust] $t did not run --version"; fail=1; fi
done

test "$fail" -eq 0 || { echo "FAIL: boot-native behavioral suite had failures"; exit 1; }
echo "PASS: td-native disk boot — qcow2 booted through GRUB and the full behavioral suite (kernel, sshd, default-deny, key-login, container-host, rust userland) passed over SSH, with NO Guile in the guest and NO (gnu tests)/(gnu build marionette) — td's own VM-boot harness."
