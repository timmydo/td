# cryptsetup 2.8.8 fixtures

These five fixtures were captured in a QEMU/KVM guest running td's Linux
7.2.13, using the static, target-built cryptsetup 2.8.8 with `BLKID
KERNEL_CAPI`. `generate.rs` records the exact commands. Each `.bin` is the
first 1 MiB of the same 32 MiB virtio block device after the named operation;
each `.json` is stock `luksDump --dump-json-metadata` output at that point.
The binary bytes are copied unchanged. The prefix includes both 16 KiB
metadata copies and every live keyslot area. It is a detached-header
fixture, not a complete encrypted filesystem or a data-recovery fixture.

| State | Operation since the preceding capture |
| --- | --- |
| fresh | Format with td's profile, slot 0 and fixed volume key |
| added | Add slot 1 using slot 0 |
| token | Import td-protector token 0 naming slot 1 |
| killed | Kill slot 1, leaving token 0 with an empty keyslots list |
| unbound | Add an unbound slot 1 with its own digest and no segments |

All keys are public test data. Slot 0 uses 48 ASCII zeroes; slot 1 uses
`ABCDEFGHABCDEFGHABCDEFGHABCDEFGH`. The volume key is the 64 bytes 0 through
63, including for the unbound slot. Each capture also asks stock cryptsetup
to dump slot 0's volume key, compares it with those bytes, and checks that
the dump operations did not alter the captured prefix. Ordinary tests check
the keyslots, digests, token membership and both independent header copies.
They need neither AF_ALG nor cryptsetup nor QEMU.

This is increment B4a. Interrupted checksum/journal reencryption fixtures
and their stock-resumption proof remain B4b; these ordinary snapshots make
no reencryption or device-mapper interoperability claim.

## Regeneration

Run from the repository root. Supply the paths of the realized td recipes
`linux-x86-64` (`bzImage`), `td-init` (`bin/td-init`) and
`cryptsetup-x86-64` (`bin/cryptsetup`). The generator checks cryptsetup's
exact version/feature line, boots with KVM, has no guest network, and only
attaches two newly created disposable disks. It refuses an existing output
directory. `TMPDIR` must point inside this worktree's agent scratch.

```text
mkdir -p .td-build-cache/agent-scratch
rustc --edition 2021 -C opt-level=2 -C relocation-model=static \
  -C target-feature=+crt-static -C linker=gcc \
  td-luks/fixtures/generate.rs -o .td-build-cache/agent-scratch/luks-generate
TMPDIR="$PWD/.td-build-cache/agent-scratch" \
  .td-build-cache/agent-scratch/luks-generate \
  KERNEL/bzImage TD_INIT/bin/td-init CRYPTSETUP/bin/cryptsetup \
  .td-build-cache/agent-scratch/captured
```

The host needs its ordinary static C runtime development libraries for the
standalone, host-built capture helper. On Guix, expose the matching glibc
static output in the host's `LIBRARY_PATH` when compiling it. This helper is
test preparation, never a target recipe input. The guest runs no host file
or program other than the helper embedded in its initramfs.

The process exits successfully only after QEMU powers down and the output
disk carries the guest's completion marker. Inspect `captured/guest.log`,
which records the kernel, cryptsetup version and each command. Copy the
`.bin` and `.json` files into this directory together, then run:

```text
cargo test --release --manifest-path td-luks/Cargo.toml --test fixtures
```

Salts, AF split stripes and erased area bytes are random, so regeneration
produces different binary answers. Review the new hashes and metadata, run
the tests and record the generation inputs again. Delete owned disposable
capture output and the helper when finished.

## Capture receipt

Captured on 2026-10-27. SHA-256 of the three input executables and of each
committed binary follows (store basenames identify the realized inputs):

```text
dba2fe9c94f7a30ee586fe6a7c1661880685445448a30f8bbb06891c3cfeca8e  nqsafszr7s07038ch2hqs76j7bfz479m-linux-x86-64-7.2.13/bzImage
543cc39b962deb16944084ffdb0a764222326b49e6695f83abe58d9b7f797ae4  3bd08qbk9kmzrij1bq1d0zhqd7nzai4k-td-init-0.1/bin/td-init
5e9c95bb365f9cf65699443ed26007476818756bb3a39db13d9f48b7951d11ae  d40assx547y6h2cw82wk9vvnbysxy9mj-cryptsetup-x86-64-2.8.8/bin/cryptsetup
40b5a8724fb8f2466d942ecfdcd8c0e4b4c6fa8b8c2bf9ec974f1b6097a2f11d  td-luks/fixtures/added.bin
5023f8efcbcc497c5570cbd843896457ad6b160918dc2a261d9a25dcde2ec442  td-luks/fixtures/fresh.bin
d766dcdb1c0144109458fc86b59b761a737af7dd5d818333bc83909ee4654f42  td-luks/fixtures/killed.bin
bdd0bbd6e80f28bdd23dba2b9952f2ac1c26eed7e15c70a12875b305df7ad5a1  td-luks/fixtures/token.bin
2b80ed1216f6da433ac9c4bed623fd3f00c7ab770ae54962be4b24bfca039b6a  td-luks/fixtures/unbound.bin
```
