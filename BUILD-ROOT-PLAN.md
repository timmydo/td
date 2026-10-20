# Plan: replace td's C bootstrap with a Debian build root and upstream Rust

Status: proposal, 2026-10-19, branch `build-root-rolling`. Not normative.
Its trust-model text becomes the AGENTS.md / td-profiler/DESIGN.md
amendment in the first increment that uses the build root; this file is
deleted when the last increment lands. The spike scratch has been
deleted; its findings are summarized under "Facts" below.

## Decisions

| Question | Decision |
|---|---|
| Role of the container image | C build environment only. Its binaries, libraries, static archives and crt objects never ship. |
| Base | Debian 13 "trixie" slim, pinned by registry layer digest, plus pinned .debs from snapshot.debian.org (cmake included) |
| libc | **glibc 2.41, built from source in the build root.** Rust target stays `x86_64-unknown-linux-gnu` |
| GCC runtime | GCC's `libgcc` (libgcc_s.so.1, libgcc.a, libgcc_eh.a, crtbegin/crtend) built from the pinned GCC 14 source in the build root |
| Rust | Upstream prebuilt gnu-host rustc/cargo/rust-std 1.96.0, retargeted onto td glibc/libgcc/zlib by the existing rust-stage0 transform |
| Linking without GCC | `td-cc` becomes a GCC-free link driver: rustc's `cc`, forwarding to `rust-lld` with td's glibc and libgcc start-up files |
| Old ladder | Deleted entirely (stage0, Mes, TinyCC, GCC/glibc/binutils ladder, source-built Rust/LLVM, bootstrap-root) |
| Self-hosting | Image ships upstream Rust + td-cc + glibc + libgcc; C rebuilds need the build root (fetched on demand, cached) |
| Git | Upstream Git 2.55.0 from source in the build root (grit rejected) |
| btrfs-progs | Built in the build root now; replaced by a Rust `td-btrfs` later |
| Allocator / musl | Not applicable; glibc malloc stays. musl may be revisited separately later |
| Downloads | Every image layer, .deb and source is an ordinary sha256 pin in `~/.td/sources`; upstream is contacted only on a cache miss |

## Trust model after the migration (amends AGENTS.md "Build and trust model")

- **Zone 1, target artifact graph**: everything that ships. Either td Rust
  compiled by the upstream toolchain, or C compiled *from pinned source*
  inside the build root against td's own glibc headers and libraries.
- **Build root (new, marked)**: Debian's gcc/binutils/make/cmake/etc. May
  be a recipe's sandbox root and tools; ordinary input channels refuse it;
  no output may contain or link its files. Enforced by a closure scan that
  rejects any build-root store reference, and by C recipes compiling with
  `-nostdinc`/`-nostdlib` against td glibc and td libgcc only.
- **Upstream Rust binaries (trust root)**: rustc, cargo, rust-lld and
  rust-std rlibs. Replaces stage0 provenance as the root of trust for Rust.
- **Zone 3 (foreign application payloads)**: unchanged.
- Withdrawn claims: "every executable descends from stage0" and "no
  prebuilt LLVM in the closure". Source-bootstrap reports name Debian and
  the Rust dist as roots.
- Profiler contract: upstream rustc/cargo/rust-lld and rust-std are a
  declared frame-pointer/debug-companion boundary. td Rust keeps
  `-C force-frame-pointers=yes` + debug companions; td-built C (glibc,
  libgcc, packages) keeps `-fno-omit-frame-pointer -g` + companions.

## Facts the plan rests on (from the spikes and the tree)

- Registry: anonymous token flow; blob GET 307s to an expiring CDN URL;
  pinning the blob digest URL needs no manifest GET (no rate-limit cost).
  Trixie slim = one gzip layer, 29.8 MB.
- 93 .debs / 92 MB give gcc 14.2, binutils 2.44, make, flex, bison, bc,
  perl, libelf-dev, pkgconf, cmake, rsync, cpio, xz, patch. Kernel alone
  needs 49. No dpkg: data.tar.xz extraction + 7 symlink fixups.
- td's kernel config (7.1.4) builds in the root in 158 s at -j16.
- Static td programs (td-sh, td-init, td-kexec, ...) are already static
  glibc (`+crt-static`, `relocation-model=static`); the gnu target keeps
  them unchanged.
- rust-stage0 already retargets the upstream gnu snapshot onto td's
  glibc/libgcc/zlib with td's ELF editor, so no new ELF capability is
  needed. Upstream gnu rustc needs glibc, libgcc_s and zlib at runtime.
