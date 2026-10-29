# Activation interoperability oracle

`activation.rs` boots a disposable KVM guest with a statically linked
host test build of td-luks, the pinned cryptsetup 2.8.8 KERNEL_CAPI
binary, td-init and the test kernel. Host builds are test preparation;
none becomes a target recipe input. It never opens the host's
`/dev/mapper/control`. The helper's own ioctl module runs only as guest
PID 1, and is the same source the td-luks binary compiles.

Build td-luks into a private target directory with the host's normal GCC
and matching static glibc available (a static non-PIE executable is
required). For example, from the worktree root:

```text
mkdir -p .td-build-cache/agent-scratch/activation
RUSTFLAGS='-C relocation-model=static -C target-feature=+crt-static -C linker=gcc' cargo build --release --manifest-path td-luks/Cargo.toml --target-dir .td-build-cache/agent-scratch/activation/target
rustc --edition=2021 -Dunused -Copt-level=2 -Crelocation-model=static -Ctarget-feature=+crt-static -Clinker=gcc td-luks/fixtures/activation.rs -o .td-build-cache/agent-scratch/activation/oracle
TMPDIR="$PWD/.td-build-cache/agent-scratch/activation" .td-build-cache/agent-scratch/activation/oracle KERNEL TD-INIT CRYPTSETUP .td-build-cache/agent-scratch/activation/target/release/td-luks NEW-OUTPUT-DIR
```

If static glibc is installed separately, host compiler preparation sets
`LIBRARY_PATH` to its library directory for both builds. `NEW-OUTPUT-DIR`
must not exist. Inputs are the same pinned cryptsetup, kernel and td-init
used by the fixture generator in [README.md](README.md). The helper owns
and reaps QEMU, bounds the run to 180 seconds and requires the guest's
completion marker; success retains the disposable volume and guest log.
It checks the cryptsetup version before booting. The complete test needs
no host root privilege or host AF_ALG, only access to `/dev/kvm`.

The guest proves:

- stock-formatted volumes activate through td-luks, with byte-equal
  DM_TABLE_STATUS parameters, the expected UUID and backing dependency;
- wrong passphrases and wrong raw volume keys return 2; absent status
  returns 4; duplicate activation returns 1 and leaves the live table;
- stock recognizes and closes td-luks mappings; td-luks closes stock
  mappings, removing stock's block-node entry;
- two 4096-byte plaintext units written through td-luks's mapping read
  identically after reopening with stock, covering the plain64 IV unit;
- suspend/resume exposes the expected status, and a deliberately failing
  table load leaves no named mapping or inactive table;
- read-only activation with and without an explicit slot reads the same
  plaintext, reports read-only through device mapper and sysfs, refuses
  write access and duplicate writable opens, and leaves the complete
  backing disk byte-identical after close;
- raw-key activation verifies the digest; explicit-slot activation works;
- a stale symlink is replaced, while a regular mapper entry is preserved;
- three batches of 24 concurrent raw-key opens leave exactly one
  winner with its published mapper entry intact;
- a td-luks format opens with stock; an unbound slot tests successfully
  but cannot activate; a regular file cannot be activated.

The ordinary tests cover packet bounds, unsafe confinement and activation
credentials without a guest. This live oracle remains a manual increment-C
check until the cutover retires cryptsetup; it is not an offline unit test
and does not claim interrupted reencryption coverage.
