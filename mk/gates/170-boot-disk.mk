# 3b. Disk-image boot (triage #2) — boot the qcow2 through its GRUB bootloader
#     (not the direct-kernel VM the `test` gate uses), so the bootloader,
#     partition table and disk image are actually exercised. Same honest two-step
#     lower-then-realise as `test`. Heavier (builds a second full image + boots
#     it), so it runs after the cheap gates.
HEAVY_GATES += boot-disk
boot-disk:
	@echo ">> boot-disk: boot the qcow2 disk through GRUB + assert kernel"
	$(call realise-system-test,(tests boot),%test-td-disk-boot,disk-boot)
