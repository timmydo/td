# M10.3 manual rollback (M10-design.md step 5, "Roll back"; the DESIGN §7.1
# acceptance test). End-to-end: the guix-free placer's output — live labeled
# per-generation root filesystems (--mkfs) + the managed GRUB menu — is
# assembled into a real MBR/GRUB disk (system/td-disk.scm), and the marionette
# test (tests/rollback.scm) boots ONE persistent qcow2 overlay of it TWICE:
# generation 2 (the GRUB default) is asserted three independent ways (cmdline
# bare-label root=, mounted-root-IS-the-labeled-filesystem, /run/current-system ==
# gen-2's system path — the placer's gnu.system wiring), the manual rollback
# act writes `set default=td-gen-1` into the boot partition's td/default.cfg
# (the hook the managed block sources) plus a persistence sentinel, the guest
# reboots cleanly, and generation 1 is asserted the same three ways — with the
# sentinel, the selection, gen-2's placed files and BOTH menu entries proven to
# have survived the reboot (persistent placed state; rolling back never
# destroys the newer generation). Before booting: `--check` both new artifacts
# (the mkfs tree and the assembled disk — prime directive 1) and validate the
# tree with tests/place-check.scm in mkfs mode (superblock label/UUID, search
# line). Two-step lower-then-realise for the marionette derivation, as in
# `test`/`boot-disk` (honest exit status).
SYSTEM_GATES += rollback
rollback:
	@echo ">> rollback: boot gen 2, roll back to gen 1 via the GRUB menu, assert identity + persistence (M10.3)"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/rollback-drv.scm 2>/dev/null`; \
	tree_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_TREE=//p'`; \
	disk_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_DISK=//p'`; \
	test -n "$$tree_drv" -a -n "$$disk_drv" || { echo "ERROR: could not lower the rollback derivations" >&2; exit 1; }; \
	echo ">> placed tree (mkfs) derivation: $$tree_drv"; \
	echo ">> rollback disk derivation:      $$disk_drv"; \
	tree=`$(GUIX) build "$$tree_drv"`; \
	disk=`$(GUIX) build "$$disk_drv"`; \
	echo ">> check: reproducibility of the mkfs placed tree AND the assembled disk (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$tree_drv" "$$disk_drv"; \
	echo ">> validate the mkfs tree (live labeled roots via superblock, boot wiring, search line)"; \
	TD_PLACED="$$tree" TD_PRESENT="1 2" TD_ABSENT="" TD_MKFS=1 TD_BOOT_LABEL=td-boot \
	  $(GUIX) repl $(LOAD) tests/place-check.scm; \
	drv=`printf '%s\n' \
	    '(use-modules (guix) (gnu tests) (tests rollback))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (format #t "DRV=~a~%"' \
	    '          (derivation-file-name' \
	    '           (run-with-store store (system-test-value %test-td-rollback)))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the rollback test derivation" >&2; exit 1; }; \
	echo ">> realise rollback test derivation: $$drv"; \
	$(GUIX) build "$$drv"
