# Private backend source inventory

This is the complete private crypto and mail-backend lock inventory. Cargo.lock records exact
registry archive SHA-256 checksums. `builder/src/crypto_policy.rs` pins the
complete lock bytes, exact direct manifests and root Cargo config, and selected
normal/build features
for the initial x86-64 Linux GNU/musl host targets. The portable musl artifact
and its compiler/sysroot pins remain M03b2.

The mail lock additionally includes local std-only td-header, td-json, td-mime and
td-nfc. They add no registry source or private backend input; their exact
manifests, locks and source staging are admitted alongside td-crypto and
td-mta.

Direct inputs are Rustls 0.23.45 (std, TLS 1.2, AWS-LC), aws-lc-rs 1.18.1
(alloc, non-FIPS), and webpki-roots 1.0.8 (compiled Mozilla root data).
Rustls owns TLS, webpki validates certificates, pki-types represents their
private backend forms; aws-lc-rs/sys supplies native crypto (AWS-LC 5.7.0).
The remaining active packages are their build tools or small internal support
libraries. The mail crate additionally selects private rusqlite 0.40.2
(blob, bundled, hooks, limits) and its bundled SQLite 3.53.2 source. The crypto
crate does not select SQLite. No async runtime is selected.

The host native build uses the provisioned C compiler, assembler and archiver,
plus the crate's pregenerated assembly and GNU/musl bindings. The forced
host wrapper applies the x86-64 baseline, generic tuning, retained frame
pointers, -g1 debug information and deterministic file-prefix mappings to
both AWS-LC and SQLite C compilation. These flags do not change the selected
dependency graph; host checks do not qualify the portable artifact. The build
runs no Perl, Go, NASM, CMake or bindgen generator. The compatible versions, sysroots and
runtime libraries for the portable build remain M03b2's pinned input set.
It forces AWS_LC_SYS_USE_SYSTEM=0, CMAKE_BUILDER=0, PREBUILT_NASM=0 and
STATIC=1, EXTERNAL_BINDGEN=0; incompatible AWS_LC_SYS overrides fail before Cargo runs.
The cmake/pkg-config Rust packages compile, but their external programs are
not selected by this source/cc build. Rustls forwards the prebuilt-nasm feature;
on Linux this does not consume a prebuilt NASM object. The wrapper does not
claim a hermetic portable toolchain; M03b2 owns that qualification and decoys.

Inactive entries are retained by Cargo's lock resolution, including ring.
They are fetched and checksum-verified as locked source data, but absent from
the selected normal/build graph; the exact active-graph check rejects their
activation. Cargo metadata alone includes optional inactive edges and is not
used as evidence that a backend actually compiles.

License expressions below are upstream package declarations, not a replacement
for their license/notice files. Verified vendor archives retain those files;
artifact packaging must retain all applicable notices, including root data.

