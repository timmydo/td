use crate::shared::{
    initramfs_cpio_shape_check, unpack_into, BUILD_ROOT_PATH as PATH, BUILD_ROOT_SH as SH,
    EROFS_MARKER, EROFS_PROBE_CONTENT, EROFS_PROBE_SENTINEL, USERLAND_MARKER,
};
use crate::types::{Recipe, Step, TextEdit};

/// The build-tree directory EXTRA_FIRMWARE_DIR names, relative to the
/// kernel source tree, so the image names no store path.
const FIRMWARE_DIR: &str = "td-firmware";
/// The build root's cp, which may write into the build tree.
const CP: &str = "/usr/bin/cp";

/// linux-firmware's WCN6855 files ath11k requests at probe (AGENTS.md
/// "Device firmware"), each by its pin and the name the driver asks for.
pub const FIRMWARE: &[(&str, &str)] = &[
    (
        "linux-firmware-ath11k-wcn6855-amss-source",
        "ath11k/WCN6855/hw2.0/amss.bin",
    ),
    (
        "linux-firmware-ath11k-wcn6855-m3-source",
        "ath11k/WCN6855/hw2.0/m3.bin",
    ),
    (
        "linux-firmware-ath11k-wcn6855-board-2-source",
        "ath11k/WCN6855/hw2.0/board-2.bin",
    ),
    (
        "linux-firmware-ath11k-wcn6855-regdb-source",
        "ath11k/WCN6855/hw2.0/regdb.bin",
    ),
];

