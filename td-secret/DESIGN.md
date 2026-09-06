# Local credential manager

## File-backed stores before enrollment

This is a dependency-free Rust implementation of APPLICATIONS.md §W.4.
The unenrolled backend is authorized by ordinary uid ownership. It does not
claim hardware protection, user authentication, elevation, memory locking,
or guaranteed erasure of Rust/compiler copies of secret buffers. Filling
owned buffers with zero is best effort. The unconfined user can read the
master; jailed applications cannot traverse or mount the store. No server
or external synchronization exists.

`/var/lib/td/secrets/<uid>` is a mode-0700 directory, with regular mode-0600
single-link files owned by uid. Directory traversal pins every component
and rejects symlinks. Reads reject wrong ownership, mode, type, links and
oversized input. The store lock serializes each complete read or update;
contention fails closed. Publication uses a random exclusive temporary,
file fsync, rename and directory fsync. An existing malformed master is
never replaced, and a missing master in a nonempty store is an error.

The `master` file contains exactly 32 bytes from the kernel random source.
HKDF-SHA256 (RFC 5869) extracts with salt `td-secret/store/v1`, then expands
one block with the authenticated application name as info. The derived
32-byte key stays in td-owned code. Each `APP.NAME` record has this layout:

| Bytes | Meaning |
|---|---|
| 0..8 | ASCII `TDSEC001` |
| 8..20 | Fresh random 96-bit nonce |
| 20..end-16 | ChaCha20 ciphertext, 1..4096 bytes |
| last 16 | Poly1305 authentication tag |

AEAD follows RFC 8439. The associated data is UTF-8
`td-secret/record/v1/APP/APP.NAME`. Both names contain 1..64 ASCII letters,
digits, hyphens or underscores, so neither the filename nor associated data
has ambiguous boundaries. Authentication precedes decryption. Nonces are
independent per write; this small local store does not approach the random
96-bit nonce collision budget. Rename provides crash atomicity, not rollback
protection against a writer able to replace the store or disk state.

## Credential interface

The activated desktop portal serves `td.Secret1` version 1:

- `Retrieve(s name) -> (h credential, s receipt)` performs a broker
  `GetConnectionCredentials` lookup for the original unique sender. Only an
  authenticated application at uid 1000 is currently admitted. No caller
  supplies its application name; `FLATPAK_ID` is not consulted by the portal.
- The credential descriptor names an already-unlinked regular file in the
  runtime filesystem. It is reopened read-only before transfer. The master
  and derived keys never cross D-Bus. Secret bytes never enter log messages,
  method arguments, process arguments, or persistent temporary plaintext.
- `Received(s receipt)` consumes the token only on the original connection.
  It grants no authority. The td-owned client calls it after reading the
  bounded file and requires the exact reply before handing bytes to
  td-mail.
- Pending lookups plus unacknowledged deliveries are capped at 16; each owner
  may have four pending lookups and deliveries combined. Lookups and receipts expire after 20 seconds,
  with the service's ten-second audit retiring expired entries. Disconnect
  notification also retires that caller's entries. Replies from a peer other
  than the broker cannot resolve a pending identity.

The service refuses incoming descriptors. Its ancillary reader closes every
received fd and preserves the count for decoding and InvalidArgs replies.
The helper negotiates descriptor transfer, reads one bounded frame at a time,
owns every installed descriptor through rejection, and accepts only the
reply from the broker-resolved activated portal name. It validates file type,
unlinked status and length, and holds one 20-second exchange deadline.
The transport surface and source confinement are recorded in UNSAFE.md §15.

## Writers and migration

Firstboot creates the per-user store and `mail/main` placeholder. It imports
the former provisioner's `password` file before publishing portal-mode
configuration and deleting the old file. Existing stored bytes survive every
boot. A conflict between a stored credential and a legacy file is refused;
neither is silently discarded. This migration supports the exact previous
provisioner's path and main account, with explicit refusal for renamed
accounts, custom password-file paths, multiline strings and ambiguous
credential sources.
A mail refusal leaves its source data intact and does not prevent news
configuration from being provisioned.

