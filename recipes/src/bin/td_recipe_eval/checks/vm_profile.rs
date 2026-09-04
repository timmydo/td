//! The POSIX-sh launcher a release bundle ships, rendered from the qemu
//! profile `checks/run.rs` boots.
//!
//! Two things boot the td demo machine: `run.rs`, for whoever has the checkout
//! and can build it, and the `start` script in a bundle from `checks/bundle.rs`,
//! for everyone else. They must boot the SAME machine — a bundle that hands out
//! a differently-configured guest is a demo of something the project never
//! tests — so the script's qemu invocation is generated from the same token
//! lists `run.rs::interactive_command` passes to `Command`, and a test there
//! walks one against the other.
//!
//! Everything here is pure: constants and string rendering, no I/O and no
//! process spawning, so the entire launcher is unit-testable without a qemu.

use std::fmt::Write as _;

use crate::checks::qemu_boot::{
    QEMU_USER_NETDEV, QEMU_USER_NET_DEVICE, SYSTEM_GUEST_MEMORY_MIB,
};

/// Machine type. First on the command line, ahead of the accelerator.
pub(crate) const MACHINE: &[&str] = &["-M", "pc"];

/// The `-m` flag, called out because the launcher substitutes an operator
/// override for the value that follows it and nothing else in `platform()`.
const MEMORY_FLAG: &str = "-m";

/// The rest of the machine, in `run.rs`'s order: guest RAM, no reboot (a guest
/// reset exits qemu), no host config file, user-mode networking, virtio
/// graphics, a silent audio device, and an ABSOLUTE pointer — a relative PS/2
/// mouse accumulates deltas and leaves the right and bottom screen edges
/// unreachable.
pub(crate) fn platform() -> Vec<&'static str> {
    vec![
        MEMORY_FLAG,
        SYSTEM_GUEST_MEMORY_MIB,
        "-no-reboot",
        "-no-user-config",
        "-vga",
        "none",
        "-netdev",
        QEMU_USER_NETDEV,
        "-device",
        QEMU_USER_NET_DEVICE,
        "-device",
        "virtio-vga",
        "-audiodev",
        "none,id=audio0",
        "-device",
        "intel-hda",
        "-device",
        "hda-output,audiodev=audio0",
        "-device",
        "virtio-tablet-pci",
    ]
}

/// Kernel command line. No `panic=-1`: an operator wants a panic left on screen
/// to read. No autotest token, so the greeter is an ordinary interactive shell.
pub(crate) const APPEND: &str = "console=ttyS0 rdinit=/init";

/// The blockdev id tying `-drive` to `-device`.
///
/// Written in three places — this launcher's `-drive`, `DISK_DEVICE` below,
/// and `qemu_boot::drive_arg` for the Rust runner — and a mismatch is NOT a
/// compile error. qemu fails at startup with `Property 'virtio-blk-pci.drive'
/// can't find value '...'`. The drift test substitutes the whole `-drive`
/// value away before comparing, so without the tests that pin every site to
/// this constant, renaming the id would leave the suite green and every
/// shipped bundle unbootable.
pub(crate) const DRIVE_ID: &str = "disk0";

/// The blk device that binds the volume `-drive` declares.
pub(crate) const DISK_DEVICE: &str = "virtio-blk-pci,drive=disk0";

/// Payload names inside a bundle. The launcher hard-codes them, so they are
/// part of the bundle's shape rather than a caller's choice.
pub(crate) const KERNEL_NAME: &str = "bzImage";
pub(crate) const INITRD_NAME: &str = "selector-initramfs.cpio";
pub(crate) const LAUNCHER_NAME: &str = "start";
pub(crate) const CHECKSUMS_NAME: &str = "SHA256SUMS";
pub(crate) const README_NAME: &str = "README.md";

/// The environment variable the launcher reads for guest RAM.
pub(crate) const MEMORY_ENV: &str = "TD_VM_MEMORY";

/// How a qcow2 volume's clusters are compressed.
///
/// Measured on td's own 1.7 GiB image: zlib in qcow2's default 64 KiB clusters
/// gives 626 MiB, zstd in 1 MiB clusters gives 549 MiB — 12% off the download
/// for nothing but a convert flag. The cluster size is half of that and only
/// helps zstd: DEFLATE's window is 32 KiB, so a bigger cluster gives it nothing
/// (zlib at 1 MiB measured 629 MiB, slightly WORSE), while zstd compresses
/// across the whole cluster.
///
/// The cost is that zstd-compressed qcow2 is unreadable by qemu older than 5.1,
/// which is why this is a choice a bundle records rather than a constant, and
/// why the shipped README states the version the image it ships actually needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    Zstd,
    Zlib,
}

