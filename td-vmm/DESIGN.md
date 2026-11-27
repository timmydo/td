# td-vmm

td-vmm runs a guest as the unprivileged process that opened the sandbox's
private KVM device. It uses the host's Linux x86-64 KVM API and builds
offline from the checkout. In a new session of a native td-sandbox with
`td-sandbox device NAME kvm on`, from a checkout and with the two image
paths readable by the sandbox account, run

```sh
cargo build --locked --release --manifest-path td-vmm/Cargo.toml
td-vmm/target/release/td-vmm /path/to/bzImage /path/to/initramfs.cpio 'console=ttyS0 panic=-1 rdinit=/init'
```

The diskless kernel recipe's initramfs writes `TD-USERLAND-OK` from td-sh
and reboots. The caller owns the wall-time and output bounds and checks
the guest's evidence. A process exit alone establishes no guest verdict.

## Direct boot

One vCPU starts in 32-bit protected mode with a flat GDT, paging off,
the Linux boot parameters in ESI and the protected-mode payload at
its preferred address (its linked address for a fixed kernel). The loader prepares a zeroed parameter page, a bounded command
line, an E820 memory map and a page-aligned initramfs below the kernel's
address limit in 256 MiB of anonymous RAM. Both input files are read
only. The guest can address only its own memory. KVM supplies the PIC,
IOAPIC, local APIC and PIT. A read-only CMOS clock supplies UTC. COM1 provides transmit and its IRQ on line
4. The machine terminates on a guest keyboard-controller reset. A KVM
shutdown exit, including a triple fault, is an error.

The ABI follows the [Linux x86 boot protocol](https://docs.kernel.org/arch/x86/boot.html)
and the [Linux KVM API](https://docs.kernel.org/virt/kvm/api.html).

## Raw block device

Append `--disk-ro IMAGE` to present one raw disk as `/dev/vda`.
`--disk-rw IMAGE` gives the guest write access to the existing image
and takes an exclusive file lock. Use a disposable copy for installation
and recovery tests. Read-only sessions use a shared lock and open the
image read-only. Images have a nonzero size in whole 512-byte sectors.

The guest kernel's legacy virtio PCI driver discovers one block function,
one 128-entry split queue, an I/O BAR and INTx on IRQ 11. Device requests
use checked guest physical offsets and sector ranges. A malformed ring
fails the run. The disk supports scatter/gather reads and writes, flush
and device identity. Every request is served while the vCPU is stopped.
The read-only EROFS fixture's initramfs prints `TD-EROFS-RO-OK` only
after the kernel mounts the attached image and td-util reads its expected
sentinel. Its device interface is specified by
[Virtio 1.2](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html).

## Integration tier design

The runner interface is a machine description owned by td. Each integration
oracle retains ownership of its fixtures, timeouts, power-cut points and
verdict. These are the requirements for selecting the runner for each
existing step. Each step checks its own guest or management evidence.

| Integration workloads | Machine and evidence required |
| --- | --- |
| VM lifecycle, workspace and enrollment | Independent instance disks, exclusive ownership across supervisor loss, live run state and isolated force-stop for two simultaneous machines. |
| Headless server and admin update | Persistent block storage, guest-initiated reboot, cloud seed media and metadata, a network with the test feed and loopback SSH forwards, and real guest health/rollback evidence. |
| System and deployment rollback | Selector and kexec handoffs, writable Btrfs media, physical input, captured display and serial channels, and evidence from the unmodified services and desktop. |
| Audio routing and hotplug | HDA and USB audio devices with separately captured PCM streams, timed device insertion and removal, and the existing waveform checks. |
| Live media and installation | Firmware boot from the produced media, writable EFI variables, the disk controllers and boot order of each scenario, then boot from the installed disk. |

Each backend needs its own oracle memo identity, including the monitor
and device implementation that produced the result. The integration
runner counts a step as passed only after it observes the td behavior
that step specifies. The PostgreSQL steps retain their existing native runners. The
`vm-workspace` step runs the workspace and enrollment cases using
td-vm's own disk operations. The independent `vm-lifecycle` step runs
two real KVM guests on
private raw disks, kills one supervisor, proves lock retention and
duplicate-launch and deletion refusal, then force-stops each guest
through its own endpoint.

With `--control SOCKET`, the initialized machine binds a mode-0600 Unix
datagram socket in a private directory. The four bytes `stop` request a
power cut, including while the vCPU is halted. The manager holds the
instance lock when replacing a stale endpoint or requesting a stop.
The guest has no access to this host endpoint.

## Host boundary

Only `src/kvm.rs` can enter the kernel through raw calls, adopt a newly
created descriptor, or borrow a mapping as bytes. It opens the fixed
`/dev/kvm` path without following its final link, verifies character
device 10:232 and API 12, and passes only value-pinned requests. The VM
and vCPU are private to one thread. The registered RAM mapping outlives
both descriptors. Userspace borrows RAM before registration and the
vCPU run page between synchronous runs. All exit-data ranges are checked
against the mapping. The optional disk is an explicitly named regular file. Device I/O uses
positioned reads and writes on the memory file after the vCPU exits. No
Rust slice aliases memory registered with KVM. The syscall inventory is recorded in
`UNSAFE.md`.
