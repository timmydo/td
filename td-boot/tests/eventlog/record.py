"""Record OVMF TPM event logs under swtpm for td-boot's prediction tests.

Usage: python3 -I record.py KERNEL SELECTOR_CPIO SWTPM WORKDIR

A host fixture, not a recipe and not a build, test or runtime input: it
needs the host's qemu-system-x86_64 with its bundled edk2 (OVMF) firmware,
KVM, and the swtpm the encrypted oracles pin. It boots two chains, each
KERNEL as EFI/BOOT/BOOTX64.EFI from a vvfat ESP, on q35 with an swtpm
TPM 2.0 on tpm-tis as the encrypted oracles attach it, each with a fresh
TPM and a fresh copy of the firmware variables. Each INITRD is the
selector initramfs followed by an archive whose /init mounts securityfs
and prints the event log (od hex) and SHA-256 PCRs 4, 7 and 9 on the
serial console, then exits (panic=-1, -no-reboot). Chain b is chain a
with 512 zero bytes after the kernel, which Authenticode hashes, and 512
after the initramfs, which the kernel's unpacker skips.

Writes WORKDIR/ovmf-a.log, WORKDIR/ovmf-b.log and WORKDIR/ovmf.txt: per
chain its three PCRs, the kernel's plain SHA-256, the SHA-256 digest of
its log's one PCR 4 boot-application event (`application`, which the
tests require td-boot's Authenticode to reproduce) and the INITRD's
SHA-256.
"""
import hashlib
import os
import shutil
import struct
import subprocess
import sys
import time

INIT = b"""#!/bin/sh
/bin/mount -t devtmpfs dev /dev
/bin/mount -t proc proc /proc
/bin/mount -t sysfs sysfs /sys
/bin/mount -t securityfs securityfs /sys/kernel/security
/bin/td-util printf '1 1 1 1\\n' > /proc/sys/kernel/printk
/bin/td-util printf 'TD-LOG-BEGIN\\n'
/bin/td-util od -A n -t x1 -v /sys/kernel/security/tpm0/binary_bios_measurements
/bin/td-util printf 'TD-LOG-END\\n'
/bin/td-util printf 'TD-PCR 4 '
/bin/td-util cat /sys/class/tpm/tpm0/pcr-sha256/4
/bin/td-util printf 'TD-PCR 7 '
/bin/td-util cat /sys/class/tpm/tpm0/pcr-sha256/7
/bin/td-util printf 'TD-PCR 9 '
/bin/td-util cat /sys/class/tpm/tpm0/pcr-sha256/9
/bin/td-util printf 'TD-DONE\\n'
"""


def newc(entries):
    out = bytearray()
    ino = 1
    for name, mode, data in entries + [("TRAILER!!!", 0, b"")]:
        nm = name.encode() + b"\0"
        hdr = "070701" + "".join(
            "%08X" % v
            for v in (ino, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(nm), 0)
        )
        out += hdr.encode() + nm
        while len(out) % 4:
            out += b"\0"
        out += data
        while len(out) % 4:
            out += b"\0"
        ino += 1
    while len(out) % 512:
        out += b"\0"
    return bytes(out)


def application(log):
    """The SHA-256 digest of the log's one PCR 4 boot-application event."""
    size = struct.unpack_from("<I", log, 28)[0]
    spec = log[32:32 + size]
    count = struct.unpack_from("<I", spec, 24)[0]
    sizes = dict(struct.unpack_from("<HH", spec, 28 + 4 * i) for i in range(count))
    at = 32 + size
    found = []
    while at < len(log):
        pcr, kind, digests = struct.unpack_from("<III", log, at)
        at += 12
        sha256 = None
        for _ in range(digests):
            alg = struct.unpack_from("<H", log, at)[0]
            if alg == 0x000B:
                sha256 = log[at + 2:at + 2 + 32].hex()
            at += 2 + sizes[alg]
        at += 4 + struct.unpack_from("<I", log, at)[0]
        if pcr == 4 and kind == 0x80000003:
            found.append(sha256)
    if len(found) != 1:
        raise SystemExit("expected one PCR 4 boot application, found %d" % len(found))
    return found[0]


