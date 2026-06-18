# boot-disk-native — td's OWN VM-boot test harness (move-off-Guile §5, lever 3):
# boot the td qcow2 under qemu and assert the full behavioral suite over SSH from
# the HOST, with NO Guile in the guest and NO (gnu tests) / (gnu build marionette).
# This is the td-native reconstruction of the boot.scm marionette test (gate
# boot-disk): it boots the UN-instrumented image (no marionette backdoor service —
# the durable bonus the boot.scm comment flagged as the follow-up) and drives the
# same assertions (kernel, sshd, default-deny, key-login, container-host, rust
# userland) via ssh. All legs are DURABLE — they hold with no Guix oracle (the
# system boots and does its job). The qcow2 build still uses guix `system image`
# (the config/toolchain layer, retired last §5); only the TEST HARNESS moves here.
# Reusable primitives live in tests/vm-lib.sh; this gate's assertions in
# tests/boot-native.sh. Runs after the cheap gates (boots a full VM).
SYSTEM_GATES += boot-disk-native
boot-disk-native:
	@echo ">> boot-disk-native: boot the td qcow2 under qemu + full behavioral suite over SSH (td's own harness, no marionette/Guile in guest)"
	@set -euo pipefail; \
	out=`printf '%s\n' \
	    '(use-modules (guix) (guix gexp) (gnu image) (gnu system image) (tests boot))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (format #t "DRV=~a~%" (derivation-file-name (run-with-store store (lower-object %native-disk-image))))' \
	    '  (format #t "KERNEL=~a~%" %expected-kernel-release))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null`; \
	drv=`printf '%s\n' "$$out" | sed -n 's/^DRV=//p'`; \
	k=`printf '%s\n' "$$out" | sed -n 's/^KERNEL=//p'`; \
	test -n "$$drv" -a -n "$$k" || { echo "ERROR: could not lower the native qcow2 drv / kernel" >&2; exit 1; }; \
	echo ">> native qcow2 drv: $$drv (expect kernel $$k)"; \
	img=`$(GUIX) build "$$drv"`; \
	test -f "$$img" || { echo "ERROR: built qcow2 is not a file: $$img" >&2; exit 1; }; \
	qd=`for p in $$($(GUIX) build qemu-minimal 2>/dev/null); do [ -x "$$p/bin/qemu-system-x86_64" ] && echo "$$p" && break; done`; \
	od=`for p in $$($(GUIX) build openssh 2>/dev/null); do [ -x "$$p/bin/ssh" ] && echo "$$p" && break; done`; \
	nssw=`for p in $$($(GUIX) build nss-wrapper 2>/dev/null); do [ -f "$$p/lib/libnss_wrapper.so" ] && echo "$$p/lib/libnss_wrapper.so" && break; done`; \
	test -n "$$qd" -a -n "$$od" -a -n "$$nssw" || { echo "ERROR: could not resolve qemu/openssh/nss-wrapper" >&2; exit 1; }; \
	export VM_QEMU="$$qd/bin/qemu-system-x86_64" VM_QEMU_IMG="$$qd/bin/qemu-img" VM_SSH="$$od/bin/ssh" VM_NSS_WRAPPER="$$nssw"; \
	sh tests/boot-native.sh "$$img" "$$k"
