# Local credential manager

## File-backed stores before enrollment

This is a dependency-free Rust implementation of APPLICATIONS.md §W.4.
The unenrolled backend is authorized by ordinary uid ownership. It does
not claim hardware protection, user authentication, elevation, memory
locking, or guaranteed erasure of Rust/compiler copies of secret
buffers. Filling owned buffers with zero is best effort. The portal
service owns the master; the human user and jailed applications cannot
traverse or mount the store. Offline disk readers can still recover the
unenrolled master. The application portal refuses this backend; it exists
only for firstboot provisioning and migration into token enrollment.
No server or external synchronization exists.

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

The activated desktop portal serves `td.Secret1` version 1. Application
lookup requires a valid token protector and the current volatile release;
file and legacy TPM-only backends are refused even if their keys are readable.
The unenrolled boot oracle requires a broker-authenticated mail refusal,
`portal: TD-SECRET-LOCKED app=mail name=main`, instead of a credential receipt.

The interface is:

- `Retrieve(s name) -> (h credential, s receipt)` performs a broker
  `GetConnectionCredentials` lookup for the original unique sender. Only an
  authenticated application whose external UID matches its immutable
  assignment is admitted. The portal checks the broker-reported UID and
  AppId together. The human UID cannot register as an application. No caller
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

`td-secret set [--recovery] APP/NAME` submits one credential descriptor
from the human session. The root console writer has been removed. Application
and entry names remain typed fields, never paths. Physical secure attention
selects the queued request and one fresh assertion authorizes its exact
snapshot. Neither root's former console syntax nor an existing volatile
release substitutes for that consent. Firstboot remains the initial writer
of an unenrolled placeholder; see the named-write intake contract below.

## TPM protection and legacy migration

Physical enrollment fixes SHA-256 PCR 7 through the immutable request.
The selected PCR must be nonzero, and the selection and composite digest
travel in the sealed object. This checks that measurements exist,
**not what measured components they represent**. The operator must establish
that the platform's measurement chain covers the intended firmware and boot
path. The current QEMU direct-kernel deployment path does not establish a
measured-deployment policy. It remains unenrolled; TPM presence alone never
enrolls it. The previous console `seal` and `release` commands are removed.
Legacy TPM-only bundles are accepted only as locked migration inputs to
presented FIDO enrollment; they cannot release application credentials.

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
selected PCR state loses access. This legacy format remains readable for
migration; no production command creates it. Token recovery does not replace
the TPM or authorize a changed PCR policy. Policy-authorized upgrades are not
yet provided. Do not enroll a store whose measured updates require recovery.

Firstboot isolates every deployed session's existing store and removes
volatile releases before application-home provisioning. An invalid home
cannot leave an existing store human-owned after successful migration.
Migration refusal is handled as described above. File stores are provisioned
but cannot serve applications; all sealed stores remain locked without TPM
or token I/O and without placeholder writes. No root-console release bypass
remains. The private presented unlock worker removes the old volatile key
before contacting the token and publishes a replacement only after a verified
assertion, successful unseal and legacy cleanup. The portal
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
The portal service can read the released key and credentials. Each
application registers only from its assigned external UID and reads its
private state; human-UID launchers cannot impersonate it through the
broker. TPM possession is not user identity. PCR
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
support for a device that omits it. The consumers below separately validate CTAP CBOR, the enrolled
credential identity, RP hash, presence flags and the signed challenge,
and bind enrollment metadata to the sealed store. A successful signature check alone is no authorization.
Session release, recovery enrollment, trusted input and one-operation
writes consume this transport under the contracts below.

Tests cover report boundaries, literal wire encoding, sequence and channel
substitution, keepalive state, initialization binding, malformed TPM Names
and tickets, and cleanup failures. The target recipe compiles and runs the
ordinary crate tests with its source-built toolchain. The explicit
`emulator_es256` oracle uses an independently generated OpenSSL P-256
signature fixture, rejects changed public coordinates, digest and signature
components, and repeats verification beyond the TPM transient-slot count.
The fixture requires no OpenSSL at test time. Neither these tests nor
their software authenticator framing inputs prove a physical token touch.

## USB token transport

`fido_device.rs` discovers at most 256 fixed `/dev/hidrawN` names. It
requires a root-owned, root-group, mode-0600 character device and the
kernel's USB HID bus metadata. Discovery reads metadata without opening
the device. The report descriptor must describe one FIDO usage-page
0xf1d0/application-usage 1 collection with exactly one unnumbered 64-byte
input and output report. Reports use byte-sized data/variable/absolute
fields and the FIDO input/output usages. Numbered reports, features,
nested/additional collections, push/pop and unsupported items refuse;
this deliberately supports a narrower profile than general HID.
Descriptor and uevent reads are bounded. The kernel and root-owned
`/dev` and `/sys` are trusted; a device name or bus claim is never an
enrolled token identity. The FIDO signature establishes that identity.

The built-in kernel profile enables USB, PCI xHCI, HID, generic HID,
hidraw and USB HID. The prompted parents are explicitly enabled after
allnoconfig; derived USB_XHCI_PCI is checked after olddefconfig without
a fictitious direct pin. The profile does not add legacy USB host
controller drivers. Raw token nodes stay root-only and never enter an
application jail or the compositor's input-device delegation. Separate
USB keyboard/pointer interfaces do enter the compositor's startup evdev
roster; its trusted-device boundary is specified in
`td-compositor/DESIGN.md` under Physical secure attention.

