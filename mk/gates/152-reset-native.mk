# reset-native — td's OWN per-test CoW-reset ephemerality test (move-off-Guile §5,
# lever 3): the td-native reconstruction of the reset.scm marionette test. Boots a
# NON-volatile td qcow2 over explicit CoW overlays and asserts, over SSH from the
# host (no Guile in the guest, no (gnu tests)/marionette), that guest-dirtied
# state persists across a reboot on the SAME overlay (negative control) and is
# gone on a fresh overlay over the same read-only backing image (the reset).
# Locks in the fresh-state-per-test guarantee §1.5 names. DURABLE (no Guix oracle).
# Reusable primitives in tests/vm-lib.sh; assertions in tests/reset-native.sh.
HEAVY_GATES += reset-native
reset-native:
	@echo ">> reset-native: CoW-reset ephemerality over SSH (td's own harness, no marionette/Guile in guest)"
	@set -euo pipefail; \
	drv=`$(GUIX) repl $(LOAD) tests/reset-native-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the native reset qcow2 drv" >&2; exit 1; }; \
	echo ">> native non-volatile qcow2 drv: $$drv"; \
	img=`$(GUIX) build "$$drv"`; \
	test -f "$$img" || { echo "ERROR: built qcow2 is not a file: $$img" >&2; exit 1; }; \
	qd=`for p in $$($(GUIX) build qemu-minimal 2>/dev/null); do [ -x "$$p/bin/qemu-system-x86_64" ] && echo "$$p" && break; done`; \
	od=`for p in $$($(GUIX) build openssh 2>/dev/null); do [ -x "$$p/bin/ssh" ] && echo "$$p" && break; done`; \
	nssw=`for p in $$($(GUIX) build nss-wrapper 2>/dev/null); do [ -f "$$p/lib/libnss_wrapper.so" ] && echo "$$p/lib/libnss_wrapper.so" && break; done`; \
	test -n "$$qd" -a -n "$$od" -a -n "$$nssw" || { echo "ERROR: could not resolve qemu/openssh/nss-wrapper" >&2; exit 1; }; \
	export VM_QEMU="$$qd/bin/qemu-system-x86_64" VM_QEMU_IMG="$$qd/bin/qemu-img" VM_SSH="$$od/bin/ssh" VM_NSS_WRAPPER="$$nssw"; \
	sh tests/reset-native.sh "$$img"
