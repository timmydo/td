# Private backend source inventory

This is the complete private crypto and mail-backend lock inventory. Cargo.lock records exact
registry archive SHA-256 checksums. `builder/src/crypto_policy.rs` pins the
complete lock bytes, exact direct manifests and root Cargo config, and selected
normal/build features
for the initial x86-64 Linux GNU/musl host targets. The portable musl artifact
and its compiler/sysroot pins remain M03b2.

The mail lock additionally includes local std-only td-civil, td-header,
td-json, td-mime and td-nfc. They add no registry source or private backend input; their exact
manifests, locks and source staging are admitted alongside td-crypto and
td-mta.

Direct inputs are Rustls 0.23.45 (std, TLS 1.2, AWS-LC), aws-lc-rs 1.18.1
(alloc, non-FIPS), and webpki-roots 1.0.8 (compiled Mozilla root data).
Rustls owns TLS, webpki validates certificates, pki-types represents their
private backend forms; aws-lc-rs/sys supplies native crypto (AWS-LC 5.7.0).
The remaining active packages are their build tools or small internal support
libraries. Mail additionally selects the private PostgreSQL closure below.
No async runtime is selected.

The host native build uses the provisioned C compiler, assembler and archiver,
plus the crate's pregenerated assembly and GNU/musl bindings. The forced
host wrapper applies the x86-64 baseline, generic tuning, retained frame
pointers, -g1 debug information and deterministic file-prefix mappings to
AWS-LC C compilation. These flags do not change the selected
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

## PostgreSQL backend

The user approved tokio-postgres plus its transitive dependencies. The driver
pin is 0.7.18 with default features disabled and no optional integrations.
Tokio 1.53.3 is also directly pinned with only io-util, for the
private stream adapter; it adds no feature beyond the driver's closure.
The mail lock adds 76 registry packages; 53 newly locked packages are active
on x86-64 Linux. Previously locked cfg-if 1.0.5 and getrandom 0.4.3 also become
active for mail only. Crypto's original selected graph is unchanged. The
active-graph guard separately rejects driver version drift, Tokio runtime/
net/time activation and a separate native-TLS connector. PostgreSQL is the only mail backend.

Tokio provides AsyncRead/AsyncWrite and synchronization without an executor,
reactor or timer. bytes/futures/tokio-util support protocol buffering; phf,
parking_lot and other utilities support the driver. SCRAM uses hmac/sha2,
rand/getrandom/chacha20 and stringprep's Unicode preparation. Its Unicode
normalization is private to password authentication, not mail interpretation.
The upstream protocol crate also compiles MD5 support; admission does not
authorize legacy authentication. The future connector must enforce the
POSTGRES authentication policy and keep TLS behind td-crypto. Neither an
OpenSSL nor a native-TLS connector is included.

These are external implementations with their own allocation, entropy and
panic behavior, not bounded td-owned primitives. Connector qualification must
bound in-flight requests, protocol input, SCRAM work and notice/error retention
before activation. No live PostgreSQL or portable resource qualification is
claimed by compiling this closure. The driver has no new native C build;
newly selected build scripts are getrandom and parking_lot_core
(sanitizer configuration), proc-macro2 (compiler probes) and quote (compiler
version). async-trait is a build-time
procedural macro. Only declared Rust tools may run those scripts/probes in
target builds. The offline vendor verifies every source archive, including
inactive target packages. Exact versions/checksums live in td-mta/Cargo.lock;
manifest, lock and selected features are pinned by crypto_policy.rs.