One root-owned Session starts `/proc/self/exe hid-worker`, retaining the
same executable version across deployment changes, with a cleared
environment and private inherited Unix socket stdio. Its typed arguments
contain only the bounded device index and
expected inode/rdev; the helper reopens without symlink following and
revalidates the device and descriptor. It never reads credentials or
store keys, changes device permissions, or spawns another process.
Root-only helper access grants no elevation. It accepts only a complete
report write or a request to read one report. Linux hidraw writes carry
a leading zero report ID, making 65 bytes; reads must return exactly
64 bytes, with a 65-byte receive buffer detecting oversized reports.
Short writes and failures have an unknown device outcome and are never
replayed. The API follows [Linux hidraw](https://docs.kernel.org/hid/hidraw.html).

Device input uses blocking reads without periodic touch polling.
USB output would block even with O_NONBLOCK, as the pinned kernel's
usbhid output path uses a synchronous USB transfer. The parent retains
an owned Child and uses one absolute socket-I/O deadline, at most two
minutes, across startup, fresh kernel-nonce channel allocation, every
write/read, keepalive and CBOR exchange. Partial stream traffic and
other channels cannot extend it. Any error poisons the Session and
kills and reaps its worker; Drop also kills and reaps it. The helper
arms a separate watchdog thread before device access; that thread requests
process exit after two minutes even while the I/O thread blocks. It also
checks the same lifetime between device operations and exits on parent
channel EOF. Abrupt parent death therefore leaves an independent exit
request even when device I/O cannot observe EOF. Uninterruptible kernel
waits can delay process exit and the parent's synchronous reaping. The
calling Session method or Drop can therefore return after the deadline;
the deadline bounds accepted protocol replies, not teardown completion.
This is not a hard real-time bound on an unresponsive kernel. No late
reply is accepted as authorization.

Every td-owned HID worker takes a nonblocking exclusive file lock before
opening a token. The stable empty `operation.lock` lives under root-owned
mode-0700 `/run/td-fido`, with root-owned mode-0600 single-link regular-file
metadata. The runtime and lock are opened through retained directory
handles without following leaf symlinks; existing invalid metadata is
refused rather than repaired. No worker renames or removes the lock.
The worker requires the deployment's procfs and root-owned mode-0755
`/run`. Creation normalizes only newly created objects, so a concurrent
creator or termination during normalization can leave a refusal until
trusted runtime provisioning or reboot; no existing object is repaired.
This volatile lock never survives reboot and is not recovery metadata.
The worker owns it through all device I/O until actual process exit, so a
parent crash cannot release exclusivity while its child still holds a
token. A lock refusal crosses the private startup channel as a fixed
busy-or-unavailable status, without file metadata or payload bytes.
A busy transport refuses promptly; there is no queued touch, stale
PID recovery, or retry. The independent watchdog remains the backstop for
an orphaned worker, including while it holds the lock.

This serializes all td-owned workers across tokens and sessions. Root and
the kernel remain trusted: the advisory lock cannot constrain a different
root program that opens hidraw directly. Trusted root must not replace
or overmount the runtime or lock while workers exist: that can split the
lock inode just as unlinking it would. The paired authority must
serialize complete presented operations, including any gap between worker
sessions, and bind a fresh assertion to each request. A transport lock is
neither consent nor enrollment. Host fixtures prove cross-process exclusion,
release after child exit and invalid metadata refusal; these do not claim
root path admission or physical token presence.

The trusted consumer must negotiate getInfo message limits before
constructing requests, bind fresh challenges to presented operations,
and verify enrollment/assertions through the metadata API. Keepalives
are transport progress only. This increment provides no enrollment UI,
session release, persistent store change or one-operation elevation.
Ordinary tests cover descriptor/profile refusals, real child/socket
ownership, successful repeated exchanges, stalled and malformed workers,
keepalive floods and partial-traffic deadline refusal. These fixtures
do not themselves prove physical token presence or a USB controller.

## CTAP assertion codec

`fido_cbor.rs` implements CTAP's canonical CBOR profile, bounded to 7609
input bytes, 1024 total values, and four nested maps or arrays. Byte and
UTF-8 strings borrow the caller's owned message. Integer and length
arguments must use the shortest representation; indefinite items, tags,
duplicate or unordered keys, invalid UTF-8, and trailing bytes are refused.
Map keys use the CTAP order for integers, strings and simple values;
complex container keys are unsupported. Floating-point representations
remain distinct by their encoded width, as CTAP requires. The prefix
reader supports embedded values; the whole-value reader requires complete
consumption. The bounded encoder preallocates its maximum capacity and validates its
complete output before returning it. Drop clears abandoned or failed output;
success transfers the allocation to a caller responsible for clearing it. These limits deliberately refuse larger future responses.
The ordinary CTAP message-size negotiation remains a transport obligation:
1024 bytes by default, enlarged only by the authenticator's getInfo value.

`fido_ctap.rs` builds one `authenticatorGetAssertion` request for the fixed
local relying-party ID `td.invalid`, one nonempty credential ID of at most
1024 bytes, and an exact 32-byte client-data hash. It requests user presence
and does not request user verification. It neither implements PIN handling
nor changes token policy: a token requiring an unsupported authorization
returns an error. The request is bounded by the caller's negotiated message
limit and the HID limit, including its command byte. Its typed owner retains
the same allow-list identity and client-data hash for response verification,
and is consumed on success or failure. Owned request buffers clear on drop
on a best-effort basis. Protocol diagnostics contain no payload bytes.

A response must have a successful CTAP status and canonical complete CBOR.
An optional credential descriptor must match the sole requested ID and
`public-key` type. Omission is permitted for the single-entry allow list.
A returned user entity must carry a bounded byte-string handle. A credential
count, if present, must be one; `userSelected` is forbidden for this request.
Unknown response members are ignored after bounded structural validation.
The authenticator data must match SHA-256 of the fixed relying-party ID,
set UP, clear the attested-data flag, and have consistent backup flags.
Reserved bits are ignored for compatibility; their exact received values
remain covered by the signature. The optional extension tail must be one complete
map with text keys; unsolicited extensions are accepted because CTAP permits
extensions without input. No tail is accepted when ED is clear.

The verifier hashes the ENTIRE authenticator data, including extensions,
followed by the request's exact client-data hash. It parses two positive,
minimal DER ECDSA integers into bounded 32-byte components, then verifies
through the TPM ES256 primitive. COSE keys must identify public-only EC2,
ES256 and P-256 with exact coordinate lengths; curve membership is checked
by the TPM. Returned counter, UV and backup flags are observations only.
Zero counters are accepted; no persistent clone-detection policy is claimed.

The caller must obtain the public key and credential ID from enrollment
metadata bound to the sealed store, and construct client data binding fresh
kernel randomness and the exact trusted operation. This codec cannot prove
those caller obligations. Reusing a challenge, substituting unbound metadata,
or accepting an unsolicited request would defeat the intended authorization.
No new command, device I/O, enrollment, store release or elevation is enabled
by this increment. Enrollment, getInfo policy and physical transport remain
subsequent work. The relying-party ID is a local namespace, not a contacted
server or a web-origin authorization mechanism.

The profile follows [CTAP 2.3 sections 6.2 and 8](https://fidoalliance.org/specs/fido-v2.3-ps-20260226/fido-client-to-authenticator-protocol-v2.3-ps-20260226.html)
and [WebAuthn authenticator data](https://www.w3.org/TR/webauthn-3/#sctn-authenticator-data).
Tests exercise independent literal request bytes, every truncation, canonical
encoding and resource boundaries, identity/flag/extension substitutions, and
DER/COSE rejection. An opt-in pinned TPM emulator test accepts a separately
OpenSSL-signed assertion and rejects changed client data, extension bytes and
signature bytes. Only public fixture material is committed; OpenSSL is not a
test dependency. This is a software protocol oracle, not a physical-token or
secure-attention demonstration.

## Token enrollment protocol

`fido_enroll.rs` negotiates the removable, presence-only CTAP2 profile with
getInfo. It admits advertised FIDO_2_0, FIDO_2_1 or FIDO_2_3; preview-only,
U2F-only and unknown-only devices are refused. FIDO_2_2 is not a defined
version string: CTAP 2.3 section 6.4 explicitly forbids advertising it.
It requires a 16-byte AAGUID, boolean options, user-presence
support, no platform attachment and no alwaysUv policy. ES256 must appear
when an algorithm list is advertised. Message and ID limits are positive,
clamped to td's existing bounds, with the specified 1024-byte message default.
An already configured PIN/UV token may create a non-discoverable credential
without PIN handling only when a modern version advertises makeCredUvNotRqd.
The implementation never changes token settings or retries around policy
refusals. Capabilities are untrusted hints, not identity or consent.

MakeCredential owns its exact client-data hash and requests only ES256 for
`td.invalid`, with a fresh opaque 32-byte user handle and fixed display text.
It sets rk=false; the default up=true and uv=false stay implicit, keeping
unsupported option keys absent. The request includes its command byte in the
negotiated size limit. Recovery enrollment must exclude the already proved
primary credential ID on the proposed second token; an oversized exclusion
refuses the operation rather than silently dropping that protection. Thus the second token must
support the primary token's actual credential-ID length. Separate primary
and recovery constructors make a recovery request require an already proved
Credential; its exclusion cannot be omitted through the public API.

The bounded response parser checks the RP hash, UP and AT flags, AAGUID
consistency (or an anonymized zero AAGUID for the none format), ID and public-only P-256 COSE key, and complete extension tail.
Reserved flag bits are ignored for compatibility. Backup-eligible or backed-up
credentials are refused by this device-bound profile. The attestation statement
is optional and structurally parsed when present, but not trusted: no attestation CA, manufacturer claim
or verifier dependency is added. This response produces only an EnrollmentProof
request with a different fresh client-data hash and the single returned ID.
Only a successful TPM-verified assertion under that returned key, with signed
UP and device-bound backup flags, produces a Credential. Request buffers and
credential ID/key buffers are cleared on drop on a best-effort basis.

The trusted caller must supply kernel randomness bound to the exact
secure-attention operation. Merely differing from the MakeCredential hash is
not a freshness oracle. It must bind the complete public credential bytes,
logical UID, RP and explicit second-token or unrecoverable policy into the TPM
metadata digest before atomic publication. A recovery workflow must require
an operator to use a second token and prove both credentials before publishing.
The enrollment API does not retain counters or UV observations for a clone
policy, nor persist unsigned AAGUID hints as identity. Exclusion detects
reuse of an ordinary authenticator; neither AAGUID nor an
unsigned capability claim proves that two credentials reside on distinct
physical hardware. Malicious/cloned authenticators remain outside that claim.
This increment enables no device I/O, console enrollment, boot release or
credential write; those consumers must perform the atomic policy cutover.

The profile follows [CTAP 2.3 sections 6.1, 6.2 and 6.4](https://fidoalliance.org/specs/fido-v2.3-ps-20260226/fido-client-to-authenticator-protocol-v2.3-ps-20260226.html).
Ordinary tests exercise policy refusal, negotiation defaults and limits,
recovery exclusion, all authenticator-data truncations and substituted fields.
An explicit pinned-emulator oracle uses the independent public OpenSSL fixture
to prove that enrollment requires the returned key and fresh proof challenge.

## TPM binding for enrollment metadata

The safe TPM API offers a `BoundKey` prerequisite for token enrollment.
It personalizes the existing ECC storage primary with a caller-computed
32-byte enrollment digest in the input public template's `unique.x`, with
empty `unique.y`. The TPM derives its primary key from its hierarchy seed
and input template, so another digest selects another parent and cannot
load the original sealed child. The generated parent public point is
structurally checked separately: each coordinate contains 1 through 32 bytes,
its extent has no trailing data, and the returned Name hashes the complete
public area. The trusted TPM generates the point; td does not implement an
independent curve-membership check for the storage parent. This uses the
existing CreatePrimary/Create/Load/PCR/Unseal commands and no new syscall.
Before sealing, td compares the bound parent's Name with the empty-unique
parent's Name and refuses equality. It retires the extra parent before
creating the bound one. This checks for ignored personalization on the
connected TPM; it does not independently prove a trusted TPM's derivation
algorithm or distinguish every possible digest.
The primary derivation follows [TPM 1.83 Part 1 sections 27.2.7 and 27.6.3](https://trustedcomputinggroup.org/wp-content/uploads/TPM-2.0-1.83-Part-1-Architecture.pdf).

A bound envelope is `TDBOUND1`, the 32-byte digest, and a two-byte
big-endian length followed by the ordinary encoded sealed child. The whole
envelope is at most 4096 bytes and rejects truncation, unknown magic and
trailing data. Its inner child retains the existing PCR-only policy and
sealed logical UID. `unseal_bound` first compares the envelope digest to
one computed by its caller from actual metadata. Editing both the digest
and metadata still fails when the TPM loads the child under the changed
parent. Removing the wrapper and using the old empty-unique parent also
fails. The zero digest remains a distinct 32-byte personalization, not an
alias for the empty unique field.

This is metadata integrity, not TPM validation of FIDO2 presence. The later
caller must hash a domain-separated canonical representation of the complete
UID, relying-party, enrolled-key and recovery policy; require the matching
FIDO assertion on the trusted operation; and only then invoke unseal.
The current oracle is the pinned software TPM; no particular hardware vendor
is certified by this increment. A device refusing this published primary
template fails closed, without selecting another parent. The persistent token protector below consumes this API; no console
command or automatic boot release consumes it.
The existing unbound API remains for current stores until the atomic FIDO
cutover. This API does not authorize token replacement or rollback a lost
recovery policy. A metadata change requires authorized unseal with the old
metadata, resealing under the new digest, and verification of the new sealed
key before atomically publishing metadata and envelope as one durable record.
Until publication completes, the old record must remain usable. Publishing
new metadata alone would make the old child unloadable and lose the store.
The current root/TPM/measurement trust boundary is unchanged.

An opt-in pinned emulator oracle proves original-metadata release, changed
metadata-plus-envelope refusal, stripped-wrapper refusal, UID substitution
refusal, restart persistence, and changed-PCR refusal. Ordinary tests cover
complete codec bounds, metadata mismatch before any TPM I/O, exact primary
template bytes for bound/zero/unbound inputs, and refusal when the TPM returns
the same primary Name for bound and unbound input. The emulator also proves
that a key sealed with a zero digest cannot use the unbound parent.

## Canonical enrollment metadata and release prerequisite

`fido_metadata.rs` encodes one logical UID, the fixed RP, one primary
credential and an explicit recovery choice. Construction requires verified
Credential values; recovery requires a distinct credential ID and public
key. This detects duplicate enrollment records, not distinct physical
hardware. The trusted enrollment flow must still require a second token or
explicit unrecoverability.

The format is `TDENROL1`, a big-endian u32 UID, a one-byte RP length and
`td.invalid`, a policy byte (0 unrecoverable, 1 second token), then the
primary and, for policy 1, recovery record. Each record is a big-endian
u16 nonempty credential-ID length, at most 1024 ID bytes, and the fixed
77-byte canonical public EC2/ES256/P-256 COSE map. No optional COSE hints
survive normalization; the complete signing key does. Decoding refuses
oversized input, other versions/RPs/policies, a UID different from the
independently admitted session, duplicate IDs/keys, malformed key shapes,
truncation and trailing data. The maximum record is 2230 bytes.

SHA-256 of `td-secret-enrollment-v1` plus a zero byte and the exact
canonical record supplies the TPM parent binding. Decode proves structure,
not disk integrity: an attacker may edit public metadata, but cannot load
the existing child under the resulting changed parent. Atomic publication
of metadata and its sealed envelope remains the store consumer's obligation.
A full old record can still be rolled back; there is no anti-rollback claim.

A release request owns the chosen enrolled key, ID, UID, full metadata
binding and challenge. Neither a replacement key nor a caller-selected
binding can be passed to its unseal method. It verifies the complete signed
assertion through the TPM, requires signed presence and device-bound backup
flags, and only then unseals the child bound to that metadata and UID.
Selecting recovery on an explicitly unrecoverable record refuses before
request encoding. The later trusted caller must supply fresh kernel
randomness bound to one presented secure-attention operation; this API
cannot establish freshness or consent from arbitrary caller bytes.

The persistent protector below consumes these safe metadata APIs. They
introduce no device access, root command or automatic token release. The
current TPM-only backend stays active until the trusted FIDO activation
flow can enroll existing stores. Root and the trusted TPM
remain in the trust boundary; this is a td-owned software ordering check,
not a TPM policy that itself evaluates FIDO presence. This store profile
requires presence only: it does not request PIN/UV or enforce a signature
counter. Possession of an enrolled token and a touch can authorize release
on the bound platform when the trusted operation is presented. Owned credential-ID
and request buffers are cleared on drop on a best-effort basis.

Ordinary tests pin independent literal encoding and a Python hashlib
binding vector, all truncations and record bounds, and malformed or
ambiguous recovery records. The pinned-emulator oracle enrolls two
independently OpenSSL-signed public fixtures, releases through either role,
and refuses changed challenges/signatures, the wrong role's key, removed
recovery, substituted metadata plus envelope digest, and another UID.
Only public fixture bytes are committed; no signing key or OpenSSL runtime
dependency is added. These are protocol tests, not physical-token evidence.

## Persistent token protector prerequisite

`TDFIDO01` joins canonical enrollment metadata and its matching TPM-bound
key into one immutable protector: eight magic bytes, a big-endian u16
metadata length and metadata, then a big-endian u16 key length and key.
The complete protector is bounded to 8192 bytes. Decoding independently
checks the logical session UID, exact metadata binding, canonical inner
formats and absence of trailing data. These checks prove structure; the
TPM authenticates the sealed child's parent on release.

A token-protected store uses `TDSEAL02` with the same record framing and
bounds as `TDSEAL01`, replacing its TPM-only envelope with this protector.
The outer version must match the inner kind. A malformed token protector
cannot select the old TPM-only or file-master backend. The complete
protector, including both credentials and recovery policy, supplies the
volatile release fingerprint.

The safe root enrollment API requires an already proved metadata value
from the trusted enrollment flow. It validates the no-swap, zero-core and
volatile-runtime requirements and clears any prior runtime release before
parsing enrollment metadata or reading the old protector. For an existing
TPM-only store it unseals the old master directly into the root operation's
memory; that migration key is never published to the portal runtime. The
store retains one decoded protector/record snapshot throughout migration.
Its advisory exclusive lock excludes cooperating readers and writers; the
credential service itself remains trusted.
It rotates the master, verifies a real bound seal/unseal roundtrip without
publishing that key, re-encrypts every credential, and atomically publishes
the protector and records together. File-backed stores use their existing
private master; token-protected stores refuse replacement before TPM access.
Owned old and new master buffers are cleared on return, on a best-effort basis.
It retires legacy files and clears the volatile release. A failed operation
before publication preserves the previous persistent store; after publication,
the token format is authoritative even if cleanup needs a retry. Every
attempt, including malformed metadata or a refused replacement, clears the
volatile release. A prepublication failure can therefore require another
release of the unchanged TPM-only store; no migration requires an earlier
runtime release or temporarily grants portal access. A mistaken enrollment
attempt against an already token-enrolled store also clears its live release;
restoring that session requires another fresh token assertion.
There is no token replacement or downgrade operation. A missing master in
an existing store directory always refuses initialization, even if only the
lock remains: deleting a sealed bundle cannot silently mint a new master or
placeholder. An interrupted first creation that never published its master
also refuses automatic retry and requires explicit repair. This is missing-
record protection, not an authenticated history marker: the credential
service is trusted, and whole-store substitution retains the rollback limit.

The pinned-emulator migration oracle starts with a locked TPM-only store,
converts two application records without a runtime release, reopens the
published token store, and proves that both enrolled roles recover the
original credentials. The old master cannot decrypt the new records. Failed
old-key acquisition preserves the original bundle byte-for-byte; a refused
token replacement clears the runtime release without accessing the TPM.
Ordinary tests cover relocking before malformed metadata and protector
admission. These oracles exercise TPM protocol and public signature fixtures,
not a physical enrollment gesture.

A release request owns the exact protector snapshot and the selected
primary or recovery assertion. Starting a request clears any old volatile
key before parsing or selecting the token. Completing a request clears
any old release before comparing the snapshot to the current stored
protector, verifying signed presence, and unsealing. With a valid writable runtime, every refusal leaves
it locked; publication follows successful verification and
legacy cleanup. The store's stable lock covers each API call. The paired
authority must serialize the entire presented operation across separate
calls and supply a fresh challenge bound to that operation.

Firstboot clears a prior volatile release before opening an existing store,
so malformed persistent bytes cannot bypass relocking. It recognizes the
token format during isolation and provisioning, clears volatile release state and retires interrupted legacy store files
without accessing a token or TPM. A locked store does not prevent
application configuration or compositor startup. It neither reads nor
creates a placeholder credential in an enrolled locked store. A legacy
plaintext mail credential still present beside that application is
refused and retained for explicit migration; it is never silently erased.
The public console `release` command is removed; release requires the
paired physical-attention flow.

The paired authority invokes the private workers below after physical
secure-attention selection and exact trusted presentation. Enrollment
creates the token protector; session release requires an enrolled token;
each queued credential write requires a separate token assertion. The stock
image remains explicitly unenrolled. TPM-only stores receive no automatic
release and require enrollment through the same trusted flow. Root and the
TPM remain trusted;
whole-store rollback, historical extents and best-effort memory clearing
retain their existing limits. The token assertion is checked by td-owned
software before TPM unseal, not by a TPM policy that understands FIDO.

Ordinary fixtures cover atomic rotation, failed roundtrip preservation,
complete format bounds and wrong-UID/version refusal. The pinned TPM
emulator seals and reopens an actual encrypted store, releases it through
both independently signed public token fixtures, and rejects changed
signatures, another protector and changed PCRs while removing any prior
volatile key. This is software protocol evidence, not a physical touch
or trusted presentation demonstration.

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
in td-authd/DESIGN.md and UNSAFE.md §16. The portal receives no writable
human-home grant, ACL fallback, or file ownership rewrite. Application
views are separately specified in td-authd/DESIGN.md. The portal starts
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

## Private unlock worker

`unlock-operation --uid UID` is a root-only child controller for the
paired authority session extension. It requires single-threaded startup, only
standard inherited descriptors, and an unnamed root-owned socketpair on
stdin; neither log descriptor may duplicate the child endpoint inode.
The descriptor-directory iterator observes itself as fd 3, after
standard-fd presence is checked and before stdin is cloned. The trusted
parent creates and exclusively retains its endpoint; it must never
delegate it, including as a child log descriptor. The child normalizes
its endpoint to blocking mode. This child transport relies on that root
launch invariant; it is distinct from the public/compositor channel
authenticated with credentials and pidfds. This private worker exposes no
listener or public human CLI; the admitted paired root authority is its
sole caller after physical selection.

Frames have a big-endian u16 length and 1..289 payload bytes, with one
five-second deadline spanning header and payload and an overall 120-second
operation deadline derived from the HID session lifetime. These are
cooperative checks; the parent must also enforce the child lifetime if
TPM I/O blocks. The first frame is the canonical TDCONS01 description
of an Unlock operation for the configured UID. Other operations refuse.
After startup and endpoint admission, before receiving the first frame,
the child clears any previous volatile key. Malformed input and an initial
disconnect cannot retain a release. A startup refusal requires the parent
to clear the prior generation; it cannot claim child cleanup. The child
then verifies the installed session's store ownership and requires a
token protector. It sends `10`, a fresh 32-byte kernel-random round value, and the complete
description, then requires `11` followed by exactly that round value and
description as the parent's presentation acknowledgement. The round value
is private protocol data and is not part of the displayed description.
The parent may paint after receiving this invitation, but completed
presentation and its acknowledgement must both fit within five seconds.
If painting cannot meet that bound, the operation fails before token I/O.
Only then does it discover and initialize exactly one connected token.
No token operation is retried after an uncertain response.

The CTAP challenge is SHA-256 of `td-secret/presented-unlock/v1` plus a
zero byte and the complete canonical description. The parent must supply a
fresh unpredictable nonce, admit that request, and acknowledge only the
exact completed trusted presentation. Primary and recovery requests select
the corresponding enrolled key. The child retains the bound protector and
assertion response, sends `12` plus a second fresh 32-byte round value and
the description, and waits for exactly `13` plus that round value and
description before TPM verification, unseal and publication. An earlier
round cannot be prequeued or replayed as the later acknowledgement.
The commit invitation also requires its answer within five seconds, after
the token assertion; a delayed decision fails and spends that touch.
Neither deadline permits retrying an uncertain token operation.
The existing bound release API compares the current protector snapshot.
The child returns `14` on success; the parent must also require successful
child exit, because expiry after writing that frame can still fail and
clear the key. Protocol bytes are hexadecimal here.
No credential, assertion or key is returned on this channel.

The parent must serialize cancellation and commit acknowledgement: the
commit record is the execution point, not a revocable approval. Before
sending it, cancellation, peer loss, deadline or a stale presentation must
close the endpoint and kill/reap this child. The parent must also supervise
its lifetime and clear session keys on generation loss. Token work happens
in the separate child so that the parent's terminal heartbeat can continue.
Those parent duties are not activated by this child entry point. After any
reported failure, including loss of the final success response, the child
attempts to clear the volatile key. A process killed after publication
requires the parent's generation cleanup; this child alone cannot promise
cleanup after SIGKILL. Existing HID worker ownership bounds device I/O.

Socket tests run the actual framed controller and prove that missing or
mismatched presentation acknowledgement cannot call token acquisition,
and missing or mismatched commit acknowledgement cannot call publication.
They cover complete-request challenge changes, frame bounds, expiration and
unsupported operations. Hardware release continues to use the existing
TPM/assertion oracles; these protocol tests do not claim a physical touch.

The ignored root fixture
`operation::tests::initial_failures_remove_a_seeded_runtime_key_in_the_root_vm`
requires `td.operation-fixture=1` on the kernel command line, no persistent
store, tmpfs `/run`, and the production binary at `/bin/td-secret`. It
executes that binary on real root-owned socketpairs and proves that initial
EOF, truncation, malformed descriptors, a wrong owner and an unknown
operation all remove a seeded portal-owned runtime key. It also exercises
the production descriptor inventory and unnamed-socket admission. Ordinary
host tests never run this fixture.

The private root-only `lock-session --uid UID` entry invokes the existing
volatile-session lock directly, before any persistent-store access. It
accepts only a canonical human UID and requires root through that API.
The authority uses it for generation preparation and teardown as well
as after killing and reaping an abandoned unlock child. Cleanup never
replaces a still-owned unlock worker; reversing that order would permit
a late publication after cleanup. This entry grants no secret access and has no
human consent flow. Paired generation startup and teardown invoke it under
the supervision rules in `td-authd/DESIGN.md`.

The disposable root fixture also runs `lock-session` against a seeded
portal-owned runtime key and requires successful exit with the key absent.

## Private enrollment worker

`enroll-operation --uid UID` uses the private unlock worker's root startup,
unnamed socketpair, descriptor inventory, bounded framing and deadline
checks. This private child has no public CLI. The admitted paired root
authority invokes it after physical enrollment selection.
The first canonical request must select Enroll, SHA-256 PCR 7, one explicit
recovery policy, and CreatePrimary for the configured session owner. The root
parent supplies the fresh unpredictable nonce. The paired compositor
activates this entry only after an explicit physical enrollment choice and
read-only store inspection. It presents every exact step before replying;
legacy console enrollment and automatic boot release are removed.

The child clears any runtime release before receiving the request, retains
the installed store's exclusive lock, and refuses an already token-enrolled
store. It generates a fresh opaque 32-byte user handle for both tokens. Each
step sends `10`, a fresh private 32-byte round and its complete description,
and requires `11` plus exactly that round and description before token I/O.
All descriptions retain the initial nonce, owner, platform and recovery
policy; only the step advances, in this fixed order:

1. CreatePrimary: wait for exactly one connected token, negotiate getInfo,
   then makeCredential with a challenge bound to this displayed step.
2. ProvePrimary: retain the same token session and creation response, make a
   fresh assertion, and verify its signature and signed presence through
   the TPM before obtaining a Credential value.
3. CreateRecovery, only for SecondToken: release the primary device session,
   wait for a different device node, and create with the proved primary ID
   in the mandatory exclusion list.
4. ProveRecovery: prove the new credential on that same recovery session.

Each challenge is SHA-256 of `td-secret/presented-enrollment/v1`, a zero
byte, and the complete canonical description, so roles, steps and recovery
policy cannot share an assertion. Device-node equality is a transport hint,
not token identity. Replugging the original token may change that hint, but
its possession of the excluded primary credential must still refuse normal
CTAP creation. Metadata additionally refuses equal IDs or public keys. An
untrusted token's own implementation remains part of the physical-token
trust assumption; USB discovery cannot prove two separate pieces of hardware.

Read-only discovery waits up to 100 ms between bounded passes until one
eligible node appears. During recovery it excludes the retained primary
node before counting candidates, so inserting the recovery token alongside
the primary is supported. More than one new eligible node refuses. No CTAP request retries after uncertain
delivery. A failure can leave an unused credential on a token, but never
publishes a partially proved enrollment. Primary and recovery token sessions
are separate, with only one open at a time. The entire operation, including
all touches, token replacement, acknowledgements and TPM work, shares the
existing 120-second lifetime; this is not a new window per step. The root
supervisor must enforce that deadline and kill/reap before cleanup if a TPM
call blocks. Child cooperative checks alone do not enforce a blocked syscall.

After the final proof the child constructs metadata and validates distinct
credential IDs and keys before requesting a separate `12`/`13` commit round
bound to that final step. Only that acknowledgement permits the existing
atomic token enrollment. This round is a private execution decision, not
another presentation or touch: the caller consumes the final step's completed
presentation receipt and serializes cancellation against the decision. The
already presented description names enrollment, platform and recovery policy;
no new prompt is rendered for the commit round. An existing TPM-only master is
unsealed privately for migration, never published to the portal. The child
sends `14` on success and clears runtime release on every ordinary return,
including success. The parent must also observe successful child exit before
reporting enrollment complete. Enrollment does not implicitly unlock a
session, and no token response, credential ID, secret or master travels on
this channel. Parent loss, cancellation and deadline require the same
retained-child teardown duties as unlock. Each compositor step transition
invalidates the previous receipt and fully presents the exact next request
before acknowledging it. A receipt never authorizes a different step.

Socket oracles exercise both recovery policies and omit or alter every step
and commit acknowledgement. They prove that no corresponding device call or
publication occurs, and that successful steps retain the primary credential
and bind separate challenges. An independent SHA-256 vector pins the complete
challenge encoding. These tests use the actual private framing and operation
sequence with a fake device; existing enrollment/TPM fixtures cover the
cryptographic composition. Neither is evidence of a physical token touch.

The marked disposable root fixture also exercises the production enrollment
entry on actual unnamed root socketpairs. For both unlock and enrollment,
initial EOF, truncation, malformed descriptions, a wrong owner, an unknown
operation and a valid request without installed store state must refuse and
remove the seeded portal-owned key. The existing explicit lock command is
checked alongside each entry. These fourteen cases need no TPM or token.


A failure after persistent publication does not mean the store is unenrolled.
Lost final delivery, late exit observation or cleanup failure can leave a fully
proved, enrolled store locked while the operation reports failure. The caller
must treat that completion as indeterminate, inspect current protector state
through a new admitted read-only operation, and offer a fresh unlock when it
is token protected. It must not retry enrollment as replacement or infer that
the old TPM-only master survives. Merely observing token-protected state does
not prove that this particular attempt succeeded. The compositor activation
must implement this reconciliation; this private worker exposes no query UI.

Production hardware wiring is parameterized over discovery, token sessions
and the TPM transport factory solely so ordinary fixtures can exercise the
same creation/proof path. The physical implementation still opens only the
existing fixed root device surfaces. Tests inspect the actual recovery
makeCredential CBOR exclusion list, prove separate session ownership, and
cover empty, primary-only, primary-plus-recovery and ambiguous discovery.
The test TPM accepts structurally valid verification packets as a wiring
stand-in; the separate pinned-emulator oracles establish real signature
verification. Fixtures additionally prove metadata refusal precedes the
commit invitation and lost completion does not undo a completed publication.
The root startup oracle's fourteen executions contain twelve distinct cases:
the explicit lock command is intentionally exercised alongside both workers.
It proves startup refusal and relocking, not production token I/O or presented
enrollment. The in-process framing and hardware-wiring oracles cover those
separate boundaries without claiming a physical touch.

## Read-only enrollment-state inspection

The hidden root helper `td-secret inspect-store --uid UID` resolves the
reserved portal owner through the immutable installed identity table. It
opens the existing store and existing lock without creating either, takes
the same nonblocking exclusive lock as cooperating readers and writers,
and validates the backend's bounded structure and ownership. It returns
exactly `17` followed by one byte: 0 file master, 1 TPM-only, 2 token with
explicit unrecoverability, or 3 token with a recovery credential. Failure
returns no state. The parent must require the exact result, EOF and a
successful observed exit. No path or filesystem owner comes from the peer.

Inspection does not access token/TPM devices, read a runtime key, decrypt
records, retire legacy files, normalize metadata, or create a lock/master.
The existing file-master validator reads and clears its private 32-byte
buffer internally; those bytes never enter the result. The existing lock
is taken read-only and released on close. Ordinary filesystem access-time
updates remain possible. Contention, missing state and malformed state
refuse; they never become permission to initialize a replacement store.

This is an advisory structural snapshot, not proof of a valid hardware
binding, token presence, usable recovery, or current release. A stored
recovery record still depends on the enrolled second token and the bound
platform. A subsequent operation independently validates current state.
In particular, after an enrollment whose success reply was lost, seeing
a token protector permits offering a fresh unlock operation; it does not
prove which attempt published it and never authorizes enrollment retry,
replacement, release or write. The paired root controller owns the helper
lifetime and admission, as specified in td-authd/DESIGN.md.

The inspection helper uses the private operation startup admission:
all root UIDs, single-threaded startup, only standard inherited descriptors,
and a root-owned unnamed stdin socket whose inode neither log aliases.
It writes the two-byte result directly to that socket with a two-second
write timeout. There is no buffered stdout result. The parent deadline
also bounds filesystem work and observed completion. Both production and
ordinary child fixtures use the same factory and descriptor assignment.

## Token-authorized named write backend

The root-only `Store::set_token` API consumes one owned assertion request
and its response for a single application/name and bounded credential byte
slice. Its caller must already have admitted the application and requester,
pinned the exact credential snapshot to a fresh operation nonce, presented
the immutable description, and won the one-operation commit decision. This
backend does not admit an unprivileged caller or display consent itself.
The paired consumer below supplies that admission and captures exactly one
sealed credential descriptor before starting the worker.

The store keeps its existing exclusive lock throughout the operation.
The API checks that swap is disabled and the core-dump soft limit is zero
through the same memory prerequisite helper as runtime access, without
opening or changing a runtime key. Unreadable or unsafe settings refuse
before the write callback can acquire a master.
Before signature verification or TPM unseal, it refuses malformed targets,
empty or oversized credentials, file and TPM-only stores, a changed token
protector, and adding a new record when all 128 slots are occupied. It
consumes the bound request to verify the signed assertion and unseal only
its matching TPM-protected master. An existing volatile release does not
substitute for this fresh assertion. The derived application key encrypts
one replacement record with a fresh nonce. One atomic bundle publication
preserves the protector and every unrelated encrypted record. Errors before
publication preserve the previous bundle; the existing rename/fsync contract
still permits an uncertain result after publication. There is no automatic
retry or rollback based on a missing success reply.

This operation neither publishes nor reads a session release, and does not
change any existing runtime key. The authority owns cancellation and session
cleanup policy. Master and derived-key buffers are cleared on every return
after acquisition, subject to the existing best-effort erasure limitation.
A pinned-swtpm oracle uses independently signed primary and recovery fixtures
to change one record in a locked store, verifies the other record is byte
identical, rejects invalid signatures without publication, and proves that
neither a runtime key nor persistent plaintext was produced. Host structural
cases reject invalid targets, legacy backends, changed protectors and full
stores before their unseal callback can execute. These are store-backend
oracles, not evidence of descriptor intake, a physical touch or UI consent.

## Private named-write worker

`write-operation --uid UID` admits only the existing root-owned unnamed
socketpair startup contract. Before receiving credential bytes it requires
no active swap and a zero core-dump soft limit. It accepts one canonical
Set description for its configured owner and verifies the application name
and UID against the active account set from the immutable installed registry
and verified account databases. A reservation whose application account is
absent refuses before credential intake. The request includes an
explicit primary or recovery token role, and the trusted text displays it.
The role-bearing Set wire tag is 4; the earlier unused tag 3 is refused.
All codec consumers move together; unlock and enrollment encodings retain
their existing bytes and challenge vectors.

After the description, this private root channel carries exactly one
big-endian u16 length and 1..4096 credential bytes. This separate receiving
method does not widen ordinary public-description or acknowledgement frames.
The whole input frame shares the existing five-second deadline. Its owned
buffer is retained before the first presentation invitation and cleared on
ordinary return or partial-read failure, with the same best-effort erasure
limitation as the store. No credential bytes or credential digest appear in
the public description, stdout, stderr, or any reply.

The parent must authenticate and admit the requester, pin the submitted
credential descriptor and capture its immutable contents, assign a fresh
unpredictable operation nonce, and pass only that snapshot to this worker.
The root child independently admits the application assignment and keeps
the exclusive store lock through completion. Only the paired root controller invokes this hidden entry. Its public
intake and physical selection are specified below; there is no console
write bypass.

The worker requires the exact fresh `10`/`11` presentation round before
one token assertion, using the selected enrolled role. It opens the TPM
transport before requesting presentation, so an unavailable device refuses
before any token touch. SHA-256 of
`td-secret/presented-write/v1`, a zero byte and the entire canonical request
is the assertion challenge. The parent owns the private association between
this fresh nonce and the captured credential; a public password digest is
neither needed nor exposed. There is no token fallback or retry after an
uncertain response. The fresh `12`/`13` commit round then authorizes exactly
one `Store::set_token` call. A presentation round cannot be prequeued as a
commit round. The worker sends `14` after publication, and the parent must
also observe successful exit before reporting success. A lost success
response can follow a completed atomic write; it never authorizes retry.

Write proof preparation and completion neither read nor change the runtime
key. An already unlocked session remains usable; a locked session stays
locked. Unlike unlock or enrollment, a refused write does not revoke an
existing release. Generation-loss relocking remains the root authority's
separate responsibility. The parent must serialize commit against cancel,
peer loss and expiry and kill/reap any abandoned child; the same 120-second
lifetime bounds token I/O and TPM work. Cooperative child timeouts cannot
bound a blocked TPM syscall by themselves.

Socket tests exercise the actual controller with primary and recovery
requests, maximum-sized captured input, omitted or mismatched presentation
and commit replies, and replayed rounds. They assert exact committed bytes
and that missing consent cannot execute the corresponding token or write
callback. Structural tests refuse unknown application assignments, wrong
owners, legacy Set encodings, invalid roles and truncated, empty, oversized
or expired credential frames. These tests do not claim a public descriptor
intake, physical token touch, or completed authority supervision.

The ignored `root_worker_refuses_a_retained_reservation_without_an_installed_account`
fixture requires `td.write-fixture=1` on a disposable root VM, initially
absent account files, tmpfs `/run` and the production binary at
`/bin/td-secret`. It installs an actual reserved application row without
its account and executes both write roles on real root socketpairs. Each
must fail at application admission without credential intake or a reply,
while preserving a seeded runtime key. Adding the canonical application
account/group/service-shadow entries makes the same description admissible.
The pre-fix source-built binary fails this oracle at the admission result.

## Named-write intake and physical selection

The public command is `td-secret set [--recovery] APPLICATION/NAME` in the
human session. It reads exactly 1..4096 bytes from stdin, preserving newlines;
stdin must be redirected or piped, so terminal echo cannot expose a typed
credential. No credential value or digest enters argv, logs, the prompt or
result messages. Swap must be disabled and the core-dump soft limit zero
before input is read. The client creates a close-on-exec memfd, writes the
bytes, and seals write, growth, shrinkage and further seal changes. The
private raw implementation is shared from `td-authd/src/secret_sys.rs` and
specified in UNSAFE.md §16. Owned buffers use best-effort clearing; a sealed
memfd cannot be overwritten and disappears when its final owner closes it.

The client connects only to `/run/td-authd/1000/set`, requires a root peer,
and waits for the exact `TDSET01` newline greeting before transfer. It sends
one u16 big-endian frame length, a version-1 typed target, and exactly one
SCM_RIGHTS descriptor. The target contains a one-byte role (1 primary or
2 recovery), then separately length-prefixed application and secret names,
each at most 64 ASCII bytes. Its maximum body is 132 bytes. The root
controller authenticates the actual human sender on every fragment, pins
its live pidfd, and validates the active installed application assignment.
It retains the exact sealed descriptor; there is no pathname reopen or
shared-offset read. A root-owned parent prevents endpoint replacement by
unprivileged clients. The public intake is separate from the private
compositor channel, which still refuses received rights.

Submission never opens a prompt. Pressing physical Ctrl+Alt+Esc and then a
fresh W selects at most one complete pending write. The root controller
checks protected memory again, captures the descriptor with a positional
read from zero, assigns a fresh nonce, and starts the fixed private worker.
The immutable prompt names the full application, credential, requester,
external application UID and selected token role. A presentation receipt
precedes the one token assertion; the final matching commit receipt
permits exactly one atomic encrypted-record replacement. The token proof
binds the fresh nonce and canonical target; only root knows the private
nonce-to-snapshot association. No public password hash enables guessing.

Admission has one five-second deadline and an admitted queue slot expires
after sixty seconds. Selection starts the existing 120-second operation
ceiling. There is one pending client and one secret-operation slot, with
bounded nonblocking intake work on every authority heartbeat. Application
UIDs cannot submit, including through a delegated human connection. A
changed sender, extra traffic, missing or extra descriptors, mutable or
oversized input, peer loss or expiry refuses the request. Cancellation
kills and reaps the child while preserving any previous runtime release.
Generation teardown reaps the child and then clears that release as usual.
After a commit may have reached the worker, a lost or failed result is
uncertain and must never trigger automatic retry.

The app receives only its decrypted credential through `.Secret`. This
local authority adds no remote store, synchronization service, account
password, remembered consent, privileged shell or crypto dependency in apps.