def boot(kernel, initrd, swtpm, work, name):
    """One boot of KERNEL and INITRD; returns the log and the PCR lines."""
    run = os.path.join(work, name)
    boot = os.path.join(run, "esp", "EFI", "BOOT")
    os.makedirs(boot)
    with open(os.path.join(boot, "BOOTX64.EFI"), "wb") as f:
        f.write(kernel)
    with open(os.path.join(boot, "INITRD"), "wb") as f:
        f.write(initrd)
    tpm = os.path.join(run, "tpm")
    os.makedirs(os.path.join(tpm, "state"))
    sock = os.path.join(tpm, "tpm.sock")
    emulator = subprocess.Popen(
        [swtpm, "socket", "--tpm2", "--tpmstate", "dir=state,mode=0600",
         "--ctrl", "type=unixio,path=tpm.sock,mode=0600,terminate"],
        cwd=tpm,
    )
    for _ in range(500):
        if os.path.exists(sock):
            break
        time.sleep(0.01)
    qemu = shutil.which("qemu-system-x86_64")
    share = os.path.join(os.path.dirname(os.path.dirname(os.path.realpath(qemu))), "share", "qemu")
    vars_copy = os.path.join(run, "vars.fd")
    shutil.copyfile(os.path.join(share, "edk2-i386-vars.fd"), vars_copy)
    serial = os.path.join(run, "serial.txt")
    subprocess.run(
        [qemu, "-M", "q35", "-accel", "kvm,kernel-irqchip=split", "-cpu", "Nehalem",
         "-m", "1024", "-no-reboot", "-display", "none", "-monitor", "none",
         "-no-user-config", "-vga", "none", "-nic", "none",
         "-serial", "file:" + serial,
         "-drive", "if=pflash,format=raw,unit=0,readonly=on,file="
         + os.path.join(share, "edk2-x86_64-code.fd"),
         "-drive", "if=pflash,format=raw,unit=1,readonly=off,file=" + vars_copy,
         "-chardev", "socket,id=tpm,path=" + sock,
         "-tpmdev", "emulator,id=tpm0,chardev=tpm",
         "-device", "tpm-tis,tpmdev=tpm0",
         "-drive", "if=none,id=esp,format=raw,file=fat:rw:" + os.path.join(run, "esp"),
         "-device", "virtio-blk-pci,drive=esp"],
        timeout=300,
        check=False,
    )
    emulator.wait(timeout=10)
    with open(serial, "rb") as f:
        text = f.read().decode("latin-1").replace("\r", "")
    lines = text.split("\n")
    start = lines.index("TD-LOG-BEGIN")
    end = lines.index("TD-LOG-END")
    log = bytes(int(b, 16) for line in lines[start + 1:end] for b in line.split())
    pcrs = []
    for line in lines[end + 1:]:
        if line.startswith("TD-PCR "):
            index, value = line[len("TD-PCR "):].split()
            pcrs.append("%s pcr%s %s" % (name, index, value.lower()))
    if len(pcrs) != 3:
        raise SystemExit("%s: PCRs missing from the console" % name)
    return log, pcrs


def main():
    kernel_path, selector, swtpm, work = sys.argv[1:5]
    os.makedirs(work, exist_ok=False)
    with open(kernel_path, "rb") as f:
        kernel = f.read()
    with open(selector, "rb") as f:
        initrd = f.read()
    while len(initrd) % 4:
        initrd += b"\0"
    initrd += newc([("init", 0o100755, INIT)])
    # Boot b's kernel carries 512 more bytes after its last section, which
    # Authenticode hashes, and its initramfs 512 more zero bytes, which the
    # kernel's unpacker skips: a second chain on the same firmware.
    chains = [("a", kernel, initrd), ("b", kernel + bytes(512), initrd + bytes(512))]
    lines = []
    for name, k, i in chains:
        log, pcrs = boot(k, i, swtpm, work, name)
        with open(os.path.join(work, "ovmf-%s.log" % name), "wb") as f:
            f.write(log)
        lines += pcrs
        lines.append("%s kernel-sha256 %s" % (name, hashlib.sha256(k).hexdigest()))
        lines.append("%s application %s" % (name, application(log)))
        lines.append("%s initrd %s" % (name, hashlib.sha256(i).hexdigest()))
        print(name, "log bytes", len(log))
    with open(os.path.join(work, "ovmf.txt"), "w") as f:
        f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