- After the cutover only json-c (cryptsetup's LUKS2 dependency) needs
  cmake; AWS-LC already builds with its `cc` builder.
- Toolchain size: today ~1,030 MiB (source-built Rust 576, GCC 255,
  binutils 114, glibc 86). After: upstream Rust ~650 + glibc ~86 +
  libgcc/zlib/td-cc a few MiB ≈ 740 MiB, ~290 MiB smaller.
- Not yet exercised: the gnu toolchain linking through a rust-lld driver
  with GCC unreachable (the musl spike proved the same shape for musl).

## Increments (each one green, reviewed, landable; order matters)

1. **Registry-blob fetch + cache.** New acquire variant in
   `net/src/feed.rs`: 401 → validate `auth.docker.io` realm → anonymous
   token → bearer GET → follow 307 without the header → verify sha256 →
   publish to `~/.td/sources`. Refuse unknown layer media types (no zstd
   reader). Tests against a local fixture registry.
2. **Debian lock + resolver.** `td-recipe-eval debian-root resolve`
   (std-only Rust: InRelease → Packages.xz → closure with
   Pre-Depends/Depends/Provides) writes committed
   `seed/debian-root.lock` (image digest, snapshot TS, InRelease sha256,
   rows url/sha256/size/file). `source-pins` emits its rows;
   `td-feed warm build-root` prefetches.
3. **Build-root input class.** td-builder unpacks layer (whiteouts) +
   .debs in lock order + fixups, applies the slim path-excludes, refuses
   writes through symlinks, flattens ownership, caches the tree by lock
   hash, and runs a recipe with it as `/`. Marked; ordinary channels
   refuse it; closure scan forbids references. **AGENTS.md trust-zone
   amendment lands here.** Test: hello.c recipe.
4. **Kernel in the build root.** `linux-x86-64` uses the 49-package
   subset. Deletes the bc, flex and elfutils recipes. Integration tier
   run by hand before push (boot path).
5. **td glibc, libgcc and zlib from the build root.** `glibc-x86-64`
   rebuilt with Debian gcc in the root (same 2.41 source, frame pointers,
   companions); new `gcc-runtime` recipe builds only GCC 14's target
   libgcc from the pinned source; `zlib-x86-64` moves into the root.
   Gate: their outputs reference nothing in the build root.
6. **td-cc as a GCC-free link driver.** Accepts rustc's gcc-style link
   lines (`-Wl,…`, `-nodefaultlibs`, `-pie`, `-static`, `-static-pie`,
   `-shared`, `-l…`, `-L…`, `-o`), selects td glibc/libgcc crt objects
   per output kind, runs `rust-lld`; refuses compile requests. Tests:
   link td-builder (dynamic PIE), td-sh (static ET_EXEC), a proc-macro
   dylib, a cdylib, with no gcc or binutils reachable.
7. **Rust toolchain cutover** (one atomic landing). `rust-toolchain`
   becomes the pinned upstream 1.96.0 gnu dist through the rust-stage0
   transform (now onto the step-5 runtime, rust-lld included), with
   td-cc as the linker. Recipes keep the gnu target. C inside crates
   (`ring`, AWS-LC, SQLite) compiles in the build root against td glibc.
   Amend `td-profiler/DESIGN.md`, `crypto_policy.rs` and the
   td-crypto/td-mta design docs. Deletes the source-built
   stage1/stage2/LLVM path and `td-util-boot`.
8. **C packages in the build root against td glibc**: LibreSSL (library
   only), curl, Git 2.55.0; json-c, popt, libdevmapper, util-linux libs,
   cryptsetup (static, as today); OpenSSH (`--without-openssl`);
   btrfs-progs. One landing per stack (git, cryptsetup, ssh, btrfs).
9. **Test HTTPS origin.** Replace `openssl s_server/s_client` with a
   small rustls tool on td-net; LibreSSL's command leaves the image.
10. **Image switch.** system-x86-64 ships the upstream Rust toolchain,
    td-cc driver, td glibc and libgcc instead of GCC and binutils.
11. **Delete the ladder.** `seed/` (stage0, patches, bootstrap-root,
    seed digests as applicable), stage0/Mes/TinyCC/mesboot/GCC/glibc
    ladder/binutils/make/gawk/cmake recipes, `ladder.rs`, `mes_boot.rs`,
    `stage0.rs`, `bootstrap_root.rs`, bootstrap rows of THIRD-PARTY.md
    and their LICENSES texts, re-pin text in DEVELOPMENT.md, IA32
    host-kernel requirement. The same landing sweeps every document that
    describes the ladder or the source bootstrap: README.md, AGENTS.md
    (trust model, Rust bridge, profiling contract), DEVELOPMENT.md,
    td-profiler/DESIGN.md, td-agent/DESIGN.md, APPLICATIONS.md, and the
    per-crate DESIGN.md files that cite it. This plan file is deleted
    there.
12. **td-btrfs** (later): std-only mkfs with `--rootdir/--subvol`;
    deletes btrfs-progs.

## Risks

- td-cc link driver correctness across output kinds is the main new
  risk; step 6 lands it with tests before anything depends on it.
- Docker Hub and snapshot.debian.org publish no retention guarantee; only
  the local cache protects (old digests back to 2017 still resolve).
- glibc and libgcc built by Debian's gcc must stay reproducible run to run;
  the existing reproducibility oracles apply.
- Rust upgrades now mean re-pinning upstream binaries, not rebuilding
  from source.
