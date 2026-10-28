# Reencryption runtime oracle

`reencryption.rs` is a standalone host/PID-1 fixture, not a shipped tool.
It boots the td kernel and td-init with td-luks and the pinned cryptsetup
2.8.8 KERNEL_CAPI binary. Every mapping ioctl runs in the disposable guest;
the host creates an initramfs, an owned disk and a bounded QEMU child.
There is no host block-device access or ambient cryptsetup substitution.

Build the static program and oracle from the same source tree:

```text
LIBRARY_PATH=STATIC_GLIBC_LIB RUSTFLAGS='-C relocation-model=static -C target-feature=+crt-static -C linker=gcc' cargo build --release --manifest-path td-luks/Cargo.toml --target-dir .td-build-cache/agent-scratch/reencryption-static
LIBRARY_PATH=STATIC_GLIBC_LIB rustc --edition=2021 -Dunused -Copt-level=2 -Cpanic=abort -Crelocation-model=static -Ctarget-feature=+crt-static -Clinker=gcc -Lall=.td-build-cache/agent-scratch/reencryption-static/release/deps --extern td_luks=.td-build-cache/agent-scratch/reencryption-static/release/libtd_luks.rlib --extern td_json td-luks/fixtures/reencryption.rs -o .td-build-cache/agent-scratch/reencryption-oracle
TMPDIR=WORKTREE/.td-build-cache/agent-scratch .td-build-cache/agent-scratch/reencryption-oracle MODE KERNEL TD_INIT CRYPTSETUP .td-build-cache/agent-scratch/reencryption-static/release/td-luks NEW_OUTPUT_DIRECTORY
```

These are host preparation inputs, not recipe dependencies. `STATIC_GLIBC_LIB`
names the host's static glibc library directory. Paths must be absolute for
kernel, td-init and cryptsetup. The oracle checks cryptsetup's exact banner.
Run once for each mode below, with a distinct nonexistent output directory.
A successful run retains `guest.log` and `volume.img`; the owned temporary
initramfs and working disk are removed. Each guest has a 600-second deadline.

| Mode | Evidence |
| --- | --- |
| `smoke` | Partial stack setup and final teardown close without leaked helpers, while a foreign helper UUID survives. Both resilience modes reclaim an owned orphan temporary map, reencrypt 32 MiB, remove helper maps and preserve plaintext through both tools' open/close. |
| `prefixes` | td-luks opens and recovers all 92 supported stock write prefixes, completes each through its real resume command, and stock reopens the same 2 MiB plaintext. The 14 unsafe all-new profiles are excluded as in the offline tests. |
| `concurrent` | A separate writer writes, syncs and reads back distributed 4096-byte sectors during 32 MiB online reencryption. Both tools verify the final mixed plaintext pattern. |
| `interrupt` | The owner kills its td-luks child after observing a dirty hotzone and suspended helper, checks deferred temporary-map removal, closes the retained stack, then finishes with each tool. Both modes restore every byte of the 32 MiB plaintext. |
| `locks` | Simultaneous UUID and exclusive header ownership promptly refuses td-luks start/resume/open. The UUID lock alone excludes td-luks start (status 1) and stock resume (status 5), both promptly. Stock init-only takes the metadata lock instead and succeeds under the UUID lock alone; td-luks then resumes that stock initialization after release. |

The kernel profile, metadata profile, keys and cipher parameters are those
in DESIGN.md. `replay.rs` reconstructs the committed stock prefixes, whose
capture procedure and persistence boundary are recorded in
`reencrypt/README.md`. `metadata.rs` separately checks every constructed
start/hotzone/end metadata state with stock repair/resume.

The live interruption test is a process kill followed by explicit close,
not a power cut or proof that arbitrary blocked application I/O can be
released without rebooting. An interrupted retained stack may require
unmount/close or reboot before recovery. No mounted filesystem is involved.
Checksum recovery assumes atomic 4096-byte data writes, as the selector's
resilience choice requires. Journal recovery does not make that assumption.
The separate library fault-injection tests cover final-header persistence,
free-journal cleanup and restart ordering; the guest does not inject every
physical write, torn sector, device write-cache failure or ioctl failure.