impl Compression {
    /// Every variant, for tests that must cover all of them rather than the
    /// ones somebody remembered. The `match` arms in this impl already fail to
    /// compile on a new variant, which is where its author will be; this keeps
    /// the tests from being the one place that stays quietly green. It does
    /// not enforce its own completeness — nothing in Rust can, for a list —
    /// so it sits beside those matches where the author is already looking.
    #[cfg(test)]
    const ALL: &'static [Compression] = &[Compression::Zstd, Compression::Zlib];

    /// The cluster size `qemu_img_options` asks for, in prose for the README.
    /// Pinned to that flag by a test — a README that describes a cluster size
    /// the image was not written with is the same defect as the `1 MiB` this
    /// replaced, one level down.
    /// zstd gets a large cluster because it compresses across the whole of one;
    /// DEFLATE's 32 KiB window cannot, so zlib keeps qcow2's default.
    pub(crate) fn cluster_size_label(self) -> &'static str {
        match self {
            Compression::Zstd => "1 MiB",
            Compression::Zlib => "64 KiB",
        }
    }

    /// The `-o` argument for `qemu-img convert`.
    pub(crate) fn qemu_img_options(self) -> &'static str {
        match self {
            Self::Zstd => "cluster_size=1M,compression_type=zstd",
            // Named rather than left implicit so the two invocations differ
            // only in this string, and a reader can see what the default was.
            Self::Zlib => "cluster_size=64k",
        }
    }
}

/// How the persistent volume is stored in a bundle.
///
/// The raw volume `create_persistent_volume` writes is `MIN_VOLUME_BYTES`
/// (5 GiB) of mostly-empty Btrfs. qcow2 stores only the allocated clusters and
/// qemu boots it directly, so it is what a download should be; raw is the
/// fallback for a host with no `qemu-img`, which is a qemu install missing one
/// optional tool rather than a reason to refuse to make a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiskFormat {
    Qcow2(Compression),
    Raw,
}

impl DiskFormat {
    /// The bundle file name. The extension names the format, so an operator
    /// looking at a bundle directory can see which one they have. Compression
    /// does NOT change it: both are qcow2, and a reader choosing a file should
    /// not have to know which codec produced it.
    pub(crate) fn file_name(self) -> &'static str {
        match self {
            Self::Qcow2(_) => "td-system.qcow2",
            Self::Raw => "td-system.img",
        }
    }

    /// The `format=` value in qemu's `-drive`. Named EXPLICITLY rather than
    /// left to qemu's probe: the bundle always knows which one it wrote, and
    /// probing image contents is a qemu footgun with a CVE history.
    pub(crate) fn qemu_format(self) -> &'static str {
        match self {
            Self::Qcow2(_) => "qcow2",
            Self::Raw => "raw",
        }
    }

    /// The oldest qemu that can run what this bundle ships.
    ///
    /// It belongs in the README because it is the one requirement a bundle can
    /// silently violate: a qemu that refuses the image reports an unknown
    /// compression type, and one that refuses the launcher reports an unknown
    /// option — neither says "your qemu is too old".
    ///
    /// Two floors, and the answer is whichever is higher. The disk codec sets
    /// one (5.1 is where qcow2 zstd landed). The LAUNCHER sets the other, and
    /// it applies to every bundle: `PLATFORM` emits `-audiodev`, which is
    /// qemu 4.0 and later. Reporting the codec's floor alone told a zlib or raw
    /// bundle's reader that qemu 2.4 would do, and 2.4 rejects the command line
    /// before it ever looks at the disk.
    pub(crate) fn minimum_qemu(self) -> &'static str {
        let codec_floor = match self {
            Self::Qcow2(Compression::Zstd) => (5, 1),
            Self::Qcow2(Compression::Zlib) | Self::Raw => (2, 4),
        };
        if codec_floor >= LAUNCHER_MINIMUM_QEMU_PARTS {
            match self {
                Self::Qcow2(Compression::Zstd) => "5.1",
                _ => LAUNCHER_MINIMUM_QEMU,
            }
        } else {
            LAUNCHER_MINIMUM_QEMU
        }
    }
}

/// The oldest qemu that accepts the launcher's command line, independent of
/// the disk. `-audiodev` in `platform()` is the binding option: it arrived in
/// qemu 4.0, so no bundle can claim less however its volume is stored.
pub(crate) const LAUNCHER_MINIMUM_QEMU: &str = "4.0";
const LAUNCHER_MINIMUM_QEMU_PARTS: (u32, u32) = (4, 0);