`td-secret set APP/NAME` reads credential bytes from stdin. Application and
entry names are parsed separately and never interpreted as paths. This is the
interim console operation authorized in §W.4, with no consent UI. The target
replacement binds a secure-attention token touch to one typed request and one
credential descriptor through td-authd. An enrolled store fails closed when
its TPM or volatile release is absent; no error selects a file master.
Console writes remain uid-authorized until increment (d); increment (c) must
first gate release on token presence.

## TPM enrollment and boot release: increment (b)

The root console can enroll an existing store with `td-secret seal --uid UID
--pcrs LIST --unrecoverable`. Arguments have fixed positions; UID is decimal
and LIST contains one to eight distinct SHA-256 PCR indices from 0 through
15. The eight-entry cap matches a single TPM PCR read; larger selections are
rejected before contacting the TPM. There is no default PCR selection. Every
selected PCR must be nonzero, and the exact selection and its composite
digest travel in the sealed object. This checks that measurements exist,
**not what measured components they represent**. The operator must establish
that the platform's measurement chain covers the intended firmware and boot
path. The current QEMU direct-kernel deployment path does not establish a
measured-deployment policy. It remains unenrolled; TPM presence alone never
enrolls it.

The implementation speaks bounded TPM 2.0 packets through safe file I/O to
`/dev/tpmrm0`. ACPI discovery and the TIS/FIFO and CRB drivers are built into
the target kernel. The device stays root-owned; neither the portal nor the
application receives TPM access. A deterministic ECC P-256 restricted storage
primary under the owner hierarchy wraps a fixedTPM/fixedParent keyed-hash
object. Its encrypted sensitive payload binds the owner UID to the master;
unseal rejects a transplanted or edited UID before returning any key. The
sealed object's userWithAuth bit is clear. Its sole release policy is
PolicyPCR followed by PolicyCommandCode(Unseal), with SHA-256 throughout. An
empty password cannot bypass that policy. Owner authorization is assumed
empty; a TPM whose owner hierarchy is administered otherwise is refused,
never reset or cleared. The client flushes transient objects and sessions on
success and failure. The kernel resource manager also owns the connection
lifetime. Replies, names, policy digests, templates and lengths are checked;
TPM failures are returned without retry or fallback.

Enrollment generates a new master, seals it and verifies an actual unseal
before publication. It decrypts all existing entries and re-encrypts them
under the new master. One atomic `sealed` file publishes both the sealed key
and all records. Its presence selects the backend even if legacy files remain
following a crash. After publication, the old master, old records and private
interrupted-write files are retired. Unknown files, wrong metadata, a failed
roundtrip or invalid input abort before publication. Cleanup is restartable
on successful release. No command converts a sealed store back to a file key.

The bundle format is `TDSEAL01`, a u32-length-prefixed TPM envelope, a u32
entry count, then name/record pairs with u32 lengths. Integers are
big-endian; there are at most 128 records and 600,000 bytes. Records retain
the AEAD format above and authenticate their application and entry identity.
Duplicate names, invalid lengths and trailing bytes are errors. The TPM
envelope is `TDTPM001`, a big-endian u32 owner UID, a big-endian u16 PCR
mask, a 32-byte PCR composite digest, and public/private TPM2B fields. Its
fixed format implies no recovery path. Losing the TPM owner seed or the
selected PCR state loses access: enrollment explicitly requires
`--unrecoverable`. Re-enrollment and policy-authorized upgrades are not yet
provided. Do not enroll a store whose measured updates require recovery.