// linux-x86-64 (Linux 7.2.12): the latest STABLE mainline kernel (not a
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
// =y EXPLICITLY alongside the drivers, and the grep-verify guard re-checks the
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

    // Device firmware, unmodified, where EXTRA_FIRMWARE_DIR finds it. The
    // engine's copyFile writes only into the output, so the build root's cp
    // places it in the build tree.
    steps.push(Step::MkDir {
        path: format!("{{src}}/{FIRMWARE_DIR}/ath11k/WCN6855/hw2.0"),
    });
    for (pin, name) in FIRMWARE {
        let from = format!("{{in:{pin}}}");
        let to = format!("{{src}}/{FIRMWARE_DIR}/{name}");
        steps.push(Step::run("{src}", &[CP, "--", &from, &to]));
    }
    // The chip's hw2.1 revision requests linux-firmware's hw2.1 names, which
    // are links to hw2.0's files; reading hw2.0's embeds each file once.
    steps.push(Step::substitute_text(
        "{src}/drivers/net/wireless/ath/ath11k/core.c",
        vec![TextEdit::new(
            "\t\t\t.dir = \"WCN6855/hw2.1\",",
            "\t\t\t.dir = \"WCN6855/hw2.0\",",
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
    //    (gates EROFS_FS). Do not drop them as "redundant" — the grep-verify guard on
    //    the leaves (step 4) reds the whole producer build if a parent goes missing.
    //    (PCI / BLOCK are directly selectable, with no menuconfig parent.)
    //
    //    KEXEC (image-based self-boot): the target boots via a two-hop kexec chain —
    //    a tiny immutable shim kernel+initramfs (loaded by qemu -kernel) mounts the
    //    persistent volume and kexec_file_load(2)s the selected deployment's
    //    bzImage+initramfs. Only CONFIG_KEXEC_FILE (the file-based syscall) + the
    //    KEXEC_CORE it selects are used; CONFIG_KEXEC — the legacy segment-based
    //    kexec_load(2) — is pinned OFF and guarded, since nothing here uses it and it
    //    would only widen the syscall surface (in 7.2.12 KEXEC and KEXEC_FILE are
    //    independent siblings — KEXEC_FILE does NOT depend on KEXEC — so dropping
    //    KEXEC leaves KEXEC_FILE intact, and reboot(LINUX_REBOOT_CMD_KEXEC) runs off
    //    KEXEC_CORE). In 7.2.12 KEXEC_FILE `select`s CRYPTO_LIB_SHA256 + KEXEC_CORE on
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
    //    pin those individually; note CONFIG_LIBCRC32C does NOT exist in 7.2.12 (btrfs
    //    uses the CRC32 lib, not CRYPTO_CRC32C) — a guard on it would red the build
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
    //    cryptsetup's kernel crypto backend reaches PBKDF2 and keyslot
    //    encryption through AF_ALG, so the hash and skcipher user APIs and
    //    SHA-256 (which registers hmac(sha256) itself) are built in, with the
    //    generic HMAC template for other hashes. td formats only AES-XTS with
    //    SHA-256 and imports no foreign volume, so no other cipher or hash is
    //    built in for it. td-jail's socket-family filter keeps confined
    //    applications from AF_ALG. This enables no mapping, enrollment or
    //    encrypted installation.
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
    //    explicit `prompt` line above `def_bool y` (arch/Kconfig:647), so allnoconfig
    //    can answer it and a pin is what turns it back on. SECCOMP_FILTER (:664) has
    //    NO prompt at all — it is `def_bool y` on `HAVE_ARCH_SECCOMP_FILTER && SECCOMP
    //    && NET`, so kconfig computes it and a line naming it in this list would be
    //    dropped by olddefconfig rather than honoured. It is guarded below instead:
    //    the guard is over the RESOLVED config, which is where a derived symbol can be
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
    //    freeze and mem, which nothing in td writes. QEMU binds none of the
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
    //    support. Its Radeon is not driven: amdgpu needs device firmware
    //    no reviewed pin carries yet, so its display is simpledrm's GOP
    //    framebuffer, as on any UEFI machine. AMD_PMC, which s2idle needs to
    //    reach its deepest state, is left out with RTC_CLASS behind it,
    //    since nothing in td enters s2idle. Its internal microphones sit on
    //    the ACP 6.3 PDM controller, whose SND_SOC_AMD_PS driver and SND_SOC
    //    are not built.
    //
    //    Wi-Fi: the T14s's Qualcomm WCN6855 (QCNFA765) on ath11k over PCI,
    //    with cfg80211 and mac80211 under it, all written. ATH11K_PCI
    //    selects MHI_BUS, QRTR and QRTR_MHI, ATH11K selects
    //    QCOM_QMI_HELPERS, and FW_LOADER prompts only under EXPERT, so those
    //    are guarded. Built in, ath11k probes before any root is mounted,
    //    so linux-firmware's files are embedded with EXTRA_FIRMWARE from a
    //    build-tree directory (AGENTS.md "Device firmware"), and the chip's
    //    hw2.1 revision is pointed at hw2.0's files, which linux-firmware
    //    links it to, so each is embedded once. RFKILL stays off. No
    //    regulatory.db is carried: cfg80211 starts from the world domain
    //    and ath11k applies the regulatory domain its firmware reports.
    //    CFG80211_REQUIRE_SIGNED_REGDB's default is flipped off in its
    //    Kconfig (above), since it would select the trusted keyring pinned
    //    off here; guarded with the keyring.
    //    Unasked: WLAN defaults in every vendor's menu (none of their
    //    drivers), and QRTR adds the AF_QIPCRTR socket family.
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
    //    `select` that defeats it reaches the guard, which names the symbol here
    //    rather than letting it surface as a broken applet in the boot oracle.
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                &"sed -i -r \
                 '/^#? *CONFIG_UNWINDER_ORC[ =]/d; \
                  /^#? *CONFIG_UNWINDER_FRAME_POINTER[ =]/d; \
                  /^#? *CONFIG_MODULES[ =]/d; \
                  /^#? *CONFIG_MODULE_SIG[ =]/d; \
                  /^#? *CONFIG_SYSTEM_TRUSTED_KEYRING[ =]/d; \
                  /^#? *CONFIG_SYSTEM_TRUSTED_KEYS[ =]/d; \
                  /^#? *CONFIG_DEBUG_INFO_BTF[ =]/d; \
                  /^#? *CONFIG_FTRACE[ =]/d; \
                  /^#? *CONFIG_GCC_PLUGINS[ =]/d; \
                  /^#? *CONFIG_IKHEADERS[ =]/d; \
                  /^#? *CONFIG_KERNEL_GZIP[ =]/d; \
                  /^#? *CONFIG_KERNEL_BZIP2[ =]/d; \
                  /^#? *CONFIG_KERNEL_LZMA[ =]/d; \
                  /^#? *CONFIG_KERNEL_XZ[ =]/d; \
                  /^#? *CONFIG_KERNEL_LZO[ =]/d; \
                  /^#? *CONFIG_KERNEL_LZ4[ =]/d; \
                  /^#? *CONFIG_KERNEL_ZSTD[ =]/d; \
                  /^#? *CONFIG_BINFMT_ELF[ =]/d; \
                  /^#? *CONFIG_BINFMT_SCRIPT[ =]/d; \
                  /^#? *CONFIG_BLK_DEV_INITRD[ =]/d; \
                  /^#? *CONFIG_SERIAL_8250[ =]/d; \
                  /^#? *CONFIG_SERIAL_8250_CONSOLE[ =]/d; \
                  /^#? *CONFIG_INITRAMFS_SOURCE[ =]/d; \
                  /^#? *CONFIG_PROC_FS[ =]/d; \
                  /^#? *CONFIG_SYSFS[ =]/d; \
                  /^#? *CONFIG_DEVTMPFS[ =]/d; \
                  /^#? *CONFIG_UNIX98_PTYS[ =]/d; \
                  /^#? *CONFIG_TMPFS[ =]/d; \
                  /^#? *CONFIG_BLOCK[ =]/d; \
                  /^#? *CONFIG_BLK_DEV[ =]/d; \
                  /^#? *CONFIG_PCI[ =]/d; \
                  /^#? *CONFIG_PCI_MSI[ =]/d; \
                  /^#? *CONFIG_VIRTIO[ =]/d; \
                  /^#? *CONFIG_VIRTIO_MENU[ =]/d; \
                  /^#? *CONFIG_VIRTIO_PCI[ =]/d; \
                  /^#? *CONFIG_VIRTIO_CONSOLE[ =]/d; \
                  /^#? *CONFIG_VIRTIO_BLK[ =]/d; \
                  /^#? *CONFIG_MISC_FILESYSTEMS[ =]/d; \
                  /^#? *CONFIG_EROFS_FS[ =]/d; \
                  /^#? *CONFIG_EROFS_FS_ZIP[ =]/d; \
                  /^#? *CONFIG_EROFS_FS_ZIP_ZSTD[ =]/d; \
                  /^#? *CONFIG_KEXEC[ =]/d; \
                  /^#? *CONFIG_KEXEC_FILE[ =]/d; \
                  /^#? *CONFIG_KEXEC_SIG[ =]/d; \
                  /^#? *CONFIG_KEXEC_SIG_FORCE[ =]/d; \
                  /^#? *CONFIG_RELOCATABLE[ =]/d; \
                  /^#? *CONFIG_RANDOMIZE_BASE[ =]/d; \
                  /^#? *CONFIG_BLK_DEV_LOOP[ =]/d; \
                  /^#? *CONFIG_BLK_DEV_RAM[ =]/d; \
                  /^#? *CONFIG_BLK_DEV_RAM_COUNT[ =]/d; \
                  /^#? *CONFIG_BTRFS_FS[ =]/d; \
                  /^#? *CONFIG_SCSI[ =]/d; \
                  /^#? *CONFIG_BLK_DEV_SD[ =]/d; \
                  /^#? *CONFIG_BLK_DEV_SR[ =]/d; \
                  /^#? *CONFIG_ATA[ =]/d; \
                  /^#? *CONFIG_SATA_AHCI[ =]/d; \
                  /^#? *CONFIG_BLK_DEV_NVME[ =]/d; \
                  /^#? *CONFIG_NVME_CORE[ =]/d; \
                  /^#? *CONFIG_NVME_MULTIPATH[ =]/d; \
                  /^#? *CONFIG_USB_STORAGE[ =]/d; \
                  /^#? *CONFIG_ISO9660_FS[ =]/d; \
                  /^#? *CONFIG_MD[ =]/d; \
                  /^#? *CONFIG_BLK_DEV_DM[ =]/d; \
                  /^#? *CONFIG_DM_CRYPT[ =]/d; \
                  /^#? *CONFIG_CRYPTO_AES[ =]/d; \
                  /^#? *CONFIG_CRYPTO_XTS[ =]/d; \
                  /^#? *CONFIG_CRYPTO_AES_NI_INTEL[ =]/d; \
                  /^#? *CONFIG_CRYPTO_HMAC[ =]/d; \
                  /^#? *CONFIG_CRYPTO_SHA256[ =]/d; \
                  /^#? *CONFIG_CRYPTO_USER_API_HASH[ =]/d; \
                  /^#? *CONFIG_CRYPTO_USER_API_SKCIPHER[ =]/d; \
                  /^#? *CONFIG_NET[ =]/d; \
                  /^#? *CONFIG_PACKET[ =]/d; \
                  /^#? *CONFIG_UNIX[ =]/d; \
                  /^#? *CONFIG_INET[ =]/d; \
                  /^#? *CONFIG_NETDEVICES[ =]/d; \
                  /^#? *CONFIG_VIRTIO_NET[ =]/d; \
                  /^#? *CONFIG_ETHERNET[ =]/d; \
                  /^#? *CONFIG_NET_VENDOR_INTEL[ =]/d; \
                  /^#? *CONFIG_E1000[ =]/d; \
                  /^#? *CONFIG_HYPERVISOR_GUEST[ =]/d; \
                  /^#? *CONFIG_PARAVIRT[ =]/d; \
                  /^#? *CONFIG_KVM_GUEST[ =]/d; \
                  /^#? *CONFIG_HYPERV[ =]/d; \
                  /^#? *CONFIG_HYPERV_VMBUS[ =]/d; \
                  /^#? *CONFIG_SCSI_LOWLEVEL[ =]/d; \
                  /^#? *CONFIG_HYPERV_STORAGE[ =]/d; \
                  /^#? *CONFIG_HYPERV_NET[ =]/d; \
                  /^#? *CONFIG_PCI_HYPERV[ =]/d; \
                  /^#? *CONFIG_NET_VENDOR_AMAZON[ =]/d; \
                  /^#? *CONFIG_ENA_ETHERNET[ =]/d; \
                  /^#? *CONFIG_SCSI_VIRTIO[ =]/d; \
                  /^#? *CONFIG_NET_VENDOR_GOOGLE[ =]/d; \
                  /^#? *CONFIG_GVE[ =]/d; \
                  /^#? *CONFIG_DMIID[ =]/d; \
                  /^#? *CONFIG_INPUT[ =]/d; \
                  /^#? *CONFIG_INPUT_EVDEV[ =]/d; \
                  /^#? *CONFIG_INPUT_KEYBOARD[ =]/d; \
                  /^#? *CONFIG_KEYBOARD_ATKBD[ =]/d; \
                  /^#? *CONFIG_INPUT_MOUSE[ =]/d; \
                  /^#? *CONFIG_MOUSE_PS2[ =]/d; \
                  /^#? *CONFIG_VIRTIO_INPUT[ =]/d; \
                  /^#? *CONFIG_SERIO[ =]/d; \
                  /^#? *CONFIG_SERIO_I8042[ =]/d; \
                  /^#? *CONFIG_VT[ =]/d; \
                  /^#? *CONFIG_VT_CONSOLE[ =]/d; \
                  /^#? *CONFIG_HW_CONSOLE_BINDING[ =]/d; \
                  /^#? *CONFIG_FB[ =]/d; \
                  /^#? *CONFIG_FRAMEBUFFER_CONSOLE[ =]/d; \
                  /^#? *CONFIG_DRM[ =]/d; \
                  /^#? *CONFIG_DRM_VIRTIO_GPU[ =]/d; \
                  /^#? *CONFIG_DRM_VIRTIO_GPU_KMS[ =]/d; \
                  /^#? *CONFIG_DRM_FBDEV_EMULATION[ =]/d; \
                  /^#? *CONFIG_DRM_CLIENT_DEFAULT_FBDEV[ =]/d; \
                  /^#? *CONFIG_SYSFB_SIMPLEFB[ =]/d; \
                  /^#? *CONFIG_DRM_SIMPLEDRM[ =]/d; \
                  /^#? *CONFIG_SECURITY_DMESG_RESTRICT[ =]/d; \
                  /^#? *CONFIG_ACPI[ =]/d; \
                  /^#? *CONFIG_CMDLINE_BOOL[ =]/d; \
                  /^#? *CONFIG_CMDLINE[ =]/d; \
                  /^#? *CONFIG_CMDLINE_OVERRIDE[ =]/d; \
                  /^#? *CONFIG_EFI[ =]/d; \
                  /^#? *CONFIG_EFI_STUB[ =]/d; \
                  /^#? *CONFIG_EFI_HANDOVER_PROTOCOL[ =]/d; \
                  /^#? *CONFIG_EFI_MIXED[ =]/d; \
                  /^#? *CONFIG_EFIVAR_FS[ =]/d; \
                  /^#? *CONFIG_PNP[ =]/d; \
                  /^#? *CONFIG_PNPACPI[ =]/d; \
                  /^#? *CONFIG_USB_SUPPORT[ =]/d; \
                  /^#? *CONFIG_USB[ =]/d; \
                  /^#? *CONFIG_USB_PCI[ =]/d; \
                  /^#? *CONFIG_USB_XHCI_HCD[ =]/d; \
                  /^#? *CONFIG_HID_SUPPORT[ =]/d; \
                  /^#? *CONFIG_HID[ =]/d; \
                  /^#? *CONFIG_HID_GENERIC[ =]/d; \
                  /^#? *CONFIG_UHID[ =]/d; \
                  /^#? *CONFIG_HIDRAW[ =]/d; \
                  /^#? *CONFIG_USB_HID[ =]/d; \
                  /^#? *CONFIG_TCG_TPM[ =]/d; \
                  /^#? *CONFIG_SECURITYFS[ =]/d; \
                  /^#? *CONFIG_SECURITY[ =]/d; \
                  /^#? *CONFIG_LSM[ =]/d; \
                  /^#? *CONFIG_SECURITY_NETWORK[ =]/d; \
                  /^#? *CONFIG_SECURITY_PATH[ =]/d; \
                  /^#? *CONFIG_SECURITY_SELINUX[ =]/d; \
                  /^#? *CONFIG_SECURITY_SMACK[ =]/d; \
                  /^#? *CONFIG_SECURITY_TOMOYO[ =]/d; \
                  /^#? *CONFIG_SECURITY_APPARMOR[ =]/d; \
                  /^#? *CONFIG_SECURITY_LOADPIN[ =]/d; \
                  /^#? *CONFIG_SECURITY_YAMA[ =]/d; \
                  /^#? *CONFIG_SECURITY_SAFESETID[ =]/d; \
                  /^#? *CONFIG_SECURITY_LOCKDOWN_LSM[ =]/d; \
                  /^#? *CONFIG_SECURITY_LANDLOCK[ =]/d; \
                  /^#? *CONFIG_SECURITY_IPE[ =]/d; \
                  /^#? *CONFIG_BPF_LSM[ =]/d; \
                  /^#? *CONFIG_INTEGRITY[ =]/d; \
                  /^#? *CONFIG_TCG_TIS[ =]/d; \
                  /^#? *CONFIG_TCG_CRB[ =]/d; \
                  /^#? *CONFIG_TCG_TPM2_HMAC[ =]/d; \
                  /^#? *CONFIG_HW_RANDOM_TPM[ =]/d; \
                  /^#? *CONFIG_PERF_EVENTS[ =]/d; \
                  /^#? *CONFIG_HOTPLUG_CPU[ =]/d; \
                  /^#? *CONFIG_SMP[ =]/d; \
                  /^#? *CONFIG_NR_CPUS[ =]/d; \
                  /^#? *CONFIG_USER_NS[ =]/d; \
                  /^#? *CONFIG_PID_NS[ =]/d; \
                  /^#? *CONFIG_UTS_NS[ =]/d; \
                  /^#? *CONFIG_NET_NS[ =]/d; \
                  /^#? *CONFIG_AUDIT[ =]/d; \
                  /^#? *CONFIG_COMPAT_32BIT_TIME[ =]/d; \
                  /^#? *CONFIG_IA32_EMULATION[ =]/d; \
                  /^#? *CONFIG_IA32_EMULATION_DEFAULT_DISABLED[ =]/d; \
                  /^#? *CONFIG_X86_X32_ABI[ =]/d; \
                  /^#? *CONFIG_SECCOMP[ =]/d; \
                  /^#? *CONFIG_INOTIFY_USER[ =]/d; \
                  /^#? *CONFIG_CGROUPS[ =]/d; \
                  /^#? *CONFIG_CGROUP_SCHED[ =]/d; \
                  /^#? *CONFIG_FAIR_GROUP_SCHED[ =]/d; \
                  /^#? *CONFIG_CFS_BANDWIDTH[ =]/d; \
                  /^#? *CONFIG_RT_GROUP_SCHED[ =]/d; \
                  /^#? *CONFIG_MEMCG[ =]/d; \
                  /^#? *CONFIG_CGROUP_PIDS[ =]/d; \
                  /^#? *CONFIG_SOUND[ =]/d; \
                  /^#? *CONFIG_SND[ =]/d; \
                  /^#? *CONFIG_SND_PCM[ =]/d; \
                  /^#? *CONFIG_SND_PCM_OSS[ =]/d; \
                  /^#? *CONFIG_SND_PCI[ =]/d; \
                  /^#? *CONFIG_SND_HDA_INTEL[ =]/d; \
                  /^#? *CONFIG_SND_HDA_GENERIC[ =]/d; \
                  /^#? *CONFIG_DRM_I915[ =]/d; \
                  /^#? *CONFIG_E1000E[ =]/d; \
                  /^#? *CONFIG_X86_PLATFORM_DEVICES[ =]/d; \
                  /^#? *CONFIG_ACPI_EC[ =]/d; \
                  /^#? *CONFIG_I2C[ =]/d; \
                  /^#? *CONFIG_THINKPAD_ACPI[ =]/d; \
                  /^#? *CONFIG_ACPI_AC[ =]/d; \
                  /^#? *CONFIG_ACPI_BATTERY[ =]/d; \
                  /^#? *CONFIG_ACPI_BUTTON[ =]/d; \
                  /^#? *CONFIG_ACPI_THERMAL[ =]/d; \
                  /^#? *CONFIG_ACPI_FAN[ =]/d; \
                  /^#? *CONFIG_ACPI_VIDEO[ =]/d; \
                  /^#? *CONFIG_BACKLIGHT_CLASS_DEVICE[ =]/d; \
                  /^#? *CONFIG_CPU_FREQ[ =]/d; \
                  /^#? *CONFIG_X86_INTEL_PSTATE[ =]/d; \
                  /^#? *CONFIG_CPU_IDLE[ =]/d; \
                  /^#? *CONFIG_INTEL_IDLE[ =]/d; \
                  /^#? *CONFIG_SUSPEND[ =]/d; \
                  /^#? *CONFIG_USB_EHCI_HCD[ =]/d; \
                  /^#? *CONFIG_MMC[ =]/d; \
                  /^#? *CONFIG_MMC_BLOCK[ =]/d; \
                  /^#? *CONFIG_MMC_SDHCI[ =]/d; \
                  /^#? *CONFIG_MMC_SDHCI_PCI[ =]/d; \
                  /^#? *CONFIG_MMC_RICOH_MMC[ =]/d; \
                  /^#? *CONFIG_SND_HDA_CODEC_CONEXANT[ =]/d; \
                  /^#? *CONFIG_SND_HDA_CODEC_HDMI[ =]/d; \
                  /^#? *CONFIG_X86_AMD_PSTATE[ =]/d; \
                  /^#? *CONFIG_X86_AMD_PLATFORM_DEVICE[ =]/d; \
                  /^#? *CONFIG_PINCTRL_AMD[ =]/d; \
                  /^#? *CONFIG_I2C_DESIGNWARE_CORE[ =]/d; \
                  /^#? *CONFIG_I2C_DESIGNWARE_PLATFORM[ =]/d; \
                  /^#? *CONFIG_I2C_HID[ =]/d; \
                  /^#? *CONFIG_I2C_HID_ACPI[ =]/d; \
                  /^#? *CONFIG_HID_MULTITOUCH[ =]/d; \
                  /^#? *CONFIG_SND_HDA_CODEC_REALTEK[ =]/d; \
                  /^#? *CONFIG_IOMMU_SUPPORT[ =]/d; \
                  /^#? *CONFIG_AMD_IOMMU[ =]/d; \
                  /^#? *CONFIG_IRQ_REMAP[ =]/d; \
                  /^#? *CONFIG_X86_X2APIC[ =]/d; \
                  /^#? *CONFIG_WIRELESS[ =]/d; \
                  /^#? *CONFIG_CFG80211[ =]/d; \
                  /^#? *CONFIG_MAC80211[ =]/d; \
                  /^#? *CONFIG_WLAN[ =]/d; \
                  /^#? *CONFIG_WLAN_VENDOR_ATH[ =]/d; \
                  /^#? *CONFIG_ATH11K[ =]/d; \
                  /^#? *CONFIG_ATH11K_PCI[ =]/d; \
                  /^#? *CONFIG_EXTRA_FIRMWARE[ =]/d; \
                  /^#? *CONFIG_EXTRA_FIRMWARE_DIR[ =]/d; \
                  /^#? *CONFIG_SND_ALOOP[ =]/d' .config && \
                 printf '%s\\n' \
                   'CONFIG_UNWINDER_FRAME_POINTER=y' \
                   '# CONFIG_UNWINDER_ORC is not set' \
                   '# CONFIG_MODULES is not set' \
                   '# CONFIG_MODULE_SIG is not set' \
                   '# CONFIG_SYSTEM_TRUSTED_KEYRING is not set' \
                   'CONFIG_SYSTEM_TRUSTED_KEYS=\"\"' \
                   '# CONFIG_DEBUG_INFO_BTF is not set' \
                   '# CONFIG_FTRACE is not set' \
                   '# CONFIG_GCC_PLUGINS is not set' \
                   '# CONFIG_IKHEADERS is not set' \
                   'CONFIG_KERNEL_GZIP=y' \
                   'CONFIG_BINFMT_ELF=y' \
                   'CONFIG_BINFMT_SCRIPT=y' \
                   'CONFIG_BLK_DEV_INITRD=y' \
                   'CONFIG_SERIAL_8250=y' \
                   'CONFIG_SERIAL_8250_CONSOLE=y' \
                   'CONFIG_INITRAMFS_SOURCE=\"\"' \
                   'CONFIG_PROC_FS=y' \
                   'CONFIG_SYSFS=y' \
                   'CONFIG_DEVTMPFS=y' \
                   'CONFIG_UNIX98_PTYS=y' \
                   'CONFIG_TMPFS=y' \
                   'CONFIG_BLOCK=y' \
                   'CONFIG_BLK_DEV=y' \
                   'CONFIG_PCI=y' \
                   'CONFIG_PCI_MSI=y' \
                   'CONFIG_VIRTIO=y' \
                   'CONFIG_VIRTIO_MENU=y' \
                   'CONFIG_VIRTIO_PCI=y' \
                   'CONFIG_VIRTIO_CONSOLE=y' \
                   'CONFIG_VIRTIO_BLK=y' \
                   'CONFIG_MISC_FILESYSTEMS=y' \
                   'CONFIG_EROFS_FS=y' \
                   'CONFIG_EROFS_FS_ZIP=y' \
                   'CONFIG_EROFS_FS_ZIP_ZSTD=y' \
                   '# CONFIG_KEXEC is not set' \
                   'CONFIG_KEXEC_FILE=y' \
                   '# CONFIG_KEXEC_SIG is not set' \
                   '# CONFIG_KEXEC_SIG_FORCE is not set' \
                   'CONFIG_RELOCATABLE=y' \
                   '# CONFIG_RANDOMIZE_BASE is not set' \
                   'CONFIG_BLK_DEV_LOOP=y' \
                   'CONFIG_BLK_DEV_RAM=y' \
                   'CONFIG_BLK_DEV_RAM_COUNT=1' \
                   'CONFIG_BTRFS_FS=y' \
                   'CONFIG_SCSI=y' \
                   'CONFIG_BLK_DEV_SD=y' \
                   'CONFIG_BLK_DEV_SR=y' \
                   'CONFIG_ATA=y' \
                   'CONFIG_SATA_AHCI=y' \
                   'CONFIG_BLK_DEV_NVME=y' \
                   'CONFIG_NVME_CORE=y' \
                   '# CONFIG_NVME_MULTIPATH is not set' \
                   'CONFIG_USB_STORAGE=y' \
                   'CONFIG_ISO9660_FS=y' \
                   'CONFIG_MD=y' \
                   'CONFIG_BLK_DEV_DM=y' \
                   'CONFIG_DM_CRYPT=y' \
                   'CONFIG_CRYPTO_AES=y' \
                   'CONFIG_CRYPTO_XTS=y' \
                   'CONFIG_CRYPTO_AES_NI_INTEL=y' \
                   'CONFIG_CRYPTO_HMAC=y' \
                   'CONFIG_CRYPTO_SHA256=y' \
                   'CONFIG_CRYPTO_USER_API_HASH=y' \
                   'CONFIG_CRYPTO_USER_API_SKCIPHER=y' \
                   'CONFIG_NET=y' \
                   'CONFIG_PACKET=y' \
                   'CONFIG_UNIX=y' \
                   'CONFIG_INET=y' \
                   'CONFIG_NETDEVICES=y' \
                   'CONFIG_VIRTIO_NET=y' \
                   'CONFIG_ETHERNET=y' \
                   'CONFIG_NET_VENDOR_INTEL=y' \
                   'CONFIG_E1000=y' \
                   'CONFIG_HYPERVISOR_GUEST=y' \
                   'CONFIG_PARAVIRT=y' \
                   'CONFIG_KVM_GUEST=y' \
                   'CONFIG_HYPERV=y' \
                   'CONFIG_HYPERV_VMBUS=y' \
                   'CONFIG_SCSI_LOWLEVEL=y' \
                   'CONFIG_HYPERV_STORAGE=y' \
                   'CONFIG_HYPERV_NET=y' \
                   'CONFIG_PCI_HYPERV=y' \
                   'CONFIG_NET_VENDOR_AMAZON=y' \
                   'CONFIG_ENA_ETHERNET=y' \
                   'CONFIG_SCSI_VIRTIO=y' \
                   'CONFIG_NET_VENDOR_GOOGLE=y' \
                   'CONFIG_GVE=y' \
                   'CONFIG_DMIID=y' \
                   'CONFIG_INPUT=y' \
                   'CONFIG_INPUT_EVDEV=y' \
                   'CONFIG_INPUT_KEYBOARD=y' \
                   'CONFIG_KEYBOARD_ATKBD=y' \
                   'CONFIG_INPUT_MOUSE=y' \
                   'CONFIG_MOUSE_PS2=y' \
                   'CONFIG_VIRTIO_INPUT=y' \
                   'CONFIG_SERIO=y' \
                   'CONFIG_SERIO_I8042=y' \
                   'CONFIG_VT=y' \
                   'CONFIG_VT_CONSOLE=y' \
                   'CONFIG_HW_CONSOLE_BINDING=y' \
                   'CONFIG_FB=y' \
                   'CONFIG_FRAMEBUFFER_CONSOLE=y' \
                   'CONFIG_DRM=y' \
                   'CONFIG_DRM_VIRTIO_GPU=y' \
                   'CONFIG_DRM_VIRTIO_GPU_KMS=y' \
                   'CONFIG_DRM_FBDEV_EMULATION=y' \
                   'CONFIG_DRM_CLIENT_DEFAULT_FBDEV=y' \
                   'CONFIG_SYSFB_SIMPLEFB=y' \
                   'CONFIG_DRM_SIMPLEDRM=y' \
                   '# CONFIG_SECURITY_DMESG_RESTRICT is not set' \
                   'CONFIG_ACPI=y' \
                   'CONFIG_EFI=y' \
                   'CONFIG_EFI_STUB=y' \
                   'CONFIG_CMDLINE_BOOL=y' \
                   'CONFIG_CMDLINE=@EFI_CMDLINE@' \
                   '# CONFIG_CMDLINE_OVERRIDE is not set' \
                   '# CONFIG_EFI_HANDOVER_PROTOCOL is not set' \
                   '# CONFIG_EFI_MIXED is not set' \
                   '# CONFIG_EFIVAR_FS is not set' \
                   'CONFIG_PNP=y' \
                   'CONFIG_PNPACPI=y' \
                   'CONFIG_USB_SUPPORT=y' \
                   'CONFIG_USB=y' \
                   'CONFIG_USB_PCI=y' \
                   'CONFIG_USB_XHCI_HCD=y' \
                   'CONFIG_HID_SUPPORT=y' \
                   'CONFIG_HID=y' \
                   'CONFIG_HID_GENERIC=y' \
                   'CONFIG_UHID=y' \
                   'CONFIG_HIDRAW=y' \
                   'CONFIG_USB_HID=y' \
                   'CONFIG_TCG_TPM=y' \
                   'CONFIG_SECURITYFS=y' \
                   'CONFIG_SECURITY=y' \
                   'CONFIG_LSM=\"\"' \
                   '# CONFIG_SECURITY_NETWORK is not set' \
                   '# CONFIG_SECURITY_PATH is not set' \
                   '# CONFIG_SECURITY_SELINUX is not set' \
                   '# CONFIG_SECURITY_SMACK is not set' \
                   '# CONFIG_SECURITY_TOMOYO is not set' \
                   '# CONFIG_SECURITY_APPARMOR is not set' \
                   '# CONFIG_SECURITY_LOADPIN is not set' \
                   '# CONFIG_SECURITY_YAMA is not set' \
                   '# CONFIG_SECURITY_SAFESETID is not set' \
                   '# CONFIG_SECURITY_LOCKDOWN_LSM is not set' \
                   '# CONFIG_SECURITY_LANDLOCK is not set' \
                   '# CONFIG_SECURITY_IPE is not set' \
                   '# CONFIG_BPF_LSM is not set' \
                   '# CONFIG_INTEGRITY is not set' \
                   'CONFIG_TCG_TIS=y' \
                   'CONFIG_TCG_CRB=y' \
                   '# CONFIG_TCG_TPM2_HMAC is not set' \
                   '# CONFIG_HW_RANDOM_TPM is not set' \
                   'CONFIG_PERF_EVENTS=y' \
                   'CONFIG_SMP=y' \
                   'CONFIG_NR_CPUS=256' \
                   'CONFIG_HOTPLUG_CPU=y' \
                   'CONFIG_USER_NS=y' \
                   'CONFIG_PID_NS=y' \
                   'CONFIG_UTS_NS=y' \
                   'CONFIG_NET_NS=y' \
                   'CONFIG_AUDIT=y' \
                   'CONFIG_COMPAT_32BIT_TIME=y' \
                   'CONFIG_IA32_EMULATION=y' \
                   '# CONFIG_IA32_EMULATION_DEFAULT_DISABLED is not set' \
                   '# CONFIG_X86_X32_ABI is not set' \
                   'CONFIG_SECCOMP=y' \
                   'CONFIG_INOTIFY_USER=y' \
                   'CONFIG_CGROUPS=y' \
                   'CONFIG_CGROUP_SCHED=y' \
                   'CONFIG_FAIR_GROUP_SCHED=y' \
                   'CONFIG_CFS_BANDWIDTH=y' \
                   '# CONFIG_RT_GROUP_SCHED is not set' \
                   'CONFIG_MEMCG=y' \
                   'CONFIG_CGROUP_PIDS=y' \
                   'CONFIG_SOUND=y' \
                   'CONFIG_SND=y' \
                   'CONFIG_SND_PCM=y' \
                   '# CONFIG_SND_PCM_OSS is not set' \
                   'CONFIG_SND_PCI=y' \
                   'CONFIG_SND_HDA_INTEL=y' \
                   'CONFIG_SND_HDA_GENERIC=y' \
                   'CONFIG_SND_ALOOP=y' \
                   'CONFIG_DRM_I915=y' \
                   'CONFIG_E1000E=y' \
                   'CONFIG_X86_PLATFORM_DEVICES=y' \
                   'CONFIG_ACPI_EC=y' \
                   'CONFIG_I2C=y' \
                   'CONFIG_THINKPAD_ACPI=y' \
                   'CONFIG_ACPI_AC=y' \
                   'CONFIG_ACPI_BATTERY=y' \
                   'CONFIG_ACPI_BUTTON=y' \
                   'CONFIG_ACPI_THERMAL=y' \
                   'CONFIG_ACPI_FAN=y' \
                   'CONFIG_ACPI_VIDEO=y' \
                   'CONFIG_BACKLIGHT_CLASS_DEVICE=y' \
                   'CONFIG_CPU_FREQ=y' \
                   'CONFIG_X86_INTEL_PSTATE=y' \
                   'CONFIG_CPU_IDLE=y' \
                   'CONFIG_INTEL_IDLE=y' \
                   'CONFIG_SUSPEND=y' \
                   'CONFIG_USB_EHCI_HCD=y' \
                   'CONFIG_MMC=y' \
                   'CONFIG_MMC_BLOCK=y' \
                   'CONFIG_MMC_SDHCI=y' \
                   'CONFIG_MMC_SDHCI_PCI=y' \
                   'CONFIG_MMC_RICOH_MMC=y' \
                   'CONFIG_SND_HDA_CODEC_CONEXANT=y' \
                   'CONFIG_SND_HDA_CODEC_HDMI=y' \
                   'CONFIG_X86_AMD_PSTATE=y' \
                   'CONFIG_X86_AMD_PLATFORM_DEVICE=y' \
                   'CONFIG_PINCTRL_AMD=y' \
                   'CONFIG_I2C_DESIGNWARE_CORE=y' \
                   'CONFIG_I2C_DESIGNWARE_PLATFORM=y' \
                   'CONFIG_I2C_HID=y' \
                   'CONFIG_I2C_HID_ACPI=y' \
                   'CONFIG_HID_MULTITOUCH=y' \
                   'CONFIG_SND_HDA_CODEC_REALTEK=y' \
                   'CONFIG_IOMMU_SUPPORT=y' \
                   'CONFIG_AMD_IOMMU=y' \
                   'CONFIG_IRQ_REMAP=y' \
                   'CONFIG_X86_X2APIC=y' \
                   'CONFIG_WIRELESS=y' \
                   'CONFIG_CFG80211=y' \
                   'CONFIG_MAC80211=y' \
                   'CONFIG_WLAN=y' \
                   'CONFIG_WLAN_VENDOR_ATH=y' \
                   'CONFIG_ATH11K=y' \
                   'CONFIG_ATH11K_PCI=y' \
                   'CONFIG_EXTRA_FIRMWARE=\"ath11k/WCN6855/hw2.0/amss.bin ath11k/WCN6855/hw2.0/m3.bin ath11k/WCN6855/hw2.0/board-2.bin ath11k/WCN6855/hw2.0/regdb.bin\"' \
                   'CONFIG_EXTRA_FIRMWARE_DIR=\"td-firmware\"' >> .config"
                    .replace(
                        "@EFI_CMDLINE@",
                        &format!("{:?}", crate::shared::efi_default_cmdline()),
                    ),
            ],
        )
        .env("PATH", PATH),
    );

    // 3) olddefconfig normalises the choice (takes defaults for newly-visible
    //    symbols, non-interactive).
    steps.push(mk(&["olddefconfig"]));

    // 4) Guard: the minimal invariant must hold before the full build. Fail HERE,
    //    with a named symbol, if the unwinder flip or the no-modules pin did not
    //    take. (objtool IS expected on x86_64 7.x — not asserted absent.)
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                &"grep -q '^CONFIG_UNWINDER_FRAME_POINTER=y' .config || { echo 'frame-pointer unwinder not selected' >&2; exit 1; }; \
                 grep -q '^CONFIG_KERNEL_GZIP=y' .config || { echo 'gzip kernel compression not selected (bzImage would need another compressor)' >&2; exit 1; }; \
                 grep -q '^CONFIG_BINFMT_ELF=y' .config || { echo 'BINFMT_ELF off — the kernel could not exec the initramfs userland' >&2; exit 1; }; \
                 grep -q '^CONFIG_BINFMT_SCRIPT=y' .config || { echo 'BINFMT_SCRIPT off — the kernel could not exec the #! /init script' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLK_DEV_INITRD=y' .config || { echo 'BLK_DEV_INITRD off — the kernel could not load the initramfs' >&2; exit 1; }; \
                 grep -q '^CONFIG_SERIAL_8250_CONSOLE=y' .config || { echo '8250 serial console off — no ttyS0 boot output for the qemu check' >&2; exit 1; }; \
                 grep -q '^CONFIG_DEVTMPFS=y' .config || { echo 'DEVTMPFS off - init cannot mount /dev, so the userland has no ttyS0/console devices' >&2; exit 1; }; \
                 grep -q '^CONFIG_PROC_FS=y' .config || { echo 'PROC_FS off - a usable userland (ps/mount/init) needs /proc' >&2; exit 1; }; \
                 grep -q '^CONFIG_SYSFS=y' .config || { echo 'SYSFS off - a usable userland needs /sys' >&2; exit 1; }; \
                 grep -q '^CONFIG_TMPFS=y' .config || { echo 'TMPFS off - a usable userland needs writable /var,/tmp,/run mounts' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLOCK=y' .config || { echo 'BLOCK off - no block layer for the virtio-blk erofs disk' >&2; exit 1; }; \
                 grep -q '^CONFIG_PCI=y' .config || { echo 'PCI off - virtio-blk-pci (the -M pc transport) needs the PCI bus' >&2; exit 1; }; \
                 grep -q '^CONFIG_PCI_MSI=y' .config || { echo 'PCI_MSI off - target PCI profile selects MSI/MSI-X support' >&2; exit 1; }; \
                 grep -q '^CONFIG_VIRTIO_PCI=y' .config || { echo 'VIRTIO_PCI off - no virtio transport on the -M pc PCI bus' >&2; exit 1; }; \
                 grep -q '^CONFIG_VIRTIO_CONSOLE=y' .config || { echo 'VIRTIO_CONSOLE off - no td VM bridge' >&2; exit 1; }; \
                 grep -q '^CONFIG_VIRTIO_BLK=y' .config || { echo 'VIRTIO_BLK off - the erofs disk (/dev/vda) would not appear' >&2; exit 1; }; \
                 grep -q '^CONFIG_EROFS_FS_ZIP=y' .config || exit 1; \
                 grep -q '^CONFIG_EROFS_FS_ZIP_ZSTD=y' .config || exit 1; \
                 grep -q '^CONFIG_EROFS_FS=y' .config || { echo 'EROFS_FS off - the read-only erofs root could not be mounted' >&2; exit 1; }; \
                 if grep -q '^CONFIG_OVERLAY_FS=y' .config; then echo 'OVERLAY_FS on - /etc is immutable and writable state uses direct mounts; no overlay user remains' >&2; exit 1; fi; \
                 grep -q '^CONFIG_PRINTK=y' .config || { echo 'PRINTK off — no kernel console output' >&2; exit 1; }; \
                 grep -q '^CONFIG_TTY=y' .config || { echo 'TTY off — the serial console needs the tty layer' >&2; exit 1; }; \
                 grep -q '^CONFIG_UNIX98_PTYS=y' .config || { echo 'UNIX98_PTYS off — /dev/ptmx would not exist, so td-term could not allocate a pty and the graphical session would come up with no terminal' >&2; exit 1; }; \
                 grep -q '^CONFIG_KEXEC_FILE=y' .config || { echo 'KEXEC_FILE off — the shim boots the selected deployment via kexec_file_load(2)' >&2; exit 1; }; \
                 grep -q '^CONFIG_RELOCATABLE=y' .config || { echo 'RELOCATABLE off — a non-relocatable bzImage is rejected by the x86 kexec_file_load loader (boots via -kernel, fails via kexec)' >&2; exit 1; }; \
                 grep -q '^CONFIG_BTRFS_FS=y' .config || { echo 'BTRFS_FS off — the persistent volume is one btrfs filesystem (@var plus the loop-mounted EROFS root blobs)' >&2; exit 1; }; \
                 grep -q '^CONFIG_MD=y' .config || { echo 'MD off — device mapper needs its prompted parent enabled' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLK_DEV_DM=y' .config || { echo 'BLK_DEV_DM off — encrypted volumes need built-in device mapper' >&2; exit 1; }; \
                 grep -q '^CONFIG_DM_CRYPT=y' .config || { echo 'DM_CRYPT off — encrypted volumes need the built-in crypt target' >&2; exit 1; }; \
                 grep -q '^CONFIG_CRYPTO_AES=y' .config || { echo 'CRYPTO_AES off — disk encryption needs a generic AES fallback' >&2; exit 1; }; \
                 grep -q '^CONFIG_CRYPTO_XTS=y' .config || { echo 'CRYPTO_XTS off — disk encryption needs the generic XTS mode' >&2; exit 1; }; \
                 grep -q '^CONFIG_CRYPTO_AES_NI_INTEL=y' .config || { echo 'CRYPTO_AES_NI_INTEL off — disk encryption needs the x86 accelerated AES-XTS implementation' >&2; exit 1; }; \
                 grep -q '^CONFIG_CRYPTO_HMAC=y' .config || { echo 'CRYPTO_HMAC off — cryptsetup needs the generic HMAC template for hashes other than SHA-256 through AF_ALG' >&2; exit 1; }; \
                 grep -q '^CONFIG_CRYPTO_SHA256=y' .config || { echo 'CRYPTO_SHA256 off — cryptsetup hashes and derives keyslot keys with SHA-256 and hmac(sha256) through AF_ALG' >&2; exit 1; }; \
                 grep -q '^CONFIG_CRYPTO_USER_API_HASH=y' .config || { echo 'CRYPTO_USER_API_HASH off — cryptsetup kernel crypto backend needs the AF_ALG hash interface' >&2; exit 1; }; \
                 grep -q '^CONFIG_CRYPTO_USER_API_SKCIPHER=y' .config || { echo 'CRYPTO_USER_API_SKCIPHER off — cryptsetup kernel crypto backend needs the AF_ALG skcipher interface' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLK_DEV_LOOP=y' .config || { echo 'BLK_DEV_LOOP off — the immutable EROFS root is a file inside btrfs, loop-mounted read-only' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLK_DEV_RAM=y' .config || { echo 'BLK_DEV_RAM off - a live boot keeps its volatile volume on /dev/ram0' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLK_DEV_RAM_COUNT=1$' .config || { echo 'BLK_DEV_RAM_COUNT is not 1 - the live profile uses exactly /dev/ram0' >&2; exit 1; }; \
                 grep -q '^CONFIG_SCSI=y' .config || { echo 'SCSI off - offline optical/USB installation requires built-in media support' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLK_DEV_SD=y' .config || { echo 'BLK_DEV_SD off - offline optical/USB installation requires built-in media support' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLK_DEV_SR=y' .config || { echo 'BLK_DEV_SR off - offline optical/USB installation requires built-in media support' >&2; exit 1; }; \
                 grep -q '^CONFIG_ATA=y' .config || { echo 'ATA off - offline optical/USB installation requires built-in media support' >&2; exit 1; }; \
                 grep -q '^CONFIG_SATA_AHCI=y' .config || { echo 'SATA_AHCI off - offline optical/USB installation requires built-in media support' >&2; exit 1; }; \
                 grep -q '^CONFIG_BLK_DEV_NVME=y' .config || { echo 'BLK_DEV_NVME off - NVMe installation requires its built-in PCI driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_NVME_CORE=y' .config || { echo 'NVME_CORE off - NVMe installation requires built-in namespace support' >&2; exit 1; }; \
                 grep -q '^# CONFIG_NVME_MULTIPATH is not set' .config || { echo 'NVME_MULTIPATH on - installation fixture requires direct controller namespaces' >&2; exit 1; }; \
                 grep -q '^CONFIG_USB_STORAGE=y' .config || { echo 'USB_STORAGE off - offline optical/USB installation requires built-in media support' >&2; exit 1; }; \
                 grep -q '^CONFIG_ISO9660_FS=y' .config || { echo 'ISO9660_FS off - offline optical/USB installation requires built-in media support' >&2; exit 1; }; \
                 grep -q '^CONFIG_NET=y' .config || { echo 'NET off — no networking stack for td-netd link-up/DHCP' >&2; exit 1; }; \
                 grep -q '^CONFIG_INET=y' .config || { echo 'INET off — no IPv4/TCP/UDP for DHCP or resolve/reach' >&2; exit 1; }; \
                 grep -q '^CONFIG_PACKET=y' .config || { echo 'PACKET off — AF_PACKET raw-socket family unavailable' >&2; exit 1; }; \
                 grep -q '^CONFIG_UNIX=y' .config || { echo 'UNIX off — AF_UNIX local-socket family unavailable' >&2; exit 1; }; \
                 grep -q '^CONFIG_NETDEVICES=y' .config || { echo 'NETDEVICES off (menuconfig parent dropped) — no NIC drivers are visible' >&2; exit 1; }; \
                 grep -q '^CONFIG_VIRTIO_NET=y' .config || { echo 'VIRTIO_NET off — the qemu user-net virtio NIC would not appear' >&2; exit 1; }; \
                 grep -q '^CONFIG_E1000=y' .config || { echo 'E1000 off (ETHERNET/NET_VENDOR_INTEL menuconfig parents likely dropped) — no real Intel NIC driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_INPUT_EVDEV=y' .config || { echo 'INPUT_EVDEV off — td-compositor cannot read /dev/input/eventN' >&2; exit 1; }; \
                 grep -q '^CONFIG_KEYBOARD_ATKBD=y' .config || { echo 'KEYBOARD_ATKBD off — the qemu PS/2 keyboard would not appear' >&2; exit 1; }; \
                 grep -q '^CONFIG_MOUSE_PS2=y' .config || { echo 'MOUSE_PS2 off — the qemu PS/2 pointer would not appear' >&2; exit 1; }; \
                 grep -q '^CONFIG_VIRTIO_INPUT=y' .config || { echo 'VIRTIO_INPUT off — the qemu virtio tablet would not appear, so the pointer could not reach the far edges of the screen' >&2; exit 1; }; \
                 grep -q '^CONFIG_SERIO_I8042=y' .config || { echo 'SERIO_I8042 off — qemu PS/2 input has no controller' >&2; exit 1; }; \
                 grep -q '^CONFIG_VT_CONSOLE=y' .config || { echo 'VT_CONSOLE off — the framebuffer recovery console is unavailable' >&2; exit 1; }; \
                 grep -q '^CONFIG_FRAMEBUFFER_CONSOLE=y' .config || { echo 'FRAMEBUFFER_CONSOLE off — virtio fbdev cannot host the recovery console' >&2; exit 1; }; \
                 grep -q '^CONFIG_DRM_VIRTIO_GPU=y' .config || { echo 'DRM_VIRTIO_GPU off — qemu virtio-vga cannot expose /dev/fb0' >&2; exit 1; }; \
                 grep -q '^CONFIG_DRM_VIRTIO_GPU_KMS=y' .config || { echo 'DRM_VIRTIO_GPU_KMS off — virtio-gpu has no scanout' >&2; exit 1; }; \
                 grep -q '^CONFIG_DRM_FBDEV_EMULATION=y' .config || { echo 'DRM_FBDEV_EMULATION off — td-compositor has no /dev/fb0 software scanout' >&2; exit 1; }; \
                 grep -q '^CONFIG_DRM_CLIENT_DEFAULT_FBDEV=y' .config || { echo 'DRM_CLIENT_DEFAULT_FBDEV off — the selected virtio DRM client will not create fb0' >&2; exit 1; }; \
                 grep -q '^CONFIG_SYSFB_SIMPLEFB=y' .config || { echo 'SYSFB_SIMPLEFB off — a UEFI GOP framebuffer is not offered as a simple-framebuffer device, so the selector has no keyboard console on a display td has no driver for' >&2; exit 1; }; \
                 grep -q '^CONFIG_DRM_SIMPLEDRM=y' .config || { echo 'DRM_SIMPLEDRM off — nothing drives the firmware framebuffer, so fbcon has no display before or without a native driver' >&2; exit 1; }; \
                 if grep -q '^CONFIG_DRM_EFIDRM=' .config; then echo 'DRM_EFIDRM set — it requires SYSFB_SIMPLEFB off; simpledrm is the one firmware framebuffer driver' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_DRM_VESADRM=' .config; then echo 'DRM_VESADRM set — it requires SYSFB_SIMPLEFB off; simpledrm is the one firmware framebuffer driver' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_FB_EFI=' .config; then echo 'FB_EFI set — legacy efifb would take a GOP mode simple-framebuffer cannot describe; simpledrm is the one firmware framebuffer driver' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_FB_VESA=' .config; then echo 'FB_VESA set — legacy vesafb would take a VBE mode simple-framebuffer cannot describe; simpledrm is the one firmware framebuffer driver' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_KEXEC=y' .config; then echo 'KEXEC (legacy segment-based kexec_load) on — only KEXEC_FILE is used; the legacy syscall is kept off to not widen the surface' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_KEXEC_SIG=y' .config; then echo 'KEXEC_SIG on — would demand a trusted-keyring signature policy td does not ship' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_RANDOMIZE_BASE=y' .config; then echo 'RANDOMIZE_BASE (KASLR) on — pinned off for a deterministic kexec boot' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_MODULES=y' .config; then echo 'MODULES on (would need module tooling)' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_DEBUG_INFO_BTF=y' .config; then echo 'BTF on (would need pahole)' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_DMESG_RESTRICT=y' .config; then echo 'SECURITY_DMESG_RESTRICT on — unprivileged /dev/kmsg reads become EPERM, so the shipped /bin/dmesg breaks for ordinary users' >&2; exit 1; fi; \
                 grep -q '^CONFIG_ACPI=y' .config || { echo 'ACPI off - TPM enrollment requires ACPI discovery and TPM2 device support' >&2; exit 1; }; \
                 grep -q '^CONFIG_EFI=y$' .config || { echo 'EFI off - firmware entry requires EFI runtime support' >&2; exit 1; }; \
                 grep -q '^CONFIG_CMDLINE_BOOL=y$' .config || { echo 'EFI built-in command line missing' >&2; exit 1; }; \
                 grep -Fxq 'CONFIG_CMDLINE=@EFI_CMDLINE@' .config || { echo 'EFI initrd command line mismatch' >&2; exit 1; }; \
                 grep -q '^# CONFIG_CMDLINE_OVERRIDE is not set$' .config || { echo 'firmware command line must remain usable' >&2; exit 1; }; \
                 grep -q '^CONFIG_EFI_STUB=y$' .config || { echo 'EFI_STUB off - bzImage cannot be loaded by UEFI' >&2; exit 1; }; \
                 grep -q '^# CONFIG_EFI_HANDOVER_PROTOCOL is not set$' .config || { echo 'deprecated EFI handover disabled policy missing' >&2; exit 1; }; \
                 grep -q '^# CONFIG_EFI_MIXED is not set$' .config || { echo '32-bit EFI firmware disabled policy missing' >&2; exit 1; }; \
                 grep -q '^# CONFIG_EFIVAR_FS is not set$' .config || { echo 'EFI variable filesystem disabled policy missing' >&2; exit 1; }; \
                 grep -q '^CONFIG_PNP=y' .config || { echo 'PNP off - TPM enrollment requires ACPI discovery and TPM2 device support' >&2; exit 1; }; \
                 grep -q '^CONFIG_PNPACPI=y' .config || { echo 'PNPACPI off - TPM enrollment requires ACPI discovery and TPM2 device support' >&2; exit 1; }; \
                 grep -q '^CONFIG_USB_SUPPORT=y' .config || { echo 'USB_SUPPORT off - FIDO USB transport requires built-in xHCI and hidraw' >&2; exit 1; }; \
                 grep -q '^CONFIG_USB=y' .config || { echo 'USB off - FIDO USB transport requires built-in xHCI and hidraw' >&2; exit 1; }; \
                 grep -q '^CONFIG_USB_PCI=y' .config || { echo 'USB_PCI off - FIDO USB transport requires built-in xHCI and hidraw' >&2; exit 1; }; \
                 grep -q '^CONFIG_USB_XHCI_HCD=y' .config || { echo 'USB_XHCI_HCD off - FIDO USB transport requires built-in xHCI and hidraw' >&2; exit 1; }; \
                 grep -q '^CONFIG_HID_SUPPORT=y' .config || { echo 'HID_SUPPORT off - FIDO USB transport requires built-in xHCI and hidraw' >&2; exit 1; }; \
                 grep -q '^CONFIG_HID=y' .config || { echo 'HID off - FIDO USB transport requires built-in xHCI and hidraw' >&2; exit 1; }; \
                 grep -q '^CONFIG_HID_GENERIC=y' .config || { echo 'HID_GENERIC off - FIDO USB transport requires built-in xHCI and hidraw' >&2; exit 1; }; \
                 grep -q '^CONFIG_UHID=y' .config || { echo 'UHID off - secret VM fixture requires kernel HID transport' >&2; exit 1; }; \
                 grep -q '^CONFIG_HIDRAW=y' .config || { echo 'HIDRAW off - FIDO USB transport requires built-in xHCI and hidraw' >&2; exit 1; }; \
                 grep -q '^CONFIG_USB_HID=y' .config || { echo 'USB_HID off - FIDO USB transport requires built-in xHCI and hidraw' >&2; exit 1; }; \
                 grep -q '^CONFIG_USB_XHCI_PCI=y' .config || { echo 'USB_XHCI_PCI off - FIDO USB transport requires built-in xHCI and hidraw' >&2; exit 1; }; \
                 grep -q '^CONFIG_TCG_TPM=y' .config || { echo 'TCG_TPM off - TPM enrollment requires ACPI discovery and TPM2 device support' >&2; exit 1; }; \
                 grep -q '^CONFIG_SECURITYFS=y$' .config || { echo 'SECURITYFS off - the selector reads the TPM event log through securityfs' >&2; exit 1; }; \
                 grep -q '^CONFIG_SECURITY=y$' .config || { echo 'SECURITY off - Linux 7.2 registers securityfs only from the LSM framework' >&2; exit 1; }; \
                 grep -Fxq 'CONFIG_LSM=\"\"' .config || { echo 'CONFIG_LSM not empty - no security module may be ordered' >&2; exit 1; }; \
                 if grep -q '^CONFIG_SECURITY_NETWORK=' .config; then echo 'SECURITY_NETWORK on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_PATH=' .config; then echo 'SECURITY_PATH on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_SELINUX=' .config; then echo 'SECURITY_SELINUX on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_SMACK=' .config; then echo 'SECURITY_SMACK on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_TOMOYO=' .config; then echo 'SECURITY_TOMOYO on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_APPARMOR=' .config; then echo 'SECURITY_APPARMOR on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_LOADPIN=' .config; then echo 'SECURITY_LOADPIN on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_YAMA=' .config; then echo 'SECURITY_YAMA on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_SAFESETID=' .config; then echo 'SECURITY_SAFESETID on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_LOCKDOWN_LSM=' .config; then echo 'SECURITY_LOCKDOWN_LSM on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_LANDLOCK=' .config; then echo 'SECURITY_LANDLOCK on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SECURITY_IPE=' .config; then echo 'SECURITY_IPE on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_BPF_LSM=' .config; then echo 'BPF_LSM on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_INTEGRITY=' .config; then echo 'INTEGRITY on - the LSM framework is built for securityfs alone, with no security module' >&2; exit 1; fi; \
                 grep -q '^CONFIG_TCG_TIS=y' .config || { echo 'TCG_TIS off - TPM enrollment requires ACPI discovery and TPM2 device support' >&2; exit 1; }; \
                 grep -q '^CONFIG_TCG_CRB=y' .config || { echo 'TCG_CRB off - TPM enrollment requires ACPI discovery and TPM2 device support' >&2; exit 1; }; \
                 grep -q '^CONFIG_PERF_EVENTS=y' .config || { echo 'PERF_EVENTS off — td-profiler cannot open its per-CPU software sampling events' >&2; exit 1; }; \
                 grep -q '^CONFIG_SMP=y$' .config || { echo 'SMP missing' >&2; exit 1; }; \
                 grep -q '^CONFIG_NR_CPUS=256$' .config || { echo 'NR_CPUS does not match the VM allocation ceiling' >&2; exit 1; }; \
                 grep -q '^CONFIG_HOTPLUG_CPU=y$' .config || { echo 'HOTPLUG_CPU missing' >&2; exit 1; }; \
                 grep -q '^CONFIG_USER_NS=y' .config || { echo 'USER_NS off — unshare(CLONE_NEWUSER) returns EINVAL, so td-jail cannot build a sandbox at all' >&2; exit 1; }; \
                 grep -q '^CONFIG_PID_NS=y' .config || { echo 'PID_NS off — a jailed app would see (and could signal) every process on the machine' >&2; exit 1; }; \
                 grep -q '^CONFIG_UTS_NS=y' .config || { echo 'UTS_NS off — a jail could not present its own hostname' >&2; exit 1; }; \
                 grep -q '^CONFIG_NET_NS=y' .config || { echo 'NET_NS off — a jail without shared=network could not be cut off from the network stack' >&2; exit 1; }; \
                 grep -q '^CONFIG_AUDIT=y' .config || { echo 'AUDIT off — the physical Firefox proof cannot account for outer seccomp denials' >&2; exit 1; }; \
                 grep -q '^CONFIG_AUDITSYSCALL=y' .config || { echo 'AUDITSYSCALL off — x86 seccomp audit records cannot carry syscall identity' >&2; exit 1; }; \
                 grep -q '^CONFIG_COMPAT_32BIT_TIME=y$' .config || { echo 'COMPAT_32BIT_TIME off — i386 code under the td-jail development policy would lack its futex and clock syscalls' >&2; exit 1; }; \
                 grep -q '^CONFIG_IA32_EMULATION=y$' .config || { echo 'IA32_EMULATION off — the td-jail probe cannot prove confinement kills i386 syscalls' >&2; exit 1; }; \
                 grep -q '^# CONFIG_IA32_EMULATION_DEFAULT_DISABLED is not set$' .config || { echo 'IA32 emulation disabled at boot — the td-jail probe needs compatibility execution' >&2; exit 1; }; \
                 grep -q '^# CONFIG_X86_X32_ABI is not set$' .config || { echo 'X86_X32_ABI on — x32 calls could reach an unmodelled compatibility ABI instead of the deliberate kill probe' >&2; exit 1; }; \
                 grep -q '^CONFIG_SECCOMP=y' .config || { echo 'SECCOMP off — seccomp(2) returns ENOSYS, so td-jail ships namespaces with no syscall filter' >&2; exit 1; }; \
                 grep -q '^CONFIG_SECCOMP_FILTER=y' .config || { echo 'SECCOMP_FILTER off — no BPF syscall filtering. It is unprompted (def_bool y on HAVE_ARCH_SECCOMP_FILTER && SECCOMP && NET), so it cannot be pinned: something took SECCOMP or NET away' >&2; exit 1; }; \
                 grep -q '^CONFIG_INOTIFY_USER=y' .config || { echo 'INOTIFY_USER off — GLib file monitoring in a jailed app degrades to polling' >&2; exit 1; }; \
                 grep -q '^CONFIG_CGROUPS=y' .config || { echo 'CGROUPS off — no cgroup v2, so an application has no aggregate CPU, memory, or pid cap' >&2; exit 1; }; \
                 grep -q '^CONFIG_CGROUP_SCHED=y' .config || { echo 'CGROUP_SCHED off — the cgroup v2 cpu controller does not exist' >&2; exit 1; }; \
                 grep -q '^CONFIG_FAIR_GROUP_SCHED=y' .config || { echo 'FAIR_GROUP_SCHED off — fair-scheduler cgroup accounting does not exist' >&2; exit 1; }; \
                 grep -q '^CONFIG_CFS_BANDWIDTH=y' .config || { echo 'CFS_BANDWIDTH off — cpu.max cannot throttle a jailed application' >&2; exit 1; }; \
                 if grep -q '^CONFIG_RT_GROUP_SCHED=y' .config; then echo 'RT_GROUP_SCHED on — real-time tasks in the populated root can block enabling the cpu controller, and td does not delegate real-time bandwidth' >&2; exit 1; fi; \
                 grep -q '^CONFIG_MEMCG=y' .config || { echo 'MEMCG off — memory.max/memory.high do not exist, so a multi-process app is bounded only per process' >&2; exit 1; }; \
                 grep -q '^CONFIG_CGROUP_PIDS=y' .config || { echo 'CGROUP_PIDS off — pids.max does not exist, so nothing bounds a fork bomb inside a jail' >&2; exit 1; }; \
                 grep -q '^CONFIG_SND_PCM=y' .config || { echo 'SND_PCM off — /dev/snd/pcmC*D*p would not exist, so td-audio has no device to open (APPLICATIONS.md K.4)' >&2; exit 1; }; \
                 grep -q '^CONFIG_SND_HDA_INTEL=y' .config || { echo 'SND_HDA_INTEL off — QEMU -device intel-hda and ordinary desktop codecs both go through it, so /proc/asound/pcm would be empty' >&2; exit 1; }; \
                 grep -q '^CONFIG_SND_HDA_GENERIC=y' .config || { echo 'SND_HDA_GENERIC off — the HDA controller would bind no codec, so the card would enumerate with no playback device' >&2; exit 1; }; \
                 grep -q '^CONFIG_SND_ALOOP=y' .config || { echo 'SND_ALOOP off — nothing can capture what was played, so the tone fixture has no in-image oracle' >&2; exit 1; }; \
                 grep -q '^CONFIG_PROC_FS=y' .config || { echo 'PROC_FS off — td-audio discovers devices through /proc/asound/pcm' >&2; exit 1; }; \
                 if grep -q '^CONFIG_SND_PCM_OSS=y' .config; then echo 'SND_PCM_OSS on — APPLICATIONS.md K.4 refuses the deprecated OSS emulation layer on the merits, and olddefconfig must not bring it back as a default' >&2; exit 1; fi; \
                 grep -q '^CONFIG_DRM_I915=y' .config || { echo 'DRM_I915 off — the T430s Ivy Bridge GPU has no KMS driver, so td-compositor has no /dev/fb0 there' >&2; exit 1; }; \
                 grep -q '^CONFIG_ETHERNET=y' .config || { echo 'ETHERNET off — NIC vendor menus stay hidden, so the e1000e driver cannot be chosen' >&2; exit 1; }; \
                 grep -q '^CONFIG_NET_VENDOR_INTEL=y' .config || { echo 'NET_VENDOR_INTEL off — the Intel NIC drivers stay hidden' >&2; exit 1; }; \
                 grep -q '^CONFIG_E1000E=y' .config || { echo 'E1000E off — the T430s Intel 82579LM wired NIC has no driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_MOUSE_PS2_SYNAPTICS=y' .config || { echo 'MOUSE_PS2_SYNAPTICS off — the T430s touchpad is driven as a bare PS/2 mouse' >&2; exit 1; }; \
                 grep -q '^CONFIG_MOUSE_PS2_TRACKPOINT=y' .config || { echo 'MOUSE_PS2_TRACKPOINT off — the TrackPoint loses its own protocol and settings' >&2; exit 1; }; \
                 grep -q '^CONFIG_X86_PLATFORM_DEVICES=y' .config || { echo 'X86_PLATFORM_DEVICES off — laptop platform drivers stay hidden, thinkpad_acpi among them' >&2; exit 1; }; \
                 grep -q '^CONFIG_ACPI_EC=y' .config || { echo 'ACPI_EC off — the embedded controller behind battery, lid and hotkeys is unreachable' >&2; exit 1; }; \
                 grep -q '^CONFIG_I2C=y' .config || { echo 'I2C off — thinkpad_acpi and i915 depend on the I2C core' >&2; exit 1; }; \
                 grep -q '^CONFIG_THINKPAD_ACPI=y' .config || { echo 'THINKPAD_ACPI off — ThinkPad hotkeys, fan, LEDs and thermal readings have no driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_ACPI_AC=y' .config || { echo 'ACPI_AC off — the AC adapter state is unknown' >&2; exit 1; }; \
                 grep -q '^CONFIG_ACPI_BATTERY=y' .config || { echo 'ACPI_BATTERY off — the laptop battery is not reported' >&2; exit 1; }; \
                 grep -q '^CONFIG_ACPI_BUTTON=y' .config || { echo 'ACPI_BUTTON off — the power button and lid switch produce no events' >&2; exit 1; }; \
                 grep -q '^CONFIG_ACPI_THERMAL=y' .config || { echo 'ACPI_THERMAL off — ACPI thermal zones are not monitored' >&2; exit 1; }; \
                 grep -q '^CONFIG_ACPI_FAN=y' .config || { echo 'ACPI_FAN off — ACPI fan devices are not driven' >&2; exit 1; }; \
                 grep -q '^CONFIG_ACPI_VIDEO=y' .config || { echo 'ACPI_VIDEO off — the panel backlight has no ACPI control' >&2; exit 1; }; \
                 grep -q '^CONFIG_BACKLIGHT_CLASS_DEVICE=y' .config || { echo 'BACKLIGHT_CLASS_DEVICE off — the panel backlight cannot be set' >&2; exit 1; }; \
                 grep -q '^CONFIG_CPU_FREQ=y' .config || { echo 'CPU_FREQ off — the CPU runs at one frequency, on battery as on AC' >&2; exit 1; }; \
                 grep -q '^CONFIG_X86_INTEL_PSTATE=y' .config || { echo 'X86_INTEL_PSTATE off — Intel CPUs get no P-state scaling driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_CPU_IDLE=y' .config || { echo 'CPU_IDLE off — idle CPUs enter no deep C-states' >&2; exit 1; }; \
                 grep -q '^CONFIG_INTEL_IDLE=y' .config || { echo 'INTEL_IDLE off — Intel CPUs get no native idle driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_SUSPEND=y' .config || { echo 'SUSPEND off — the machine cannot suspend to RAM' >&2; exit 1; }; \
                 grep -q '^CONFIG_USB_EHCI_HCD=y' .config || { echo 'USB_EHCI_HCD off — a stick in a USB 2.0 port is invisible (the T430s has one EHCI-only port)' >&2; exit 1; }; \
                 grep -q '^CONFIG_USB_EHCI_PCI=y' .config || { echo 'USB_EHCI_PCI off — PCI EHCI controllers bind no driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_MMC=y' .config || { echo 'MMC off — the SD card reader has no MMC core' >&2; exit 1; }; \
                 grep -q '^CONFIG_MMC_BLOCK=y' .config || { echo 'MMC_BLOCK off — SD cards appear as no block device' >&2; exit 1; }; \
                 grep -q '^CONFIG_MMC_SDHCI=y' .config || { echo 'MMC_SDHCI off — the SDHCI card reader has no driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_MMC_SDHCI_PCI=y' .config || { echo 'MMC_SDHCI_PCI off — the PCI SDHCI card reader binds no driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_MMC_RICOH_MMC=y' .config || { echo 'MMC_RICOH_MMC off — the Ricoh reader keeps its separate MMC function, so MMC cards are not seen through SDHCI' >&2; exit 1; }; \
                 grep -q '^CONFIG_SND_HDA_CODEC_CONEXANT=y' .config || { echo 'SND_HDA_CODEC_CONEXANT off — the T430s Conexant codec falls back to the generic parser' >&2; exit 1; }; \
                 grep -q '^CONFIG_SND_HDA_CODEC_HDMI=y' .config || { echo 'SND_HDA_CODEC_HDMI off — HDMI and DisplayPort audio codecs stay hidden' >&2; exit 1; }; \
                 grep -q '^CONFIG_SND_HDA_CODEC_HDMI_INTEL=y' .config || { echo 'SND_HDA_CODEC_HDMI_INTEL off — Intel HDMI and DisplayPort audio has no codec driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_X86_AMD_PSTATE=y' .config || { echo 'X86_AMD_PSTATE off — AMD CPUs get no P-state scaling driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_ACPI_PROCESSOR=y' .config || { echo 'ACPI_PROCESSOR off — AMD CPUs get no ACPI idle states and amd-pstate has no CPPC' >&2; exit 1; }; \
                 grep -q '^CONFIG_X86_AMD_PLATFORM_DEVICE=y' .config || { echo 'X86_AMD_PLATFORM_DEVICE off — the AMD I2C and GPIO controllers are never created from ACPI' >&2; exit 1; }; \
                 grep -q '^CONFIG_PINCTRL_AMD=y' .config || { echo 'PINCTRL_AMD off — the T14s touchpad interrupt line has no GPIO controller' >&2; exit 1; }; \
                 grep -q '^CONFIG_I2C_DESIGNWARE_CORE=y' .config || { echo 'I2C_DESIGNWARE_CORE off — the AMD I2C controller behind the T14s touchpad has no driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_I2C_DESIGNWARE_PLATFORM=y' .config || { echo 'I2C_DESIGNWARE_PLATFORM off — the ACPI-enumerated DesignWare I2C controller binds no driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_I2C_HID=y' .config || { echo 'I2C_HID off — the HID-over-I2C menu stays hidden, so the T14s touchpad has no transport' >&2; exit 1; }; \
                 grep -q '^CONFIG_I2C_HID_ACPI=y' .config || { echo 'I2C_HID_ACPI off — the ACPI-enumerated I2C touchpad binds no driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_I2C_HID_CORE=y' .config || { echo 'I2C_HID_CORE off — HID over I2C has no core' >&2; exit 1; }; \
                 grep -q '^CONFIG_HID_MULTITOUCH=y' .config || { echo 'HID_MULTITOUCH off — the T14s precision touchpad stays a relative mouse with no finger tool' >&2; exit 1; }; \
                 grep -q '^CONFIG_SND_HDA_CODEC_REALTEK=y' .config || { echo 'SND_HDA_CODEC_REALTEK off — the T14s Realtek codec falls back to the generic parser' >&2; exit 1; }; \
                 grep -q '^CONFIG_SND_HDA_CODEC_ALC269=y' .config || { echo 'SND_HDA_CODEC_ALC269 off — the ALC257 family on the T14s has no Realtek fixups' >&2; exit 1; }; \
                 grep -q '^CONFIG_GPIO_ACPI=y' .config || { echo 'GPIO_ACPI off — the I2C touchpad gets no interrupt and GPIO-signalled ACPI events (_AEI) are lost' >&2; exit 1; }; \
                 grep -q '^CONFIG_IOMMU_SUPPORT=y' .config || { echo 'IOMMU_SUPPORT off — IRQ_REMAP and the IOMMU drivers stay hidden, so X86_X2APIC cannot be built' >&2; exit 1; }; \
                 grep -q '^CONFIG_AMD_IOMMU=y' .config || { echo 'AMD_IOMMU off — the AMD IOMMU goes unused: no interrupt remapping (an x2APIC handover falls back to xAPIC) and no DMA isolation' >&2; exit 1; }; \
                 grep -q '^CONFIG_IRQ_REMAP=y' .config || { echo 'IRQ_REMAP off — X86_X2APIC depends on it' >&2; exit 1; }; \
                 grep -q '^CONFIG_X86_X2APIC=y' .config || { echo 'X86_X2APIC off — firmware that hands over in x2APIC mode leaves the kernel with its APIC disabled' >&2; exit 1; }; \
                 grep -q '^CONFIG_WIRELESS=y' .config || { echo 'WIRELESS off — no wireless stack' >&2; exit 1; }; \
                 grep -q '^CONFIG_CFG80211=y' .config || { echo 'CFG80211 off — no nl80211, so td-netd cannot drive a radio' >&2; exit 1; }; \
                 grep -q '^CONFIG_MAC80211=y' .config || { echo 'MAC80211 off — ath11k needs the soft-MAC stack' >&2; exit 1; }; \
                 grep -q '^CONFIG_WLAN=y' .config || { echo 'WLAN off — the wireless driver menu hides ath11k' >&2; exit 1; }; \
                 grep -q '^CONFIG_WLAN_VENDOR_ATH=y' .config || { echo 'WLAN_VENDOR_ATH off — the Atheros/Qualcomm menu hides ath11k' >&2; exit 1; }; \
                 grep -q '^CONFIG_ATH11K=y' .config || { echo 'ATH11K off — the T14s WCN6855 Wi-Fi has no driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_ATH11K_PCI=y' .config || { echo 'ATH11K_PCI off — the WCN6855 is a PCI device' >&2; exit 1; }; \
                 grep -q '^CONFIG_FW_LOADER=y' .config || { echo 'FW_LOADER off — no firmware reaches ath11k' >&2; exit 1; }; \
                 grep -q '^CONFIG_MHI_BUS=y' .config || { echo 'MHI_BUS off — ath11k_pci boots the WCN6855 over MHI' >&2; exit 1; }; \
                 grep -q '^CONFIG_QRTR=y' .config || { echo 'QRTR off — ath11k talks QMI to the WCN6855 over QRTR' >&2; exit 1; }; \
                 grep -q '^CONFIG_QRTR_MHI=y' .config || { echo 'QRTR_MHI off — QRTR has no transport to the WCN6855' >&2; exit 1; }; \
                 grep -q '^CONFIG_QCOM_QMI_HELPERS=y' .config || { echo 'QCOM_QMI_HELPERS off — ath11k has no QMI client' >&2; exit 1; }; \
                 grep -qx 'CONFIG_EXTRA_FIRMWARE=\"ath11k/WCN6855/hw2.0/amss.bin ath11k/WCN6855/hw2.0/m3.bin ath11k/WCN6855/hw2.0/board-2.bin ath11k/WCN6855/hw2.0/regdb.bin\"' .config || { echo 'EXTRA_FIRMWARE is not the WCN6855 set — ath11k probes before any root and finds no firmware' >&2; exit 1; }; \
                 grep -qx 'CONFIG_EXTRA_FIRMWARE_DIR=\"td-firmware\"' .config || { echo 'EXTRA_FIRMWARE_DIR is not the build-tree firmware directory' >&2; exit 1; }; \
                 if grep -q '^CONFIG_CFG80211_REQUIRE_SIGNED_REGDB=y' .config; then echo 'CFG80211_REQUIRE_SIGNED_REGDB on — it selects the trusted keyring and its openssl-linked certs/ tool' >&2; exit 1; fi; \
                 if grep -q '^CONFIG_SYSTEM_TRUSTED_KEYRING=y' .config; then echo 'SYSTEM_TRUSTED_KEYRING on — a select overrode its pin, and certs/ would need openssl' >&2; exit 1; fi; \
                 grep -q '^CONFIG_HYPERVISOR_GUEST=y' .config || { echo 'HYPERVISOR_GUEST off — guest support stays hidden, so no hypervisor drivers can be chosen' >&2; exit 1; }; \
                 grep -q '^CONFIG_PARAVIRT=y' .config || { echo 'PARAVIRT off — Hyper-V and KVM guest support need the paravirt layer' >&2; exit 1; }; \
                 grep -q '^CONFIG_KVM_GUEST=y' .config || { echo 'KVM_GUEST off — a KVM cloud guest (EC2 Nitro, GCE) has no kvmclock or paravirt EOI' >&2; exit 1; }; \
                 grep -q '^CONFIG_HYPERV=y' .config || { echo 'HYPERV off — Hyper-V (Azure) guest support is off' >&2; exit 1; }; \
                 grep -q '^CONFIG_HYPERV_VMBUS=y' .config || { echo 'HYPERV_VMBUS off — the synthetic devices of Azure are on VMBus, so neither its disk nor its NIC appears' >&2; exit 1; }; \
                 grep -q '^CONFIG_SCSI_LOWLEVEL=y' .config || { echo 'SCSI_LOWLEVEL off — SCSI host drivers stay hidden, so storvsc and virtio-scsi cannot be chosen' >&2; exit 1; }; \
                 grep -q '^CONFIG_HYPERV_STORAGE=y' .config || { echo 'HYPERV_STORAGE off — the OS disk of an Azure VM (storvsc) is invisible' >&2; exit 1; }; \
                 grep -q '^CONFIG_HYPERV_NET=y' .config || { echo 'HYPERV_NET off — the synthetic NIC of an Azure VM (netvsc) has no driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_PCI_HYPERV=y' .config || { echo 'PCI_HYPERV off — an Azure size with an NVMe disk controller presents it over Hyper-V vPCI, so its OS disk is invisible' >&2; exit 1; }; \
                 grep -q '^CONFIG_NET_VENDOR_AMAZON=y' .config || { echo 'NET_VENDOR_AMAZON off — the Amazon NIC drivers stay hidden' >&2; exit 1; }; \
                 grep -q '^CONFIG_ENA_ETHERNET=y' .config || { echo 'ENA_ETHERNET off — the ENA NIC of an EC2 Nitro instance has no driver' >&2; exit 1; }; \
                 grep -q '^CONFIG_SCSI_VIRTIO=y' .config || { echo 'SCSI_VIRTIO off — a GCE persistent disk on virtio-scsi is invisible' >&2; exit 1; }; \
                 grep -q '^CONFIG_NET_VENDOR_GOOGLE=y' .config || { echo 'NET_VENDOR_GOOGLE off — the Google NIC drivers stay hidden' >&2; exit 1; }; \
                 grep -q '^CONFIG_GVE=y' .config || { echo 'GVE off — a GCE machine series that requires gVNIC has no network' >&2; exit 1; }; \
                 grep -q '^CONFIG_DMI=y' .config || { echo 'DMI off — the firmware tables that name a cloud are not scanned' >&2; exit 1; }; \
                 grep -q '^CONFIG_DMIID=y' .config || { echo 'DMIID off — /sys/class/dmi/id is missing, so a server cannot tell which cloud it runs on' >&2; exit 1; }; \
                 grep -q '^CONFIG_HYPERV_TIMER=y' .config || { echo 'HYPERV_TIMER off — an Azure VM has no Hyper-V clocksource. It is unprompted (def_bool HYPERV && X86), so something took HYPERV away' >&2; exit 1; }; \
                 if grep -q '^CONFIG_IPC_NS=y' .config; then echo 'IPC_NS on — it is default y behind SYSVIPC||POSIX_MQUEUE, so pinning either brings it along unasked; td-jail omits CLONE_NEWIPC and APPLICATIONS.md §0 defers it deliberately' >&2; exit 1; fi".replace("@EFI_CMDLINE@", &format!("{:?}", crate::shared::efi_default_cmdline())),
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
            "{src}/arch/x86/boot/bzImage".into(),
            "{root}/initramfs.cpio".into(),
        ],
        dest: "{out}".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/vmlinux".into(),
            "{out}/bzImage".into(),
            "{out}/initramfs.cpio".into(),
        ],
        exec: false,
    });
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
    Recipe::mesboot("linux-x86-64", "7.2.12")
        .source_input("linux-kernel-source")
        .build_root()
        .native_inputs(&["td-sh", "td-init", "td-util"])
        .inputs(&firmware_pins())
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

