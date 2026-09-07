# Local credential manager

## File-backed stores before enrollment

This is a dependency-free Rust implementation of APPLICATIONS.md §W.4.
The unenrolled backend is authorized by ordinary uid ownership. It does
not claim hardware protection, user authentication, elevation, memory
locking, or guaranteed erasure of Rust/compiler copies of secret
buffers. Filling owned buffers with zero is best effort. The portal
service owns the master; the human user and jailed applications cannot
traverse or mount the store. Offline disk readers can still recover the
unenrolled master. No server or external synchronization exists.

`/var/lib/td/secrets/<uid>` is a mode-0700 directory, with regular
mode-0600 single-link files owned by the session's reserved portal UID.
Ownership checks pin the UID; these private modes grant no group
authority. New files may retain the creator's GID, while migration
assigns the service GID. The directory name and TPM envelope retain the
logical human UID; filesystem ownership is a separate argument, never an
identity inferred from the directory owner. Directory traversal pins
every component and rejects symlinks. Reads reject wrong ownership,
mode, type, links and oversized input. The store lock serializes each
complete read or update; contention fails closed. Publication uses a
random exclusive temporary, file fsync, rename and directory fsync. An
existing malformed master is never replaced, and a missing master in a
nonempty store is an error.

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
  may have four pending lookups and deliveries combined. Lookups and receipts
  expire after 20 seconds, with the service's ten-second audit retiring expired
  entries. Disconnect
  notification also retires that caller's entries. Replies from a peer other
  than the broker cannot resolve a pending identity.

The service refuses incoming descriptors. Its ancillary reader closes every
received fd and preserves the count for decoding and InvalidArgs replies.
The helper negotiates descriptor transfer, reads one bounded frame at a time,
owns every installed descriptor through rejection, and accepts only the
reply from the broker-resolved activated portal name. It consumes the exact
received descriptor into File ownership, validates file type, unlinked status
and length, and reads positionally from offset zero
without reopening procfs or changing a shared offset. A byte beyond the
advertised extent refuses growth. It holds one 20-second exchange deadline.
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

Before any human session starts, firstboot transfers each deployed
session's existing store from the human UID to its reserved portal UID.
It pins the root-owned secrets parent, changes the private leaf to root
ownership, acquires its stable lock, validates and retains all known
single-link private files, transfers their ownership, syncs them, then
publishes the portal-owned leaf and syncs both directories. A root-owned
leaf and mixed old/new file owners are restartable intermediate states.
Every admitted leaf, including an already portal-owned one, is
restricted to root ownership and mode 0700 before entry validation. This
safely narrows a widened legacy leaf mode. An empty root-owned temporary
left before its ownership assignment is removed after validation; only
mode bits within 0600 are accepted. An empty interrupted lock with those
private bits is normalized to 0600. Committed root-owned data is never
accepted. Invalid entries or contention refuse migration and leave the
quarantined leaf unavailable to both human and portal. Firstboot logs
that refusal and skips release for this session while unrelated
provisioning continues. A wrong-owner object, symlink, or failure before
quarantine is reported as unconfirmed isolation: existing filesystem
access may remain. The portal independently requires its exact owner and
private metadata; a refused object is never treated as a successful
cutover. A published service-owned leaf accepts only service-owned data
files. Once service ownership is published, a subsequent sync or unlock
failure is conservatively reported as unconfirmed isolation: the leaf
is no longer root-quarantined. This report does not assert that the human
actually retains access. No credential or master bytes change. Existing
open descriptors and historical copies cannot be revoked; this operation belongs to
sysinit before user code.

This availability rule applies to per-session migration and TPM release.
Failure to establish the trusted root-owned secrets parent, or an unexpected
filesystem error inspecting a successfully migrated leaf, still fails
firstboot. These are failures of the shared trusted filesystem prerequisite,
not a malformed entry supplied inside one user's old store. The broker
requires successful firstboot and remains unavailable in that case.

`td-secret set --uid UID APP/NAME` reads credential bytes from stdin at the
root console. It uses the immutable identity parser and installed-account
checks to resolve the logical user's portal owner; there is no implicit root
store, caller-selected filesystem owner, or human-UID writer. Application and
entry names are parsed separately and never interpreted as paths. This is the
interim console operation authorized in §W.4, with no consent UI. The target
replacement binds a secure-attention token touch to one typed request and one
credential descriptor through td-authd. An enrolled store fails closed when
its TPM or volatile release is absent; no error selects a file master.
Console writes require root until increment (d); increment (c) must first
gate release on token presence.

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

