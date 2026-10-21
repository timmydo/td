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
| Rust | Upstream prebuilt gnu-host rustc/cargo/rust-std/clippy 1.96.0, retargeted onto td glibc/libgcc/zlib by the existing rust-stage0 transform (renamed `rust-dist`) |
| Linking without GCC | `td-cc` becomes a GCC-free link driver: rustc's `cc`, forwarding to `rust-lld` with td's glibc and libgcc start-up files |
| Old ladder | Deleted entirely (stage0, Mes, TinyCC, GCC/glibc/binutils ladder, source-built Rust/LLVM, bootstrap-root) |
| Self-hosting | Image ships upstream Rust + td-cc + glibc + libgcc; C rebuilds need the build root (fetched on demand, cached) |
| Git | Upstream Git 2.55.0 from source in the build root (grit rejected) |
| btrfs-progs | Replaced by the std-only Rust `td-btrfs` (landed) |
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
- 98 .debs / 107 MB give gcc and g++ 14.2, binutils 2.44, make, flex, bison, bc,
  perl, libelf-dev, pkgconf, cmake, rsync, cpio, xz, patch. Kernel alone
  needs 49. No dpkg: data.tar.xz extraction + 7 symlink fixups.
- td's kernel config (7.1.4) builds in the root in 158 s at -j16.
- Static td programs (td-sh, td-init, td-kexec, ...) are already static
  glibc (`+crt-static`, `relocation-model=static`); the gnu target keeps
  them unchanged.
- The `rust-dist` transform retargets the upstream gnu release onto
  td's glibc/libgcc/zlib with td's ELF editor, so no new ELF capability
  was needed. Upstream gnu rustc needs glibc, libgcc_s and zlib at
  runtime. Its default linker, `cc` from PATH, is passed
  `-fuse-ld=lld -B…/gcc-ld`, which td's GCC (configured `--with-ld`)
  consumes and ignores, so it links with GNU ld; an explicitly named gcc
  infers a flavor without lld, and `-Clinker-features=-lld` pins GNU ld
  either way. rust-lld first links in increment 7.
- After the cutover only json-c (cryptsetup's LUKS2 dependency) needed
  cmake, so it moved into the root with the cutover and the ladder's
  cmake went; AWS-LC already builds with its `cc` builder.
- Toolchain size: today ~1,030 MiB (source-built Rust 576, GCC 255,
  binutils 114, glibc 86). After: upstream Rust ~650 + glibc ~86 +
  libgcc/zlib/td-cc a few MiB ≈ 740 MiB, ~290 MiB smaller.
- Not yet exercised: the gnu toolchain linking through a rust-lld driver
  with GCC unreachable (the musl spike proved the same shape for musl).

## Increments (each one green, reviewed, landable; order matters)

1. **Registry-blob fetch + cache** (landed as `td-net: fetch pinned
   container-registry blobs through the token flow`). `http::get_to_file`
   recognises an allowlisted registry blob URL and fetches it as anonymous
   token (realm from the allowlist) → bearer GET → absolute https redirect
   target without the token, under one absolute deadline; every source
   path (`warm sources`, the shared feed, `td-fetch`) inherits it. Layer
   media-type refusal moves to increments 2–3.
2. **Debian lock + resolver.** `td-feed debian-root resolve
   recipes/src/debian-root.lock` resolves the image tag to its one gzip
   layer (refusing any other layer format), reads the layer's dpkg status,
   reads each archive's Packages.xz checked against its InRelease, and
   writes the closure (Pre-Depends/Depends/Provides/Breaks, dpkg version
   order, no downgrades, whole-root consistency) below the lock's input
   lines. The xz decoder moves into td-engine so td-net can read the
   xz-only security index. `source_pins::build_root_pins()` reads the
   lock's `layer`/`deb` rows as a pin class `by_key`, `all` and
   `source-pins` never return; `td-recipe-eval build-root-pins` lists
   them and `td-feed warm build-root` caches them. The lock is a
   `PIN_TABLES` entry, so a re-pin re-keys every check.
3. **Build-root input class.** `td-builder build-root assemble` unpacks
   the layer (refusing whiteouts) and each .deb's `data.tar.xz` in lock
   order, writes through the usrmerge links only, drops the slim
   excludes, adds the update-alternatives links, and places the tree at
   an input-addressed store path keyed by the pins and the pinned tree;
   `build-root pin` writes its NAR hash into the lock, and assembly
   refuses any other tree. `Recipe::build_root()` stages it outside the
   input map; the sandbox binds it read-only as `/` (typed `BuildRoot`,
   never a builder); an output may not reference it or carry an ELF
   interpreter/RUNPATH or a symlink that resolves outside the store. Only
   `build-plan` (the check runner's path) admits the root's db; the
   daemon and `build-recipe` fail closed on a build-root drv until a
   later increment needs them. AGENTS.md and UNSAFE.md amended. Test:
   `build-root-hello-test` (freestanding C compiled and run in the root).
4. **Kernel in the build root.** First (4a), td-util gains a Rust
   `gen_init_cpio`, byte-identical to the kernel's, and the boot recipes
   pack with it, so the kernel exports no host program. Then (4b)
   `linux-x86-64` and its test build in the whole root (no subset: one
   tree serves every build-root recipe), with Debian's gcc, binutils,
   make, flex, bison, bc and libelf. The bc, flex and elfutils recipes
   and their pins are deleted, along with the kernel's glibc sysroot,
   static HOSTCC wrapper, libelf pkg-config shim and tool farm. The
   integration tier is run by hand before push (boot path).
5. **td libgcc from the build root.** New `gcc-runtime` recipe: the
   root's gcc/g++ build GCC 14.3's compiler proper from the pinned
   source, which compiles only the target libgcc (libgcc.a, libgcc_eh.a,
   libgcc_s.so.1, crtbegin/crtend) against a sysroot of td glibc, with
   frame pointers and companions. Its probes link a dynamic PIE and a
   static ET_EXEC from td glibc and it alone (`ld -nostdlib`) and run
   them. Built against today's ladder glibc; glibc moves later (8b).
   Order note: moving glibc first would re-key the whole ladder GCC and
   the source-built Rust above it on every iteration, and glibc must
   link a td libgcc, never the root's.
6. **Rust toolchain cutover** (one atomic landing). `rust-toolchain`
   becomes the pinned upstream 1.96.0 gnu dist (rustc, rust-std, Cargo,
   Clippy) through the renamed `rust-dist` transform onto td glibc,
   gcc-runtime's libgcc_s and zlib (moved into the root, with
   companions), rust-lld included, `gcc-ld/ld.lld` linked to it.
   gcc-x86-64-self stays the `cc`, and td's recipes pass
   `-Clinker-features=-lld` so they keep linking with GNU ld: the
   cutover changes the compiler's provenance, not the linker, and the
   linker moves with the driver in 7. Upstream rustc/Cargo/rust-lld/LLVM
   and rust-std are a declared profiler boundary; the image's object
   index excludes the toolchain. json-c moves into the root, so the
   ladder's cmake goes. Amends AGENTS.md (trust model, Rust toolchain,
   profiling contract), `td-profiler/DESIGN.md` and the design docs that
   cite the source-built toolchain. Deletes the source-built
   stage1/stage2/LLVM path, `rust-stage0`, `td-util-boot` and
   `cmake-x86-64`.
