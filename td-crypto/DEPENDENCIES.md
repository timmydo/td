# Private backend source inventory

This is the complete Cargo.lock inventory for M03b1. Cargo.lock records exact
registry archive SHA-256 checksums. `builder/src/crypto_policy.rs` pins the
complete lock bytes, exact direct manifests and root Cargo config, and selected
normal/build features
for the initial x86-64 Linux GNU/musl host targets. The portable musl artifact
and its compiler/sysroot pins remain M03b2.

The mail lock additionally includes the local std-only td-json package. It
adds no registry source or private backend input; the local manifest/lock and
source staging are admitted alongside td-crypto and td-mta.

Direct inputs are Rustls 0.23.45 (std, TLS 1.2, AWS-LC), aws-lc-rs 1.18.1
(alloc, non-FIPS), and webpki-roots 1.0.8 (compiled Mozilla root data).
Rustls owns TLS, webpki validates certificates, pki-types represents their
private backend forms; aws-lc-rs/sys supplies native crypto (AWS-LC 5.7.0).
The remaining active packages are their build tools or small internal support
libraries. No async runtime or mail/database library is selected.

The host native build uses the provisioned C compiler, assembler and archiver,
plus the crate's pregenerated assembly and GNU/musl bindings. It runs no Perl,
Go, NASM, CMake or bindgen generator. The compatible versions, sysroots and
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