| Package | Version | Selected graph | Upstream license expression |
| --- | --- | --- | --- |
| aws-lc-rs | 1.18.1 | active | ISC AND (Apache-2.0 OR ISC) |
| aws-lc-sys | 0.45.0 | active | ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0) |
| cc | 1.5.1 | active | MIT OR Apache-2.0 |
| cfg-if | 1.0.5 | inactive | MIT OR Apache-2.0 |
| cmake | 0.1.58 | active | MIT OR Apache-2.0 |
| dunce | 1.0.5 | active | CC0-1.0 OR MIT-0 OR Apache-2.0 |
| find-msvc-tools | 0.1.14 | active | MIT OR Apache-2.0 |
| fs_extra | 1.3.0 | active | MIT |
| getrandom | 0.2.17 | inactive | MIT OR Apache-2.0 |
| getrandom | 0.4.3 | inactive | MIT OR Apache-2.0 |
| jobserver | 0.1.35 | active | MIT OR Apache-2.0 |
| libc | 0.2.189 | active | MIT OR Apache-2.0 |
| once_cell | 1.21.4 | active | MIT OR Apache-2.0 |
| pkg-config | 0.3.34 | active | MIT OR Apache-2.0 |
| r-efi | 6.0.0 | inactive | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| ring | 0.17.14 | inactive | Apache-2.0 AND ISC |
| rustls | 0.23.45 | active | Apache-2.0 OR ISC OR MIT |
| rustls-pki-types | 1.15.1 | active | MIT OR Apache-2.0 |
| rustls-webpki | 0.103.15 | active | ISC |
| shlex | 2.0.1 | active | MIT OR Apache-2.0 |
| subtle | 2.6.1 | active | BSD-3-Clause |
| untrusted | 0.9.0 | active | ISC |
| wasi | 0.11.1+wasi-snapshot-preview1 | inactive | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| webpki-roots | 1.0.8 | active | CDLA-Permissive-2.0 |
| windows-sys | 0.52.0 | inactive | MIT OR Apache-2.0 |
| windows-targets | 0.52.6 | inactive | MIT OR Apache-2.0 |
| windows_aarch64_gnullvm | 0.52.6 | inactive | MIT OR Apache-2.0 |
| windows_aarch64_msvc | 0.52.6 | inactive | MIT OR Apache-2.0 |
| windows_i686_gnu | 0.52.6 | inactive | MIT OR Apache-2.0 |
| windows_i686_gnullvm | 0.52.6 | inactive | MIT OR Apache-2.0 |
| windows_i686_msvc | 0.52.6 | inactive | MIT OR Apache-2.0 |
| windows_x86_64_gnu | 0.52.6 | inactive | MIT OR Apache-2.0 |
| windows_x86_64_gnullvm | 0.52.6 | inactive | MIT OR Apache-2.0 |
| windows_x86_64_msvc | 0.52.6 | inactive | MIT OR Apache-2.0 |
| zeroize | 1.9.0 | active | Apache-2.0 OR MIT |

The following seven packages are active only for td-mta. Bundled SQLite
uses libsqlite3-sys's checked source archive and pregenerated bindings;
its native notices remain inside that archive. License strings are the
upstream package declarations, including their original slash spelling.

| Package | Version | Selected graph | Upstream license expression |
| --- | --- | --- | --- |
| bitflags | 2.13.2 | mail only | MIT OR Apache-2.0 |
| fallible-iterator | 0.3.0 | mail only | MIT/Apache-2.0 |
| fallible-streaming-iterator | 0.1.9 | mail only | MIT/Apache-2.0 |
| libsqlite3-sys | 0.38.2 | mail only | MIT |
| rusqlite | 0.40.2 | mail only | MIT |
| smallvec | 1.16.2 | mail only | MIT OR Apache-2.0 |
| vcpkg | 0.2.15 | mail build support | MIT/Apache-2.0 |

The shared offline vendor is prepared from the full mail lock; crypto's active
graph remains unchanged. Both drivers force LIBSQLITE3_SYS_USE_PKG_CONFIG=0
and reviewed native flags: OMIT_LOAD_EXTENSION, TEMP_STORE=3, MAX_MEMORY=16 MiB,
MAX_ALLOCATION_SIZE=9 MiB, MAX_LENGTH=33558528, MAX_SQL_LENGTH=8192,
MAX_PAGE_COUNT=2097152 and DEFAULT_CACHE_SIZE=-128. Ambient SQLite selection
controls fail before Cargo. pkg-config/vcpkg support crates compile without
selecting system SQLite. The same C frame-pointer/debug/remapping policy
applies to the bundled amalgamation. Host tests establish neither an isolated
portable SQLite build nor native maximum-input resource qualification.

Bodies use one BLOB per row and the safe incremental BLOB API. The blob
feature adds no packages. The native value ceiling covers a 32 MiB body
plus bounded row overhead. Spilling and the 8 GiB combined page ceiling are
specified in mail STORAGE.md.
The individual allocation cap accommodates SQLite's contiguous checkpoint
iterator: at the admitted maximum WAL it requests 8429736 bytes on x86-64,
inside the unchanged shared 16 MiB heap. Mail [STORAGE.md](../td-mta/STORAGE.md)
owns the calculation and explicit large-WAL refusal/truncation qualification.