/// Width the rendered qemu invocation wraps at. Comfortably inside 80 columns
/// with the continuation indent, so the shipped script reads in a narrow
/// terminal.
const WRAP_COLUMNS: usize = 66;

/// Group `tokens` so a flag stays with the value it carries.
///
/// A new group starts at every option (`-device`) and every shell expansion
/// (`$accel`); anything else joins the group before it. So `-device` and
/// `virtio-vga` are one unit that wrapping will not separate — a reader
/// scanning the shipped launcher for what the machine has should not have to
/// reassemble `-device` on one line with its value on the next.
fn groups(tokens: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in tokens {
        let starts_group = token.starts_with('-') || token.starts_with('$');
        match out.last_mut() {
            Some(group) if !starts_group => {
                group.push(' ');
                group.push_str(token);
            }
            _ => out.push((*token).to_string()),
        }
    }
    out
}

/// Join `tokens` into shell continuation lines no wider than `WRAP_COLUMNS`,
/// with `indent` before every line after the first and a trailing `\` on every
/// line but the last.
///
/// Wrapping happens between `groups`, never inside one, and a group too wide
/// for the budget takes a line of its own rather than being split: a split
/// qemu argument is a different argument.
fn wrap(tokens: &[&str], indent: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for group in groups(tokens) {
        match lines.last_mut() {
            Some(line) if line.len() + 1 + group.len() <= WRAP_COLUMNS => {
                line.push(' ');
                line.push_str(&group);
            }
            _ => lines.push(group),
        }
    }
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push_str(indent);
        }
        out.push_str(line);
        if index + 1 < lines.len() {
            out.push_str(" \\\n");
        }
    }
    out
}

/// The qemu argument list the launcher execs, as shell words.
///
/// Built from the same token lists `run.rs` passes to `Command`, with the five
/// pieces only a shell can supply spliced in: `$accel` (the probe's result),
/// `$memory`, `$display` (empty when a host display is reachable), and the
/// payload paths.
fn qemu_words() -> Vec<&'static str> {
    let mut words: Vec<&'static str> = Vec::new();
    words.extend_from_slice(MACHINE);
    // Unquoted on purpose: `$accel` and `$display` are argument LISTS the shell
    // must word-split, and both hold only qemu option names.
    words.push("$accel");
    let mut after_memory_flag = false;
    for token in platform() {
        if after_memory_flag {
            // The one value an operator may override, so the launcher reads it
            // from a variable that defaults to this very token.
            words.push("\"$memory\"");
            after_memory_flag = false;
            continue;
        }
        after_memory_flag = token == MEMORY_FLAG;
        words.push(token);
    }
    words.push("$display");
    words.push("-serial");
    words.push("mon:stdio");
    words.push("-kernel");
    words.push("\"$kernel\"");
    words.push("-initrd");
    words.push("\"$initrd\"");
    words.push("-append");
    words.push("\"$append\"");
    words.push("-drive");
    words.push("\"$drive\"");
    words.push("-device");
    words.push(DISK_DEVICE);
    words
}

