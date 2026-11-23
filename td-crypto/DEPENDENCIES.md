# Private backend source inventory

This is the complete private crypto and mail-backend lock inventory. Cargo.lock records exact
registry archive SHA-256 checksums. `builder/src/crypto_policy.rs` pins the
complete lock bytes, exact direct manifests and root Cargo config, and selected
normal/build features
for the x86-64 Linux GNU/musl host targets. PORTABLE.md owns the portable
musl artifact's pins.

The mail lock additionally includes local std-only td-alloc, td-civil, td-encoding,
td-fs, td-header, td-html, td-json, td-mime, td-nfc and td-postgres. They add no registry source or private backend input; their exact
manifests, locks and source staging are admitted alongside td-crypto and
td-mta.

td-tls-provider's lock holds Rustls 0.23.46 with no backend feature (custom
provider, std, TLS 1.2) and local std-only td-primitives. Its selected graph
is once_cell, rustls, rustls-pki-types, rustls-webpki, subtle, untrusted and
zeroize. ring and its build tools are locked through rustls-webpki's weak
feature edge but never selected. Every locked package is also in the mail
lock at the same version and checksum, so the common offline vendor serves
it; a policy test holds that.

td-crypto's direct inputs are that same Rustls line, local td-tls-provider and
td-primitives, and webpki-roots 1.0.11 (compiled Mozilla root data). Rustls
owns TLS, webpki validates certificates and pki-types represents their
forms; td-tls-provider and td-primitives supply every cryptographic
operation. Its selected graph is td-tls-provider's seven registry packages
and webpki-roots, 8 of the lock's 26 registry packages; the other 18 are
locked only. No selected package has a build script that compiles C, and no
native code is built. Mail selects no further registry package (PostgreSQL
client below). No async runtime is selected.

Inactive entries are retained by Cargo's lock resolution, including ring and
cc. They are fetched and checksum-verified as locked source data, but absent
from the selected normal/build graph; the exact active-graph check rejects
their activation. Cargo metadata alone includes optional inactive edges and
is not used as evidence that a backend actually compiles.

License expressions below are upstream package declarations, not a replacement
for their license/notice files. Verified vendor archives retain those files;
artifact packaging must retain all applicable notices, including root data.

| Package | Version | Selected graph | Upstream license expression |
| --- | --- | --- | --- |
| cc | 1.11.0 | inactive | MIT OR Apache-2.0 |
| cfg-if | 1.0.5 | inactive | MIT OR Apache-2.0 |
| find-msvc-tools | 0.1.15 | inactive | MIT OR Apache-2.0 |
| getrandom | 0.2.17 | inactive | MIT OR Apache-2.0 |
| libc | 0.2.191 | inactive | MIT OR Apache-2.0 |
| once_cell | 1.21.4 | active | MIT OR Apache-2.0 |
| ring | 0.17.14 | inactive | Apache-2.0 AND ISC |
| rustls | 0.23.46 | active | Apache-2.0 OR ISC OR MIT |
| rustls-pki-types | 1.15.1 | active | MIT OR Apache-2.0 |
| rustls-webpki | 0.103.16 | active | ISC |
| shlex | 2.0.1 | inactive | MIT OR Apache-2.0 |
| subtle | 2.6.1 | active | BSD-3-Clause |
| untrusted | 0.9.0 | active | ISC |
| wasi | 0.11.1+wasi-snapshot-preview1 | inactive | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| webpki-roots | 1.0.11 | active | CDLA-Permissive-2.0 |
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
| zeroize | 1.9.1 | active | Apache-2.0 OR MIT |

## PostgreSQL client

td-mta's PostgreSQL client is the local std-only `td-postgres` crate
(td-postgres/DESIGN.md), over td-alloc, td-encoding, td-fs and
td-primitives. It adds no registry package: the mail lock holds the
crypto lock's packages and local crates only, and mail's selected graph is
crypto's. Its SCRAM uses td-primitives' SHA-256, HMAC and PBKDF2. It
reaches the database only over a Unix socket and uses no TLS.