Firstboot isolates and releases every deployed session's existing
enrolled store after identity enrollment and before application-home
provisioning. An invalid home cannot leave the existing store
human-owned after a successful migration. Migration refusal is handled
as described above. Once the store is isolated, a failed open or TPM
release logs a diagnostic and leaves credentials unavailable while
unrelated services continue. A root-console `td-secret release --uid
UID` retries a failed automatic release for a deployed identity. Release
removes an old volatile key before attempting the TPM and publishes a
replacement only after successful unseal and legacy cleanup. The portal
reads that release at `/run/td-secret/UID/key`: a 32-byte envelope
fingerprint followed by the 32-byte master. Root owns the traversable
runtime directories; the single-link 0600 file belongs to the reserved
portal UID. Every directory and file is opened without following links.
`/run` must be tmpfs, with no nested mount beneath the secret directory,
no active swap, and a zero process core-dump soft limit. A missing
`/proc/swaps` is accepted: Linux omits it when swap support is compiled
out. Other swap-table read errors and an empty or active table are
refused. The fingerprint rejects a release for another enrolled key.
Updates rewrite the encrypted bundle atomically and never persist a
plaintext key. There is no additional daemon or external service.

This release is **automatic at boot and has no human authentication**.
The portal service can read the released key and credentials. Human-UID
launchers can still register any installed application with its fixed
grants, so application impersonation through the broker remains until
the app UID and state cutover. TPM possession is not user identity. PCR
changes after release do not revoke already released bytes. Memory
zeroing remains best effort. The kernel, root, DMA and physical TPM-bus
interception are outside this increment's boundary; its TPM sessions are
not salted or parameter-encrypted. PCR policy cannot compensate for a
boot path that fails to measure attacker-controlled code. These limits
are not FIDO2, secure attention or elevation claims.

Publication provides crash atomicity, not rollback protection or secure disk
erasure. Btrfs snapshots, backups and old extents can retain the former
master and the former encrypted records; master rotation cannot revoke a
credential value recovered from them. Operators must replace real upstream
credentials after enrollment if historical copies may have escaped. The
atomic cutover removes the active legacy mechanism, not storage history.

## FIDO2 protocol prerequisites

