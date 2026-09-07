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

The future trusted caller must supply kernel randomness bound to the exact
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
template fails closed, without selecting another parent. No store file,
console command or boot-release behavior consumes this API yet.
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

These safe APIs introduce no device access, persistent file, root command
or automatic release consumer. The current TPM-only backend stays active
until the complete FIDO store migration lands. Root and the trusted TPM
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
