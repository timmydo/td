# Reencryption metadata interoperability oracle

`metadata.rs` is a disposable host/PID-1 QEMU helper. It uses the library's
start, dirty-hotzone, completion and header commit APIs, and the existing
device-mapper module to produce old/new ciphertext. It checks every resulting
semantic header with pinned cryptsetup 2.8.8, asks stock repair/resume to finish
it, opens it with stock and compares all 2 MiB of known plaintext.

There are 22 cases: for each of checksum and journal resilience, the initial
start, each of two dirty hotzones after zero, 4096, 524288 and 1048576 bytes
have been rewritten, and each hotzone's completion. The last completion goes
directly to ordinary metadata while retaining the old keyslot and its empty
segment binding. No case uses userspace volume-data encryption.

This proves on-disk metadata and recovery compatibility. It does not implement
or prove td-luks's live reencryption stack, production data rewriting, recovery
or CLI. The helper constructs partial-write states in memory and installs each
one on a fresh disposable guest disk state; it does not simulate torn sectors
or reordered writes. The ordinary Rust tests separately interrupt every
start/final header write and sync, checking which valid copy survives.

## Run

Use a Linux x86-64 host with KVM, QEMU, the pinned kernel and td-init, and the
cryptsetup 2.8.8 KERNEL_CAPI output. The helper checks its exact version banner.
The host needs GCC and matching static glibc for this test binary; provision
those as host test tools, never as target recipe inputs. If static glibc is
outside the compiler's search path, set `LIBRARY_PATH` to its library directory.

From the repository root, choose a fresh static build directory:

```text
mkdir -p .td-build-cache/agent-scratch
RUSTFLAGS='-C relocation-model=static -C target-feature=+crt-static -C linker=gcc' cargo build --release --lib --manifest-path td-luks/Cargo.toml --target-dir .td-build-cache/agent-scratch/metadata-static
rustc --edition=2021 -Dunused -Copt-level=2 -Cpanic=abort -Crelocation-model=static -Ctarget-feature=+crt-static -Clinker=gcc -Lall=.td-build-cache/agent-scratch/metadata-static/release/deps --extern td_luks=.td-build-cache/agent-scratch/metadata-static/release/libtd_luks.rlib --extern td_json --extern td_fido td-luks/fixtures/metadata.rs -o .td-build-cache/agent-scratch/metadata-oracle
TMPDIR="$PWD/.td-build-cache/agent-scratch" .td-build-cache/agent-scratch/metadata-oracle KERNEL TD-INIT CRYPTSETUP NEW-OUTPUT-DIR
```

The output directory must not exist. Success retains `guest.log` and the final
`volume.img`; the log ends with `TD-LUKS-METADATA-PASS`. The helper owns its
scratch through `td-fs::TempDir`, reaps its QEMU child and imposes a ten-minute
deadline. All device-mapper calls occur in the guest. Remove the owned build,
helper and output directories after recording the result.
