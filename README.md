# td

An immutable, Rust-first Linux distribution — built entirely from source,
from a tiny auditable seed all the way up to the running image.

td boots onto a **read-only, content-addressed root**. Every package lives
at its own `/td/store/<hash>-name` path, and the system you run is exactly
the artifact graph that was built — from the `stage0-posix` seed on up. No
host compiler and no downloaded binary ends up in the image you boot, and
nothing in the root is mutable.

## Highlights

- **Immutable** — the root filesystem is a read-only erofs image. `/bin`
  is a pure symlink farm into `/td/store`; there is no `/usr` or `/sbin`.
  `/var` is persistent Btrfs state; `/run` and `/tmp` are volatile tmpfs.
- **Built from source** — bootstrapped from the tiny `stage0-posix` seed
  through an iterative GCC/glibc ladder and a source-built Rust toolchain.
  Host `/bin`, `/usr`, and ambient `PATH` never enter a build. (The one
  downloaded trust root — the pinned Rust bootstrap snapshot — is rebuilt
  from source and never reaches the final image.)
- **Rust-first userland** — the core file and text tools are Rust uutils;
  the boot/login/shell path is a static busybox.
- **Content-addressed** — store paths, offline builds, and fixed-output
  sources verified by SHA-256. Deployment updates are verified into a hidden
  directory, flushed, atomically published by manifest hash, and activated
  with a retained verified previous deployment. Once a fallback exists, new
  deployments receive three durable boot attempts; the first deployment is
  trusted because it has nowhere to roll back. A healthy target acknowledges
  its exact deployment, while an exhausted candidate automatically rolls back.
  A corrupt current selector is durably repaired to its verified previous
  deployment, and an explicit `td-boot rollback` remains available. Update
  transactions and boot selection are serialized per block device through
  unmount; verified read-only recovery remains available when a writable
  bookkeeping transaction is unavailable and confirms health without attempting
  to mutate that state.
  Invalid bookkeeping ownership or modes are not repaired automatically:
  verified recovery remains bootable, but a root operator must inspect and
  repair the state before normal update acknowledgement resumes.

## Requirements

- A Linux **x86-64** host with unprivileged user namespaces enabled — the
  build sandbox is rootless, so no `sudo` and no host `/td` directory is
  needed (the store is assembled inside the sandbox).
- A Rust toolchain (`cargo`) to build td's control-plane tools.
- QEMU (`qemu-system-x86_64`) to boot the image.

## Try it

The examples use `td-recipe-eval`, shorthand for `cargo run --release
--manifest-path recipes/Cargo.toml --bin td-recipe-eval --`.

Build the system and boot it under QEMU:

```sh
td-recipe-eval run system-x86-64
```

It boots a selector initramfs, verifies the current deployment on a persistent
Btrfs volume, kexecs that deployment, loop-mounts its read-only EROFS root, and
auto-logs you in as `tester` on the serial console. Type `exit` (or Ctrl-D) to
power off; Ctrl-A X force-quits QEMU. The private test volume lasts for this
interactive session and is discarded when QEMU exits; the headless
`qemu-boot-system` check first proves a pending deployment can acknowledge and
remain attempt-free, then recreates the volume, fails a candidate before the
health target for three boots, preserves `/var` through that reused-volume
sequence, and proves automatic rollback on the next boot. An explicitly
read-only disk pass exercises selector-side bookkeeping recovery; a separate
fixture proves corrupted-current fallback.

## Filesystem layout

```
/td/store/<hash>-<name>/   every package, content-addressed and read-only
/bin                       symlink farm → /td/store (busybox + uutils applets)
/etc                       generated, deployment-owned, immutable
/var                       persistent writable Btrfs @var subvolume
/run /tmp                  volatile writable tmpfs
/home /root                symlinks into /var
```

## Defining the system

A whole distribution is one Rust recipe. `recipes/src/recipes/system-x86-64.rs`
composes the kernel, busybox, and uutils into a boot selector plus a deployment
bundle containing `{bzImage, initramfs.cpio, root.erofs, manifest}`. Edit its
`SYSTEM` constant to tailor the hostname, users, auto-login, login shell, and
applet set, then `td-recipe-eval run` again.

Individual packages are recipes too — declarative Rust, no shell:

```rust
pub fn recipe() -> Recipe {
    Recipe::rust("fd", "10.2.0")
        .source_pin(SourcePin::new(
            "fd-source",
            "https://static.crates.io/crates/fd-find/fd-find-10.2.0.crate",
            "de08defa195af894cc295a43bfc65ba28903e492fd5f32f7a24bf75eafd9bf34",
            "fd-find-10.2.0.crate",
        ))
        .bins(&["fd"])
        .no_default_features()
        .features(&["completions"])
}
```

## Building

```sh
td-recipe-eval list                  # list every recipe
td-recipe-eval build-run <name>      # build one recipe into /td/store
td-recipe-eval qemu-boot-system      # headless boot proof (pass/fail)
```

## License

GPL-3.0-or-later. See [COPYING](COPYING).
