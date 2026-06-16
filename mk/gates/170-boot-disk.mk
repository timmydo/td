# 3b. Disk-image boot — boot the qcow2 through its GRUB bootloader (firmware ->
#     GRUB -> kernel -> init, NOT direct-kernel), so the bootloader, partition
#     table and disk image are actually exercised, THEN run the full behavioral
#     suite (kernel, sshd up + port, default-deny, key-login, container-host) on
#     that realistic boot. This is now the SOLE boot test: the former direct-
#     kernel `test` gate was removed (track amortize-vm-boots) and its asserts
#     moved here, eliminating one full VM boot per check. Honest two-step
#     lower-then-realise. Heavier (builds a full image + boots it + sshd/login
#     asserts), so it runs after the cheap gates.
HEAVY_GATES += boot-disk
boot-disk:
	@echo ">> boot-disk: boot the qcow2 disk through GRUB + full behavioral suite"
	$(call realise-system-test,(tests boot),%test-td-disk-boot,disk-boot)