Firstboot releases the configured mail user's enrolled store as part of
provisioning that user's valid application home. A missing or invalid home
prevents that automatic release. Other enrolled UIDs need a root-console
`td-secret release --uid UID` at each boot; the same command retries a failed
automatic release. Release removes an old volatile key before attempting the
TPM and publishes a replacement only after successful unseal and legacy
cleanup. The portal reads that release at `/run/td-secret/UID/key`: a 32-byte
envelope fingerprint followed by the 32-byte master. Root owns the
traversable runtime directories; the single-link 0600 file belongs to UID.
Every directory and file is opened without following links. `/run` must be
tmpfs, with no nested mount beneath the secret directory, no active swap, and
a zero process core-dump soft limit. A missing `/proc/swaps` is accepted:
Linux omits it when swap support is compiled out. Other swap-table read
errors and an empty or active table are refused. The fingerprint rejects a
release for another enrolled key. Updates rewrite the encrypted bundle
atomically and never persist a plaintext key. There is no additional daemon
or external service.

This release is **automatic at boot and has no human authentication**.
Unconfined same-uid programs can still read the released key or credentials;
TPM possession is not user identity. PCR changes after release do not revoke
already released bytes. Memory zeroing remains best effort. The kernel, root,
DMA and physical TPM-bus interception are outside this increment's boundary;
its TPM sessions are not salted or parameter-encrypted. PCR policy cannot
compensate for a boot path that fails to measure attacker-controlled code.
These limits are not FIDO2, secure attention or elevation claims.

Publication provides crash atomicity, not rollback protection or secure disk
erasure. Btrfs snapshots, backups and old extents can retain the former
master and the former encrypted records; master rotation cannot revoke a
credential value recovered from them. Operators must replace real upstream
credentials after enrollment if historical copies may have escaped. The
atomic cutover removes the active legacy mechanism, not storage history.

## TPM validation

The optional host oracle uses upstream swtpm 0.10.1 and libtpms 0.10.2,
compiled only for the host. They are not target artifacts, recipe tools or
Cargo dependencies. Source archives and SHA-256 pins:

- `https://codeload.github.com/stefanberger/swtpm/tar.gz/refs/tags/v0.10.1`
  `f8da11cadfed27e26d26c5f58a7b8f2d14d684e691927348906b5891f525c684`
- `https://codeload.github.com/stefanberger/libtpms/tar.gz/refs/tags/v0.10.2`
  `edac03680f8a4a1c5c1d609a10e3f41e1a129e38ff5158f0c8deaedc719fb127`

Build them using their upstream autotools instructions into a host scratch
prefix (libtpms with TPM2; swtpm can disable tests, CUSE, seccomp, SELinux
and GnuTLS for this socket-only oracle). Host compiler and development
libraries are prerequisites for that scratch build, never inputs to td's
target graph. Then run:

```
TD_TEST_SWTPM=/absolute/path/to/swtpm cargo test --frozen --manifest-path td-secret/Cargo.toml -- --include-ignored
```

The ignored tests require that explicit executable path, create fresh
emulator state and Unix sockets, and never open a hardware TPM. They cover
restart, changed PCRs, a different TPM, private-blob tampering,
unmeasured-PCR refusal, and migration plus volatile release of a real
encrypted store without persisting the new master in its store directory.
Filesystem fixtures cover volatile release, fingerprint mismatch, missing
keys, metadata, stale temporary cleanup, failed unseal, root identity, core
limits, and compiled-out swap. The fixture paths and ownership enter private
helpers; production callers retain the fixed `/run` path and root checks.
Ordinary tests also exercise malformed envelopes and replies, backend
selection, failed enrollment, master rotation and interrupted cleanup. A
normal cargo pass with those tests ignored is not TPM integration evidence.

## Portal evidence

Tests include RFC HKDF and AEAD vectors, Poly1305, bytewise ciphertext/tag
tampering, identity substitution, file metadata, reopen and migration
idempotence, broker identity refusals, descriptor ownership and live
transfer. The image requires both the unconfined probe's exact credential
refusal and the supervised mail receipt marker. This composes the
provisioner, persistent store, td-mail configuration, jailed helper,
broker-fixed identity, authenticated decryption, descriptor transport and
acknowledgement.
It is not evidence for FIDO2 release or elevation; TPM evidence is separate
from this existing desktop boot check.