/// Every pin the kernel stages: the firmware it embeds. Its notices ship
/// in the image from `linux-firmware-notices`.
fn firmware_pins() -> Vec<&'static str> {
    FIRMWARE.iter().map(|(pin, _)| *pin).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(
                text.contains(&format!("grep -q '^CONFIG_{symbol}=y' .config")),
                "{symbol}"
            );
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
            assert!(
                text.contains(&format!("grep -q '^CONFIG_{symbol}=y' .config")),
                "{symbol}"
            );
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
            assert!(
                text.contains(&format!("grep -q '^CONFIG_{symbol}=y' .config")),
                "{symbol}"
            );
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
            assert!(
                text.contains(&format!("grep -q '^CONFIG_{symbol}=y' .config")),
                "{symbol}"
            );
        }
    }

    /// The T14s's Wi-Fi: ath11k built in, with linux-firmware's files
    /// staged from their pins and named by EXTRA_FIRMWARE.
    #[test]
    fn wcn6855_wifi_is_builtin_with_its_firmware_embedded() {
        let recipe = recipe();
        let pins: Vec<String> = firmware_pins().into_iter().map(String::from).collect();
        assert_eq!(recipe.inputs, Some(pins));
        let firmware: Vec<&str> = FIRMWARE.iter().map(|(pin, _)| *pin).collect();
        let mut roster = crate::source_pins::DEVICE_FIRMWARE.to_vec();
        let mut staged = firmware.clone();
        roster.sort_unstable();
        staged.sort_unstable();
        assert_eq!(
            staged, roster,
            "the kernel stages exactly the firmware roster"
        );
        let steps = recipe.steps.unwrap_or_default();
        for (pin, name) in FIRMWARE {
            assert!(steps.iter().any(|step| matches!(
                step,
                Step::Run { argv, .. }
                    if *argv == [
                        CP.to_string(),
                        "--".into(),
                        format!("{{in:{pin}}}"),
                        format!("{{src}}/td-firmware/{name}"),
                    ]
            )));
        }
        let text = steps
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join("\n")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let names = FIRMWARE
            .iter()
            .map(|(_, name)| *name)
            .collect::<Vec<_>>()
            .join(" ");
        let extra = format!("'CONFIG_EXTRA_FIRMWARE=\"{names}\"'");
        assert_eq!(text.matches(&extra).count(), 2, "pinned and guarded");
        assert_eq!(
            text.matches("'CONFIG_EXTRA_FIRMWARE_DIR=\"td-firmware\"'")
                .count(),
            2
        );
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
            assert!(text.contains(&format!("grep -q '^CONFIG_{symbol}=y' .config")));
        }
        assert!(text.contains("if grep -q '^CONFIG_CFG80211_REQUIRE_SIGNED_REGDB=y' .config; then"));
        // Neither source patch fails the build when absent: hw2.1 silicon
        // would ask for files nothing embeds, and the keyring would return.
        // A linux-firmware tag bump must re-check that WHENCE still links
        // hw2.1's files to hw2.0's.
        for (file, from, to) in [
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
        assert!(text.contains("if grep -q '^CONFIG_SYSTEM_TRUSTED_KEYRING=y' .config; then"));
        for symbol in [
            "FW_LOADER",
            "MHI_BUS",
            "QRTR",
            "QRTR_MHI",
            "QCOM_QMI_HELPERS",
        ] {
            assert!(!text.contains(&format!("'CONFIG_{symbol}=y'")), "{symbol}");
            assert!(text.contains(&format!("grep -q '^CONFIG_{symbol}=y' .config")));
        }
    }

    /// The roster reaches the graph only through the kernel, on any input
    /// channel, so the closure query's `device-firmware` lines, which read
    /// the kernel's inputs, cannot miss a copy; and every linux-firmware
    /// pin is the roster or a notice the image ships.
    #[test]
    fn only_the_kernel_names_device_firmware() {
        for (stem, recipe) in crate::catalog::all() {
            let named = recipe
                .inputs
                .iter()
                .chain(recipe.native_inputs.iter())
                .chain(recipe.payload_inputs.iter())
                .flatten()
                .chain(recipe.source_input.iter());
            for key in named {
                if crate::source_pins::DEVICE_FIRMWARE.contains(&key.as_str()) {
                    assert_eq!(stem, "linux-x86-64", "{stem} names {key}");
                }
            }
        }
        let notices = super::super::linux_firmware_notices::NOTICES;
        for pin in crate::source_pins::all() {
            if pin.url.contains("/kernel-firmware/linux-firmware/") {
                assert!(
                    crate::source_pins::DEVICE_FIRMWARE.contains(&pin.key.as_str())
                        || notices.iter().any(|(key, _)| *key == pin.key),
                    "{} is neither firmware nor a shipped notice",
                    pin.key
                );
            }
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
            assert!(
                text.contains(&format!("grep -q '^CONFIG_{symbol}=y' .config")),
                "{symbol}"
            );
        }
        // No prompt: guarded only.
        assert!(!text.contains("CONFIG_HYPERV_TIMER[ =]/d"));
        assert!(!text.contains("'CONFIG_HYPERV_TIMER=y'"));
        assert!(text.contains("grep -q '^CONFIG_HYPERV_TIMER=y' .config"));
        assert!(!text.contains("'CONFIG_DMI=y'"));
        assert!(text.contains("grep -q '^CONFIG_DMI=y' .config"));
        // EC2 presents EBS as NVMe; x2APIC is pinned for the T14s.
        assert!(text.contains("'CONFIG_BLK_DEV_NVME=y'"));
        assert!(text.contains("grep -q '^CONFIG_X86_X2APIC=y' .config"));
        // Each guard message is a single-quoted shell word. The guard script
        // is one line, so every message in it is split out.
        let messages = text
            .split("{ echo '")
            .skip(1)
            .chain(text.split("then echo '").skip(1))
            .collect::<Vec<_>>();
        assert!(messages.len() > 100);
        for rest in messages {
            let message = rest.split("' >&2").next().unwrap_or_default();
            assert!(!message.contains('\''), "{message}");
        }
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
            assert!(text.contains(&format!("grep -q '^CONFIG_{symbol}=y' .config")));
        }
        assert!(text.contains("'# CONFIG_NVME_MULTIPATH is not set'"));
        assert!(text.contains("grep -q '^# CONFIG_NVME_MULTIPATH is not set' .config"));
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
            assert!(
                text.contains(&format!("grep -q '^CONFIG_{symbol}=y' .config ||")),
                "{symbol}"
            );
        }
        // Unavailable beside SYSFB_SIMPLEFB (EFIDRM, VESADRM) or off by
        // allnoconfig (efifb, vesafb): never written, refused if set.
        for symbol in ["DRM_EFIDRM", "DRM_VESADRM", "FB_EFI", "FB_VESA"] {
            assert!(!text.contains(&format!("'CONFIG_{symbol}=")), "{symbol}");
            assert!(
                text.contains(&format!(
                    "if grep -q '^CONFIG_{symbol}=' .config; then echo '{symbol} set"
                )),
                "{symbol}"
            );
        }
        let cmdline = format!("{:?}", crate::shared::efi_default_cmdline());
        assert!(cmdline.contains("console=tty0 console=ttyS0,115200"));
        assert!(text.contains(&format!("'CONFIG_CMDLINE={cmdline}'")));
        assert!(text.contains(&format!("grep -Fxq 'CONFIG_CMDLINE={cmdline}' .config")));
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
        assert!(text.contains("grep -q '^CONFIG_BLK_DEV_RAM=y' .config"));
        // Anchored, so a resolved count of 10 or 16 cannot satisfy it.
        assert!(text.contains("grep -q '^CONFIG_BLK_DEV_RAM_COUNT=1$' .config"));
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
                text.contains(&format!("grep -q '^CONFIG_{symbol}=y' .config")),
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
            assert!(
                text.contains(&format!("grep -q '^CONFIG_{symbol}=y$' .config")),
                "{symbol} guard"
            );
        }
        assert!(text.contains("'CONFIG_LSM=\"\"'"));
        assert!(text.contains("grep -Fxq 'CONFIG_LSM=\"\"' .config"));
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
            assert!(
                text.contains(&format!("if grep -q '^CONFIG_{symbol}=' .config")),
                "{symbol} guard"
            );
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
            "grep -q '^CONFIG_PERF_EVENTS=y' .config",
            "grep -q '^CONFIG_SMP=y$' .config",
            "grep -q '^CONFIG_NR_CPUS=256$' .config",
            "grep -q '^CONFIG_HOTPLUG_CPU=y$' .config",
        ] {
            assert!(
                command_text.contains(required),
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
            "grep -q '^CONFIG_AUDIT=y' .config",
            "grep -q '^CONFIG_AUDITSYSCALL=y' .config",
            "/^#? *CONFIG_COMPAT_32BIT_TIME[ =]/d",
            "CONFIG_COMPAT_32BIT_TIME=y",
            "grep -q '^CONFIG_COMPAT_32BIT_TIME=y$' .config",
            "/^#? *CONFIG_IA32_EMULATION[ =]/d",
            "CONFIG_IA32_EMULATION=y",
            "grep -q '^CONFIG_IA32_EMULATION=y$' .config",
            "/^#? *CONFIG_IA32_EMULATION_DEFAULT_DISABLED[ =]/d",
            "# CONFIG_IA32_EMULATION_DEFAULT_DISABLED is not set",
            "grep -q '^# CONFIG_IA32_EMULATION_DEFAULT_DISABLED is not set$' .config",
            "/^#? *CONFIG_X86_X32_ABI[ =]/d",
            "# CONFIG_X86_X32_ABI is not set",
            "grep -q '^# CONFIG_X86_X32_ABI is not set$' .config",
        ] {
            assert!(
                command_text.contains(required),
                "kernel Firefox audit contract omitted {required}"
            );
        }
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
            "grep -q '^CONFIG_CFS_BANDWIDTH=y' .config",
            "grep -q '^CONFIG_RT_GROUP_SCHED=y' .config",
        ] {
            assert!(
                command_text.contains(required),
                "kernel CPU bandwidth contract omitted {required}"
            );
        }
    }
}
