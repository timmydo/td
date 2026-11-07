use crate::shared::{
    initramfs_cpio_shape_check, unpack_into, BUILD_ROOT_PATH as PATH, BUILD_ROOT_SH as SH,
    EROFS_MARKER, EROFS_PROBE_CONTENT, EROFS_PROBE_SENTINEL, USERLAND_MARKER,
};
use crate::types::{Recipe, Step, TextEdit};

/// ath11k_pci_power_up's failed start, and the same with the link put back.
const ATH11K_MHI_FAILED: &str =
    "\t\tath11k_err(ab, \"failed to start mhi: %d\\n\", ret);\n\t\treturn ret;";
const ATH11K_MHI_FAILED_RESTORES_ASPM: &str = "\t\tath11k_err(ab, \"failed to start mhi: %d\\n\", ret);\n\t\tath11k_pci_aspm_restore(ab_pci);\n\t\treturn ret;";
/// ath10k's power-down, which is empty: only a started firmware puts its link back.
const ATH10K_POWER_DOWN: &str = "so there's no point in resetting here.\n\t */\n}";
const ATH10K_POWER_DOWN_RESTORES_ASPM: &str = "so there's no point in resetting here.\n\t */\n\tpcie_capability_clear_and_set_word(ath10k_pci_priv(ar)->pdev, PCI_EXP_LNKCTL,\n\t\t\t\t\t   PCI_EXP_LNKCTL_ASPMC,\n\t\t\t\t\t   ath10k_pci_priv(ar)->link_ctl & PCI_EXP_LNKCTL_ASPMC);\n}";
/// The same in ath12k, which copied ath11k's function.
const ATH12K_MHI_FAILED: &str =
    "\t\tath12k_err(ab, \"failed to start mhi: %d\\n\", ret);\n\t\treturn ret;";
const ATH12K_MHI_FAILED_RESTORES_ASPM: &str = "\t\tath12k_err(ab, \"failed to start mhi: %d\\n\", ret);\n\t\tath12k_pci_aspm_restore(ab_pci);\n\t\treturn ret;";

/// Every kernel config symbol the recipe decides, in `.config` syntax. It is
/// appended over allnoconfig before olddefconfig resolves the rest; the
/// lock below then holds the result, and a test holds that every line here took
/// effect. Adding a driver is a line here and the lock's diff. The reasoning is
/// in the notes at step 2 of `recipe()`, whose "guarded" means named in the
/// `HELD` tests over the lock.
const CONFIG: &[&str] = &[
    "CONFIG_SYSCALL_USER_DISPATCH=y",
    "# CONFIG_SERIAL_8250_MOXA_PCIE is not set",
    "CONFIG_UNWINDER_FRAME_POINTER=y",
    "# CONFIG_UNWINDER_ORC is not set",
    "# CONFIG_MODULES is not set",
    "# CONFIG_MODULE_SIG is not set",
    "# CONFIG_SYSTEM_TRUSTED_KEYRING is not set",
    "# CONFIG_DEBUG_INFO_BTF is not set",
    "# CONFIG_FTRACE is not set",
    "# CONFIG_GCC_PLUGINS is not set",
    "# CONFIG_IKHEADERS is not set",
    "CONFIG_KERNEL_GZIP=y",
    "CONFIG_BINFMT_ELF=y",
    "CONFIG_BINFMT_SCRIPT=y",
    "CONFIG_BLK_DEV_INITRD=y",
    "CONFIG_SERIAL_8250=y",
    "CONFIG_SERIAL_8250_CONSOLE=y",
    "CONFIG_INITRAMFS_SOURCE=\"\"",
    "CONFIG_PROC_FS=y",
    "CONFIG_SYSFS=y",
    "CONFIG_DEVTMPFS=y",
    "CONFIG_UNIX98_PTYS=y",
    "CONFIG_TMPFS=y",
    "CONFIG_BLOCK=y",
    "CONFIG_BLK_DEV=y",
    "CONFIG_PCI=y",
    "CONFIG_PCI_MSI=y",
    "CONFIG_VIRTIO=y",
    "CONFIG_VIRTIO_MENU=y",
    "CONFIG_VIRTIO_PCI=y",
    "CONFIG_VIRTIO_CONSOLE=y",
    "CONFIG_VIRTIO_BLK=y",
    "CONFIG_MISC_FILESYSTEMS=y",
    "CONFIG_EROFS_FS=y",
    "CONFIG_EROFS_FS_ZIP=y",
    "CONFIG_EROFS_FS_ZIP_ZSTD=y",
    "# CONFIG_KEXEC is not set",
    "CONFIG_KEXEC_FILE=y",
    "# CONFIG_KEXEC_SIG is not set",
    "# CONFIG_KEXEC_SIG_FORCE is not set",
    "CONFIG_RELOCATABLE=y",
    "# CONFIG_RANDOMIZE_BASE is not set",
    "CONFIG_BLK_DEV_LOOP=y",
    "CONFIG_BLK_DEV_RAM=y",
    "CONFIG_BLK_DEV_RAM_COUNT=1",
    "CONFIG_BTRFS_FS=y",
    "CONFIG_SCSI=y",
    "CONFIG_BLK_DEV_SD=y",
    "CONFIG_BLK_DEV_SR=y",
    "CONFIG_ATA=y",
    "CONFIG_SATA_AHCI=y",
    "CONFIG_BLK_DEV_NVME=y",
    "CONFIG_NVME_CORE=y",
    "# CONFIG_NVME_MULTIPATH is not set",
    "CONFIG_USB_STORAGE=y",
    "CONFIG_ISO9660_FS=y",
    "CONFIG_MD=y",
    "CONFIG_BLK_DEV_DM=y",
    "CONFIG_DM_CRYPT=y",
    "CONFIG_CRYPTO_AES=y",
    "CONFIG_CRYPTO_XTS=y",
    "CONFIG_CRYPTO_AES_NI_INTEL=y",
    "CONFIG_CRYPTO_HMAC=y",
    "CONFIG_CRYPTO_SHA256=y",
    "# CONFIG_CRYPTO_USER_API_HASH is not set",
    "# CONFIG_CRYPTO_USER_API_SKCIPHER is not set",
    "# CONFIG_CRYPTO_USER_API_RNG is not set",
    "# CONFIG_CRYPTO_USER_API_AEAD is not set",
    "CONFIG_NET=y",
    "CONFIG_PACKET=y",
    "CONFIG_UNIX=y",
    "CONFIG_INET=y",
    "CONFIG_NETDEVICES=y",
    "CONFIG_VIRTIO_NET=y",
    "CONFIG_ETHERNET=y",
    "CONFIG_NET_VENDOR_INTEL=y",
    "CONFIG_E1000=y",
    "CONFIG_HYPERVISOR_GUEST=y",
    "CONFIG_PARAVIRT=y",
    "CONFIG_KVM_GUEST=y",
    "CONFIG_HYPERV=y",
    "CONFIG_HYPERV_VMBUS=y",
    "CONFIG_SCSI_LOWLEVEL=y",
    "CONFIG_HYPERV_STORAGE=y",
    "CONFIG_HYPERV_NET=y",
    "CONFIG_PCI_HYPERV=y",
    "CONFIG_NET_VENDOR_AMAZON=y",
    "CONFIG_ENA_ETHERNET=y",
    "CONFIG_SCSI_VIRTIO=y",
    "CONFIG_NET_VENDOR_GOOGLE=y",
    "CONFIG_GVE=y",
    "CONFIG_DMIID=y",
    "CONFIG_INPUT=y",
    "CONFIG_INPUT_EVDEV=y",
    "CONFIG_INPUT_KEYBOARD=y",
    "CONFIG_KEYBOARD_ATKBD=y",
    "CONFIG_INPUT_MOUSE=y",
    "CONFIG_MOUSE_PS2=y",
    "CONFIG_VIRTIO_INPUT=y",
    "CONFIG_SERIO=y",
    "CONFIG_SERIO_I8042=y",
    "CONFIG_VT=y",
    "CONFIG_VT_CONSOLE=y",
    "CONFIG_VT_HW_CONSOLE_BINDING=y",
    "CONFIG_FB=y",
    "CONFIG_FRAMEBUFFER_CONSOLE=y",
    "CONFIG_DRM=y",
    "CONFIG_DRM_VIRTIO_GPU=y",
    "CONFIG_DRM_VIRTIO_GPU_KMS=y",
    "CONFIG_DRM_FBDEV_EMULATION=y",
    "CONFIG_DRM_CLIENT_DEFAULT_FBDEV=y",
    "CONFIG_SYSFB_SIMPLEFB=y",
    "CONFIG_DRM_SIMPLEDRM=y",
    "# CONFIG_SECURITY_DMESG_RESTRICT is not set",
    "CONFIG_ACPI=y",
    "CONFIG_EFI=y",
    "CONFIG_EFI_STUB=y",
    "CONFIG_CMDLINE_BOOL=y",
    "CONFIG_CMDLINE=@EFI_CMDLINE@",
    "# CONFIG_CMDLINE_OVERRIDE is not set",
    "# CONFIG_EFI_HANDOVER_PROTOCOL is not set",
    "# CONFIG_EFI_MIXED is not set",
    "# CONFIG_EFIVAR_FS is not set",
    "CONFIG_PNP=y",
    "CONFIG_PNPACPI=y",
    "CONFIG_USB_SUPPORT=y",
    "CONFIG_USB=y",
    "CONFIG_USB_PCI=y",
    "CONFIG_USB_XHCI_HCD=y",
    "CONFIG_HID_SUPPORT=y",
    "CONFIG_HID=y",
    "CONFIG_HID_GENERIC=y",
    "CONFIG_UHID=y",
    "CONFIG_HIDRAW=y",
    "CONFIG_USB_HID=y",
    "CONFIG_TCG_TPM=y",
    "CONFIG_SECURITYFS=y",
    "CONFIG_SECURITY=y",
    "CONFIG_LSM=\"\"",
    "# CONFIG_SECURITY_NETWORK is not set",
    "# CONFIG_SECURITY_PATH is not set",
    "# CONFIG_SECURITY_SELINUX is not set",
    "# CONFIG_SECURITY_SMACK is not set",
    "# CONFIG_SECURITY_TOMOYO is not set",
    "# CONFIG_SECURITY_APPARMOR is not set",
    "# CONFIG_SECURITY_LOADPIN is not set",
    "# CONFIG_SECURITY_YAMA is not set",
    "# CONFIG_SECURITY_SAFESETID is not set",
    "# CONFIG_SECURITY_LOCKDOWN_LSM is not set",
    "# CONFIG_SECURITY_LANDLOCK is not set",
    "# CONFIG_SECURITY_IPE is not set",
    "# CONFIG_BPF_LSM is not set",
    "# CONFIG_INTEGRITY is not set",
    "CONFIG_TCG_TIS=y",
    "CONFIG_TCG_CRB=y",
    "# CONFIG_TCG_TPM2_HMAC is not set",
    "# CONFIG_HW_RANDOM_TPM is not set",
    "CONFIG_PERF_EVENTS=y",
    "CONFIG_SMP=y",
    "CONFIG_NR_CPUS=256",
    "CONFIG_HOTPLUG_CPU=y",
    "CONFIG_USER_NS=y",
    "CONFIG_PID_NS=y",
    "CONFIG_UTS_NS=y",
    "CONFIG_NET_NS=y",
    "CONFIG_AUDIT=y",
    "CONFIG_COMPAT_32BIT_TIME=y",
    "CONFIG_IA32_EMULATION=y",
    "# CONFIG_IA32_EMULATION_DEFAULT_DISABLED is not set",
    "# CONFIG_X86_X32_ABI is not set",
    "CONFIG_SECCOMP=y",
    "CONFIG_INOTIFY_USER=y",
    "CONFIG_CGROUPS=y",
    "CONFIG_CGROUP_SCHED=y",
    "CONFIG_FAIR_GROUP_SCHED=y",
    "CONFIG_CFS_BANDWIDTH=y",
    "# CONFIG_RT_GROUP_SCHED is not set",
    "CONFIG_MEMCG=y",
    "CONFIG_CGROUP_PIDS=y",
    "CONFIG_SOUND=y",
    "CONFIG_SND=y",
    "CONFIG_SND_PCM=y",
    "# CONFIG_SND_PCM_OSS is not set",
    "CONFIG_SND_PCI=y",
    "CONFIG_SND_HDA_INTEL=y",
    "CONFIG_SND_HDA_GENERIC=y",
    "CONFIG_SND_ALOOP=y",
    "CONFIG_DRM_I915=y",
    "CONFIG_E1000E=y",
    "CONFIG_X86_PLATFORM_DEVICES=y",
    "CONFIG_ACPI_EC=y",
    "CONFIG_I2C=y",
    "CONFIG_THINKPAD_ACPI=y",
    "CONFIG_ACPI_AC=y",
    "CONFIG_ACPI_BATTERY=y",
    "CONFIG_ACPI_BUTTON=y",
    "CONFIG_ACPI_THERMAL=y",
    "CONFIG_ACPI_FAN=y",
    "CONFIG_ACPI_VIDEO=y",
    "CONFIG_BACKLIGHT_CLASS_DEVICE=y",
    "CONFIG_CPU_FREQ=y",
    "CONFIG_X86_INTEL_PSTATE=y",
    "CONFIG_CPU_IDLE=y",
    "CONFIG_INTEL_IDLE=y",
    "CONFIG_SUSPEND=y",
    "CONFIG_USB_EHCI_HCD=y",
    "CONFIG_MMC=y",
    "CONFIG_MMC_BLOCK=y",
    "CONFIG_MMC_SDHCI=y",
    "CONFIG_MMC_SDHCI_PCI=y",
    "CONFIG_MMC_RICOH_MMC=y",
    "CONFIG_SND_HDA_CODEC_CONEXANT=y",
    "CONFIG_SND_HDA_CODEC_HDMI=y",
    "CONFIG_DRM_AMDGPU=y",
    "CONFIG_DRM_AMD_DC=y",
    "CONFIG_RTC_CLASS=y",
    "CONFIG_RTC_DRV_CMOS=y",
    "CONFIG_PCIEPORTBUS=y",
    "CONFIG_PCIEASPM=y",
    "CONFIG_PCIEASPM_DEFAULT=y",
    "CONFIG_AMD_PMC=y",
    "CONFIG_X86_AMD_PSTATE=y",
    "CONFIG_X86_AMD_PLATFORM_DEVICE=y",
    "CONFIG_PINCTRL_AMD=y",
    "CONFIG_I2C_DESIGNWARE_CORE=y",
    "CONFIG_I2C_DESIGNWARE_PLATFORM=y",
    "CONFIG_I2C_HID=y",
    "CONFIG_I2C_HID_ACPI=y",
    "CONFIG_HID_MULTITOUCH=y",
    "CONFIG_SND_HDA_CODEC_REALTEK=y",
    "CONFIG_IOMMU_SUPPORT=y",
    "CONFIG_AMD_IOMMU=y",
    "CONFIG_IRQ_REMAP=y",
    "CONFIG_X86_X2APIC=y",
    "CONFIG_WIRELESS=y",
    "CONFIG_CFG80211=y",
    "CONFIG_MAC80211=y",
    "CONFIG_WLAN=y",
    "CONFIG_WLAN_VENDOR_ATH=y",
    "CONFIG_ATH11K=y",
    "CONFIG_ATH11K_PCI=y",
    "CONFIG_CPU_MITIGATIONS=y",
    "CONFIG_IGC=y",
    "CONFIG_ETHTOOL_NETLINK=y",
    "CONFIG_RTC_INTF_DEV=y",
    "CONFIG_RTC_HCTOSYS=y",
    "CONFIG_VIRTUALIZATION=y",
    "CONFIG_KVM=y",
    "CONFIG_KVM_AMD=y",
    "CONFIG_KVM_INTEL=y",
    "CONFIG_EXT4_FS=y",
    "CONFIG_EXT4_USE_FOR_EXT2=y",
    "CONFIG_VFAT_FS=y",
    "CONFIG_FAT_DEFAULT_UTF8=y",
    "CONFIG_EXFAT_FS=y",
    "CONFIG_NLS=y",
    "CONFIG_NLS_CODEPAGE_437=y",
    "CONFIG_NLS_ISO8859_1=y",
    "CONFIG_NLS_UTF8=y",
    "CONFIG_IGB=y",
    "CONFIG_NET_VENDOR_REALTEK=y",
    "CONFIG_R8169=y",
    "CONFIG_USB_NET_DRIVERS=y",
    "CONFIG_USB_USBNET=y",
    "CONFIG_USB_NET_CDCETHER=y",
    "CONFIG_USB_NET_CDC_NCM=y",
    "CONFIG_USB_RTL8152=y",
    "CONFIG_USB_NET_AX88179_178A=y",
    "CONFIG_USB_UAS=y",
    "CONFIG_SND_USB=y",
    "CONFIG_SND_USB_AUDIO=y",
    "CONFIG_UDF_FS=y",
    "# CONFIG_USB_NET_NET1080 is not set",
    "# CONFIG_USB_NET_CDC_SUBSET is not set",
    "# CONFIG_USB_NET_ZAURUS is not set",
    "# CONFIG_KERNEL_BZIP2 is not set",
    "# CONFIG_KERNEL_LZMA is not set",
    "# CONFIG_KERNEL_XZ is not set",
    "# CONFIG_KERNEL_LZO is not set",
    "# CONFIG_KERNEL_LZ4 is not set",
    "# CONFIG_KERNEL_ZSTD is not set",
    // Intel platforms: VT-d, the VMD controller RST mode hides NVMe behind, and
    // the LPSS I2C and pinctrl a touchpad sits on. VT-d stays in legacy mode
    // until an Intel machine has booted the less exercised scalable one.
    "CONFIG_INTEL_IOMMU=y",
    "# CONFIG_INTEL_IOMMU_SCALABLE_MODE_DEFAULT_ON is not set",
    "# CONFIG_INTEL_IOMMU_PERF_EVENTS is not set",
    "CONFIG_VMD=y",
    "CONFIG_X86_INTEL_LPSS=y",
    "CONFIG_MFD_INTEL_LPSS_PCI=y",
    "CONFIG_MFD_INTEL_LPSS_ACPI=y",
    "CONFIG_I2C_DESIGNWARE_PCI=y",
    "CONFIG_I2C_DESIGNWARE_BAYTRAIL=y",
    "CONFIG_PINCTRL_LYNXPOINT=y",
    "CONFIG_PINCTRL_BAYTRAIL=y",
    "CONFIG_PINCTRL_CHERRYVIEW=y",
    "CONFIG_PINCTRL_BROXTON=y",
    "CONFIG_PINCTRL_GEMINILAKE=y",
    "CONFIG_PINCTRL_SUNRISEPOINT=y",
    "CONFIG_PINCTRL_CANNONLAKE=y",
    "CONFIG_PINCTRL_ICELAKE=y",
    "CONFIG_PINCTRL_LAKEFIELD=y",
    "CONFIG_PINCTRL_JASPERLAKE=y",
    "CONFIG_PINCTRL_ELKHARTLAKE=y",
    "CONFIG_PINCTRL_TIGERLAKE=y",
    "CONFIG_PINCTRL_ALDERLAKE=y",
    "CONFIG_PINCTRL_METEORLAKE=y",
    "CONFIG_PINCTRL_METEORPOINT=y",
    "CONFIG_PINCTRL_INTEL_PLATFORM=y",
    "CONFIG_I2C_I801=y",
    "CONFIG_I2C_PIIX4=y",
    // Storage controllers.
    "CONFIG_MMC_SDHCI_ACPI=y",
    "CONFIG_MISC_RTSX_PCI=y",
    "CONFIG_MMC_REALTEK_PCI=y",
    "CONFIG_SCSI_UFSHCD=y",
    "CONFIG_SCSI_UFSHCD_PCI=y",
    "CONFIG_ATA_SFF=y",
    "CONFIG_ATA_BMDMA=y",
    "CONFIG_ATA_PIIX=y",
    "CONFIG_MEGARAID_SAS=y",
    "CONFIG_SCSI_MPT3SAS=y",
    "CONFIG_VMWARE_PVSCSI=y",
    // Wired NICs.
    "CONFIG_NET_VENDOR_BROADCOM=y",
    "CONFIG_TIGON3=y",
    "CONFIG_BNXT=y",
    "CONFIG_NET_VENDOR_ATHEROS=y",
    "CONFIG_ATL1C=y",
    "CONFIG_ALX=y",
    "CONFIG_NET_VENDOR_AQUANTIA=y",
    "CONFIG_AQTION=y",
    "CONFIG_NET_VENDOR_MARVELL=y",
    "CONFIG_SKY2=y",
    "CONFIG_IXGBE=y",
    "CONFIG_I40E=y",
    "CONFIG_VMXNET3=y",
    // USB 1.1 hosts, and companions of older USB 2.0 ones.
    "CONFIG_USB_OHCI_HCD=y",
    "CONFIG_USB_OHCI_HCD_PCI=y",
    "CONFIG_USB_UHCI_HCD=y",
    // Touchpads and common keyboards and receivers.
    "CONFIG_MOUSE_PS2_ELANTECH=y",
    "CONFIG_MOUSE_PS2_ELANTECH_SMBUS=y",
    "CONFIG_MOUSE_PS2_SYNAPTICS_SMBUS=y",
    "CONFIG_MOUSE_ELAN_I2C=y",
    "CONFIG_MOUSE_ELAN_I2C_I2C=y",
    "CONFIG_MOUSE_ELAN_I2C_SMBUS=y",
    "CONFIG_RMI4_CORE=y",
    "CONFIG_RMI4_SMB=y",
    "CONFIG_RMI4_F03=y",
    "CONFIG_RMI4_F11=y",
    "CONFIG_RMI4_F12=y",
    "CONFIG_RMI4_F30=y",
    "CONFIG_HID_RMI=y",
    "CONFIG_LEDS_CLASS_MULTICOLOR=y",
    "CONFIG_HID_LOGITECH=y",
    "CONFIG_HID_LOGITECH_DJ=y",
    "CONFIG_HID_LOGITECH_HIDPP=y",
    "CONFIG_HID_APPLE=y",
    "CONFIG_HID_LENOVO=y",
    "CONFIG_HID_MICROSOFT=y",
    // HDA codecs beyond Realtek and Conexant.
    "CONFIG_SND_HDA_CODEC_ANALOG=y",
    "CONFIG_SND_HDA_CODEC_SIGMATEL=y",
    "CONFIG_SND_HDA_CODEC_VIA=y",
    "CONFIG_SND_HDA_CODEC_CIRRUS=y",
    "CONFIG_SND_HDA_CODEC_CS420X=y",
    "CONFIG_SND_HDA_CODEC_CS8409=y",
    // Wi-Fi beyond the T14s's WCN6855, each with the firmware device-firmware pins
    // for it. Intel from 6000-series up, since DVM, MVM and MLD each drive a range
    // of them; MediaTek's MT7921/22 and MT7925; Realtek's 88 and 89; Qualcomm's
    // QCA6174 and WCN7850. RFKILL stays off: none needs it. They select
    // DEV_COREDUMP, whose root-only dumps of a crashed device last five minutes
    // and which nothing here reads; amdgpu writes them too once it is on.
    "CONFIG_WLAN_VENDOR_INTEL=y",
    "CONFIG_IWLWIFI=y",
    "CONFIG_IWLDVM=y",
    "CONFIG_IWLMVM=y",
    "CONFIG_IWLMLD=y",
    "CONFIG_WLAN_VENDOR_MEDIATEK=y",
    "CONFIG_MT7921E=y",
    "CONFIG_MT7925E=y",
    "CONFIG_WLAN_VENDOR_REALTEK=y",
    "CONFIG_RTW88=y",
    "CONFIG_RTW88_8822CE=y",
    "CONFIG_RTW88_8821CE=y",
    "CONFIG_RTW88_8822BE=y",
    "CONFIG_RTW89=y",
    "CONFIG_RTW89_8852AE=y",
    "CONFIG_RTW89_8852BE=y",
    "CONFIG_RTW89_8852CE=y",
    "CONFIG_ATH10K=y",
    "CONFIG_ATH10K_PCI=y",
    "CONFIG_ATH12K=y",
];