`fido_hid.rs` implements the 64-byte CTAP HID report profile from
[CTAP 2.3 section 11.2](https://fidoalliance.org/specs/fido-v2.3-ps-20260226/fido-client-to-authenticator-protocol-v2.3-ps-20260226.html).
It contains no device enumeration, device I/O, authorization or release
consumer. The later hidraw transport must validate that the selected
device's input and output reports match this profile, own device access,
and impose one absolute transaction deadline. Keepalives and other-channel
reports never grant presence or extend that deadline.

Messages are bounded to 7609 bytes: 57 in the initial report and at most
128 sequential continuation reports of 59 bytes each. Request encoding
supports INIT, CBOR and CANCEL; CANCEL has no response. Decoding pins the
channel and expected command, refuses reordered, duplicated, restarted,
empty or oversized responses, and closes the transaction on any error or
completion. The typed initialization transaction owns the same nonce for
request encoding and reply matching. A complete INIT response for another
nonce is ignored while waiting for this request, without resending or
extending the deadline. Resynchronizing an already allocated channel
requires the reply to return that same channel. Reports from other channels are ignored without changing the
current assembly. Keepalives are accepted only before a CBOR response
starts. Initialization checks the caller's fresh eight-byte nonce, CTAP
HID version 2, CBOR capability and a nonzero, nonbroadcast allocated
channel. Future fields after the 17-byte INIT response prefix are accepted
within the same bound. Padding beyond the advertised length is ignored.
An error report received during assembly closes the transaction and retains
the device's error code in the diagnostic. Owned request reports and response
messages clear their buffers on drop; malformed-response buffers are cleared
immediately. This is best effort, not guaranteed erasure of compiler or
caller copies. The later transport must similarly clear its raw I/O buffers.

`Client::verify_es256` uses TPM2_LoadExternal and TPM2_VerifySignature
to verify a SHA-256 digest with a public-only P-256 key and fixed-width
ECDSA components. No private signing key or new cryptographic dependency
enters td. The loaded public area has the fixed ECC/SHA-256/ECDSA/P-256
template and null hierarchy; its returned Name must match the submitted
public bytes. Success requires the exact null-hierarchy verification
ticket with an empty digest. Loaded objects are flushed on success and
rejection; failed cleanup refuses success and retains the handle for the
client's final cleanup attempt.
If verification and cleanup both fail, the returned diagnostic includes
both failures.

The command profile supports TPM2_VerifySignature (0x177), as implemented
by the pinned emulator and existing TPM 2.0 devices. TPM library revision
185 deprecates it in favor of newer digest/sequence verification commands
([TCG command specification](https://trustedcomputinggroup.org/wp-content/uploads/Trusted-Platform-Module-2.0-Library-Part-3_Commands-V185-RC4_12Dec2025.pdf)).
An unsupported command fails closed; this prerequisite does not claim
support for a device that omits it. The future consumer must separately
validate CTAP CBOR, the enrolled credential identity, RP hash, presence
flags and the signed challenge, and bind enrollment metadata to the
sealed store. A successful signature check alone is no authorization.
Session release, recovery enrollment, trusted input and one-operation
writes remain subsequent work.

Tests cover report boundaries, literal wire encoding, sequence and channel
substitution, keepalive state, initialization binding, malformed TPM Names
and tickets, and cleanup failures. The target recipe compiles and runs the
ordinary crate tests with its source-built toolchain. The explicit
`emulator_es256` oracle uses an independently generated OpenSSL P-256
signature fixture, rejects changed public coordinates, digest and signature
components, and repeats verification beyond the TPM transient-slot count.
The fixture requires no OpenSSL at test time. Neither these tests nor
their software authenticator framing inputs prove a physical token touch.

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
TD_TEST_SWTPM=/absolute/path/to/swtpm cargo test --frozen --manifest-path td-secret/Cargo.toml emulator_ -- --ignored
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
tampering, identity substitution, file metadata, exact descriptor adoption and migration
idempotence, broker identity refusals, descriptor ownership and live
transfer. The image requires both the unconfined probe's exact credential
refusal and the supervised mail receipt marker. This composes the
provisioner, persistent store, td-mail configuration, jailed helper,
broker-fixed identity, authenticated decryption, descriptor transport and
acknowledgement.
It is not evidence for FIDO2 release or elevation; TPM evidence is separate
from this existing desktop boot check.

## Portal filesystem isolation

The stock portal runs as locked service `tdp1000` (UID/GID 991), started by
its root supervisor through `exec-service-as`. Firstboot prepares its private
0700 `/run/td-portal/1000` runtime under a root-owned 0755 parent. Credential
and dialog temporaries use that runtime. The broker admits this service only
for the stock human session 1000, preserves a positively proved unconfined
lineage, and still refuses unknown lineage or application registration. Root's
live direct-child activation capability remains necessary to claim the portal
name. The private compositor socket admits only UID 991; ordinary human
clients use the public socket.

The FileChooser reads only `/var/td-portal-files/1000/Downloads`, a
root-created read-only idmapped view of the existing human Downloads
directory. The fixed root helper and its syscall contract are specified
in td-authd/DESIGN.md and UNSAFE.md §16. No writable human-home grant,
ACL fallback, or file ownership rewrite is provided. The portal starts
after grant preparation settles, even if preparation fails. FileChooser
then refuses requests before exporting a Request unless the fixed grant
root is a real directory owned by the portal; a root-owned empty
mountpoint is insufficient. Settings and Secret remain available. No
failure selects the old human-owned service profile. The empty
root-owned mountpoint directories persist under `/var`; no file contents
are copied there. The mount is recreated at boot. Shared views stay
outside the jail's reserved private-runtime trees: putting this alias
under `/run` would correctly reserve the original Downloads grant and
refuse application launch.

The ignored `ownership_transfer_preserves_bytes_and_removes_human_access`
test requires a disposable root VM and `TD_TEST_ROOT_BUSYBOX=/bin/busybox`.
It exercises real UID changes, repeat migration, an interrupted transfer,
unknown files, links, foreign ownership and contention. It checks that the
old human and an application UID cannot read the master or records, the
service can read them, and the encrypted and plaintext credential bytes
survive unchanged. It never runs by default on the development host.