7. **A GCC-free Rust link driver.** Accepts rustc's gcc-style link
   lines (`-Wl,…`, `-nodefaultlibs`, `-pie`, `-static`, `-static-pie`,
   `-shared`, `-l…`, `-L…`, `-o`), selects td glibc/gcc-runtime crt
   objects per output kind, runs `rust-lld`; refuses compile requests.
   Every Rust recipe and the image's `cc` for Cargo switch to it, and
   `-Clinker-features=-lld` leaves the target policy. Tests:
   a dynamic PIE, a static ET_EXEC, a static PIE, a proc-macro dylib and
   a cdylib, with no gcc or binutils reachable. C inside crates (`ring`,
   AWS-LC, jemalloc, blake3, oniguruma, pcre2) compiles in the build root
   against td glibc. Amends `crypto_policy.rs` and the td-crypto/td-mta
   design docs for AWS-LC's compiler.
8. **C packages in the build root against td glibc**: LibreSSL (library
   only), curl, Git 2.55.0; popt, libdevmapper, util-linux libs,
   cryptsetup (static, as today); OpenSSH (`--without-openssl`). One
   landing per stack (git, cryptsetup, ssh).
   A static link that ships uses `ladder::build_root_static_cc`: Debian
   gcc with td glibc's sysroot and gcc-runtime's `crtbeginT.o`,
   `libgcc.a` and `libgcc_eh.a` ahead of the root's, and
   `build_root_link_map_check` proves from the final link's map that
   nothing was loaded from the root. Landed: the cryptsetup and ssh
   stacks (OpenSSH links dynamically: `build_root_dynamic_cc`, and its
   map must load td glibc's libc.so.6), and zlib-x86-64 also ships the
   static archive (the ladder's zlib-x86-64-self is gone).
   8b. **glibc in the build root.** `glibc-x86-64` rebuilt in the root
   (same 2.41 source, frame pointers, companions; the root gains gawk
   and python3). Its link uses a static libgcc built from td glibc's
   installed headers first, never the root's libgcc, and gcc-runtime is
   then rebuilt against the new glibc.
9. **Test HTTPS origin.** Replace `openssl s_server/s_client` with a
   small rustls tool on td-net; LibreSSL's command leaves the image.
   Landed: td-net's `td-test-https` applet (mint, serve, probe), and
   LibreSSL builds no command.
10. **Image switch.** system-x86-64 ships the upstream Rust toolchain,
    the link driver, td glibc and libgcc instead of GCC and binutils.
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
12. **td-btrfs** (landed): std-only mkfs with `--rootdir/--subvol`;
    btrfs-progs, its pin and test are deleted; util-linux libs stay for
    cryptsetup.

## Risks

- td-cc link driver correctness across output kinds is the main new
  risk; step 6 lands it with tests before anything depends on it.
- Docker Hub and snapshot.debian.org publish no retention guarantee; only
  the local cache protects (old digests back to 2017 still resolve).
- glibc and libgcc built by Debian's gcc must stay reproducible run to run;
  the existing reproducibility oracles apply.
- Rust upgrades now mean re-pinning upstream binaries, not rebuilding
  from source.