| Package | Version | Selected mail graph | Upstream license expression |
| --- | --- | --- | --- |
| async-trait | 0.1.92 | active | MIT OR Apache-2.0 |
| base64 | 0.22.1 | active | MIT OR Apache-2.0 |
| bitflags | 2.13.2 | inactive | MIT OR Apache-2.0 |
| block-buffer | 0.12.1 | active | MIT OR Apache-2.0 |
| bumpalo | 3.20.3 | inactive | MIT OR Apache-2.0 |
| byteorder | 1.5.0 | active | Unlicense OR MIT |
| bytes | 1.12.1 | active | MIT |
| chacha20 | 0.10.2 | active | MIT OR Apache-2.0 |
| cmov | 0.5.4 | active | Apache-2.0 OR MIT |
| const-oid | 0.10.2 | active | Apache-2.0 OR MIT |
| cpufeatures | 0.3.1 | active | MIT OR Apache-2.0 |
| crypto-common | 0.2.2 | active | MIT OR Apache-2.0 |
| ctutils | 0.4.3 | active | Apache-2.0 OR MIT |
| digest | 0.11.3 | active | MIT OR Apache-2.0 |
| fallible-iterator | 0.2.0 | active | MIT/Apache-2.0 |
| futures-channel | 0.3.34 | active | MIT OR Apache-2.0 |
| futures-core | 0.3.34 | active | MIT OR Apache-2.0 |
| futures-sink | 0.3.34 | active | MIT OR Apache-2.0 |
| futures-task | 0.3.34 | active | MIT OR Apache-2.0 |
| futures-util | 0.3.34 | active | MIT OR Apache-2.0 |
| hmac | 0.13.0 | active | MIT OR Apache-2.0 |
| hybrid-array | 0.4.15 | active | MIT OR Apache-2.0 |
| js-sys | 0.3.106 | inactive | MIT OR Apache-2.0 |
| libredox | 0.1.25 | inactive | MIT |
| lock_api | 0.4.14 | active | MIT OR Apache-2.0 |
| log | 0.4.34 | active | MIT OR Apache-2.0 |
| md-5 | 0.11.0 | active | MIT OR Apache-2.0 |
| memchr | 2.8.3 | active | Unlicense OR MIT |
| objc2-core-foundation | 0.3.2 | inactive | Zlib OR Apache-2.0 OR MIT |
| objc2-system-configuration | 0.3.2 | inactive | Zlib OR Apache-2.0 OR MIT |
| parking_lot | 0.12.5 | active | MIT OR Apache-2.0 |
| parking_lot_core | 0.9.12 | active | MIT OR Apache-2.0 |
| percent-encoding | 2.3.2 | active | MIT OR Apache-2.0 |
| phf | 0.13.1 | active | MIT |
| phf_shared | 0.13.1 | active | MIT |
| pin-project-lite | 0.2.17 | active | Apache-2.0 OR MIT |
| postgres-protocol | 0.6.12 | active | MIT OR Apache-2.0 |
| postgres-types | 0.2.14 | active | MIT OR Apache-2.0 |
| proc-macro2 | 1.0.107 | active | MIT OR Apache-2.0 |
| quote | 1.0.47 | active | MIT OR Apache-2.0 |
| rand | 0.10.3 | active | MIT OR Apache-2.0 |
| rand_core | 0.10.1 | active | MIT OR Apache-2.0 |
| redox_syscall | 0.5.18 | inactive | MIT |
| rustversion | 1.0.23 | inactive | MIT OR Apache-2.0 |
| scopeguard | 1.2.0 | active | MIT OR Apache-2.0 |
| serde | 1.0.229 | inactive | MIT OR Apache-2.0 |
| serde_core | 1.0.229 | inactive | MIT OR Apache-2.0 |
| serde_derive | 1.0.229 | inactive | MIT OR Apache-2.0 |
| sha2 | 0.11.1 | active | MIT OR Apache-2.0 |
| siphasher | 1.0.4 | active | MIT OR Apache-2.0 |
| slab | 0.4.12 | inactive | MIT |
| smallvec | 1.16.2 | active | MIT OR Apache-2.0 |
| socket2 | 0.6.5 | active | MIT OR Apache-2.0 |
| stringprep | 0.1.5 | active | MIT/Apache-2.0 |
| syn | 3.0.9 | active | MIT OR Apache-2.0 |
| tinyvec | 1.13.3 | active | Zlib OR Apache-2.0 OR MIT |
| tokio | 1.53.3 | active | MIT |
| tokio-postgres | 0.7.18 | active | MIT OR Apache-2.0 |
| tokio-util | 0.7.20 | active | MIT |
| typenum | 1.20.1 | active | MIT OR Apache-2.0 |
| unicode-bidi | 0.3.18 | active | MIT OR Apache-2.0 |
| unicode-ident | 1.0.27 | active | (MIT OR Apache-2.0) AND Unicode-3.0 |
| unicode-normalization | 0.1.25 | active | MIT OR Apache-2.0 |
| unicode-properties | 0.1.4 | active | MIT/Apache-2.0 |
| wasi | 0.14.7+wasi-0.2.4 | inactive | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| wasip2 | 1.0.4+wasi-0.2.12 | inactive | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| wasite | 1.0.2 | inactive | Apache-2.0 OR BSL-1.0 OR MIT |
| wasm-bindgen | 0.2.129 | inactive | MIT OR Apache-2.0 |
| wasm-bindgen-macro | 0.2.129 | inactive | MIT OR Apache-2.0 |
| wasm-bindgen-macro-support | 0.2.129 | inactive | MIT OR Apache-2.0 |
| wasm-bindgen-shared | 0.2.129 | inactive | MIT OR Apache-2.0 |
| web-sys | 0.3.106 | inactive | MIT OR Apache-2.0 |
| whoami | 2.1.3 | active | Apache-2.0 OR BSL-1.0 OR MIT |
| windows-link | 0.2.1 | inactive | MIT OR Apache-2.0 |
| windows-sys | 0.61.2 | inactive | MIT OR Apache-2.0 |
| wit-bindgen | 0.57.1 | inactive | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