/// The resolved `.config`'s `CONFIG_` lines, sorted: what olddefconfig made
/// of `CONFIG`. An absent symbol is off. UPDATING.md, "Kernel config lock".
const CONFIG_LOCK: &str = include_str!("linux-x86-64.config");

/// The symbol a `CONFIG` line names.
fn config_symbol(line: &str) -> Option<&str> {
    let line = line.strip_prefix("# ").unwrap_or(line);
    line.split([' ', '='])
        .next()
        .filter(|s| s.starts_with("CONFIG_"))
}

/// Drops what allnoconfig wrote for each `CONFIG` symbol and appends `CONFIG`.
fn config_script() -> String {
    let deletes: String = CONFIG
        .iter()
        .filter_map(|line| config_symbol(line))
        .map(|symbol| format!("/^#? *{symbol}[ =]/d; "))
        .collect();
    let writes: String = CONFIG.iter().map(|line| format!(" '{line}'")).collect();
    format!("sed -i -r '{deletes}' .config && printf '%s\\n'{writes} >> .config").replace(
        "@EFI_CMDLINE@",
        &format!("{:?}", crate::shared::efi_default_cmdline()),
    )
}

// linux-x86-64 (Linux 7.3.1): the latest STABLE mainline kernel (not a
// longterm/LTS line), built in the Debian build root (AGENTS.md "Debian build
// root"): Debian's gcc 14.2 as CC and HOSTCC, its binutils, make, flex, bison,
// bc, perl, libelf and gzip, all at /usr/bin. Nothing of the root ships: vmlinux
// and bzImage are freestanding (`-nostdinc`, linked by `ld`, no interpreter),
// and the build-root output gate refuses an output that names the root or a
// non-store interpreter. Three artifacts land in {out}: the uncompressed
// `vmlinux` ELF, the compressed, bootable `bzImage`, and a tiny `initramfs.cpio`
// userland (see BOOTABLE below). bzImage's self-extracting payload is gzip
// (kbuild's `cat vmlinux.bin | $(KGZIP) -n -f -9 > vmlinux.bin.gz`);
// CONFIG_KERNEL_GZIP is pinned so the compressor is always gzip.
//
// BOOTABLE (re #529): the config also turns on the 8250 serial console
// (SERIAL_8250 + SERIAL_8250_CONSOLE, PRINTK/TTY), ELF + #! exec (BINFMT_ELF,
// BINFMT_SCRIPT), and initramfs load (BLK_DEV_INITRD), and the recipe packs a
// tiny EXTERNAL initramfs (gen_init_cpio) holding td's STATIC td-sh, td-init and
// td-util plus a /init that prints a marker on ttyS0 and reboots. A THIRD {out} artifact
// (initramfs.cpio) lands alongside vmlinux/bzImage.
//
// USABLE (re #541): the config also turns on the pseudo-filesystems a real
// userland needs — PROC_FS (/proc), SYSFS (/sys), DEVTMPFS (the /dev filesystem
// that init mounts to populate /dev with ttyS0/console/null/…), and TMPFS
// (/var,/tmp,/run). CONFIG_DEVTMPFS_MOUNT is deliberately NOT managed here: it does not
// auto-mount /dev on an initramfs boot (only when the kernel mounts a real root),
// so the `system-x86-64` init mounts devtmpfs itself; a hand-packed /dev/console
// node still covers init's own stdio before that first sysinit line runs.
// Downstream initramfs are packed by td-util's `gen_init_cpio`, not by this
// kernel's in-tree hostprog, which stays a build-only tool.
//
// READ-ONLY ROOT (re #549): the config also turns on what the two-stage boot
// needs to pivot from the initramfs onto a disk-backed, read-only erofs
// `/td/store` root — the block layer (BLOCK), the PCI bus and virtio-over-PCI
// transport the `-M pc` qemu machine exposes (PCI, VIRTIO, VIRTIO_PCI), the
// virtio block device the erofs image rides (VIRTIO_BLK → /dev/vda), the erofs
// filesystem itself (EROFS_FS with zstd — matching the `td-builder
// mkfs-erofs` writer, #548). Writable state is a direct `/var` mount; `/etc`
// remains on EROFS, so OverlayFS is deliberately absent. BLK_DEV_INITRD stays
// on: the kernel still boots via a minimal initramfs that then mounts and
// switch_roots into the erofs disk (the switch_root itself is #550). The
// behavioural proof that this source-built kernel boots to a real userland is the HOST-SIDE tool
// `td-recipe-eval qemu-boot linux-x86-64` (checks/qemu_boot.rs): it boots the
// bzImage + initramfs under host qemu (TCG) and asserts the userland marker
// reaches the console. It is host-side (not a sandboxed gate check) because a qemu
// boot needs host qemu, which the gate's host-free sandbox hides — see the note
// at the Recipe builder below. In-sandbox CI coverage is the artifact shape
// checks (this recipe's producer rung + linux-x86-64-test).
//
// NETWORKING (link-up + DHCP, re the td-netd bring-up daemon): the config turns
// on the networking stack the static `td-netd` needs — NET (the whole stack),
// INET (IPv4/TCP/UDP, for DHCP over UDP and `reach`/`resolve` over TCP/UDP), UNIX
// (AF_UNIX, the baseline local-socket family), and PACKET (AF_PACKET; td-netd
// itself does NOT use a raw socket — it broadcasts DHCP over a normal UDP socket
// plus an explicit limited-broadcast route — but PACKET is the conventional
// raw-socket family general net tooling expects, so it is enabled once here). The
// NIC drivers ride the SAME menuconfig-parent trap the erofs leaves hit (#549):
// VIRTIO_NET (the qemu user-net `-device virtio-net-pci` NIC) sits under the
// prompted `menuconfig NETDEVICES`, and E1000 (a real Intel NIC, and qemu's
// `-device e1000`) sits under `menuconfig ETHERNET` → `config NET_VENDOR_INTEL`.
// All three parents (NETDEVICES, ETHERNET, NET_VENDOR_INTEL) are forced off by
// allnoconfig and would drop their leaves under olddefconfig, so they are pinned
// =y EXPLICITLY alongside the drivers, and the resolved lock re-checks the
// leaves. The behavioural proof is the host-side `qemu-boot-net` oracle (QEMU
// user-net; td-netd brings the link up, then resolve + reach a host).
//
// Host tools: kconfig's lexer/parser need flex and bison, objtool (force-
// selected on x86_64 7.x) and modpost link libelf through pkg-config, and
// timeconst.h needs bc; the build root carries all of them. Avoided by config
// (audited): openssl (no module signing / trusted keys), pahole (BTF off),
// python and kmod; perl, python3, cpio and rsync are in the root but this
// config runs none of them. INITRAMFS_SOURCE stays "" and IKHEADERS/modules stay off.
// The external initramfs below is packed by td-util's gen_init_cpio, the same
// packer system-x86-64 uses.
pub fn recipe() -> Recipe {
    // A recipe's own source lands in TD_INPUT_MAP under the LOCAL name `{name}-source`
    // (`linux-x86-64-source`), synthesized from the recipe NAME — NOT under its
    // `sourceInput` PIN KEY. When the two differ (this recipe renames the shared
    // `linux-kernel-source` pin locally),
    // steps MUST reference
    // the local `{in:linux-x86-64-source}`; `{in:linux-kernel-source}` is not a map key
    // and expands to nothing ("no input `linux-kernel-source' in TD_INPUT_MAP"). The
    // pin key stays on `.source_input(...)` below, which is what gates/fetches the bytes.
    let mut steps = unpack_into("linux-x86-64-source", "{src}");

    // The EFI initrd table is one-shot loader data; its storage can become
    // kernel text after kexec. Discard it before the common EFI parser clears it.
    steps.push(Step::substitute_text(
        "{src}/arch/x86/platform/efi/quirks.c",
        vec![
            TextEdit::new(
                "\tif (!data->smbios)\n\t\tgoto out_memremap;\n\n",
                "",
                1,
            ),
            TextEdit::new(
                "\t\tif (!efi_guidcmp(guid, SMBIOS_TABLE_GUID))",
                "\t\tif (data->smbios && !efi_guidcmp(guid, SMBIOS_TABLE_GUID))",
                1,
            ),
            TextEdit::new(
                "\t\t/* Do not bother to play with mem attr table across kexec */\n\t\tif (!efi_guidcmp(guid, EFI_MEMORY_ATTRIBUTES_TABLE_GUID))",
                "\t\t/* Neither table survives kexec; the initrd uses boot_params. */\n\t\tif (!efi_guidcmp(guid, EFI_MEMORY_ATTRIBUTES_TABLE_GUID) ||\n\t\t    !efi_guidcmp(guid, LINUX_EFI_INITRD_MEDIA_GUID))",
                1,
            ),
        ],
    ));

    // The embedded cpio must share the external archive's fixed epoch. Upstream
    // swallows a missing date command and otherwise falls back to wall time.
    steps.push(Step::substitute_text(
        "{src}/usr/gen_initramfs.sh",
        vec![TextEdit::new(
            "timestamp=\"$(date -d\"$1\" +%s || :)\"",
            "timestamp=1",
            1,
        )],
    ));

    // The chip's hw2.1 revision requests linux-firmware's hw2.1 names, which
    // are links to hw2.0's files; reading hw2.0's carries each file once.
    steps.push(Step::substitute_text(
        "{src}/drivers/net/wireless/ath/ath11k/core.c",
        vec![TextEdit::new(
            "\t\t\t.dir = \"WCN6855/hw2.1\",",
            "\t\t\t.dir = \"WCN6855/hw2.0\",",
            1,
        )],
    ));
    // Power-up turns the endpoint's ASPM off for the download, and only power-down
    // or a booted firmware turns it back on. A selector has no firmware to start,
    // its probe unwinds without either, and kexec resets no link, so the
    // deployment would inherit the link out of L1 and never restore it.
    steps.push(Step::substitute_text(
        "{src}/drivers/net/wireless/ath/ath11k/pci.c",
        vec![TextEdit::new(
            ATH11K_MHI_FAILED,
            ATH11K_MHI_FAILED_RESTORES_ASPM,
            1,
        )],
    ));
    steps.push(Step::substitute_text(
        "{src}/drivers/net/wireless/ath/ath10k/pci.c",
        vec![TextEdit::new(
            ATH10K_POWER_DOWN,
            ATH10K_POWER_DOWN_RESTORES_ASPM,
            1,
        )],
    ));
    steps.push(Step::substitute_text(
        "{src}/drivers/net/wireless/ath/ath12k/pci.c",
        vec![TextEdit::new(
            ATH12K_MHI_FAILED,
            ATH12K_MHI_FAILED_RESTORES_ASPM,
            1,
        )],
    ));
    // cfg80211 defaults to demanding a signed regulatory.db, and that
    // selects the trusted keyring, whose certs/ tool links openssl, which
    // the build root lacks. No regulatory.db is carried, so the signature
    // would guard nothing; the prompt is reachable only under EXPERT, so
    // the default is flipped here instead.
    steps.push(Step::substitute_text(
        "{src}/net/wireless/Kconfig",
        vec![TextEdit::new(
            "\tbool \"require regdb signature\" if CFG80211_CERTIFICATION_ONUS\n\tdefault y\n",
            "\tbool \"require regdb signature\" if CFG80211_CERTIFICATION_ONUS\n\tdefault n\n",
            1,
        )],
    ));

    // Reproducibility: pin the build identity so mkcompile_h needs no
    // date/whoami/hostname, and SOURCE_DATE_EPOCH stamps any embedded timestamp.
    // Every tool is the build root's, named so a PATH slip cannot pick another.
    let mk = |args: &[&str]| -> Step {
        let mut argv = vec![
            "/usr/bin/make",
            "ARCH=x86_64",
            "CC=/usr/bin/gcc",
            "HOSTCC=/usr/bin/gcc",
            "HOSTPKG_CONFIG=/usr/bin/pkg-config",
            "LEX=/usr/bin/flex",
            "YACC=/usr/bin/bison",
            "SHELL=/usr/bin/bash",
            "CONFIG_SHELL=/usr/bin/bash",
        ];
        argv.extend_from_slice(args);
        Step::run("{src}", &argv)
            .env("PATH", PATH)
            .env("SHELL", SH)
            .env("CONFIG_SHELL", SH)
            .env("SOURCE_DATE_EPOCH", "1")
            .env("KBUILD_BUILD_TIMESTAMP", "Thu Jan  1 00:00:01 UTC 1970")
            .env("KBUILD_BUILD_USER", "td")
            .env("KBUILD_BUILD_HOST", "td")
    };

    // 1) allnoconfig: the smallest kconfig (keeps CONFIG_64BIT=y under ARCH=x86_64),
    //    generated by the HOSTCC-built `conf` — which is compiled from the
    //    flex/bison-generated kconfig lexer/parser, so this first step already
    //    exercises flex + bison.
    steps.push(mk(&["allnoconfig"]));

    // 2) Config deltas over allnoconfig. The ONE required change is the unwinder:
    //    allnoconfig on x86_64 takes the choice default UNWINDER_ORC, which needs
    //    the ORC-generation objtool pass; frame-pointer avoids that (objtool ITSELF
    //    still builds+runs for static-call/uaccess — unavoidable on x86_64, hence
    //    libelf). The remaining pins are DEFENSIVE: allnoconfig already leaves them
    //    off, but pinning off the symbols that would pull in openssl, pahole or
    //    kmod (absent from the build root) keeps an allnoconfig default drift
    //    across sub-versions from failing deep in the build.
    //
    //    READ-ONLY ROOT menuconfig parents (re #549): three of the erofs-root leaves
    //    sit under a PROMPTED `menuconfig` parent that `allnoconfig` forces off, so
    //    the leaf alone is invisible and `olddefconfig` would silently drop it (unlike
    //    e.g. SHMEM behind TMPFS, which has no prompt and keeps its `default y`). The
    //    parents must therefore be set EXPLICITLY: CONFIG_VIRTIO_MENU (gates
    //    VIRTIO_PCI), CONFIG_BLK_DEV (gates VIRTIO_BLK), CONFIG_MISC_FILESYSTEMS
    //    (gates EROFS_FS). Do not drop them as "redundant" — the resolved lock on
    //    the leaves (step 4) reds the whole producer build if a parent goes missing.
    //    (PCI / BLOCK are directly selectable, with no menuconfig parent.)
    //
    //    KEXEC (image-based self-boot): the target boots via a two-hop kexec chain —
    //    a tiny immutable shim kernel+initramfs (loaded by qemu -kernel) mounts the
    //    persistent volume and kexec_file_load(2)s the selected deployment's
    //    bzImage+initramfs. Only CONFIG_KEXEC_FILE (the file-based syscall) + the
    //    KEXEC_CORE it selects are used; CONFIG_KEXEC — the legacy segment-based
    //    kexec_load(2) — is pinned OFF and guarded, since nothing here uses it and it
    //    would only widen the syscall surface (in 7.3.1 KEXEC and KEXEC_FILE are
    //    independent siblings — KEXEC_FILE does NOT depend on KEXEC — so dropping
    //    KEXEC leaves KEXEC_FILE intact, and reboot(LINUX_REBOOT_CMD_KEXEC) runs off
    //    KEXEC_CORE). In 7.3.1 KEXEC_FILE `select`s CRYPTO_LIB_SHA256 + KEXEC_CORE on
    //    its own (verified against kernel/Kconfig.kexec), so NO CRYPTO menuconfig is
    //    needed for kexec itself; dm-crypt below selects CRYPTO separately.
    //    CONFIG_RELOCATABLE=y is REQUIRED: the x86
    //    kexec-bzimage64 loader rejects a non-relocatable image, so a kernel that
    //    boots fine via `-kernel` still fails via kexec — the classic
    //    works-direct/fails-kexec divergence. allnoconfig drops RELOCATABLE despite
    //    its `default y`, so it is set explicitly; enabling it makes RANDOMIZE_BASE
    //    (KASLR, default y) visible, pinned OFF here for a deterministic boot.
    //    KEXEC_SIG/_FORCE are pinned OFF, consistent with the disabled trusted-keyring
    //    policy above (no signature/keyring machinery ships).
    //
    //    BTRFS + LOOP (persistent volume): the single persistent /dev/vda is one
    //    btrfs filesystem holding @var plus the immutable root images — each a
    //    reproducible EROFS blob loop-mounted read-only — so BLK_DEV_LOOP is needed to
    //    mount an EROFS file out of the btrfs. CONFIG_BTRFS_FS=y self-selects its whole
    //    dependency set (CRC32, CRYPTO_LIB_SHA256/BLAKE2B, ZLIB/LZO/ZSTD, RAID6_PQ,
    //    XOR_BLOCKS, XXHASH, FS_IOMAP — verified against fs/btrfs/Kconfig), so do NOT
    //    pin those individually; note CONFIG_LIBCRC32C does NOT exist in 7.3.1 (btrfs
    //    uses the CRC32 lib, not CRYPTO_CRC32C) — a test holding it would red
    //    permanently. BTRFS_FS sits under `if BLOCK` (BLOCK already =y) and
    //    BLK_DEV_LOOP under `menuconfig BLK_DEV` (already =y), so no new menuconfig
    //    parent is required.
    //
    //    BLK_DEV_RAM (live media, td-install/MEDIA.md): a live boot's volatile
    //    volume is a Btrfs filesystem on /dev/ram0, which allocates pages only as
    //    they are written and frees them on discard. BLK_DEV_RAM_COUNT=1 creates
    //    that one device at boot; the live selector sizes it with a
    //    `brd.rd_size=` token, so an installed boot keeps an unused
    //    default-size device holding no memory. Also under `menuconfig BLK_DEV`.
    //    Enabling it changes how an initrd that fails to unpack is logged; see
    //    td-install/DESIGN.md §6 on appended cpio archives.
    //
    //    FIDO USB: explicit prompted parents survive allnoconfig. xHCI PCI
    //    support has no prompt and is checked only after resolution. hidraw
    //    stays root-owned; the token worker validates the FIDO report profile.
    //
    //    DISK ENCRYPTION (td-install/ENCRYPTION.md): MD is the prompted parent
    //    of BLK_DEV_DM; DM_CRYPT selects CRYPTO but not AES-XTS. Keep generic
    //    AES/XTS for CPUs without AES-NI and the accelerated implementation
    //    for ordinary laptops. Unlock must work before the volume is mounted.
    //    td-luks performs header and keyslot cryptography in userspace.
    //    No AF_ALG user API is admitted; dm-crypt retains AES-XTS.
    //
    //    SOFTWARE UI: the first graphical profile writes XRGB8888 through the
    //    virtio-gpu driver's fbdev client and reads QEMU's PS/2 devices through
    //    evdev. Every driver is built in because modules remain forbidden.
    //    VIRTIO_INPUT is the pointer that can reach the edges: QEMU's PS/2 mouse
    //    reports MOTION, and a host cursor at the last column of the window sends
    //    a delta the guest accumulates to somewhere short of it — so the right
    //    and bottom edges are unreachable however far the mouse is pushed. The
    //    virtio tablet reports a PLACE instead, over a span the compositor asks
    //    for with EVIOCGABS. It sits under the VIRTIO_MENU parent already pinned
    //    for the erofs root, and its two dependencies — VIRTIO and INPUT — are
    //    both pinned and guarded above, so it needs no new menuconfig parent and
    //    nothing new below it. PS/2 stays on beside it, since a relative device
    //    is still what an ordinary machine has.
    //    DRM_CLIENT_SELECTION is selected by DRM_VIRTIO_GPU; the explicit pins
    //    on its fbdev client and default keep /dev/fb0 from depending on a
    //    Kconfig default. td-compositor drives the card itself and keeps
    //    /dev/fb0 only as its fallback on a machine without one. The VT/fbcon
    //    console shows until the compositor takes the card and again after it
    //    exits (drm_lastclose); ttyS0 remains /dev/console.
    //
    //    FIRMWARE FRAMEBUFFER (td-install/ENCRYPTION.md "Keyboard console"):
    //    SYSFB_SIMPLEFB offers a UEFI GOP framebuffer as a simple-framebuffer
    //    device and DRM_SIMPLEDRM drives it, so fbcon draws the VT on a display
    //    td has no driver for. sysfb_init is a device initcall in
    //    drivers/firmware, linked after drivers/gpu, and virtio-gpu and i915
    //    disable sysfb when they probe, so where they drive the display
    //    simpledrm never binds and their card stays card0. DRM_EFIDRM and
    //    DRM_VESADRM require SYSFB_SIMPLEFB off; they, and the legacy efifb and
    //    vesafb that would take a mode simple-framebuffer cannot describe, are
    //    guarded unset so simpledrm is the one driver of a UEFI GOP
    //    framebuffer (FB_VGA16 and FB_UVESA, off by allnoconfig, are not
    //    guarded; they serve only non-UEFI boots).
    //
    //    SANDBOXED APPLICATIONS (APPLICATIONS.md §0): the namespace, seccomp and
    //    cgroup symbols td-jail needs. Every one of them is PROMPTED, so allnoconfig
    //    answers it `n` regardless of a `default y` — which is why PID_NS and UTS_NS
    //    are off today despite defaulting on.
    //
    //    SECCOMP is pinned; SECCOMP_FILTER is NOT, and must not be. SECCOMP carries an
    //    explicit `prompt` line above `def_bool y` (arch/Kconfig:664), so allnoconfig
    //    can answer it and a pin is what turns it back on. SECCOMP_FILTER (:681) has
    //    NO prompt at all — it is `def_bool y` on `HAVE_ARCH_SECCOMP_FILTER && SECCOMP
    //    && NET`, so kconfig computes it and a line naming it in this list would be
    //    dropped by olddefconfig rather than honoured. It is held by the lock instead:
    //    the lock is the RESOLVED config, which is where a derived symbol can be
    //    observed at all. NET is already =y above, so the dependency is satisfied —
    //    stated rather than left to be discovered, since seccomp silently depending on
    //    the network stack is not a thing anyone guesses.
    //
    //    NET_NS is already =y and is pinned anyway, because today it arrives as a SIDE
    //    EFFECT: its prompt is invisible at allnoconfig (NET is off), CONFIG_NET=y is
    //    pinned above, and olddefconfig then takes its `default y`. Nothing names it,
    //    so nothing would notice if a future NET rework took it away. The pin makes an
    //    incidental symbol a declared one.
    //
    //    AUDIT is compiled in for one test-only use: the physical-input Firefox
    //    audit boot passes audit=1 and asks td-jail's outer filter to log its
    //    denials. Other direct-kernel QEMU oracles pass audit=0, which prevents
    //    initialization and even unconditional seccomp-kill printk records.
    //    Firmware defaults omit audit policy: an earlier audit=0 cannot be undone
    //    by a caller's later audit=1, even with CMDLINE_OVERRIDE disabled.
    //    AUDITSYSCALL is the x86-derived child that gives those records syscall
    //    identity; like SECCOMP_FILTER it has no independent policy choice and is
    //    guarded only after olddefconfig resolves the dependency graph.
    //
    //    IPC_NS is NOT enabled and is guarded OFF. It is `default y` on `SYSVIPC ||
    //    POSIX_MQUEUE`, both off, so it cannot be enabled alone — and the same
    //    olddefconfig mechanism that handed us NET_NS would hand us an IPC namespace
    //    the moment somebody pins SYSVIPC for an unrelated reason. td-jail omits
    //    CLONE_NEWIPC until that is a decision rather than a side effect.
    //    Audio arrives here with td-audio (§K.4). SOUND/SND/SND_PCM are the core;
    //    SND_HDA_INTEL and SND_HDA_GENERIC are the codec path that covers both
    //    QEMU's `-device intel-hda` and ordinary desktop hardware (since 7.2
    //    SND_HDA_INTEL depends on PCI itself, not on the SND_PCI menu, which stays
    //    pinned as the menu the other PCI sound drivers sit under), and
    //    SND_ALOOP is the loopback device that lets a test capture what was played
    //    without a speaker. SND_HDA itself is NOT pinned: SND_HDA_INTEL selects it
    //    and it has no prompt of its own, so olddefconfig drops a line naming it
    //    and writing one would read as a pin without being one. SND_PCM_OSS is
    //    pinned OFF rather than left to
    //    olddefconfig: §K.4 refuses OSS emulation on the merits ("Choosing a
    //    deprecated compatibility shim to make an unsafe roster smaller is precisely
    //    the bad trade the maintainer's premise forbids"), and a `default y` that
    //    quietly enabled `sound/core/oss/pcm_oss.c` would put that shim in the image
    //    while every td-audio ioctl kept using the native interface. FUSE_FS is
    //    still deliberately absent: it lands with the Documents portal.
    //
    //    ThinkPad T430s drivers arrive ahead of its hardware validation
    //    (td-install/INSTALLER.md): i915, e1000e, thinkpad_acpi with the ACPI
    //    battery/AC/button/thermal/fan/video and backlight drivers, cpufreq and
    //    idle, SUSPEND, EHCI, the Ricoh SDHCI reader and the Conexant and HDMI
    //    codecs. MOUSE_PS2_SYNAPTICS, MOUSE_PS2_TRACKPOINT and
    //    SND_HDA_CODEC_HDMI_INTEL prompt only under EXPERT and USB_EHCI_PCI has no
    //    prompt, so like SND_HDA they are guarded, not written. Unasked: DRM_I915
    //    selects ACPI_WMI, X86_PLATFORM_DEVICES, IOSF_MBI, INTEL_GTT and
    //    SND_HDA_I915; THINKPAD_ACPI selects NVRAM (a root-only /dev/nvram), HWMON
    //    and LEDS; SND_HDA_CODEC_HDMI selects SND_DYNAMIC_MINORS, harmless since
    //    td-audio opens PCMs by name, and defaults in other vendors' HDMI codecs.
    //    SUSPEND brings PM, PM_SLEEP and ACPI_SLEEP into every kernel, QEMU's
    //    too: runtime PM is compiled into each driver and /sys/power/state offers
    //    freeze and mem; td-authd's physical sleep operation writes mem. QEMU binds none of the
    //    laptop's devices, so /dev/fb0 there stays virtio-gpu's.
    //
    //    ThinkPad T14s Gen 4 AMD (Ryzen 7040) drivers arrive the same way
    //    (td-install/INSTALLER.md). It shares thinkpad_acpi, the ACPI
    //    platform set, NVMe, xHCI and the PS/2 keyboard and TrackPoint
    //    above; on top of them come amd-pstate, the AMD ACPI platform
    //    devices with their GPIO controller and DesignWare I2C, HID over
    //    I2C with hid-multitouch for its touchpad (td-compositor/DESIGN.md
    //    reads it as a touchpad from INPUT_PROP_POINTER and the finger tool),
    //    the Realtek codec on the same HDA controller driver, and x2APIC.
    //    X86_AMD_PSTATE was already on, selected by SCHED_MC_PRIO (default y
    //    once SMP is pinned), and I2C_HID by its default once I2C is
    //    visible; both are written so losing that select or default cannot
    //    drop the scaling driver or the touchpad transport unnoticed.
    //    Guarded, not written: I2C_HID_CORE (no prompt, selected by
    //    I2C_HID_ACPI), GPIO_ACPI (no prompt; the touchpad's interrupt and
    //    the platform's GPIO-signalled ACPI events, _AEI), SND_HDA_CODEC_ALC269
    //    (prompting only under EXPERT) and
    //    ACPI_PROCESSOR (the AMD idle states), which the pinned
    //    X86_AMD_PSTATE and X86_INTEL_PSTATE both select.
    //
    //    x2APIC: AMD firmware commonly hands the CPU over in x2APIC mode
    //    with its IOMMU on, and a kernel without X86_X2APIC then turns the
    //    local APIC off entirely (one CPU, no MSI). X86_X2APIC depends on
    //    IRQ_REMAP, which needs only IOMMU_SUPPORT; with no remapping driver
    //    the kernel would switch such a handover back to xAPIC. AMD_IOMMU is
    //    written so remapping actually runs, keeping x2APIC, and so devices
    //    are DMA-isolated by the upstream lazy-flush default. Neither QEMU
    //    machine td boots (pc, q35) has an AMD IOMMU; KVM guests take
    //    x2APIC from KVM_GUEST (CLOUD GUESTS below), not from this.
    //
    //    Unasked: X86_AMD_PLATFORM_DEVICE selects COMMON_CLK (with HAVE_CLK
    //    and PM_CLK's def_bool behind it) and PINCTRL; PINCTRL_AMD selects
    //    GPIOLIB, which brings GPIO_ACPI and GPIO_CDEV (prompting only under
    //    EXPERT: a root-only /dev/gpiochipN), and SERIAL_8250 then selects
    //    SERIAL_MCTRL_GPIO; the Realtek menu defaults in every Realtek codec
    //    family, whose library selects the side-codec component; AMD_IOMMU
    //    selects the IOMMU API, IOVA, SVA, page fault and PCI ATS/PRI/PASID
    //    support. AMDGPU and DC drive Phoenix with its pinned firmware,
    //    including DMCUB for display power management.
    //    AMD_PMC and the CMOS RTC support s2idle entry and wake accounting.
    //    PCIe ASPM follows the firmware policy, without forcing unsupported states.
    //    Its internal microphones sit on
    //    the ACP 6.3 PDM controller, whose SND_SOC_AMD_PS driver and SND_SOC
    //    are not built.
    //
    //    Wi-Fi: the T14s's Qualcomm WCN6855 (QCNFA765) on ath11k over PCI,
    //    with cfg80211 and mac80211 under it, all written. ATH11K_PCI
    //    selects MHI_BUS, QRTR and QRTR_MHI, ATH11K selects
    //    QCOM_QMI_HELPERS, and FW_LOADER prompts only under EXPERT, so those
    //    are guarded. Built in, ath11k probes before any root is mounted,
    //    so linux-firmware's files come from the device-firmware recipe
    //    (AGENTS.md "Device firmware"), and the chip's hw2.1 revision is pointed
    //    at hw2.0's files, which linux-firmware links it to, so each is carried
    //    once. RFKILL stays off. No
    //    regulatory.db is carried: cfg80211 starts from the world domain
    //    and ath11k applies the regulatory domain its firmware reports.
    //    CFG80211_REQUIRE_SIGNED_REGDB's default is flipped off in its
    //    Kconfig (above), since it would select the trusted keyring pinned
    //    off here; guarded with the keyring.
    //    Unasked: WLAN defaults in every vendor's menu (none of their
    //    drivers), and QRTR adds the AF_QIPCRTR socket family.
    //
    //    Ryzen desktop (AM4 Ryzen 7 5700G on a 500-series board): its wired
    //    NIC is the Intel I225-V, driven by IGC under the already pinned
    //    NET_VENDOR_INTEL. IGC depends on PTP_1588_CLOCK_OPTIONAL, which
    //    follows the built-in PTP clock, and on ETHTOOL_NETLINK, prompted but
    //    invisible at allnoconfig (NET is off), so it is written with IGC.
    //    The rest of the machine is the T14s's set: amd-pstate, the AMD
    //    IOMMU, xHCI, NVMe, AHCI and the HDA controller, whose ALC1220 is
    //    bound by the Realtek ALC882 family. CPU_SUP_AMD, SND_HDA_CODEC_ALC882
    //    and SND_HDA_CODEC_HDMI_ATI prompt only under EXPERT, so they are
    //    guarded, not written. Its Cezanne GPU (Renoir family, which
    //    amdgpu names green_sardine) is driven by AMDGPU and DC with the
    //    ten files amdgpu requests for it beside Phoenix's: PSP
    //    ASD and TA, the gfx CE/PFP/ME/MEC/RLC microcode, SDMA, VCN and the
    //    DCN 2.1 DMCUB. MODULE_FIRMWARE also declares green_sardine_mec2,
    //    but gfx v9 skips that request on GC 9.3.0 and reuses MEC, and
    //    Renoir loads no gpu_info file. Every one of these blocks requests
    //    its file in early init, before amdgpu takes the display from the
    //    GOP framebuffer, so a missing file leaves simpledrm driving it
    //    rather than a dark panel. That is every selector boot: only the
    //    deployment initramfs holds these files, and the loader finds them
    //    there because it waits for the initramfs to unpack. ath11k's probe
    //    fails there too, after a bounded wait, with its link put back.
    //    DisplayPort audio follows amdgpu's audio endpoint.
    //
    //    CLOCK (td-time/DESIGN.md): td-time writes the RTC through
    //    /dev/rtc0 (RTC_INTF_DEV). On x86 the boot clock is read from the
    //    CMOS clock whatever RTC_HCTOSYS says; RTC_HCTOSYS is the RTC
    //    class's own seeding at boot and resume. Both are prompted but
    //    invisible at allnoconfig (RTC_CLASS is off there), so they are
    //    written beside the pinned RTC_CLASS.
    //
    //    KVM HOST: VIRTUALIZATION is the prompted menu allnoconfig forces
    //    off; KVM with KVM_AMD and KVM_INTEL builds /dev/kvm, so a td
    //    machine can host QEMU guests at native speed. The node is a
    //    root-only devtmpfs node and stays so: until a device policy hands
    //    it to a session, only root services can create VMs, and td's
    //    integration oracles (DEVELOPMENT.md) still cannot run as the
    //    session account. By default (kvm.enable_virt_at_load) the vendor
    //    modules enable VMX or SVM on every CPU when they load, so a
    //    built-in KVM does that at boot even if no VM is created. CPU_MITIGATIONS (below) is on, and
    //    VMSCAPE, which protects the VMM's userspace from its guests,
    //    is guarded with the other mitigations. KVM_X86 has no prompt and is
    //    guarded.
    //    Unasked: KVM_X86 selects the generic KVM core (EVENTFD, MMU and
    //    preempt notifiers, VHOST_TASK, KVM_VFIO with no VFIO driver,
    //    USER_RETURN_NOTIFIER), SCHED_INFO (/proc/PID/schedstat),
    //    GUEST_PERF_EVENTS, PERF_GUEST_MEDIATED_PMU and KVM_GUEST_MEMFD
    //    (with XARRAY_MULTI), and PERF_EVENTS, already on; KVM_COMPAT
    //    follows IA32_EMULATION; KVM_IOAPIC, KVM_SMM and KVM_HYPERV default
    //    on; KVM_INTEL selects X86_FRED, used only on CPUs that implement
    //    FRED; KVM_AMD_SEV stays off without the PSP crypto driver.
    //
    //    REMOVABLE AND FOREIGN FILESYSTEMS: ext4 for disks other systems
    //    format, VFAT for USB sticks, cameras and EFI system partitions, and
    //    exFAT, which SDXC cards and large sticks carry. FAT reads 8.3 names
    //    through its default codepage 437 and long names through its default
    //    iocharset, iso8859-1, and refuses a mount if either is missing, so
    //    both are built in; FAT_DEFAULT_UTF8 makes long names UTF-8, which
    //    exFAT's utf8 default already is, and NLS_UTF8 serves an explicit
    //    iocharset=utf8. NLS is their prompted parent, already selected by
    //    ACPI, USB, FAT and exFAT among others. EXT4_USE_FOR_EXT2 (prompted,
    //    invisible at allnoconfig) mounts ext2 and ext3 through ext4. None of
    //    these filesystems allows an unprivileged user-namespace mount.
    //    Unasked: EXT4_FS selects JBD2, CRC16, CRC32, FS_IOMAP and
    //    BUFFER_HEAD; VFAT_FS selects FAT_FS; FAT selects LEGACY_DIRECT_IO
    //    and exFAT FS_IOMAP.
    //
    //    DRIVER FAMILIES: td builds no modules, so hardware binds only to a
    //    built-in driver, and allnoconfig starts with none. Rather than one
    //    board's chips at a time, the common families a desktop or laptop
    //    meets are built in whole: Intel IGB and Realtek R8169 (behind
    //    NET_VENDOR_REALTEK) wired NICs; USB Ethernet behind USB_NET_DRIVERS:
    //    usbnet with CDC Ethernet, CDC NCM (docks and tethering) and ASIX
    //    AX88179, and Realtek's RTL8152 family; USB Attached SCSI for USB
    //    SSDs and enclosures; USB audio behind SND_USB (playback: td-seatd
    //    hands the audio account playback nodes only); and UDF for Blu-ray
    //    and UDF media. Only UDF_FS is visible at allnoconfig; the rest sit
    //    behind NET, USB or SND, and several would follow by default, so the
    //    pins hold them as the vendor menus' are held. R8169 and RTL8152 ask
    //    for optional firmware patches, which no pin supplies: the chips run
    //    their ROM's firmware, as without linux-firmware. Defaults that come
    //    with them: IGB_HWMON, and usbnet's ASIX AX8817X (AX88772 adapters,
    //    with PHYLINK and AX88796B_PHY) and RTL8153_ECM. usbnet's
    //    host-to-host cables (NET1080, the CDC subset) and Zaurus links also
    //    default on, and are pinned off: a module-free kernel binds any USB
    //    device to every built-in driver, so a driver nobody uses is surface.
    //    Selected, so not written: IGB's I2C_ALGOBIT; R8169's FW_LOADER,
    //    PHYLIB, REALTEK_PHY (and PHY_PACKAGE) and CRC32; RTL8152's MII and
    //    CRYPTO_LIB_SHA256; AX88179's PHYLIB; UDF's CRC_ITU_T, BUFFER_HEAD
    //    and LEGACY_DIRECT_IO; USB audio's SND_HWDEP, SND_RAWMIDI and
    //    BITREVERSE.
    //
    //    GENERAL HARDWARE: the families at the end of `CONFIG`, for machines td
    //    has not met, each needing no firmware or running without its optional
    //    firmware. Intel laptops: VMD, or VMD-based RST's NVMe disks are invisible
    //    to the installer (AHCI-remapped RST stays a firmware setting to turn
    //    off); the LPSS I2C controllers, I2C_DESIGNWARE_PCI and each client
    //    platform's pinctrl, since a touchpad's interrupt is a GPIO and without
    //    its controller the I2C_HID the T14s brought defers forever. VT-d, so a
    //    machine whose firmware has a DMAR table gets lazy DMA isolation as AMD
    //    ones do and x2APIC survives a handover. It is on where firmware has it
    //    on, so the T430s's i915, e1000e and card reader run translated there.
    //    SMBus controllers carry RMI4 touchpads. UHCI and OHCI serve USB 1.1
    //    keyboards and the companion ports of older USB 2.0 hosts. HID++ reports
    //    a wireless device's battery as a power supply of Device scope, which
    //    the mains check and td-agent's report leave out. Left out, and why:
    //    other vendors' laptop drivers, whose keys nothing reads and which bring
    //    RFKILL, and Intel's HID and virtual-button events, whose keys nothing
    //    reads either; USB4 and Thunderbolt,
    //    whose PCIe tunnels are a DMA path that wants its own decision; SOF audio,
    //    Wi-Fi and Bluetooth, which need firmware; touch behind Intel THC;
    //    native drivers for displays simpledrm already serves; and filesystems
    //    nothing mounts.
    //
    //    CLOUD GUESTS (td-svc/DESIGN.md §10, "Cloud images"): the drivers a
    //    server image needs to find its disk and network on Azure, EC2 Nitro
    //    and GCE. Azure's synthetic devices are on VMBus: storvsc presents the
    //    OS disk as SCSI (sd*), and netvsc is its NIC; its NVMe-only sizes
    //    present the disk controller over Hyper-V vPCI (PCI_HYPERV), as they
    //    do accelerated-networking VFs, whose mlx5 and MANA drivers are not
    //    built, so netvsc's synthetic path carries the traffic. Nitro
    //    presents EBS as NVMe, already built in, and its NIC as ENA. GCE uses
    //    virtio-net, virtio-scsi or NVMe, and gVNIC on the machine series that
    //    require it. HYPERVISOR_GUEST is the one prompted parent allnoconfig
    //    forces off; SCSI_LOWLEVEL and the NIC vendor menus are invisible
    //    there (SCSI and NET are off), so olddefconfig already gave them
    //    their default y, and their pins are defensive. HYPERV selects
    //    PARAVIRT, and KVM_GUEST, which defaults on once PARAVIRT is, is
    //    pinned on purpose: Nitro and GCE are KVM. X86_X2APIC is the T14s's,
    //    above. Hyper-V's interrupt remapping, which a large Azure VM's
    //    x2APIC needs, has had no symbol of its own since 7.2: HYPERV
    //    builds drivers/iommu/hyperv/ and the pinned IRQ_REMAP its
    //    hv-irq-remap-x86.o, so the HYPERV and IRQ_REMAP guards hold it.
    //    HYPERV_TIMER has no prompt and is guarded only. DMIID exports the
    //    firmware's DMI identity in /sys/class/dmi/id, by which a server
    //    tells which cloud's metadata service to ask (td-svc/DESIGN.md §10,
    //    "Cloud metadata"); DMI itself prompts only under EXPERT, so it is
    //    guarded.
    //    Unasked: KVM_GUEST selects PARAVIRT_CLOCK, ARCH_CPUIDLE_HALTPOLL and
    //    X86_HV_CALLBACK_VECTOR and defaults in HALTPOLL_CPUIDLE (CPU_IDLE is
    //    on) with CPU_IDLE_GOV_HALTPOLL, inert without KVM_HINTS_REALTIME,
    //    and PTP_1588_CLOCK_KVM with PTP_1588_CLOCK_VMCLOCK (PTP_1588_CLOCK
    //    already defaults on from ETHERNET); HYPERV selects IRQ_MSI_LIB, and
    //    HYPERV_VMBUS defaults in HYPERV_KEYBOARD and selects the already
    //    enabled SYSFB; PCI_HYPERV selects PCI_HYPERV_INTERFACE; HYPERV_NET
    //    selects NLS and UCS2_STRING; ENA selects DIMLIB and NET_DEVLINK; GVE
    //    selects PAGE_POOL. Every other SCSI_LOWLEVEL driver defaults off. So
    //    QEMU's KVM guests now take kvmclock, x2APIC and paravirtual EOI, and
    //    register /dev/ptp0; TCG guests and the laptops keep their clocks,
    //    and each driver binds only its hypervisor's devices, so no other
    //    device set changes.
    //
    //    COMPAT_32BIT_TIME has a visible prompt, so allnoconfig turns it off.
    //    It and IA32_EMULATION stay on because td-jail's development syscall
    //    policy admits ordinary i386 code beside x86-64 (APPLICATIONS.md §C),
    //    and i386 code that issues the 32-bit time syscalls (older libcs, raw
    //    syscalls) needs COMPAT_32BIT_TIME for its time and futex calls; and the QEMU oracle's seccomp probe runs an
    //    unconfined i386 syscall before proving td-jail's filter kills one
    //    (UNSAFE.md). td-jail-test runs the same probe on the build host
    //    (DEVELOPMENT.md, Host kernel).
    //
    //    EFI needs ACPI; EFI_STUB selects the already enabled RELOCATABLE.
    //
    //    SECURITYFS and SECURITY (both pinned ON) exist only so securityfs
    //    registers: the selector's prediction reads the firmware's TPM event
    //    log from it (td-install/ENCRYPTION.md "Prediction"). TCG_TPM only
    //    implies SECURITYFS, and Linux 7.2 registers securityfs from the LSM
    //    framework's initcall alone, so SECURITYFS without SECURITY builds a
    //    filesystem nothing registers. No security module is built or active:
    //    every LSM and the hook sets are pinned OFF, INTEGRITY (default y
    //    once SECURITY is on) with them, and CONFIG_LSM is empty, so only the
    //    always-first capability module runs, as it does without SECURITY.
    //    Runtime-map support follows KEXEC_CORE. Other EFI menu symbols retain
    //    upstream defaults except the pinned-off efivarfs/handover/mixed modes.
    //
    //    SECURITY_DMESG_RESTRICT (pinned OFF): with it on, an unprivileged
    //    /dev/kmsg open is EPERM, so the /bin/dmesg system-x86-64 ships from
    //    TD_UTIL_APPLETS breaks for ordinary users. Its `menu "Security options"`
    //    parent has no config symbol, so unlike the erofs-root leaves above no
    //    parent pin is needed. A `default` flip is absorbed by the pin; only a
    //    `select` that defeats it reaches the lock's diff, which names the symbol here
    //    rather than letting it surface as a broken applet in the boot oracle.
    //
    //    CPU VULNERABILITY MITIGATIONS: x86 prompts for CPU_MITIGATIONS as a
    //    menuconfig (ARCH_CONFIGURES_CPU_MITIGATIONS), so allnoconfig answers
    //    it `n` despite its `default y`, and with it off every mitigation is
    //    compiled out and the `mitigations=` parameter cannot restore one.
    //    It is pinned on, and each mitigation under it keeps its upstream
    //    default: page-table isolation, retpoline and return thunks, the
    //    AMD UNRET/IBPB/SRSO/TSA and Intel IBRS/call-depth/BHI/MDS/TAA/
    //    MMIO/L1TF/GDS/RFDS/SRBDS/ITS entry, exit and boot-time work,
    //    RETBleed, Spectre v1/v2 and SSB. Each is invisible at allnoconfig
    //    (its menu is off), so they are guarded, not written; the kernel
    //    enables only those the running CPU needs. SLS is left at
    //    upstream's default, off. VMSCAPE, which depends on KVM, is
    //    guarded with the rest. Those that need CPU microcode (IBPB, IBRS, STIBP, VERW
    //    buffer clearing, SRBDS, GDS, SRSO's IBPB forms) take effect only
    //    where the microcode has them: td's deployment initramfs offers the
    //    vendors' current set (AGENTS.md "CPU microcode"), and a selector runs
    //    on what platform firmware supplied.
    //    Unasked: RETPOLINE selects OBJTOOL, already built for x86_64;
    //    CALL_DEPTH_TRACKING selects CALL_THUNKS, whose call padding brings
    //    16-byte function alignment, and HAVE_DYNAMIC_FTRACE_NO_PATCHABLE,
    //    inert with FTRACE off; ITS selects EXECMEM for its thunk pages;
    //    x86 then selects ARCH_HAS_CPU_ATTACK_VECTORS, the attack-vector
    //    controls of `mitigations=`; ADDRESS_MASKING (LAM, unpinned)
    //    becomes unavailable.
    steps.push(Step::run("{src}", &[SH, "-c", &config_script()]).env("PATH", PATH));

    // 3) olddefconfig normalises the choice (takes defaults for newly-visible
    //    symbols, non-interactive).
    steps.push(mk(&["olddefconfig"]));

    // 4) The resolved config is exactly the committed lock, so every default
    //    olddefconfig takes and every symbol a kernel or toolchain bump moves
    //    is a reviewed line. Fail here, before the full build, with the diff.
    steps.push(Step::WriteFile {
        path: "{root}/kernel-config.lock".into(),
        content: CONFIG_LOCK.into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "grep '^CONFIG_' .config | LC_ALL=C sort > {root}/kernel-config.resolved; \
                 diff -u {root}/kernel-config.lock {root}/kernel-config.resolved >&2 || \
                 { echo 'resolved kernel config differs from recipes/src/recipes/linux-x86-64.config: review this diff and apply it (UPDATING.md, \"Kernel config lock\")' >&2; exit 1; }",
            ],
        )
        .env("PATH", PATH),
    );

    // 5) Build vmlinux + the bootable bzImage with the root's binutils and
    //    gzip, the host make env scrubbed.
    //    (`bzImage` builds `vmlinux` as a prerequisite; both are listed so a
    //    kbuild change that stops re-emitting the raw ELF still lands it.)
    steps.push(
        mk(&[
            "-j{jobs}",
            "LD=/usr/bin/ld",
            "AR=/usr/bin/ar",
            "NM=/usr/bin/nm",
            "OBJCOPY=/usr/bin/objcopy",
            "OBJDUMP=/usr/bin/objdump",
            "STRIP=/usr/bin/strip",
            "KGZIP=/usr/bin/gzip",
            "vmlinux",
            "bzImage",
        ])
        .env("MAKEFLAGS", "")
        .env("MFLAGS", "")
        .env("GNUMAKEFLAGS", "")
        .env("MAKELEVEL", ""),
    );

    // ---- Bootable userland: a static td-sh initramfs (re #529) ----
    // The kernel is now serial-console + initramfs capable (the config deltas
    // above add the 8250 console, BINFMT_ELF/SCRIPT, and BLK_DEV_INITRD). Pack a
    // tiny initramfs whose /init prints a marker on ttyS0, optionally mounts an
    // attached erofs disk read-only (re #549) and prints a second marker, then
    // reboots — so the sibling qemu boot checks can prove the source-built
    // kernel both reaches a real userland AND mounts the read-only erofs root. The
    // initramfs is an EXTERNAL artifact (qemu -initrd), keeping bzImage a pure
    // kernel — INITRAMFS_SOURCE stays "".
    //
    // td-util's gen_init_cpio packs the newc cpio from a spec WITHOUT needing
    // mknod privilege: the `nod /dev/console` entry
    // is written straight into the archive, which the unprivileged host-free
    // sandbox could not create on a real filesystem. td-sh, td-init and td-util
    // are STATIC, so the initramfs is self-contained — no glibc closure, no host
    // bytes.

    // The /init the kernel execs (rdinit=/init): a #! script (BINFMT_SCRIPT) that
    // prints the userland marker the boot check greps for, THEN — if a virtio-blk
    // erofs disk is attached (the `qemu-boot-erofs` tool, re #549) — mounts it
    // read-only and prints a second marker on success, then `reboot -f` so qemu
    // (-no-reboot) exits cleanly. echo is a td-sh builtin, so the userland
    // marker prints even if the reboot were unavailable (the boot check's
    // wall-clock ceiling then bounds the run).
    //
    // Applet reachability: this initramfs packs td-sh (as /bin/sh), td-init and
    // td-util and no per-applet symlinks, so every applet is reached explicitly:
    // `mount` and `reboot` as `/bin/td-init <applet>`, `mkdir`, `sleep` and `cat`
    // as `/bin/td-util <applet>`. `echo` and `[` are td-sh builtins.
    //
    // Disk probe (gated on /dev/vda so a diskless boot is unaffected — the plain
    // `qemu-boot` check kills qemu on USERLAND_MARKER, printed first, before this
    // runs): mount devtmpfs so the virtio-blk node appears, settle briefly for the
    // async probe, then mount /dev/vda READ-ONLY as erofs and read the sentinel the
    // image was built with. The sentinel's CONTENT is read back with `cat` and
    // string-compared (not just `test -f`), so a corrupt/missing DATA block cannot
    // green — EROFS_MARKER prints only when the mount AND the data read-back both
    // succeed, making it a true read-only-erofs-mount proof. mount stderr is left on
    // the console (no `2>/dev/null`) so a mount failure is visible in the boot
    // oracle's captured serial log. This is the seed of the #550 two-stage boot,
    // where the `reboot -f` becomes the switch_root into the mounted root.
    steps.push(Step::WriteFile {
        path: "{root}/initramfs/init".into(),
        content: format!(
            "#!/bin/sh\n\
             echo {USERLAND_MARKER}\n\
             /bin/td-util mkdir -p /mnt\n\
             /bin/td-init mount -t devtmpfs dev /dev\n\
             n=0\n\
             while [ \"$n\" -lt 5 ] && ! [ -b /dev/vda ]; do /bin/td-util sleep 1; n=$((n+1)); done\n\
             if [ -b /dev/vda ]; then \
             if /bin/td-init mount -t erofs -o ro /dev/vda /mnt; then \
             if [ \"$(/bin/td-util cat /mnt/{EROFS_PROBE_SENTINEL})\" = {EROFS_PROBE_CONTENT} ]; then echo {EROFS_MARKER}; fi; \
             fi; \
             fi\n\
             exec /bin/td-init reboot -f\n"
        ),
        exec: true,
    });
    // gen_init_cpio spec: /dev/console for init's stdio, static td-sh with a
    // /bin/sh -> td-sh symlink for the #! interpreter, td-init, td-util, and /init.
    steps.push(Step::WriteFile {
        path: "{root}/initramfs/spec".into(),
        content: "dir /dev 0755 0 0\n\
                  nod /dev/console 0600 0 0 c 5 1\n\
                  dir /bin 0755 0 0\n\
                  file /bin/td-sh {in:td-sh}/bin/td-sh 0755 0 0\n\
                  slink /bin/sh /bin/td-sh 0777 0 0\n\
                  file /bin/td-init {in:td-init}/bin/td-init 0755 0 0\n\
                  file /bin/td-util {in:td-util}/bin/td-util 0755 0 0\n\
                  file /init {root}/initramfs/init 0755 0 0\n"
            .into(),
        exec: false,
    });
    // Pack the initramfs. `-t 1` pins every entry's mtime (1s past the epoch):
    // /init is written fresh by this build, so a stat mtime would differ across
    // otherwise-identical builds of this content-addressed artifact.
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "'{in:td-util}/bin/td-util' gen_init_cpio -t 1 {root}/initramfs/spec > {root}/initramfs.cpio",
            ],
        )
        .env("PATH", PATH),
    );

    // Land the uncompressed ELF + its symbol map + the bootable bzImage + the
    // external initramfs.
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{src}/vmlinux".into(),
            "{src}/System.map".into(),
            // What each built-in driver binds: `hwcheck --builtin` reads it to
            // say, from any Linux system, whether this kernel fits that machine.
            "{src}/modules.builtin.modinfo".into(),
            "{src}/arch/x86/boot/bzImage".into(),
            "{root}/initramfs.cpio".into(),
        ],
        dest: "{out}".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/vmlinux".into(),
            "{out}/bzImage".into(),
            "{out}/modules.builtin.modinfo".into(),
            "{out}/initramfs.cpio".into(),
        ],
        exec: false,
    });
    // A kernel without modules holds drivers' device tables here only because
    // modpost writes them for vmlinux too, so a later kbuild could leave a
    // table of a few explicit aliases that reads as no drivers at all.
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "for a in nvme.alias=pci: xhci_pci.alias=pci: usbhid.alias=usb:; do \
                 grep -aqF \"$a\" '{out}/modules.builtin.modinfo' || \
                 { echo \"modules.builtin.modinfo lacks $a: hwcheck --builtin would read as no drivers\" >&2; exit 1; }; done",
            ],
        )
        .env("PATH", PATH),
    );
    // [native-arch] vmlinux must be an ELF64 x86-64 linked executable (EXEC, not a
    // stray relocatable .o) — caught at the producer rung.
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "h=$(/usr/bin/readelf -h '{out}/vmlinux'); \
                 printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || { echo 'vmlinux is not ELF64' >&2; exit 1; }; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo 'vmlinux is not x86-64' >&2; exit 1; }; \
                 printf '%s\\n' \"$h\" | grep -i 'type:'    | grep -qi 'EXEC'   || { echo 'vmlinux is not a linked ELF executable (EXEC)' >&2; exit 1; }",
            ],
        )
        .env("PATH", PATH),
    );
    // [bzImage] the compressed image must carry the x86 boot-setup header: the
    // 0xAA55 boot signature at 0x1fe and the "HdrS" (48 64 72 53) magic at 0x202
    // (arch/x86/boot/header.S), read with od's offset seek.
    // A size floor first rejects a header-only/truncated image that would carry
    // those two constants but no kernel payload (a real allnoconfig bzImage is
    // megabytes; 64 KiB cleanly separates it from a <1 KiB truncation). Asserted
    // here at the producer rung; the sibling test re-checks and also scans for the
    // embedded gzip payload.
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "sz=$(wc -c < '{out}/bzImage'); \
                 [ \"$sz\" -ge 65536 ] || { echo \"bzImage: implausibly small ($sz bytes) — header-only/truncated image\" >&2; exit 1; }; \
                 set -- $(od -An -tx1 -j 510 -N 2 '{out}/bzImage'); \
                 [ \"$1$2\" = 55aa ] || { echo 'bzImage: missing 0xAA55 boot signature at 0x1fe' >&2; exit 1; }; \
                 set -- $(od -An -tx1 -j 514 -N 4 '{out}/bzImage'); \
                 [ \"$1$2$3$4\" = 48647253 ] || { echo 'bzImage: missing HdrS setup-header magic at 0x202' >&2; exit 1; }",
            ],
        )
        .env("PATH", PATH),
    );
    steps.push(Step::AssertEfiApplication {
        path: "{out}/bzImage".into(),
    });
    // [initramfs] the packed userland must be a real, COMPLETE newc cpio carrying
    // the whole bootable userland — not merely a well-formed header. The shared
    // `initramfs_cpio_shape_check` helper (recipes/src/shared.rs) parses the archive
    // with td-util `cpio -t` — a real newc walk that reds on a truncated/corrupt
    // stream and yields the exact member names — then asserts init, bin/td-sh,
    // bin/sh, bin/td-init, bin/td-util and dev/console are all present and the
    // `TD-USERLAND-OK` /init marker is
    // packed. The producer rung and the fast `linux-x86-64-test` tier run the SAME
    // check so they cannot drift. The sibling qemu boot tool is the behavioural proof
    // (it boots this cpio); this is the fast producer-rung shape check.
    let initramfs_check =
        initramfs_cpio_shape_check("{out}/initramfs.cpio", "{in:td-util}/bin/td-util");
    steps.push(Step::run("{out}", &[SH, "-c", &initramfs_check]).env("PATH", PATH));

    // td-sh, td-init and td-util are initramfs contents; td-util also packs it.
    Recipe::mesboot("linux-x86-64", "7.3.1")
        .source_input("linux-kernel-source")
        .build_root()
        .native_inputs(&["td-sh", "td-init", "td-util"])
        .steps(steps)
    // No behavioural boot check is registered here: a qemu boot needs HOST qemu,
    // which the gate's host-free `pivot_root` sandbox deliberately hides (the
    // sandbox exposes only td-built tools by absolute /td/store path — that is why
    // the RustToolchain check can run the td-BUILT rustc, but a host binary like
    // qemu is unreachable there). Wiring the boot as a sandboxed recipe check would
    // make it fail on `find_qemu` on every real runner — a permanently-red, green-
    // washed check. The boot is instead a HOST-SIDE tool, `td-recipe-eval
    // qemu-boot linux-x86-64` (checks/qemu_boot.rs), run OUTSIDE the sandbox by an
    // operator or developer. Automated in-sandbox coverage is the shape checks
    // above (producer rung) and the linux-x86-64-test BuildOnly check, which
    // build the bzImage + initramfs and assert they are well-formed.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What must hold in the resolved lock, and why it matters if not.
    const HELD: &[(&str, &str)] = &[
        ("CONFIG_UNWINDER_FRAME_POINTER=y", "frame-pointer unwinder not selected"),
        ("CONFIG_KERNEL_GZIP=y", "gzip kernel compression not selected (bzImage would need another compressor)"),
        ("CONFIG_BINFMT_ELF=y", "BINFMT_ELF off — the kernel could not exec the initramfs userland"),
        ("CONFIG_BINFMT_SCRIPT=y", "BINFMT_SCRIPT off — the kernel could not exec the #! /init script"),
        ("CONFIG_BLK_DEV_INITRD=y", "BLK_DEV_INITRD off — the kernel could not load the initramfs"),
        ("CONFIG_SERIAL_8250_CONSOLE=y", "8250 serial console off — no ttyS0 boot output for the qemu check"),
        ("CONFIG_DEVTMPFS=y", "DEVTMPFS off - init cannot mount /dev, so the userland has no ttyS0/console devices"),
        ("CONFIG_PROC_FS=y", "PROC_FS off - a usable userland (ps/mount/init) needs /proc"),
        ("CONFIG_SYSFS=y", "SYSFS off - a usable userland needs /sys"),
        ("CONFIG_TMPFS=y", "TMPFS off - a usable userland needs writable /var,/tmp,/run mounts"),
        ("CONFIG_BLOCK=y", "BLOCK off - no block layer for the virtio-blk erofs disk"),
        ("CONFIG_PCI=y", "PCI off - virtio-blk-pci (the -M pc transport) needs the PCI bus"),
        ("CONFIG_PCI_MSI=y", "PCI_MSI off - target PCI profile selects MSI/MSI-X support"),
        ("CONFIG_VIRTIO_PCI=y", "VIRTIO_PCI off - no virtio transport on the -M pc PCI bus"),
        ("CONFIG_VIRTIO_CONSOLE=y", "VIRTIO_CONSOLE off - no td VM bridge"),
        ("CONFIG_VIRTIO_BLK=y", "VIRTIO_BLK off - the erofs disk (/dev/vda) would not appear"),
        ("CONFIG_EROFS_FS_ZIP=y", ""),
        ("CONFIG_EROFS_FS_ZIP_ZSTD=y", ""),
        ("CONFIG_EROFS_FS=y", "EROFS_FS off - the read-only erofs root could not be mounted"),
        ("CONFIG_PRINTK=y", "PRINTK off — no kernel console output"),
        ("CONFIG_TTY=y", "TTY off — the serial console needs the tty layer"),
        ("CONFIG_UNIX98_PTYS=y", "UNIX98_PTYS off — /dev/ptmx would not exist, so td-term could not allocate a pty and the graphical session would come up with no terminal"),
        ("CONFIG_KEXEC_FILE=y", "KEXEC_FILE off — the shim boots the selected deployment via kexec_file_load(2)"),
        ("CONFIG_RELOCATABLE=y", "RELOCATABLE off — a non-relocatable bzImage is rejected by the x86 kexec_file_load loader (boots via -kernel, fails via kexec)"),
        ("CONFIG_BTRFS_FS=y", "BTRFS_FS off — the persistent volume is one btrfs filesystem (@var plus the loop-mounted EROFS root blobs)"),
        ("CONFIG_MD=y", "MD off — device mapper needs its prompted parent enabled"),
        ("CONFIG_BLK_DEV_DM=y", "BLK_DEV_DM off — encrypted volumes need built-in device mapper"),
        ("CONFIG_DM_CRYPT=y", "DM_CRYPT off — encrypted volumes need the built-in crypt target"),
        ("CONFIG_CRYPTO_AES=y", "CRYPTO_AES off — disk encryption needs a generic AES fallback"),
        ("CONFIG_CRYPTO_XTS=y", "CRYPTO_XTS off — disk encryption needs the generic XTS mode"),
        ("CONFIG_CRYPTO_AES_NI_INTEL=y", "CRYPTO_AES_NI_INTEL off — disk encryption needs the x86 accelerated AES-XTS implementation"),
        ("CONFIG_CRYPTO_HMAC=y", "CRYPTO_HMAC off — kernel consumers need HMAC"),
        ("CONFIG_CRYPTO_SHA256=y", "CRYPTO_SHA256 off — kernel consumers need SHA-256"),
        ("CONFIG_BLK_DEV_LOOP=y", "BLK_DEV_LOOP off — the immutable EROFS root is a file inside btrfs, loop-mounted read-only"),
        ("CONFIG_BLK_DEV_RAM=y", "BLK_DEV_RAM off - a live boot keeps its volatile volume on /dev/ram0"),
        ("CONFIG_BLK_DEV_RAM_COUNT=1", "BLK_DEV_RAM_COUNT is not 1 - the live profile uses exactly /dev/ram0"),
        ("CONFIG_SCSI=y", "SCSI off - offline optical/USB installation requires built-in media support"),
        ("CONFIG_BLK_DEV_SD=y", "BLK_DEV_SD off - offline optical/USB installation requires built-in media support"),
        ("CONFIG_BLK_DEV_SR=y", "BLK_DEV_SR off - offline optical/USB installation requires built-in media support"),
        ("CONFIG_ATA=y", "ATA off - offline optical/USB installation requires built-in media support"),
        ("CONFIG_SATA_AHCI=y", "SATA_AHCI off - offline optical/USB installation requires built-in media support"),
        ("CONFIG_BLK_DEV_NVME=y", "BLK_DEV_NVME off - NVMe installation requires its built-in PCI driver"),
        ("CONFIG_NVME_CORE=y", "NVME_CORE off - NVMe installation requires built-in namespace support"),
        ("CONFIG_USB_STORAGE=y", "USB_STORAGE off - offline optical/USB installation requires built-in media support"),
        ("CONFIG_ISO9660_FS=y", "ISO9660_FS off - offline optical/USB installation requires built-in media support"),
        ("CONFIG_NET=y", "NET off — no networking stack for td-netd link-up/DHCP"),
        ("CONFIG_INET=y", "INET off — no IPv4/TCP/UDP for DHCP or resolve/reach"),
        ("CONFIG_PACKET=y", "PACKET off — AF_PACKET raw-socket family unavailable"),
        ("CONFIG_UNIX=y", "UNIX off — AF_UNIX local-socket family unavailable"),
        ("CONFIG_NETDEVICES=y", "NETDEVICES off (menuconfig parent dropped) — no NIC drivers are visible"),
        ("CONFIG_VIRTIO_NET=y", "VIRTIO_NET off — the qemu user-net virtio NIC would not appear"),
        ("CONFIG_E1000=y", "E1000 off (ETHERNET/NET_VENDOR_INTEL menuconfig parents likely dropped) — no real Intel NIC driver"),
        ("CONFIG_INPUT_EVDEV=y", "INPUT_EVDEV off — td-compositor cannot read /dev/input/eventN"),
        ("CONFIG_KEYBOARD_ATKBD=y", "KEYBOARD_ATKBD off — the qemu PS/2 keyboard would not appear"),
        ("CONFIG_MOUSE_PS2=y", "MOUSE_PS2 off — the qemu PS/2 pointer would not appear"),
        ("CONFIG_VIRTIO_INPUT=y", "VIRTIO_INPUT off — the qemu virtio tablet would not appear, so the pointer could not reach the far edges of the screen"),
        ("CONFIG_SERIO_I8042=y", "SERIO_I8042 off — qemu PS/2 input has no controller"),
        ("CONFIG_SYSCALL_USER_DISPATCH=y", "SYSCALL_USER_DISPATCH off — allnoconfig answers n to the prompt Linux 7.3 gave it, removing a prctl applications could use on 7.2"),
        ("CONFIG_VT_CONSOLE=y", "VT_CONSOLE off — the framebuffer recovery console is unavailable"),
        ("CONFIG_FRAMEBUFFER_CONSOLE=y", "FRAMEBUFFER_CONSOLE off — virtio fbdev cannot host the recovery console"),
        ("CONFIG_DRM_VIRTIO_GPU=y", "DRM_VIRTIO_GPU off — qemu virtio-vga cannot expose /dev/fb0"),
        ("CONFIG_DRM_VIRTIO_GPU_KMS=y", "DRM_VIRTIO_GPU_KMS off — virtio-gpu has no scanout"),
        ("CONFIG_DRM_FBDEV_EMULATION=y", "DRM_FBDEV_EMULATION off — td-compositor has no /dev/fb0 software scanout"),
        ("CONFIG_DRM_CLIENT_DEFAULT_FBDEV=y", "DRM_CLIENT_DEFAULT_FBDEV off — the selected virtio DRM client will not create fb0"),
        ("CONFIG_SYSFB_SIMPLEFB=y", "SYSFB_SIMPLEFB off — a UEFI GOP framebuffer is not offered as a simple-framebuffer device, so the selector has no keyboard console on a display td has no driver for"),
        ("CONFIG_DRM_SIMPLEDRM=y", "DRM_SIMPLEDRM off — nothing drives the firmware framebuffer, so fbcon has no display before or without a native driver"),
        ("CONFIG_ACPI=y", "ACPI off - TPM enrollment requires ACPI discovery and TPM2 device support"),
        ("CONFIG_EFI=y", "EFI off - firmware entry requires EFI runtime support"),
        ("CONFIG_CMDLINE_BOOL=y", "EFI built-in command line missing"),
        ("CONFIG_CMDLINE=@EFI_CMDLINE@", "EFI initrd command line mismatch"),
        ("CONFIG_EFI_STUB=y", "EFI_STUB off - bzImage cannot be loaded by UEFI"),
        ("CONFIG_PNP=y", "PNP off - TPM enrollment requires ACPI discovery and TPM2 device support"),
        ("CONFIG_PNPACPI=y", "PNPACPI off - TPM enrollment requires ACPI discovery and TPM2 device support"),
        ("CONFIG_USB_SUPPORT=y", "USB_SUPPORT off - FIDO USB transport requires built-in xHCI and hidraw"),
        ("CONFIG_USB=y", "USB off - FIDO USB transport requires built-in xHCI and hidraw"),
        ("CONFIG_USB_PCI=y", "USB_PCI off - FIDO USB transport requires built-in xHCI and hidraw"),
        ("CONFIG_USB_XHCI_HCD=y", "USB_XHCI_HCD off - FIDO USB transport requires built-in xHCI and hidraw"),
        ("CONFIG_HID_SUPPORT=y", "HID_SUPPORT off - FIDO USB transport requires built-in xHCI and hidraw"),
        ("CONFIG_HID=y", "HID off - FIDO USB transport requires built-in xHCI and hidraw"),
        ("CONFIG_HID_GENERIC=y", "HID_GENERIC off - FIDO USB transport requires built-in xHCI and hidraw"),
        ("CONFIG_UHID=y", "UHID off - secret VM fixture requires kernel HID transport"),
        ("CONFIG_HIDRAW=y", "HIDRAW off - FIDO USB transport requires built-in xHCI and hidraw"),
        ("CONFIG_USB_HID=y", "USB_HID off - FIDO USB transport requires built-in xHCI and hidraw"),
        ("CONFIG_USB_XHCI_PCI=y", "USB_XHCI_PCI off - FIDO USB transport requires built-in xHCI and hidraw"),
        ("CONFIG_TCG_TPM=y", "TCG_TPM off - TPM enrollment requires ACPI discovery and TPM2 device support"),
        ("CONFIG_SECURITYFS=y", "SECURITYFS off - the selector reads the TPM event log through securityfs"),
        ("CONFIG_SECURITY=y", "SECURITY off - Linux 7.2 registers securityfs only from the LSM framework"),
        ("CONFIG_LSM=\"\"", "CONFIG_LSM not empty - no security module may be ordered"),
        ("CONFIG_TCG_TIS=y", "TCG_TIS off - TPM enrollment requires ACPI discovery and TPM2 device support"),
        ("CONFIG_TCG_CRB=y", "TCG_CRB off - TPM enrollment requires ACPI discovery and TPM2 device support"),
        ("CONFIG_PERF_EVENTS=y", "PERF_EVENTS off — td-profiler cannot open its per-CPU software sampling events"),
        ("CONFIG_SMP=y", "SMP missing"),
        ("CONFIG_NR_CPUS=256", "NR_CPUS does not match the VM allocation ceiling"),
        ("CONFIG_HOTPLUG_CPU=y", "HOTPLUG_CPU missing"),
        ("CONFIG_USER_NS=y", "USER_NS off — unshare(CLONE_NEWUSER) returns EINVAL, so td-jail cannot build a sandbox at all"),
        ("CONFIG_PID_NS=y", "PID_NS off — a jailed app would see (and could signal) every process on the machine"),
        ("CONFIG_UTS_NS=y", "UTS_NS off — a jail could not present its own hostname"),
        ("CONFIG_NET_NS=y", "NET_NS off — a jail without shared=network could not be cut off from the network stack"),
        ("CONFIG_AUDIT=y", "AUDIT off — the physical Firefox proof cannot account for outer seccomp denials"),
        ("CONFIG_AUDITSYSCALL=y", "AUDITSYSCALL off — x86 seccomp audit records cannot carry syscall identity"),
        ("CONFIG_COMPAT_32BIT_TIME=y", "COMPAT_32BIT_TIME off — i386 code under the td-jail development policy would lack its futex and clock syscalls"),
        ("CONFIG_IA32_EMULATION=y", "IA32_EMULATION off — the td-jail probe cannot prove confinement kills i386 syscalls"),
        ("CONFIG_SECCOMP=y", "SECCOMP off — seccomp(2) returns ENOSYS, so td-jail ships namespaces with no syscall filter"),
        ("CONFIG_SECCOMP_FILTER=y", "SECCOMP_FILTER off — no BPF syscall filtering. It is unprompted (def_bool y on HAVE_ARCH_SECCOMP_FILTER && SECCOMP && NET), so it cannot be pinned: something took SECCOMP or NET away"),
        ("CONFIG_INOTIFY_USER=y", "INOTIFY_USER off — GLib file monitoring in a jailed app degrades to polling"),
        ("CONFIG_CGROUPS=y", "CGROUPS off — no cgroup v2, so an application has no aggregate CPU, memory, or pid cap"),
        ("CONFIG_CGROUP_SCHED=y", "CGROUP_SCHED off — the cgroup v2 cpu controller does not exist"),
        ("CONFIG_FAIR_GROUP_SCHED=y", "FAIR_GROUP_SCHED off — fair-scheduler cgroup accounting does not exist"),
        ("CONFIG_CFS_BANDWIDTH=y", "CFS_BANDWIDTH off — cpu.max cannot throttle a jailed application"),
        ("CONFIG_MEMCG=y", "MEMCG off — memory.max/memory.high do not exist, so a multi-process app is bounded only per process"),
        ("CONFIG_CGROUP_PIDS=y", "CGROUP_PIDS off — pids.max does not exist, so nothing bounds a fork bomb inside a jail"),
        ("CONFIG_SND_PCM=y", "SND_PCM off — /dev/snd/pcmC*D*p would not exist, so td-audio has no device to open (APPLICATIONS.md K.4)"),
        ("CONFIG_SND_HDA_INTEL=y", "SND_HDA_INTEL off — QEMU -device intel-hda and ordinary desktop codecs both go through it, so /proc/asound/pcm would be empty"),
        ("CONFIG_SND_HDA_GENERIC=y", "SND_HDA_GENERIC off — the HDA controller would bind no codec, so the card would enumerate with no playback device"),
        ("CONFIG_SND_ALOOP=y", "SND_ALOOP off — nothing can capture what was played, so the tone fixture has no in-image oracle"),
        ("CONFIG_PROC_FS=y", "PROC_FS off — td-audio discovers devices through /proc/asound/pcm"),
        ("CONFIG_DRM_I915=y", "DRM_I915 off — the T430s Ivy Bridge GPU has no KMS driver, so td-compositor has no /dev/fb0 there"),
        ("CONFIG_ETHERNET=y", "ETHERNET off — NIC vendor menus stay hidden, so the e1000e driver cannot be chosen"),
        ("CONFIG_NET_VENDOR_INTEL=y", "NET_VENDOR_INTEL off — the Intel NIC drivers stay hidden"),
        ("CONFIG_E1000E=y", "E1000E off — the T430s Intel 82579LM wired NIC has no driver"),
        ("CONFIG_MOUSE_PS2_SYNAPTICS=y", "MOUSE_PS2_SYNAPTICS off — the T430s touchpad is driven as a bare PS/2 mouse"),
        ("CONFIG_MOUSE_PS2_TRACKPOINT=y", "MOUSE_PS2_TRACKPOINT off — the TrackPoint loses its own protocol and settings"),
        ("CONFIG_X86_PLATFORM_DEVICES=y", "X86_PLATFORM_DEVICES off — laptop platform drivers stay hidden, thinkpad_acpi among them"),
        ("CONFIG_ACPI_EC=y", "ACPI_EC off — the embedded controller behind battery, lid and hotkeys is unreachable"),
        ("CONFIG_I2C=y", "I2C off — thinkpad_acpi and i915 depend on the I2C core"),
        ("CONFIG_THINKPAD_ACPI=y", "THINKPAD_ACPI off — ThinkPad hotkeys, fan, LEDs and thermal readings have no driver"),
        ("CONFIG_ACPI_AC=y", "ACPI_AC off — the AC adapter state is unknown"),
        ("CONFIG_ACPI_BATTERY=y", "ACPI_BATTERY off — the laptop battery is not reported"),
        ("CONFIG_ACPI_BUTTON=y", "ACPI_BUTTON off — the power button and lid switch produce no events"),
        ("CONFIG_ACPI_THERMAL=y", "ACPI_THERMAL off — ACPI thermal zones are not monitored"),
        ("CONFIG_ACPI_FAN=y", "ACPI_FAN off — ACPI fan devices are not driven"),
        ("CONFIG_ACPI_VIDEO=y", "ACPI_VIDEO off — the panel backlight has no ACPI control"),
        ("CONFIG_BACKLIGHT_CLASS_DEVICE=y", "BACKLIGHT_CLASS_DEVICE off — the panel backlight cannot be set"),
        ("CONFIG_CPU_FREQ=y", "CPU_FREQ off — the CPU runs at one frequency, on battery as on AC"),
        ("CONFIG_X86_INTEL_PSTATE=y", "X86_INTEL_PSTATE off — Intel CPUs get no P-state scaling driver"),
        ("CONFIG_CPU_IDLE=y", "CPU_IDLE off — idle CPUs enter no deep C-states"),
        ("CONFIG_INTEL_IDLE=y", "INTEL_IDLE off — Intel CPUs get no native idle driver"),
        ("CONFIG_SUSPEND=y", "SUSPEND off — the machine cannot suspend to RAM"),
        ("CONFIG_USB_EHCI_HCD=y", "USB_EHCI_HCD off — a stick in a USB 2.0 port is invisible (the T430s has one EHCI-only port)"),
        ("CONFIG_USB_EHCI_PCI=y", "USB_EHCI_PCI off — PCI EHCI controllers bind no driver"),
        ("CONFIG_MMC=y", "MMC off — the SD card reader has no MMC core"),
        ("CONFIG_MMC_BLOCK=y", "MMC_BLOCK off — SD cards appear as no block device"),
        ("CONFIG_MMC_SDHCI=y", "MMC_SDHCI off — the SDHCI card reader has no driver"),
        ("CONFIG_MMC_SDHCI_PCI=y", "MMC_SDHCI_PCI off — the PCI SDHCI card reader binds no driver"),
        ("CONFIG_MMC_RICOH_MMC=y", "MMC_RICOH_MMC off — the Ricoh reader keeps its separate MMC function, so MMC cards are not seen through SDHCI"),
        ("CONFIG_SND_HDA_CODEC_CONEXANT=y", "SND_HDA_CODEC_CONEXANT off — the T430s Conexant codec falls back to the generic parser"),
        ("CONFIG_SND_HDA_CODEC_HDMI=y", "SND_HDA_CODEC_HDMI off — HDMI and DisplayPort audio codecs stay hidden"),
        ("CONFIG_SND_HDA_CODEC_HDMI_INTEL=y", "SND_HDA_CODEC_HDMI_INTEL off — Intel HDMI and DisplayPort audio has no codec driver"),
        ("CONFIG_DRM_AMDGPU=y", "DRM_AMDGPU off - T14s suspend requires it"),
        ("CONFIG_DRM_AMD_DC=y", "DRM_AMD_DC off - T14s suspend requires it"),
        ("CONFIG_RTC_CLASS=y", "RTC_CLASS off - T14s suspend requires it"),
        ("CONFIG_RTC_DRV_CMOS=y", "RTC_DRV_CMOS off - T14s suspend requires it"),
        ("CONFIG_PCIEPORTBUS=y", "PCIEPORTBUS off - PCIe devices cannot follow firmware power policy"),
        ("CONFIG_PCIEASPM=y", "PCIEASPM off - PCIe devices cannot follow firmware power policy"),
        ("CONFIG_PCIEASPM_DEFAULT=y", "PCIEASPM_DEFAULT off - PCIe devices cannot follow firmware power policy"),
        ("CONFIG_AMD_PMC=y", "AMD_PMC off - T14s suspend requires it"),
        ("CONFIG_X86_AMD_PSTATE=y", "X86_AMD_PSTATE off — AMD CPUs get no P-state scaling driver"),
        ("CONFIG_ACPI_PROCESSOR=y", "ACPI_PROCESSOR off — AMD CPUs get no ACPI idle states and amd-pstate has no CPPC"),
        ("CONFIG_X86_AMD_PLATFORM_DEVICE=y", "X86_AMD_PLATFORM_DEVICE off — the AMD I2C and GPIO controllers are never created from ACPI"),
        ("CONFIG_PINCTRL_AMD=y", "PINCTRL_AMD off — the T14s touchpad interrupt line has no GPIO controller"),
        ("CONFIG_I2C_DESIGNWARE_CORE=y", "I2C_DESIGNWARE_CORE off — the AMD I2C controller behind the T14s touchpad has no driver"),
        ("CONFIG_I2C_DESIGNWARE_PLATFORM=y", "I2C_DESIGNWARE_PLATFORM off — the ACPI-enumerated DesignWare I2C controller binds no driver"),
        ("CONFIG_I2C_HID=y", "I2C_HID off — the HID-over-I2C menu stays hidden, so the T14s touchpad has no transport"),
        ("CONFIG_I2C_HID_ACPI=y", "I2C_HID_ACPI off — the ACPI-enumerated I2C touchpad binds no driver"),
        ("CONFIG_I2C_HID_CORE=y", "I2C_HID_CORE off — HID over I2C has no core"),
        ("CONFIG_HID_MULTITOUCH=y", "HID_MULTITOUCH off — the T14s precision touchpad stays a relative mouse with no finger tool"),
        ("CONFIG_SND_HDA_CODEC_REALTEK=y", "SND_HDA_CODEC_REALTEK off — the T14s Realtek codec falls back to the generic parser"),
        ("CONFIG_SND_HDA_CODEC_ALC269=y", "SND_HDA_CODEC_ALC269 off — the ALC257 family on the T14s has no Realtek fixups"),
        ("CONFIG_GPIO_ACPI=y", "GPIO_ACPI off — the I2C touchpad gets no interrupt and GPIO-signalled ACPI events (_AEI) are lost"),
        ("CONFIG_IOMMU_SUPPORT=y", "IOMMU_SUPPORT off — IRQ_REMAP and the IOMMU drivers stay hidden, so X86_X2APIC cannot be built"),
        ("CONFIG_AMD_IOMMU=y", "AMD_IOMMU off — the AMD IOMMU goes unused: no interrupt remapping (an x2APIC handover falls back to xAPIC) and no DMA isolation"),
        ("CONFIG_IRQ_REMAP=y", "IRQ_REMAP off — X86_X2APIC depends on it"),
        ("CONFIG_X86_X2APIC=y", "X86_X2APIC off — firmware that hands over in x2APIC mode leaves the kernel with its APIC disabled"),
        ("CONFIG_WIRELESS=y", "WIRELESS off — no wireless stack"),
        ("CONFIG_CFG80211=y", "CFG80211 off — no nl80211, so td-netd cannot drive a radio"),
        ("CONFIG_MAC80211=y", "MAC80211 off — ath11k needs the soft-MAC stack"),
        ("CONFIG_WLAN=y", "WLAN off — the wireless driver menu hides ath11k"),
        ("CONFIG_WLAN_VENDOR_ATH=y", "WLAN_VENDOR_ATH off — the Atheros/Qualcomm menu hides ath11k"),
        ("CONFIG_ATH11K=y", "ATH11K off — the T14s WCN6855 Wi-Fi has no driver"),
        ("CONFIG_ATH11K_PCI=y", "ATH11K_PCI off — the WCN6855 is a PCI device"),
        ("CONFIG_FW_LOADER=y", "FW_LOADER off — no firmware reaches ath11k"),
        ("CONFIG_MHI_BUS=y", "MHI_BUS off — ath11k_pci boots the WCN6855 over MHI"),
        ("CONFIG_QRTR=y", "QRTR off — ath11k talks QMI to the WCN6855 over QRTR"),
        ("CONFIG_QRTR_MHI=y", "QRTR_MHI off — QRTR has no transport to the WCN6855"),
        ("CONFIG_QCOM_QMI_HELPERS=y", "QCOM_QMI_HELPERS off — ath11k has no QMI client"),
        ("CONFIG_HYPERVISOR_GUEST=y", "HYPERVISOR_GUEST off — guest support stays hidden, so no hypervisor drivers can be chosen"),
        ("CONFIG_PARAVIRT=y", "PARAVIRT off — Hyper-V and KVM guest support need the paravirt layer"),
        ("CONFIG_KVM_GUEST=y", "KVM_GUEST off — a KVM cloud guest (EC2 Nitro, GCE) has no kvmclock or paravirt EOI"),
        ("CONFIG_HYPERV=y", "HYPERV off — Hyper-V (Azure) guest support is off"),
        ("CONFIG_HYPERV_VMBUS=y", "HYPERV_VMBUS off — the synthetic devices of Azure are on VMBus, so neither its disk nor its NIC appears"),
        ("CONFIG_SCSI_LOWLEVEL=y", "SCSI_LOWLEVEL off — SCSI host drivers stay hidden, so storvsc and virtio-scsi cannot be chosen"),
        ("CONFIG_HYPERV_STORAGE=y", "HYPERV_STORAGE off — the OS disk of an Azure VM (storvsc) is invisible"),
        ("CONFIG_HYPERV_NET=y", "HYPERV_NET off — the synthetic NIC of an Azure VM (netvsc) has no driver"),
        ("CONFIG_PCI_HYPERV=y", "PCI_HYPERV off — an Azure size with an NVMe disk controller presents it over Hyper-V vPCI, so its OS disk is invisible"),
        ("CONFIG_NET_VENDOR_AMAZON=y", "NET_VENDOR_AMAZON off — the Amazon NIC drivers stay hidden"),
        ("CONFIG_ENA_ETHERNET=y", "ENA_ETHERNET off — the ENA NIC of an EC2 Nitro instance has no driver"),
        ("CONFIG_SCSI_VIRTIO=y", "SCSI_VIRTIO off — a GCE persistent disk on virtio-scsi is invisible"),
        ("CONFIG_NET_VENDOR_GOOGLE=y", "NET_VENDOR_GOOGLE off — the Google NIC drivers stay hidden"),
        ("CONFIG_GVE=y", "GVE off — a GCE machine series that requires gVNIC has no network"),
        ("CONFIG_DMI=y", "DMI off — the firmware tables that name a cloud are not scanned"),
        ("CONFIG_DMIID=y", "DMIID off — /sys/class/dmi/id is missing, so a server cannot tell which cloud it runs on"),
        ("CONFIG_HYPERV_TIMER=y", "HYPERV_TIMER off — an Azure VM has no Hyper-V clocksource. It is unprompted (def_bool HYPERV && X86), so something took HYPERV away"),
        ("CONFIG_CPU_MITIGATIONS=y", "CPU_MITIGATIONS off — every CPU vulnerability mitigation is compiled out and mitigations= cannot restore one"),
        ("CONFIG_RTC_INTF_DEV=y", "RTC_INTF_DEV off — /dev/rtc0 is missing, so td-time cannot persist a correction to the RTC"),
        ("CONFIG_RTC_HCTOSYS=y", "RTC_HCTOSYS off — the RTC class does not re-seed the system clock at boot or resume"),
        ("CONFIG_IGC=y", "IGC off — the Intel I225/I226 wired NIC of the Ryzen desktop has no driver"),
        ("CONFIG_ETHTOOL_NETLINK=y", "ETHTOOL_NETLINK off — IGC depends on it"),
        ("CONFIG_CPU_SUP_AMD=y", "CPU_SUP_AMD off — AMD processors lose their detection and quirks, and KVM_AMD cannot be built"),
        ("CONFIG_CPU_SUP_INTEL=y", "CPU_SUP_INTEL off — Intel processors lose their detection, quirks and mitigations"),
        ("CONFIG_SND_HDA_CODEC_ALC882=y", "SND_HDA_CODEC_ALC882 off — the desktop ALC1220 has no Realtek fixups"),
        ("CONFIG_SND_HDA_CODEC_HDMI_ATI=y", "SND_HDA_CODEC_HDMI_ATI off — AMD HDMI and DisplayPort audio has no codec driver"),
        ("CONFIG_VIRTUALIZATION=y", "VIRTUALIZATION off — the KVM menu stays hidden"),
        ("CONFIG_KVM=y", "KVM off — /dev/kvm is missing, so QEMU guests run only under emulation"),
        ("CONFIG_KVM_X86=y", "KVM_X86 off — no vendor KVM module survived resolution"),
        ("CONFIG_KVM_AMD=y", "KVM_AMD off — AMD-V hosts cannot run KVM guests"),
        ("CONFIG_KVM_INTEL=y", "KVM_INTEL off — VT-x hosts cannot run KVM guests"),
        ("CONFIG_EXT4_FS=y", "EXT4_FS off — ext4 disks formatted by other systems cannot be mounted"),
        ("CONFIG_JBD2=y", "JBD2 off — ext4 has no journal layer"),
        ("CONFIG_EXT4_USE_FOR_EXT2=y", "EXT4_USE_FOR_EXT2 off — ext2 and ext3 volumes do not mount through ext4"),
        ("CONFIG_FAT_FS=y", "FAT_FS off — VFAT has no FAT core"),
        ("CONFIG_VFAT_FS=y", "VFAT_FS off — USB sticks and SD cards formatted FAT cannot be mounted"),
        ("CONFIG_FAT_DEFAULT_UTF8=y", "FAT_DEFAULT_UTF8 off — FAT long names are not UTF-8 by default"),
        ("CONFIG_FAT_DEFAULT_CODEPAGE=437", "FAT_DEFAULT_CODEPAGE is not 437 — the built-in codepage would not be the one FAT mounts with"),
        ("CONFIG_FAT_DEFAULT_IOCHARSET=\"iso8859-1\"", "FAT_DEFAULT_IOCHARSET is not iso8859-1 — the built-in iocharset would not be the one FAT mounts with"),
        ("CONFIG_EXFAT_FS=y", "EXFAT_FS off — SDXC cards and large sticks formatted exFAT cannot be mounted"),
        ("CONFIG_EXFAT_DEFAULT_IOCHARSET=\"utf8\"", "EXFAT_DEFAULT_IOCHARSET is not utf8 — exFAT names would need an NLS table"),
        ("CONFIG_NLS=y", "NLS off — FAT and exFAT have no character sets"),
        ("CONFIG_NLS_CODEPAGE_437=y", "NLS_CODEPAGE_437 off — FAT refuses to mount without its default codepage"),
        ("CONFIG_NLS_ISO8859_1=y", "NLS_ISO8859_1 off — VFAT refuses to mount without its default iocharset"),
        ("CONFIG_NLS_UTF8=y", "NLS_UTF8 off — a mount asking iocharset=utf8 fails"),
        ("CONFIG_IGB=y", "IGB off — the Intel 82575/82576/I210/I211/I350 wired NICs many desktop and server boards carry have no driver"),
        ("CONFIG_NET_VENDOR_REALTEK=y", "NET_VENDOR_REALTEK off — R8169 is hidden behind it"),
        ("CONFIG_R8169=y", "R8169 off — the Realtek 8111/8168/8125 wired NICs most consumer boards carry have no driver"),
        ("CONFIG_USB_NET_DRIVERS=y", "USB_NET_DRIVERS off — every USB Ethernet driver is hidden behind it"),
        ("CONFIG_USB_USBNET=y", "USB_USBNET off — the CDC, NCM and AX88179 adapters are drivers on it"),
        ("CONFIG_USB_NET_CDCETHER=y", "USB_NET_CDCETHER off — class-compliant USB Ethernet adapters and phone tethering have no driver"),
        ("CONFIG_USB_NET_CDC_NCM=y", "USB_NET_CDC_NCM off — USB-C docks and NCM tethering have no driver"),
        ("CONFIG_USB_RTL8152=y", "USB_RTL8152 off — Realtek RTL8152/8153/8156 USB and dock Ethernet has no driver"),
        ("CONFIG_USB_NET_AX88179_178A=y", "USB_NET_AX88179_178A off — ASIX AX88179 USB 3 gigabit adapters have no driver"),
        ("CONFIG_USB_UAS=y", "USB_UAS off — USB SSDs and enclosures fall back to slow bulk-only transport"),
        ("CONFIG_SND_USB=y", "SND_USB off — USB audio is hidden behind it"),
        ("CONFIG_SND_USB_AUDIO=y", "SND_USB_AUDIO off — USB headsets and DACs have no driver"),
        ("CONFIG_UDF_FS=y", "UDF_FS off — Blu-ray discs, packet-written DVDs and UDF-formatted media cannot be mounted"),
        ("CONFIG_MITIGATION_PAGE_TABLE_ISOLATION=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_RETPOLINE=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_RETHUNK=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_UNRET_ENTRY=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_CALL_DEPTH_TRACKING=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_IBPB_ENTRY=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_IBRS_ENTRY=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_SRSO=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_GDS=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_RFDS=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_SPECTRE_BHI=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_MDS=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_TAA=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_MMIO_STALE_DATA=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_L1TF=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_RETBLEED=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_SPECTRE_V1=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_SPECTRE_V2=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_SRBDS=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_SSB=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_ITS=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_TSA=y", "its upstream default did not survive resolution, or this kernel renamed it"),
        ("CONFIG_MITIGATION_VMSCAPE=y", "its upstream default did not survive resolution, or this kernel renamed it"),
    ];

    /// What the resolved lock must not turn on, and why.
    const HELD_OFF: &[(&str, &str)] = &[
        (
            "CONFIG_INTEL_IOMMU_SCALABLE_MODE_DEFAULT_ON",
            "VT-d scalable mode on - no Intel machine has booted it",
        ),
        (
            "CONFIG_INTEL_IOMMU_PERF_EVENTS",
            "VT-d PMU on - nothing here asked for it",
        ),
        ("CONFIG_OVERLAY_FS", "OVERLAY_FS on - /etc is immutable and writable state uses direct mounts; no overlay user remains"),
        ("CONFIG_NVME_MULTIPATH", "NVME_MULTIPATH on - installation fixture requires direct controller namespaces"),
        ("CONFIG_SERIAL_8250_MOXA_PCIE", "SERIAL_8250_MOXA_PCIE set — it defaults to SERIAL_8250 since Linux 7.3, and td supports no such card"),
        ("CONFIG_DRM_EFIDRM", "DRM_EFIDRM set — it requires SYSFB_SIMPLEFB off; simpledrm is the one firmware framebuffer driver"),
        ("CONFIG_DRM_VESADRM", "DRM_VESADRM set — it requires SYSFB_SIMPLEFB off; simpledrm is the one firmware framebuffer driver"),
        ("CONFIG_FB_EFI", "FB_EFI set — legacy efifb would take a GOP mode simple-framebuffer cannot describe; simpledrm is the one firmware framebuffer driver"),
        ("CONFIG_FB_VESA", "FB_VESA set — legacy vesafb would take a VBE mode simple-framebuffer cannot describe; simpledrm is the one firmware framebuffer driver"),
        ("CONFIG_KEXEC", "KEXEC (legacy segment-based kexec_load) on — only KEXEC_FILE is used; the legacy syscall is kept off to not widen the surface"),
        ("CONFIG_KEXEC_SIG", "KEXEC_SIG on — would demand a trusted-keyring signature policy td does not ship"),
        ("CONFIG_RANDOMIZE_BASE", "RANDOMIZE_BASE (KASLR) on — pinned off for a deterministic kexec boot"),
        ("CONFIG_MODULES", "MODULES on (would need module tooling)"),
        ("CONFIG_DEBUG_INFO_BTF", "BTF on (would need pahole)"),
        ("CONFIG_SECURITY_DMESG_RESTRICT", "SECURITY_DMESG_RESTRICT on — unprivileged /dev/kmsg reads become EPERM, so the shipped /bin/dmesg breaks for ordinary users"),
        ("CONFIG_CMDLINE_OVERRIDE", "firmware command line must remain usable"),
        ("CONFIG_EFI_HANDOVER_PROTOCOL", "deprecated EFI handover disabled policy missing"),
        ("CONFIG_EFI_MIXED", "32-bit EFI firmware disabled policy missing"),
        ("CONFIG_EFIVAR_FS", "EFI variable filesystem disabled policy missing"),
        ("CONFIG_SECURITY_NETWORK", "SECURITY_NETWORK on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_SECURITY_PATH", "SECURITY_PATH on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_SECURITY_SELINUX", "SECURITY_SELINUX on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_SECURITY_SMACK", "SECURITY_SMACK on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_SECURITY_TOMOYO", "SECURITY_TOMOYO on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_SECURITY_APPARMOR", "SECURITY_APPARMOR on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_SECURITY_LOADPIN", "SECURITY_LOADPIN on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_SECURITY_YAMA", "SECURITY_YAMA on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_SECURITY_SAFESETID", "SECURITY_SAFESETID on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_SECURITY_LOCKDOWN_LSM", "SECURITY_LOCKDOWN_LSM on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_SECURITY_LANDLOCK", "SECURITY_LANDLOCK on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_SECURITY_IPE", "SECURITY_IPE on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_BPF_LSM", "BPF_LSM on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_INTEGRITY", "INTEGRITY on - the LSM framework is built for securityfs alone, with no security module"),
        ("CONFIG_IA32_EMULATION_DEFAULT_DISABLED", "IA32 emulation disabled at boot — the td-jail probe needs compatibility execution"),
        ("CONFIG_X86_X32_ABI", "X86_X32_ABI on — x32 calls could reach an unmodelled compatibility ABI instead of the deliberate kill probe"),
        ("CONFIG_RT_GROUP_SCHED", "RT_GROUP_SCHED on — real-time tasks in the populated root can block enabling the cpu controller, and td does not delegate real-time bandwidth"),
        ("CONFIG_SND_PCM_OSS", "SND_PCM_OSS on — APPLICATIONS.md K.4 refuses the deprecated OSS emulation layer on the merits, and olddefconfig must not bring it back as a default"),
        ("CONFIG_CFG80211_REQUIRE_SIGNED_REGDB", "CFG80211_REQUIRE_SIGNED_REGDB on — it selects the trusted keyring and its openssl-linked certs/ tool"),
        ("CONFIG_SYSTEM_TRUSTED_KEYRING", "SYSTEM_TRUSTED_KEYRING on — a select overrode its pin, and certs/ would need openssl"),
        ("CONFIG_USB_NET_NET1080", "USB_NET_NET1080 on — NetChip host-to-host cables are USB drivers nothing asked for"),
        ("CONFIG_USB_NET_CDC_SUBSET", "USB_NET_CDC_SUBSET on — Belkin and ARM Linux host-to-host links are USB drivers nothing asked for"),
        ("CONFIG_USB_NET_ZAURUS", "USB_NET_ZAURUS on — Sharp Zaurus links are USB drivers nothing asked for"),
        ("CONFIG_IPC_NS", "IPC_NS on — it is default y behind SYSVIPC||POSIX_MQUEUE, so pinning either brings it along unasked; td-jail omits CLONE_NEWIPC and APPLICATIONS.md §0 defers it deliberately"),
    ];

    /// The lock names this exact `CONFIG_X=value` line.
    fn locked(line: &str) -> bool {
        CONFIG_LOCK.lines().any(|held| held == line)
    }

    /// The lock turns this `CONFIG_X` on, at any value.
    fn set(symbol: &str) -> bool {
        let prefix = format!("{symbol}=");
        CONFIG_LOCK.lines().any(|held| held.starts_with(&prefix))
    }

    #[test]
    fn the_lock_holds_what_the_kernel_needs() {
        for (line, why) in HELD {
            let line = line.replace(
                "@EFI_CMDLINE@",
                &format!("{:?}", crate::shared::efi_default_cmdline()),
            );
            assert!(locked(&line), "{line}: {why}");
        }
        for (symbol, why) in HELD_OFF {
            assert!(!set(symbol), "{symbol} on: {why}");
        }
        assert!(
            !CONFIG_LOCK
                .lines()
                .any(|line| line.starts_with("CONFIG_CRYPTO_USER_API")),
            "deprecated AF_ALG user interface enabled"
        );
    }

    #[test]
    fn the_lock_is_sorted_config_lines() {
        let lines: Vec<&str> = CONFIG_LOCK.lines().collect();
        assert!(lines.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(lines.iter().all(|line| line.starts_with("CONFIG_")));
    }

    #[test]
    fn config_decides_each_symbol_once() {
        let mut symbols: Vec<&str> = CONFIG
            .iter()
            .filter_map(|line| config_symbol(line))
            .collect();
        assert_eq!(symbols.len(), CONFIG.len());
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(symbols.len(), CONFIG.len());
        // Each line is one single-quoted shell word in `config_script`.
        assert!(CONFIG.iter().all(|line| !line.contains('\'')));
    }

    /// A pin olddefconfig drops, under a parent that is off or a symbol this
    /// kernel renamed, leaves the lock's diff empty and the driver absent.
    #[test]
    fn every_config_decision_took_effect() {
        let cmdline = format!("{:?}", crate::shared::efi_default_cmdline());
        for line in CONFIG {
            let line = line.replace("@EFI_CMDLINE@", &cmdline);
            match line.strip_prefix("# ") {
                Some(off) => {
                    let symbol = off.trim_end_matches(" is not set");
                    assert!(!set(symbol), "{symbol} is decided off but the lock has it");
                }
                None => assert!(locked(&line), "{line} is decided but the lock lacks it"),
            }
        }
    }

    /// `WriteFile` content passes through the placeholder expander, so a brace
    /// would be rewritten before the comparison.
    #[test]
    fn the_lock_holds_no_placeholder() {
        assert!(!CONFIG_LOCK.contains(['{', '}']));
    }

    /// The ThinkPad T430s's hardware is built in and survives config
    /// resolution: display, wired network, pointing devices, platform and
    /// power, USB 2.0, the SD reader and audio codecs.
    #[test]
    fn t430s_drivers_are_builtin_after_config_resolution() {
        let text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for symbol in [
            "DRM_I915",
            "ETHERNET",
            "NET_VENDOR_INTEL",
            "E1000E",
            "X86_PLATFORM_DEVICES",
            "ACPI_EC",
            "I2C",
            "THINKPAD_ACPI",
            "ACPI_AC",
            "ACPI_BATTERY",
            "ACPI_BUTTON",
            "ACPI_THERMAL",
            "ACPI_FAN",
            "ACPI_VIDEO",
            "BACKLIGHT_CLASS_DEVICE",
            "CPU_FREQ",
            "X86_INTEL_PSTATE",
            "CPU_IDLE",
            "INTEL_IDLE",
            "SUSPEND",
            "USB_EHCI_HCD",
            "MMC",
            "MMC_BLOCK",
            "MMC_SDHCI",
            "MMC_SDHCI_PCI",
            "MMC_RICOH_MMC",
            "SND_HDA_CODEC_CONEXANT",
            "SND_HDA_CODEC_HDMI",
        ] {
            let pin = format!("'CONFIG_{symbol}=y'");
            let stale = format!("/^#? *CONFIG_{symbol}[ =]/d;");
            assert_eq!(text.matches(&stale).count(), 1, "{symbol}");
            assert_eq!(text.matches(&pin).count(), 1, "{symbol}");
            assert!(locked(&format!("CONFIG_{symbol}=y")), "{symbol}");
        }
        // No prompt here, so a written line would read as a pin without
        // being one: guarded only.
        for symbol in [
            "MOUSE_PS2_SYNAPTICS",
            "MOUSE_PS2_TRACKPOINT",
            "USB_EHCI_PCI",
            "SND_HDA_CODEC_HDMI_INTEL",
        ] {
            assert!(
                !text.contains(&format!("CONFIG_{symbol}[ =]/d")),
                "{symbol}"
            );
            assert!(!text.contains(&format!("'CONFIG_{symbol}=y'")), "{symbol}");
            assert!(locked(&format!("CONFIG_{symbol}=y")), "{symbol}");
        }
    }

    /// The T14s Gen 4 AMD's own drivers beside the shared laptop set:
    /// amd-pstate, the AMD I2C/GPIO platform, the I2C touchpad, the
    /// Realtek codec and x2APIC with the interrupt remapping it needs.
    #[test]
    fn t14s_gen4_amd_drivers_are_builtin_after_config_resolution() {
        let text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for symbol in [
            "DRM_AMDGPU",
            "DRM_AMD_DC",
            "RTC_CLASS",
            "RTC_DRV_CMOS",
            "PCIEPORTBUS",
            "PCIEASPM",
            "PCIEASPM_DEFAULT",
            "AMD_PMC",
            "X86_AMD_PSTATE",
            "X86_AMD_PLATFORM_DEVICE",
            "PINCTRL_AMD",
            "I2C_DESIGNWARE_CORE",
            "I2C_DESIGNWARE_PLATFORM",
            "I2C_HID",
            "I2C_HID_ACPI",
            "HID_MULTITOUCH",
            "SND_HDA_CODEC_REALTEK",
            "IOMMU_SUPPORT",
            "AMD_IOMMU",
            "IRQ_REMAP",
            "X86_X2APIC",
        ] {
            let pin = format!("'CONFIG_{symbol}=y'");
            let stale = format!("/^#? *CONFIG_{symbol}[ =]/d;");
            assert_eq!(text.matches(&stale).count(), 1, "{symbol}");
            assert_eq!(text.matches(&pin).count(), 1, "{symbol}");
            assert!(locked(&format!("CONFIG_{symbol}=y")), "{symbol}");
        }
        // Selected by a pinned symbol, or without a visible prompt: guarded
        // only.
        for symbol in [
            "ACPI_PROCESSOR",
            "I2C_HID_CORE",
            "GPIO_ACPI",
            "SND_HDA_CODEC_ALC269",
        ] {
            assert!(
                !text.contains(&format!("CONFIG_{symbol}[ =]/d")),
                "{symbol}"
            );
            assert!(!text.contains(&format!("'CONFIG_{symbol}=y'")), "{symbol}");
            assert!(locked(&format!("CONFIG_{symbol}=y")), "{symbol}");
        }
    }

    /// The Ryzen desktop's I225-V NIC, the RTC interfaces td-time uses, a
    /// KVM host for QEMU guests, the filesystems removable and foreign
    /// disks carry, each with the character sets its default mount options
    /// load, and the common wired, USB Ethernet, UAS, USB audio and UDF
    /// families, with usbnet's unused host-to-host links held off.
    #[test]
    fn desktop_nic_clock_kvm_host_and_removable_filesystems_are_builtin() {
        let steps = recipe().steps.unwrap_or_default();
        let runs = steps
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>();
        let text = runs.join("\n");
        let config = runs
            .iter()
            .find(|run| run.contains("sed -i -r"))
            .cloned()
            .unwrap_or_default();
        assert!(config.contains("'CONFIG_IGC=y'"), "the config step");
        for symbol in [
            "IGC",
            "ETHTOOL_NETLINK",
            "RTC_INTF_DEV",
            "RTC_HCTOSYS",
            "VIRTUALIZATION",
            "KVM",
            "KVM_AMD",
            "KVM_INTEL",
            "EXT4_FS",
            "EXT4_USE_FOR_EXT2",
            "VFAT_FS",
            "FAT_DEFAULT_UTF8",
            "EXFAT_FS",
            "NLS",
            "NLS_CODEPAGE_437",
            "NLS_ISO8859_1",
            "NLS_UTF8",
            "IGB",
            "NET_VENDOR_REALTEK",
            "R8169",
            "USB_NET_DRIVERS",
            "USB_USBNET",
            "USB_NET_CDCETHER",
            "USB_NET_CDC_NCM",
            "USB_RTL8152",
            "USB_NET_AX88179_178A",
            "USB_UAS",
            "SND_USB",
            "SND_USB_AUDIO",
            "UDF_FS",
        ] {
            let pin = format!("'CONFIG_{symbol}=y'");
            let stale = format!("/^#? *CONFIG_{symbol}[ =]/d;");
            assert_eq!(text.matches(&stale).count(), 1, "{symbol}");
            assert_eq!(text.matches(&pin).count(), 1, "{symbol}");
            assert!(locked(&format!("CONFIG_{symbol}=y")), "{symbol}");
        }
        for symbol in ["USB_NET_NET1080", "USB_NET_CDC_SUBSET", "USB_NET_ZAURUS"] {
            let off = format!("# CONFIG_{symbol} is not set");
            assert_eq!(
                text.matches(&format!("/^#? *CONFIG_{symbol}[ =]/d;"))
                    .count(),
                1,
                "{symbol}"
            );
            assert_eq!(text.matches(&format!("'{off}'")).count(), 1, "{symbol}");
            assert!(!set(&format!("CONFIG_{symbol}")), "{symbol}");
        }
        // Unprompted, selected, or prompting only under EXPERT: guarded only.
        for symbol in [
            "CPU_SUP_AMD",
            "CPU_SUP_INTEL",
            "SND_HDA_CODEC_ALC882",
            "SND_HDA_CODEC_HDMI_ATI",
            "KVM_X86",
            "JBD2",
            "FAT_FS",
        ] {
            assert!(
                !text.contains(&format!("CONFIG_{symbol}[ =]/d")),
                "{symbol}"
            );
            assert!(!text.contains(&format!("'CONFIG_{symbol}=y'")), "{symbol}");
            assert!(locked(&format!("CONFIG_{symbol}=y")), "{symbol}");
        }
        // The defaults the built-in tables serve are checked, not written.
        for line in [
            "CONFIG_FAT_DEFAULT_CODEPAGE=437",
            "CONFIG_FAT_DEFAULT_IOCHARSET=\"iso8859-1\"",
            "CONFIG_EXFAT_DEFAULT_IOCHARSET=\"utf8\"",
        ] {
            assert!(!config.contains(&format!("'{line}'")), "{line}");
            assert!(locked(line), "{line}");
        }
        // Modules stay off: every driver above is built in.
        assert!(config.contains("'# CONFIG_MODULES is not set'"));
    }

    /// CPU_MITIGATIONS is the prompted parent allnoconfig turns off; with it
    /// pinned, every mitigation keeps its upstream default and is checked
    /// after resolution rather than written.
    #[test]
    fn cpu_vulnerability_mitigations_are_compiled_in() {
        let text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            text.matches("/^#? *CONFIG_CPU_MITIGATIONS[ =]/d;").count(),
            1
        );
        assert_eq!(text.matches("'CONFIG_CPU_MITIGATIONS=y'").count(), 1);
        assert!(locked("CONFIG_CPU_MITIGATIONS=y"));
        for name in [
            "PAGE_TABLE_ISOLATION",
            "RETPOLINE",
            "RETHUNK",
            "UNRET_ENTRY",
            "CALL_DEPTH_TRACKING",
            "IBPB_ENTRY",
            "IBRS_ENTRY",
            "SRSO",
            "GDS",
            "RFDS",
            "SPECTRE_BHI",
            "MDS",
            "TAA",
            "MMIO_STALE_DATA",
            "L1TF",
            "RETBLEED",
            "SPECTRE_V1",
            "SPECTRE_V2",
            "SRBDS",
            "SSB",
            "ITS",
            "TSA",
            "VMSCAPE",
        ] {
            assert!(locked(&format!("CONFIG_MITIGATION_{name}=y")), "{name}");
        }
        assert!(!text.contains("'CONFIG_MITIGATION_"));
        assert!(!set("CONFIG_MITIGATION_SLS"));
    }

    /// The T14s's Wi-Fi: ath11k built in, with linux-firmware's files
    /// carried by the device-firmware recipe.
    #[test]
    fn wcn6855_wifi_is_builtin_with_its_firmware_outside_the_kernel() {
        let recipe = recipe();
        assert_eq!(recipe.inputs, None, "the kernel names no firmware");
        let steps = recipe.steps.unwrap_or_default();
        let text = steps
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("EXTRA_FIRMWARE"));
        assert!(locked("CONFIG_EXTRA_FIRMWARE=\"\""));
        for symbol in [
            "WIRELESS",
            "CFG80211",
            "MAC80211",
            "WLAN",
            "WLAN_VENDOR_ATH",
            "ATH11K",
            "ATH11K_PCI",
        ] {
            let stale = format!("/^#? *CONFIG_{symbol}[ =]/d;");
            assert_eq!(text.matches(&stale).count(), 1, "{symbol}");
            assert_eq!(text.matches(&format!("'CONFIG_{symbol}=y'")).count(), 1);
            assert!(locked(&format!("CONFIG_{symbol}=y")));
        }
        assert!(!set("CONFIG_CFG80211_REQUIRE_SIGNED_REGDB"));
        // None of the source patches fails the build when absent: hw2.1 silicon
        // would ask for files nothing carries, the keyring would return, and a
        // failed selector probe would keep Wi-Fi's link out of L1 in the deployment.
        // A linux-firmware tag bump must re-check that WHENCE still links
        // hw2.1's files to hw2.0's.
        for (file, from, to) in [
            (
                "{src}/drivers/net/wireless/ath/ath11k/pci.c",
                ATH11K_MHI_FAILED,
                ATH11K_MHI_FAILED_RESTORES_ASPM,
            ),
            (
                "{src}/drivers/net/wireless/ath/ath10k/pci.c",
                ATH10K_POWER_DOWN,
                ATH10K_POWER_DOWN_RESTORES_ASPM,
            ),
            (
                "{src}/drivers/net/wireless/ath/ath12k/pci.c",
                ATH12K_MHI_FAILED,
                ATH12K_MHI_FAILED_RESTORES_ASPM,
            ),
            (
                "{src}/drivers/net/wireless/ath/ath11k/core.c",
                "\t\t\t.dir = \"WCN6855/hw2.1\",",
                "\t\t\t.dir = \"WCN6855/hw2.0\",",
            ),
            (
                "{src}/net/wireless/Kconfig",
                "\tbool \"require regdb signature\" if CFG80211_CERTIFICATION_ONUS\n\tdefault y\n",
                "\tbool \"require regdb signature\" if CFG80211_CERTIFICATION_ONUS\n\tdefault n\n",
            ),
        ] {
            assert!(
                steps.iter().any(|step| matches!(
                    step,
                    Step::SubstituteText { file: f, edits }
                        if f == file
                            && edits.len() == 1
                            && edits.iter().all(|e| e.from == from && e.to == to && e.expect == 1)
                )),
                "{file}"
            );
        }
        assert!(!set("CONFIG_SYSTEM_TRUSTED_KEYRING"));
        for symbol in [
            "FW_LOADER",
            "MHI_BUS",
            "QRTR",
            "QRTR_MHI",
            "QCOM_QMI_HELPERS",
        ] {
            assert!(!text.contains(&format!("'CONFIG_{symbol}=y'")), "{symbol}");
            assert!(locked(&format!("CONFIG_{symbol}=y")));
        }
    }

    /// td-svc/DESIGN.md §10 "Cloud images": the disk and NIC drivers of
    /// Azure, EC2 Nitro and GCE are built in and survive config resolution.
    #[test]
    fn cloud_guest_drivers_are_builtin_after_config_resolution() {
        let text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for symbol in [
            "HYPERVISOR_GUEST",
            "PARAVIRT",
            "KVM_GUEST",
            "HYPERV",
            "HYPERV_VMBUS",
            "SCSI_LOWLEVEL",
            "HYPERV_STORAGE",
            "HYPERV_NET",
            "PCI_HYPERV",
            "NET_VENDOR_AMAZON",
            "ENA_ETHERNET",
            "SCSI_VIRTIO",
            "NET_VENDOR_GOOGLE",
            "GVE",
            "DMIID",
        ] {
            let pin = format!("'CONFIG_{symbol}=y'");
            let stale = format!("/^#? *CONFIG_{symbol}[ =]/d;");
            assert_eq!(text.matches(&stale).count(), 1, "{symbol}");
            assert_eq!(text.matches(&pin).count(), 1, "{symbol}");
            assert!(locked(&format!("CONFIG_{symbol}=y")), "{symbol}");
        }
        // No prompt: guarded only.
        assert!(!text.contains("CONFIG_HYPERV_TIMER[ =]/d"));
        assert!(!text.contains("'CONFIG_HYPERV_TIMER=y'"));
        assert!(locked("CONFIG_HYPERV_TIMER=y"));
        assert!(!text.contains("'CONFIG_DMI=y'"));
        assert!(locked("CONFIG_DMI=y"));
        // EC2 presents EBS as NVMe; x2APIC is pinned for the T14s.
        assert!(text.contains("'CONFIG_BLK_DEV_NVME=y'"));
        assert!(locked("CONFIG_X86_X2APIC=y"));
    }

    #[test]
    fn installation_media_drivers_are_builtin_after_config_resolution() {
        let text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for symbol in [
            "SCSI",
            "BLK_DEV_SD",
            "BLK_DEV_SR",
            "ATA",
            "SATA_AHCI",
            "USB_STORAGE",
            "ISO9660_FS",
            "BLK_DEV_NVME",
            "NVME_CORE",
            "PCI_MSI",
        ] {
            assert!(text.contains(&format!("'CONFIG_{symbol}=y'")));
            assert!(locked(&format!("CONFIG_{symbol}=y")));
        }
        assert!(text.contains("'# CONFIG_NVME_MULTIPATH is not set'"));
        assert!(!set("CONFIG_NVME_MULTIPATH"));
    }

    /// ENCRYPTION.md "Keyboard console": simpledrm over sysfb's
    /// simple-framebuffer is the one UEFI GOP framebuffer driver, and the
    /// built-in command line puts the VT before the serial console.
    #[test]
    fn firmware_framebuffer_and_vt_console_prefix_are_pinned_and_checked() {
        let text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for symbol in ["SYSFB_SIMPLEFB", "DRM_SIMPLEDRM"] {
            let stale = format!("/^#? *CONFIG_{symbol}[ =]/d;");
            assert_eq!(text.matches(&stale).count(), 1, "{symbol}");
            assert_eq!(
                text.matches(&format!("'CONFIG_{symbol}=y'")).count(),
                1,
                "{symbol}"
            );
            assert!(locked(&format!("CONFIG_{symbol}=y")), "{symbol}");
        }
        // Unavailable beside SYSFB_SIMPLEFB (EFIDRM, VESADRM) or off by
        // allnoconfig (efifb, vesafb): never written, refused if set.
        for symbol in ["DRM_EFIDRM", "DRM_VESADRM", "FB_EFI", "FB_VESA"] {
            assert!(!text.contains(&format!("'CONFIG_{symbol}=")), "{symbol}");
            assert!(!set(&format!("CONFIG_{symbol}")), "{symbol}");
        }
        let cmdline = format!("{:?}", crate::shared::efi_default_cmdline());
        assert!(cmdline.contains("console=tty0 console=ttyS0,115200"));
        assert!(text.contains(&format!("'CONFIG_CMDLINE={cmdline}'")));
        assert!(locked(&format!("CONFIG_CMDLINE={cmdline}")));
    }

    #[test]
    fn exactly_one_builtin_ram_disk_is_pinned_and_checked_after_resolution() {
        let text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for pin in ["'CONFIG_BLK_DEV_RAM=y'", "'CONFIG_BLK_DEV_RAM_COUNT=1'"] {
            assert!(text.contains(pin), "{pin} missing");
        }
        for stale in ["CONFIG_BLK_DEV_RAM[ =]/d", "CONFIG_BLK_DEV_RAM_COUNT[ =]/d"] {
            assert!(text.contains(stale), "{stale} missing");
        }
        assert!(locked("CONFIG_BLK_DEV_RAM=y"));
        // Anchored, so a resolved count of 10 or 16 cannot satisfy it.
        assert!(locked("CONFIG_BLK_DEV_RAM_COUNT=1"));
    }

    #[test]
    fn tpm_discovery_and_drivers_are_pinned_and_checked_after_resolution() {
        let text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for symbol in ["ACPI", "PNP", "PNPACPI", "TCG_TPM", "TCG_TIS", "TCG_CRB"] {
            assert!(
                text.contains(&format!("'CONFIG_{symbol}=y'")),
                "{symbol} missing pin"
            );
            assert!(
                locked(&format!("CONFIG_{symbol}=y")),
                "{symbol} missing resolved guard"
            );
        }
    }

    #[test]
    fn securityfs_takes_the_lsm_framework_and_no_security_module() {
        let text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for symbol in ["SECURITYFS", "SECURITY"] {
            assert!(
                text.contains(&format!("'CONFIG_{symbol}=y'")),
                "{symbol} pin"
            );
            assert!(locked(&format!("CONFIG_{symbol}=y")), "{symbol} lock");
        }
        assert!(text.contains("'CONFIG_LSM=\"\"'"));
        assert!(locked("CONFIG_LSM=\"\""));
        for symbol in [
            "SECURITY_NETWORK",
            "SECURITY_PATH",
            "SECURITY_SELINUX",
            "SECURITY_SMACK",
            "SECURITY_TOMOYO",
            "SECURITY_APPARMOR",
            "SECURITY_LOADPIN",
            "SECURITY_YAMA",
            "SECURITY_SAFESETID",
            "SECURITY_LOCKDOWN_LSM",
            "SECURITY_LANDLOCK",
            "SECURITY_IPE",
            "BPF_LSM",
            "INTEGRITY",
        ] {
            assert!(
                text.contains(&format!("'# CONFIG_{symbol} is not set'")),
                "{symbol} pin"
            );
            assert!(!set(&format!("CONFIG_{symbol}")), "{symbol} lock");
        }
    }

    #[test]
    fn profiler_perf_events_and_smp_hotplug_are_builtin() {
        let command_text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for required in [
            "CONFIG_PERF_EVENTS=y",
            "CONFIG_SMP=y",
            "CONFIG_NR_CPUS=256",
            "CONFIG_HOTPLUG_CPU=y",
        ] {
            assert!(
                command_text.contains(required) && locked(required),
                "kernel profiler contract omitted {required}"
            );
        }
    }

    #[test]
    fn firefox_outer_filter_audit_is_compiled_but_boot_opt_in() {
        let command_text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for required in [
            "/^#? *CONFIG_AUDIT[ =]/d",
            "CONFIG_AUDIT=y",
            "/^#? *CONFIG_COMPAT_32BIT_TIME[ =]/d",
            "CONFIG_COMPAT_32BIT_TIME=y",
            "/^#? *CONFIG_IA32_EMULATION[ =]/d",
            "CONFIG_IA32_EMULATION=y",
            "/^#? *CONFIG_IA32_EMULATION_DEFAULT_DISABLED[ =]/d",
            "# CONFIG_IA32_EMULATION_DEFAULT_DISABLED is not set",
            "/^#? *CONFIG_X86_X32_ABI[ =]/d",
            "# CONFIG_X86_X32_ABI is not set",
        ] {
            assert!(
                command_text.contains(required),
                "kernel Firefox audit contract omitted {required}"
            );
        }
        for line in [
            "CONFIG_AUDIT=y",
            "CONFIG_AUDITSYSCALL=y",
            "CONFIG_COMPAT_32BIT_TIME=y",
            "CONFIG_IA32_EMULATION=y",
        ] {
            assert!(locked(line), "{line}");
        }
        assert!(!set("CONFIG_IA32_EMULATION_DEFAULT_DISABLED"));
        assert!(!set("CONFIG_X86_X32_ABI"));
        assert_eq!(command_text.matches("'CONFIG_AUDITSYSCALL=y'").count(), 0);
    }

    #[test]
    fn fair_scheduler_bandwidth_is_builtin_and_rt_groups_are_off() {
        let command_text = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for required in [
            "CONFIG_CGROUP_SCHED=y",
            "CONFIG_FAIR_GROUP_SCHED=y",
            "CONFIG_CFS_BANDWIDTH=y",
            "# CONFIG_RT_GROUP_SCHED is not set",
        ] {
            assert!(
                command_text.contains(required),
                "kernel CPU bandwidth contract omitted {required}"
            );
        }
        assert!(locked("CONFIG_CFS_BANDWIDTH=y"));
        assert!(!set("CONFIG_RT_GROUP_SCHED"));
    }
}
