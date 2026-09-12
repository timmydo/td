# Native QEMU installation fixture

This crate is a diagnostic PID-1 program for `td-recipe-eval qemu-install`.
It is built from source through the ordinary Rust ladder and is never packed
in the system or graphical installer profile. It adds no user-facing device
admission, destructive consent, account configuration or signing identity.

The host oracle exclusively creates each sparse target inside its private
scratch directory and exposes it as a writable virtio disk with the fixture
serial. The ISO stays read-only. The fixture refuses outside PID 1 and admits
exactly one whole virtio disk with that serial; it accepts no command-line
destination. This is test identification, not a real installer authorization
mechanism. Firmware variables are private copies, networking is disabled,
and the host boot runner owns deadlines and QEMU teardown. Each boot defaults
to 180 seconds; a positive TD_QEMU_BOOT_TIMEOUT_SECS overrides that limit.
A refusal keeps PID 1 alive for the host to collect its diagnostic and
terminate the VM at the deadline.

The complete crate tree is a local_source input pinned by
seed/seed-digests.txt. The catalog and affected-check routing include it in
local-source verification; it is not a production system input.

The live initramfs holds source-built tools, a tiny deployment signed under a
per-run throwaway key, the public trust root and the fixed selector. The
private key remains on the host outside derivations. The fixture calls the
actual td-install layout and volume primitives, including td-boot's verified
publication. Its success marker follows both successful commands and sync.

The host then detaches media and cold-boots only the destination through
firmware. The fixed selector authenticates and kexecs the installed tiny
deployment. Its init verifies and loop-mounts the installed EROFS payload,
reads its sentinel, and writes a synced count in Btrfs @var. A second cold
boot must observe and advance that count. The reported deployment ID must
match the host's signed manifest. Successful installed boots also acknowledge
that deployment through td-boot. An otherwise identical source offered under
a second public key must produce an authentication refusal and no installation
success marker. Both optical and USB installation boots
use the same ISO bytes and fresh destination disks.

The fixture's serial and partition-two convention do not implement production
volume discovery. It does not launch the compositor, configure an account,
retain an installation signing key, or test a full desktop deployment. Live
media payloads fit in RAM; this does not prove mounting ISO9660 from Linux.
The final installer still needs the activation evidence in
../td-install/INSTALLER.md, including stable volume identity and settings.

All operations use safe Rust and existing td-init/td-boot applets. No syscall
surface or external dependency is added. Unit tests cover refusal outside
PID 1; the host oracle is the executable integration test. A serial marker is
accepted only after its named operation and persistence barriers finish.