/// Render the `start` script a bundle ships.
///
/// `format` decides the disk file name and the `format=` qemu reads it with;
/// nothing else varies, so a bundle made on a host without `qemu-img` differs
/// from one made with it in exactly those two strings.
pub(crate) fn launcher_script(format: DiskFormat) -> String {
    let mut out = String::new();
    // A generated file that looks hand-written invites an edit the next bundle
    // silently discards, so say where it comes from on line 2.
    out.push_str(
        "#!/bin/sh\n\
         # td demo VM launcher. GENERATED by `td-recipe-eval bundle` from\n\
         # recipes/src/bin/td_recipe_eval/checks/vm_profile.rs — edit the generator,\n\
         # not this copy.\n\
         #\n\
         # Boots the prebuilt td system beside it under QEMU: a graphical virtio\n\
         # framebuffer when this host has a display, and an interactive serial\n\
         # console on this terminal either way.\n\
         set -eu\n\
         \n\
         # Every payload is named relative to the bundle, so ./start works from any\n\
         # directory and the bundle can be moved or renamed as a unit.\n\
         case \"$0\" in\n\
         \x20   */*) here=${0%/*} ;;\n\
         \x20   *) here=. ;;\n\
         esac\n\
         cd \"$here\"\n\
         \n",
    );
    let _ = writeln!(out, "kernel={KERNEL_NAME}");
    let _ = writeln!(out, "initrd={INITRD_NAME}");
    let _ = writeln!(out, "disk={}", format.file_name());
    let _ = writeln!(out, "format={}", format.qemu_format());
    let _ = writeln!(out, "append='{APPEND}'");
    let _ = writeln!(
        out,
        "memory=${{{MEMORY_ENV}:-{SYSTEM_GUEST_MEMORY_MIB}}}"
    );
    out.push_str(
        "\n\
         usage() {\n\
         \x20   cat <<'USAGE'\n\
         usage: ./start [--persist] [--help]\n\
         \n\
         Boots the td demo system under QEMU. The guest's serial console is wired\n\
         to this terminal, so the shell is right here; when this host has a\n\
         display, QEMU also opens a window on the graphical framebuffer.\n\
         \n\
         \x20 --persist   Keep guest writes. By default the guest runs on a throwaway\n\
         \x20             overlay: the shipped image is never modified and every boot\n\
         \x20             starts from the same state. With --persist, writes go into\n\
         \x20             the image file itself and its checksum stops matching.\n\
         \n\
         Environment:\n\
         \x20 TD_QEMU_ACCEL  force `kvm`, `hvf` or `tcg` instead of probing\n\
         \x20 TD_VM_MEMORY   guest RAM in MiB\n\
         \n\
         In the guest: `exit` or Ctrl-D at the shell shuts the machine down and\n\
         QEMU exits. To force-quit QEMU at any time: Ctrl-A then X.\n\
         USAGE\n\
         }\n\
         \n\
         persist=0\n\
         while [ $# -gt 0 ]; do\n\
         \x20   case $1 in\n\
         \x20       --persist) persist=1 ;;\n\
         \x20       -h|--help) usage; exit 0 ;;\n\
         \x20       *)\n\
         \x20           printf 'start: unknown option %s\\n' \"$1\" >&2\n\
         \x20           usage >&2\n\
         \x20           exit 2\n\
         \x20           ;;\n\
         \x20   esac\n\
         \x20   shift\n\
         done\n\
         \n\
         # PATH first, then the places a distribution puts qemu when PATH is bare —\n\
         # the same list td's own runner searches.\n\
         qemu=$(command -v qemu-system-x86_64 2>/dev/null) || qemu=\n\
         if [ -z \"$qemu\" ]; then\n\
         \x20   for dir in /run/current-system/profile/bin /usr/bin /usr/local/bin /bin; do\n\
         \x20       if [ -x \"$dir/qemu-system-x86_64\" ]; then\n\
         \x20           qemu=$dir/qemu-system-x86_64\n\
         \x20           break\n\
         \x20       fi\n\
         \x20   done\n\
         fi\n\
         if [ -z \"$qemu\" ]; then\n\
         \x20   echo 'start: qemu-system-x86_64 not found. Install QEMU (Debian/Ubuntu:' >&2\n\
         \x20   echo '       qemu-system-x86; Fedora: qemu-system-x86; Arch: qemu-full;' >&2\n\
         \x20   echo '       macOS: brew install qemu) and run ./start again.' >&2\n\
         \x20   exit 1\n\
         fi\n\
         \n\
         # An incomplete download is the likeliest way this goes wrong, and qemu's\n\
         # own message for a missing -kernel never mentions the bundle.\n\
         for payload in \"$kernel\" \"$initrd\" \"$disk\"; do\n\
         \x20   if [ ! -f \"$payload\" ]; then\n\
         \x20       printf 'start: %s is missing from this bundle.\\n' \"$payload\" >&2\n\
         \x20       echo '       Download every file listed in SHA256SUMS into one' >&2\n\
         \x20       echo '       directory, then run ./start from there.' >&2\n\
         \x20       exit 1\n\
         \x20   fi\n\
         done\n\
         \n\
         # Hardware acceleration accelerates only a guest of the HOST's own\n\
         # architecture, so an x86_64 guest is emulated on anything else no matter\n\
         # what the host permits. Both boot; one boots minutes faster.\n\
         accel=\n\
         case \"${TD_QEMU_ACCEL:-}\" in\n\
         \x20   kvm) accel='-accel kvm' ;;\n\
         \x20   hvf) accel='-accel hvf' ;;\n\
         \x20   tcg) accel='-accel tcg' ;;\n\
         \x20   '')\n\
         \x20       # tcg stays behind the fast accelerator in every branch: one that\n\
         \x20       # opens can still be refused at VM-creation time, and qemu then\n\
         \x20       # walks on to the next -accel instead of exiting.\n\
         \x20       if [ \"$(uname -m)\" != x86_64 ]; then\n\
         \x20           accel='-accel tcg'\n\
         \x20           echo 'start: this host is not x86_64, so the guest is emulated' >&2\n\
         \x20           echo '       instruction by instruction and boots several times' >&2\n\
         \x20           echo '       slower. No device permission changes that.' >&2\n\
         \x20       elif [ \"$(uname -s)\" = Darwin ]; then\n\
         \x20           accel='-accel hvf -accel tcg'\n\
         \x20           echo 'start: trying the Hypervisor.framework accelerator.' >&2\n\
         \x20       elif ( exec 3<>/dev/kvm ) 2>/dev/null; then\n\
         \x20           # Opened the way qemu opens it (O_RDWR) rather than tested for\n\
         \x20           # mode bits: an ACL, or a group this shell does not see, makes\n\
         \x20           # -r/-w disagree with the open that actually decides. Opening\n\
         \x20           # the control node creates no VM and the descriptor closes\n\
         \x20           # with the subshell.\n\
         \x20           accel='-accel kvm -accel tcg'\n\
         \x20           echo 'start: using KVM.' >&2\n\
         \x20       else\n\
         \x20           accel='-accel tcg'\n\
         \x20           echo 'start: /dev/kvm did not open — the guest is emulated' >&2\n\
         \x20           echo '       instruction by instruction and boots several times' >&2\n\
         \x20           echo '       slower. Read/write access to /dev/kvm (usually' >&2\n\
         \x20           echo '       membership in the `kvm` group, then a fresh login)' >&2\n\
         \x20           echo '       is what makes this fast.' >&2\n\
         \x20       fi\n\
         \x20       ;;\n\
         \x20   *)\n\
         \x20       printf 'start: TD_QEMU_ACCEL=%s is not an accelerator; use kvm, hvf or tcg.\\n' \\\n\
         \x20           \"$TD_QEMU_ACCEL\" >&2\n\
         \x20       exit 2\n\
         \x20       ;;\n\
         esac\n\
         \n\
         # A terminal-only host gets no display frontend; the virtio framebuffer\n\
         # stays attached and the serial console is still interactive. macOS has\n\
         # neither DISPLAY nor WAYLAND_DISPLAY and still has a window server, so\n\
         # testing only for those two suppressed qemu's native Cocoa window on\n\
         # every Mac and left the serial console as the whole interface.\n\
         display='-display none'\n\
         wayland_socket=\n\
         case \"${WAYLAND_DISPLAY:-}\" in\n\
         \x20   '') ;;\n\
         \x20   # Absolute since Wayland 1.21, and then it is the whole path\n\
         \x20   # rather than a name under XDG_RUNTIME_DIR. Joining it onto the\n\
         \x20   # runtime dir anyway yields /run/user/1000//abs/path, which never\n\
         \x20   # exists — so the launcher fell back to serial-only on exactly the\n\
         \x20   # hosts where the Rust runner opens a window.\n\
         \x20   /*) wayland_socket=$WAYLAND_DISPLAY ;;\n\
         \x20   *) wayland_socket=${XDG_RUNTIME_DIR:-}/$WAYLAND_DISPLAY ;;\n\
         esac\n\
         if [ \"$(uname -s)\" = Darwin ]; then\n\
         \x20   display=\n\
         elif [ -n \"${DISPLAY:-}\" ]; then\n\
         \x20   display=\n\
         elif [ -n \"$wayland_socket\" ] && [ -e \"$wayland_socket\" ]; then\n\
         \x20   display=\n\
         fi\n\
         \n\
         # snapshot=on keeps every guest write in a temporary overlay qemu discards\n\
         # on exit, so the downloaded image stays byte-identical to its checksum.\n\
         drive=\"if=none,format=$format,id=disk0,file=$disk\"\n\
         if [ \"$persist\" -eq 0 ]; then\n\
         \x20   drive=\"$drive,snapshot=on\"\n\
         fi\n\
         \n\
         # $accel and $display are argument lists the shell must word-split; every\n\
         # path is quoted. The guest boots td's selector, which verifies and kexecs\n\
         # the deployment on /dev/vda, mounts its read-only EROFS root and @var, and\n\
         # switch_roots into it.\n\
         exec \"$qemu\" \\\n\
         \x20   ",
    );
    out.push_str(&wrap(&qemu_words(), "    "));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the module: the shipped launcher boots the machine
    /// `run.rs` boots. `run.rs` owns the stronger test — it walks the actual
    /// `Command` against this script — but a token silently dropped from
    /// `platform()` should red here too, next to the renderer that dropped it.
    #[test]
    fn the_launcher_carries_every_profile_token() {
        let script = launcher_script(DiskFormat::Qcow2(Compression::Zstd));
        for token in MACHINE.iter().copied().chain(platform()) {
            // The memory VALUE is deliberately replaced by `$memory`; it still
            // has to survive as that variable's default.
            if token == SYSTEM_GUEST_MEMORY_MIB {
                continue;
            }
            assert!(script.contains(token), "launcher dropped {token}");
        }
        assert!(script.contains(APPEND), "launcher dropped the kernel cmdline");
        assert!(script.contains(DISK_DEVICE), "launcher dropped the disk device");
        assert!(
            script.contains(&format!("memory=${{{MEMORY_ENV}:-{SYSTEM_GUEST_MEMORY_MIB}}}")),
            "the RAM override must default to the profile's own size"
        );
    }

    /// Rendering must not fuse two arguments into one word. `-vga none` and
    /// `-audiodev none,id=audio0` both carry `none`, so a wrap bug that ate a
    /// space would still satisfy a naive `contains` on either name.
    #[test]
    fn platform_arguments_stay_separate_words() {
        let script = launcher_script(DiskFormat::Qcow2(Compression::Zstd));
        let words: Vec<String> = script
            .split_whitespace()
            .filter(|word| *word != "\\")
            .map(str::to_string)
            .collect();
        for token in platform() {
            if token == SYSTEM_GUEST_MEMORY_MIB {
                continue;
            }
            assert!(
                words.iter().any(|word| word == token),
                "{token} is not a word of its own in the rendered script"
            );
        }
    }

    /// The format is the only thing the disk choice changes, and it has to
    /// change BOTH the file the launcher looks for and the format qemu opens
    /// it with — a qcow2 opened as raw boots nothing recognisable.
    #[test]
    fn the_disk_format_reaches_both_the_name_and_the_drive() {
        for &compression in Compression::ALL {
            let qcow2 = launcher_script(DiskFormat::Qcow2(compression));
            assert!(qcow2.contains("disk=td-system.qcow2"));
            assert!(qcow2.contains("format=qcow2"));
            assert!(!qcow2.contains("td-system.img"));
        }

        let raw = launcher_script(DiskFormat::Raw);
        assert!(raw.contains("disk=td-system.img"));
        assert!(raw.contains("format=raw"));
        assert!(!raw.contains("td-system.qcow2"));
    }

    /// The default has to be the non-destructive one: a downloaded image whose
    /// first boot rewrites it can never be checked against `SHA256SUMS` again,
    /// and the operator has no way back short of downloading it a second time.
    #[test]
    fn writes_are_thrown_away_unless_persist_is_asked_for() {
        let script = launcher_script(DiskFormat::Qcow2(Compression::Zstd));
        assert!(script.contains("persist=0"));
        assert!(script.contains("snapshot=on"));
        assert!(script.contains("if [ \"$persist\" -eq 0 ]; then"));
    }

    /// `set -eu` plus an unset variable is an exit, and each of these is read
    /// on a path where the operator may never have set it.
    #[test]
    fn every_optional_variable_is_expanded_with_a_default() {
        let script = launcher_script(DiskFormat::Qcow2(Compression::Zstd));
        for name in ["DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", MEMORY_ENV] {
            let bare = format!("${{{name}}}");
            assert!(
                !script.contains(&bare),
                "{name} is expanded with no default, which `set -u` turns into an exit"
            );
        }
        // TD_QEMU_ACCEL is read bare only inside the arm that has already
        // proved it set and non-empty — the one place `set -u` cannot fire.
        assert!(script.contains("\"${TD_QEMU_ACCEL:-}\""));
    }

    #[test]
    fn an_oversized_group_gets_its_own_line_rather_than_being_split() {
        let long = "a".repeat(WRAP_COLUMNS + 10);
        let rendered = wrap(&["-x", &long, "-y"], "  ");
        assert!(rendered.contains(&long), "a wrapped token was split");
        // `-x <long>` is one group and outgrows the budget on its own; `-y`
        // starts a group and so cannot join it.
        assert_eq!(rendered.lines().count(), 2);
        assert!(rendered.starts_with(&format!("-x {long} \\\n")));
    }

    /// A flag separated from its value across a line break is legal shell and
    /// unreadable documentation — and the launcher is the file a new user reads
    /// to find out what the VM actually is.
    #[test]
    fn a_flag_never_leaves_its_value_on_another_line() {
        let script = launcher_script(DiskFormat::Qcow2(Compression::Zstd));
        let (_, exec_line) = script
            .split_once("exec \"$qemu\"")
            .expect("the launcher execs qemu");
        for line in exec_line.lines() {
            let words: Vec<&str> = line
                .split_whitespace()
                .filter(|word| *word != "\\")
                .collect();
            // A line may not END on a bare option that takes a value. Options
            // that take none are the exception, so check the pairing instead:
            // the last word may be an option only if it is one of those.
            if let Some(last) = words.last() {
                if last.starts_with('-') {
                    assert!(
                        ["-no-reboot", "-no-user-config"].contains(last),
                        "line ends on {last}, separating it from its value:\n{line}"
                    );
                }
            }
        }
    }

    #[test]
    fn wrapping_keeps_every_token_and_its_order() {
        let tokens = ["-M", "pc", "-m", "2048", "-no-reboot", "-vga", "none"];
        let rendered = wrap(&tokens, "    ");
        let flat: Vec<&str> = rendered
            .split_whitespace()
            .filter(|word| *word != "\\")
            .collect();
        assert_eq!(flat, tokens);
    }

    /// The one class of defect no assertion above can see: a syntax error.
    ///
    /// `sh -n` parses without executing, and reads the program on stdin, so
    /// this is a real POSIX-shell parse of the exact bytes a bundle ships —
    /// no temporary file, no qemu, nothing run. A launcher that does not parse
    /// fails on the user's machine with a message about the generator they do
    /// not have, so the cost of finding out late is high.
    ///
    /// An absent `/bin/sh` is reported and tolerated rather than failing:
    /// td's own gate sandbox is host-free by construction, and a test that
    /// demands a host shell would red there for a reason unrelated to the
    /// launcher. Every host that cuts a release has one.
    #[test]
    fn the_rendered_launcher_parses_as_posix_sh() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        for format in [DiskFormat::Qcow2(Compression::Zstd), DiskFormat::Raw] {
            let script = launcher_script(format);
            let spawned = Command::new("/bin/sh")
                .arg("-n")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();
            let Ok(mut child) = spawned else {
                eprintln!("/bin/sh is unavailable; the launcher was not parse-checked");
                return;
            };
            let stdin = child.stdin.as_mut().expect("piped stdin");
            stdin
                .write_all(script.as_bytes())
                .expect("write the launcher to sh");
            let output = child.wait_with_output().expect("sh -n");
            assert!(
                output.status.success(),
                "the rendered {format:?} launcher is not valid POSIX sh:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }


    /// The codec changes the file's BYTES, never the launcher: qemu reads the
    /// compression out of the qcow2 header itself. A launcher that varied here
    /// would be a second place for the two to disagree.
    #[test]
    fn compression_does_not_reach_the_launcher() {
        let zstd = launcher_script(DiskFormat::Qcow2(Compression::Zstd));
        let zlib = launcher_script(DiskFormat::Qcow2(Compression::Zlib));
        assert_eq!(zstd, zlib);
    }

    /// The qemu floor a bundle states must cover BOTH the image it wrote and
    /// the launcher beside it. Saying 2.4 over a zstd image sends a user to a
    /// qemu that cannot open it; saying 2.4 over any bundle at all sends them
    /// to one that rejects `-audiodev` before it reaches the disk.
    #[test]
    fn the_stated_qemu_floor_covers_the_launcher_and_the_codec() {
        assert_eq!(DiskFormat::Qcow2(Compression::Zstd).minimum_qemu(), "5.1");
        // The launcher's floor, not the codec's 2.4.
        assert_eq!(
            DiskFormat::Qcow2(Compression::Zlib).minimum_qemu(),
            LAUNCHER_MINIMUM_QEMU
        );
        assert_eq!(DiskFormat::Raw.minimum_qemu(), LAUNCHER_MINIMUM_QEMU);
        // Compared as NUMBERS, not as strings. `"10.0" < "4.0"` lexicographically,
        // so a string comparison here would silently invert the first time a
        // floor reached double digits — and pass, saying nothing.
        let parts = |version: &str| -> (u32, u32) {
            let mut fields = version.split('.');
            let major = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
            let minor = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
            (major, minor)
        };
        assert!(parts("10.0") > parts(LAUNCHER_MINIMUM_QEMU));
        for format in [
            DiskFormat::Qcow2(Compression::Zstd),
            DiskFormat::Qcow2(Compression::Zlib),
            DiskFormat::Raw,
        ] {
            assert!(
                parts(format.minimum_qemu()) >= parts(LAUNCHER_MINIMUM_QEMU),
                "{format:?} claims a qemu older than the launcher needs"
            );
        }
    }

    /// The option that sets the launcher floor has to still be in the profile.
    /// If `-audiodev` were dropped, the floor would be overstated rather than
    /// wrong — but if a NEWER option is added, the floor becomes a lie, and
    /// this is where a reader is reminded to check.
    #[test]
    fn the_launcher_floor_names_an_option_the_profile_still_emits() {
        assert!(platform().contains(&"-audiodev"));
        assert_eq!(LAUNCHER_MINIMUM_QEMU, "4.0");
    }

    /// Only zstd asks for the big cluster, because only zstd can use it.
    #[test]
    fn the_cluster_size_travels_with_the_codec() {
        let zstd = Compression::Zstd.qemu_img_options();
        assert!(zstd.contains("compression_type=zstd"));
        assert!(zstd.contains("cluster_size=1M"));
        let zlib = Compression::Zlib.qemu_img_options();
        assert!(!zlib.contains("compression_type"));
        assert!(zlib.contains("cluster_size=64k"));
    }


    /// The blockdev id is written in three files and a mismatch is a startup
    /// failure, not a compile error. These pin every site to `DRIVE_ID` so a
    /// rename cannot pass: the launcher's `-drive`, the `-device` that binds
    /// it, and the Rust runner's own `drive_arg`.
    #[test]
    fn every_site_that_names_the_blockdev_uses_one_id() {
        assert_eq!(DISK_DEVICE, format!("virtio-blk-pci,drive={DRIVE_ID}"));

        let script = launcher_script(DiskFormat::Qcow2(Compression::Zstd));
        assert!(
            script.contains(&format!("id={DRIVE_ID},")),
            "the launcher's -drive does not declare {DRIVE_ID}"
        );

        // The runner's half, so the two cannot drift apart either.
        let rust_drive = crate::checks::qemu_boot::drive_arg(
            std::path::Path::new("volume.btrfs"),
            false,
        );
        assert!(
            rust_drive.to_string_lossy().contains(&format!("id={DRIVE_ID},")),
            "drive_arg does not declare {DRIVE_ID}"
        );

        // Every `-device` that binds a drive comes from DISK_DEVICE. The gate
        // qemu-boot path had its own hardcoded copy: those boots red loudly
        // rather than shipping, so it was not the bundle hazard above, but a
        // rename would still have broken them with nothing to say why.
        let sources = [
            include_str!("qemu_boot.rs"),
            include_str!("run.rs"),
            include_str!("bundle.rs"),
            include_str!("vm_profile.rs"),
        ];
        // Derived from the constant rather than written out, so this test's
        // own source does not contain the string it is searching for.
        let needle = DISK_DEVICE.trim_end_matches(DRIVE_ID);
        assert!(needle.ends_with('='));
        for source in sources {
            for (number, line) in source.lines().enumerate() {
                // The definition and the lines that derive from it name
                // DISK_DEVICE or DRIVE_ID; a hardcoded fourth site names
                // neither, which is exactly the one this is looking for.
                let is_literal = line.contains(needle)
                    && !line.contains("DISK_DEVICE")
                    && !line.contains("DRIVE_ID");
                assert!(
                    !is_literal,
                    "line {} spells the blk device out instead of using DISK_DEVICE: {}",
                    number + 1,
                    line.trim()
                );
            }
        }
    }

    /// The README's prose and the flag qemu-img was actually given.
    ///
    /// These drift silently: nothing links a sentence to a `-o` value, and the
    /// last time they disagreed the README told every reader the image used
    /// 1 MiB clusters when `--zlib` had written 64 KiB ones.
    #[test]
    fn the_cluster_size_prose_matches_the_flag_the_image_was_written_with() {
        for &compression in Compression::ALL {
            let options = compression.qemu_img_options();
            let label = compression.cluster_size_label();
            // "1 MiB" -> "1M", "64 KiB" -> "64k": the same number, in the
            // suffix qemu-img spells it with.
            let (value, unit) = label.split_at(
                label.find(' ').unwrap_or(label.len()),
            );
            let suffix = match unit.trim() {
                "MiB" => "M",
                "KiB" => "k",
                other => panic!("unhandled cluster unit {other}"),
            };
            let expected = format!("cluster_size={value}{suffix}");
            assert!(
                options.contains(&expected),
                "{compression:?} says {label} but passes {options}"
            );
        }
    }

    /// A launcher qemu never sees is not a launcher.
    #[test]
    fn the_script_is_a_sh_program_that_execs_qemu() {
        let script = launcher_script(DiskFormat::Qcow2(Compression::Zstd));
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("set -eu"));
        assert!(script.contains("exec \"$qemu\""));
        assert!(script.ends_with('\n'));
    }
}
