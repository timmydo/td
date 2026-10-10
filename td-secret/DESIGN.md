# Local credential manager

The portable personal-vault workstream for td-pass is specified separately
in [PORTABLE.md](PORTABLE.md). It targets TPM-independent primary/backup
FIDO2 protection and supported foreign-Linux use. Its staged prerequisites
do not change the application credential interfaces or protection below.
The planned login-key root worker and record in
[../td-login/TOKEN-LOGIN.md](../td-login/TOKEN-LOGIN.md) reuse that
PIN/hmac-secret flow and the root-only USB transport below; they do not
change application-store protection either.

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
`/dev/tpmrm0`. That client is the sibling crate td-tpm
([td-tpm/DESIGN.md](../td-tpm/DESIGN.md)), shared with the disk protector;
td-secret keeps its formats and policy: PCR list parsing, the `TDTPM001` and
`TDBOUND1` envelopes, the owner-UID payload prefix, the refusal of an
unmeasured PCR, and ES256 verification. ACPI discovery and the TIS/FIFO and
CRB drivers are built into the target kernel. The device stays root-owned;
neither the portal nor the application receives TPM access. A deterministic
ECC P-256 restricted storage primary under the owner hierarchy wraps a
fixedTPM/fixedParent keyed-hash object. Its encrypted sensitive payload binds
the owner UID to the master; unseal rejects a transplanted or edited UID
before returning any key. The sealed object's userWithAuth bit is clear. Its
sole release policy is PolicyPCR followed by PolicyCommandCode(Unseal), with
SHA-256 throughout. An empty password cannot bypass that policy. Owner
authorization is assumed empty; a TPM whose owner hierarchy is administered
otherwise is refused, never reset or cleared. The client flushes transient
objects and sessions on success and failure. The kernel resource manager also
owns the connection lifetime. Replies, names, policy digests, templates and
lengths are checked; TPM failures are returned without retry or fallback.

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

Release requires the presented FIDO2 operation described below. The portal
service can read the released key and credentials. Each
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

Planned, not current: `td-install/ENCRYPTION.md` increment 8 (8a)
moves the CTAP code this section and PORTABLE.md describe (report
framing, CBOR, the CTAP codecs, the PIN protocols, hmac-secret, P-256,
AES, hidraw admission with its worker, and the test-only virtual
authenticator) into the std-only sibling crate `td-fido`, which forbids
`unsafe`, with no behaviour change, so that td-boot's selector can
unlock a disk with a FIDO2 token and td-tpm can salt its sessions;
td-secret keeps its stores, workers, records and its `unsafe` surface,
and its tests and guests pass unchanged. The module names below then
name td-fido's files. The dependency-free boundary PORTABLE.md states is
unchanged. Increment 8b then requires `FIDO_2_1` and a no-PIN probe at
login-key creation (TOKEN-LOGIN.md, "Token profile").

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

`fido_device.rs` discovers at most 256 fixed `/dev/hidrawN` names. Root
admission, the only mode on td, requires a root-owned, root-group,
mode-0600 character device and the kernel's USB HID bus metadata, and its
discovery reads metadata without opening the device. Desktop admission,
below, differs in its node mode and group, its denial report, its
process identity and its lock location. The report descriptor must
describe one FIDO usage-page 0xf1d0/application-usage 1 collection with
exactly one unnumbered 64-byte input and output report. Reports use
byte-sized data/variable/absolute fields and the FIDO input/output
usages. Numbered reports, features, nested/additional collections,
push/pop and unsupported items refuse; this deliberately supports a
narrower profile than general HID.
Descriptor and uevent reads are bounded. The kernel and root-owned
`/dev` and `/sys` are trusted; a device name or bus claim is never an
enrolled token identity. The FIDO signature establishes that identity.

The built-in kernel profile enables USB, PCI xHCI, HID, generic HID,
hidraw, USB HID and UHID. UHID permits the guest fixture below to exercise
kernel HID I/O; its root-only device is not delegated to applications.
The prompted parents are explicitly enabled after
allnoconfig; derived USB_XHCI_PCI is checked after olddefconfig without
a fictitious direct pin. Besides EHCI, built in for the ThinkPad T430s's
USB 2.0 port, the profile adds no legacy USB host controller drivers.
Raw token nodes stay root-only and never enter an application jail or the
compositor's input-device delegation. Separate USB keyboard/pointer
interfaces do enter the compositor's startup evdev
roster; its trusted-device boundary is specified in
`td-compositor/DESIGN.md` under Physical secure attention.

A root Session starts `/proc/self/exe hid-worker`, retaining the
same executable version across deployment changes, with a cleared
environment and private inherited Unix socket stdio. Its typed arguments
contain only the bounded device index and
expected inode/rdev, plus the runtime directory for desktop admission's
`hid-worker-desktop` role; the helper reopens without symlink following and
revalidates the device and descriptor. It never reads credentials or
store keys, changes device permissions, or spawns another process.
Root admission's helper grants no elevation. It accepts only a complete
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

A caller may opt into cancellation with `Session::open_cancellable` and
one cloneable, one-way `Cancellation` handle for the whole operation. The
parent caps each socket wait at 50 milliseconds to observe revocation,
while retaining the original absolute deadline. This polls only the
private worker socket; it does not poll the USB token or replay a report.
An interrupted or timed-out stream read/write resumes only its remaining
bytes. Startup, write acknowledgements, partial reports and keepalives all
observe cancellation. The parent checks again after decoding a completed
reply; cancellation or expiration drops that reply and closes the Session.
The existing `open` path retains deadline-only blocking waits.

Cancellation requests no CTAPHID_CANCEL exchange: a worker can already be
blocked inside device I/O, so another device request cannot be relied on
for teardown. An observed cancellation kills and reaps the owned worker,
with the same kernel-wait limitation described above. It cannot undo a
command already received by the token; no failed or uncertain operation
is retried. Stopping the worker does not prove the token has left its
presence wait: a subsequent operation may find it busy until its own
command completes or times out. This follows from the lack of a device
abort in this path; see [CTAP transaction abort](https://fidoalliance.org/specs/fido-v2.2-ps-20250714/fido-client-to-authenticator-protocol-v2.2-ps-20250714.html).
Only a newly presented operation may start another request, never an
automatic retry of the cancelled operation.
The cancellation handle has no reset and does not itself own
the worker. Between calls the operation owner must drop an idle Session
and all pending protocol state on revocation. A cancellation racing after
the final transport check still requires the consumer's authorization
check before releasing a secret or publishing state. These transport
primitives do not yet wire lock, suspend or UI events to the portable
backend. Child/socket fixtures exercise early and in-flight cancellation,
reaping, partial progress without replay and the unchanged deadline.

`Session::check_active` exposes typed Cancelled, Expired and Closed states
for protocol owners checking after local work. Cancellation or expiry stays
observable after an I/O error retires the worker, without interpreting the
transport's diagnostic strings. Existing deadline-only callers retain their
string error API. The portable transaction runner and explicit manual
hardware diagnostic are specified in `PORTABLE.md`. The diagnostic uses
the existing root-only Session admission, a host-owned terminal PIN prompt,
and no store or application authorization path. Its private terminal and
process-protection syscall surface is recorded in `UNSAFE.md` section 15.

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

Desktop admission is a second, explicit mode fixed when a device is
discovered, for the portable vault's standalone host adapter in
[PORTABLE.md](PORTABLE.md). It requires an ordinary account whose four
real, effective, saved and filesystem IDs are equal and nonzero, for both
user and group, so neither may be root. A node must still be a root-owned
character device with owner read/write and no execute, world or special
mode bits; any group is accepted. The kernel's open decision, through the
host's group or ACL policy, is the grant. Discovery reads metadata only,
as root discovery does; no process opens a desktop node outside the
bounded worker. A worker whose open the kernel refuses with a permission
error writes a distinct initialization byte, and the parent reports the
session as denied, distinct from a missing or busy token. Descriptor and
bus checks are unchanged.

A desktop Session refuses before spawning unless the parent's
`XDG_RUNTIME_DIR` is absolute, and passes it to
`/proc/self/exe hid-worker-desktop` with the device numbers. The worker
rechecks its identity, requires an absolute runtime path owned by the
account with mode 0700, and takes the same stable `td-fido/operation.lock`
there, owned by the account in any group. This serializes only that
account's td-owned workers on that host; it cannot exclude root, other
accounts or other programs. The same-account worker can be inspected or
signalled by the account's other processes, so desktop mode inherits the
host account boundary stated for standalone mode. On td, root-console
admission remains the only mode reaching a token, and raw nodes stay
root-only.

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
target graph. A host distribution's build of the same releases (swtpm
0.10.1 against libtpms 0.10.2) serves equally: it is host tooling in the
same way, outside every td artifact. The oracles check only swtpm's
`--version` line; swtpm reports no libtpms release (its `--print-info`
names the TPM specification and firmware date, not the library), so the
libtpms version is the operator's to match and is unchecked. Then run:

```
TD_TEST_SWTPM=/absolute/path/to/swtpm cargo test --frozen --manifest-path td-secret/Cargo.toml emulator_ -- --ignored
```

The ignored tests require that explicit executable path, create fresh
emulator state and Unix sockets, and never open a hardware TPM. The
emulator helper lives in td-secret's test module over td-tpm's public
client, which td-firstboot and td-portal also compile; td-tpm has no
emulator oracle of its own. The ignored emulator tests cover restart,
changed PCRs, a different TPM, private-blob tampering, unmeasured-PCR
refusal, and migration plus volatile release of a real encrypted store
without persisting the new master in its store directory. Filesystem
fixtures cover volatile release, fingerprint mismatch, missing keys,
metadata, stale temporary cleanup, failed unseal, root identity, core
limits, and compiled-out swap. The fixture paths and ownership enter
private helpers; production callers retain the fixed `/run` path and root
checks. Ordinary tests also exercise malformed envelopes and replies,
backend selection, failed enrollment, master rotation and interrupted
cleanup. A normal cargo pass with those tests ignored is not TPM
integration evidence.

### TPM through the QEMU guest device

`td-recipe-eval qemu-secret --tpm /absolute/path/to/swtpm` requires the
same pinned host swtpm described above and adds four cold TPM guest boots
and the eleven HID guests below to the authority checks. The host starts a
private software TPM control socket and attaches QEMU's emulated TIS device; there is no host TPM
passthrough. The source-built test executable calls the unchanged
`Device::open` and `Client` implementations through `/dev/tpmrm0`.
`td-recipe-eval qemu-install-encrypted --tpm` (td-install/ENCRYPTION.md
"Acceptance evidence") requires the same pinned emulator and starts it
the same way, from fresh state for each leg, attached to a UEFI firmware
boot whose measurements reach it. `td-recipe-eval qemu-boot-encrypted --tpm` starts
it the same way, but keeps one state across the installed machine's
installation and boots, as a machine's TPM would, and uses a fresh state
only in its fresh-TPM leg.

The sealing guest extends SHA-256 PCR 7 with a fixed fixture measurement,
seals a known fixture key with a metadata binding, verifies immediate
unseal, and persists only the encoded sealed object on a fresh one-MiB
raw disk. Subsequent guests receive that disk read-only. Both QEMU and
swtpm restart between cases; the next two cases retain the first TPM's
state directory. Cold reopen must reproduce the fixture PCR state and
recover the key. The PCR-change case first recovers it, then extends PCR
7 again and requires a TPM PolicyPCR refusal. The final case uses a fresh
TPM state directory, proves the PCR state still matches, and requires a
TPM Load refusal for the transplanted object. Transport errors cannot
stand in for those expected TPM command refusals.

Every guest requires its one-test passing summary, fixture marker, and
clean exit within 180 seconds. Emulator startup and shutdown each have a
ten-second deadline; failure includes bounded emulator diagnostics.
Private disk, sockets and TPM state are removed on ordinary completion or
failure. This proves the guest device transport, sealed-object persistence
and TPM/PCR binding. The key and measurements are fixtures; it does not
prove a measured deployment, FIDO2 presence, session authorization or a
credential-store write. The stock deployment remains unenrolled.

### HID through the QEMU guest kernel

The same optional command runs eleven further isolated guests using a
test-only virtual token created through Linux
[UHID](https://docs.kernel.org/hid/uhid.html). The fixture requires its
kernel opt-in and exact case selector before opening `/dev/uhid`, then
checks that it is a root-owned, root-group mode-0600 character device.
Its FIDO report descriptor creates a kernel hidraw node with USB bus
metadata. The normal bounded `Device::discover` and root-only admission
checks select it without changing permissions or accepting alternate
device paths. The fixture launches the source-built `/bin/td-secret
hid-worker` and exercises the unchanged Session initialization and CBOR
exchange. Production Session creation still uses `/proc/self/exe`; a
test build's `Session::open` starts `/bin/td-secret` instead, since a
test harness cannot serve the worker role, and a source test pins that
choice to `cfg!(test)`. The UHID event code is `fido_uhid.rs`'s ("UHID
binding").

The assertion case exchanges a fresh HID initialization nonce and a
fragmented CTAP request and response. Its known challenge, public key and
signature come from the independent OpenSSL fixture used by the host
TPM oracle; no signer or private token key is included. The virtual token
compares every reconstructed request byte-for-byte with the expected
sequence before counting it or responding, including the changed challenge.
Verification uses
the production TPM device client through `/dev/tpmrm0`. A second request
changes the challenge while the virtual token replays the original
signature; only the specific TPM VerifySignature refusal counts as the
expected rejection. A transport failure cannot satisfy that assertion.

The deadline case sends repeated waiting-for-presence keepalives without
an assertion. The Session's five-second deadline must expire, poison the
session, and kill and reap the production worker. The fixture must have
received the full CTAP request and emitted at least five keepalives; silence
does not satisfy the case. Both cases verify worker
retirement and disappearance of the virtual hidraw node after the fixture
closes its UHID descriptor. Fixture event reads are nonblocking, its thread
has a fifteen-second lifetime, and Drop stops and joins that thread.
Every guest also retains the outer 180-second boot bound and exact one-test
passing-summary requirement. Only the four cold desktop guests described
below attach a persistent filesystem disk.

This is evidence for kernel HID transport, production worker ownership,
assertion verification and deadline refusal. The USB metadata and presence
bit are software fixtures: this does not test a USB controller, establish
physical presence, enroll physical recovery tokens, release a session, or authorize
a credential write. Ordinary crate tests leave these guest-only cases
explicitly ignored. UHID device creation remains a trusted-root operation.


### Fresh virtual-token enrollment and recovery

Two of those HID guests compose the production enrollment and metadata APIs
with the guest TPM: `fido-enroll-single` chooses explicit unrecoverability,
and `fido-enroll-recovery` proves distinct primary and recovery credentials.
Each virtual credential has a fresh TPM-generated, null-hierarchy P-256
signing key. A test-only helper sends CreatePrimary and Sign through the
existing safe TPM transport, checks the returned template and Name, and
encodes the ECDSA result as minimal DER. The existing client owns and flushes
the transient handle. No private key is exported or persisted, and no
signing command or helper is compiled into the production program. This is
a software authenticator using the same emulated TPM as the verifier, not
independent hardware or an additional production cryptographic dependency.
The earlier fixed OpenSSL signature oracle remains an independent vector.

The virtual token supplies getInfo, none-format makeCredential, and signed
getAssertion replies over UHID and the production HID worker. Every complete
request must match the fixture's ordered expected bytes before any reply or
signature. Creation, proof and release use fresh guest kernel randomness.
The assertion responder also checks the complete canonical RP, credential
allow list, challenge and options before signing the received challenge.
The enrollment API must verify a separate proof before yielding a credential;
recovery creation excludes the proved primary ID. Reconnecting the original
virtual credential produces the specific credential-excluded CTAP status.

The fixture roundtrips canonical metadata and its PCR-7-bound sealed key,
then requires a fresh assertion to unseal the original random master through
each enrolled role. An unrecoverable record refuses recovery selection.
Replaying each role's prior assertion with a fresh challenge must reach the
specific TPM VerifySignature refusal. A recovery key signing the primary
credential's exact request must reach the same refusal; an unrelated
transport or parsing failure cannot count as either negative result.

Each exchange closes its Session and removes its virtual HID node before the
next registration. These responder threads have a sixty-second cooperative
lifetime; their Drop stops and joins them. TPM I/O can delay thread teardown,
so the guest's existing 180-second outer bound remains the final backstop.
These cases use no persistent disk and publish no store or session key.
They prove fresh credential/proof/recovery protocol composition and
assertion-before-unseal, not the private enrollment/unlock workers, trusted
presentation, physical presence, portal release or authorized writes.


### Private operations with virtual tokens

`fido-operations-single` and `fido-operations-recovery` run the production
`/bin/td-secret` enrollment, unlock and named-write workers over unnamed
root socketpairs. Each disposable guest installs a minimal root-owned
principal/account fixture and initializes a portal-owned file-backed store
with two known credentials. The existing guest TPM and UHID token helpers
supply fresh keys and signed assertions. No alternate executable, token
path, store path or environment setting enters a production worker.

The fixture parent checks every complete presentation and commit invitation
against its canonical request. It enables exactly the expected CTAP commands
only after receiving that step's presentation invitation, before echoing
its private acknowledgement. A token command before its invitation, or a repeated or out-of-order
command, fails the fixture. Expected challenges independently hash the documented
domain and full request, including the operation nonce, role and typed write
target. Enrollment checks the same opaque user handle across both tokens and
the complete recovery exclusion list. The primary virtual device remains
connected while the recovery device is inserted, exercising production
selection of a different node and separate HID-worker ownership.

Successful enrollment must retire the file master and individual records,
publish a token store, and leave it locked. Both policies then exercise
cancelled unlock and write commit rounds: parent EOF must produce the
specific authority-disconnected error, preserve the entire store bundle,
and leave no release. Successful unlock makes the original credentials
readable through the application-secret backend. A successful named write
changes the selected credential while the store is locked, produces no
runtime key, and requires another fresh unlock before readback. The other
credential must survive. The recovery policy additionally writes and unlocks
through the recovery token; the unrecoverable policy requires the specific
missing-recovery refusal after getInfo without an assertion.

The parent owns each worker, requires successful exit after the success
frame, bounds exit observation and stderr, and kills/reaps on unwinding.
Each token script must be completely consumed, with the exact exchange count,
and closing it must remove its HID node. The guest finishes with its release
cleared. Blocking TPM or teardown I/O retains the outer 180-second VM bound.

These are production worker and store-transition oracles in a disposable
guest filesystem, not cold-persistent store or physical-presence proofs.
The parent supplies simulated presentation acknowledgements and private
credential frames. It does not exercise td-authd's public descriptor intake,
compositor receipts, physical secure attention or the application's portal
connection. The earlier authority/intake guests and protocol negative cases
remain separate checks; they are not a combined desktop acceptance claim.

### Compositor and public write integration

The `fido-desktop` guest starts the production compositor at UID/GID 993
through `td-login exec-service-as`, paired with the production root
`td-authd terminal-serve`. The authority runs the real firstboot reservation
check against a disposable root-owned account table and matching ledger.
The compositor loads the normal immutable application policy, uses the
QEMU framebuffer, and opens the guest evdev devices after session preparation.
The fixture assigns those device nodes to the compositor just as trusted
seat setup does; the FIDO hidraw node remains root-only.

A separate UHID keyboard supplies ordinary key reports. X outside secure
attention must cause no token traffic or enrollment. Ctrl+Alt+Esc followed
by X enrolls an explicitly unrecoverable store through the compositor's
physical-input adapter, immutable renderer, private client, authority
controller and production token workers. Enrollment leaves the store locked;
a new attention lifetime with U must obtain a fresh assertion to release it.

The real human-UID `td-secret set mail/main` client submits credential bytes
through its sealed descriptor. Before W selection it must remain pending,
with no additional token command and an unchanged store bundle. A new
attention lifetime with W authorizes the write. Successful public completion
must accompany a changed bundle and correct credential readback, preserving
the other application record. Killing the compositor then requires observed
authority exit, runtime-key removal and listener retirement. A replacement
pair must start locked and require another attention selection and assertion
before reading the written credential.

The virtual token accepts the complete fixed CTAP command sequence and
requires distinct nonzero creation/assertion challenges and signs requests
using the earlier emulated-TPM signer. It does not
substitute presentation or commit acknowledgements: the running compositor
supplies those through its normal framebuffer receipt path. Exact challenge
binding and refusal cases remain covered by the preceding independent
fixtures. This test exercises one recovery policy; the preceding worker
fixtures retain both policies.

The same guest starts the stock session broker at UID 992 and the root
portal supervisor, which activates its direct child at UID 991. Two
source-built test applications run through the production td-jail entry,
with immutable mail/news UID assignments, real cgroup delegation, namespace
and seccomp confinement, and completed broker registration. Disposable
package and account files are prepared by the guest controller. A disposable
tmpfs backs /var; the production prepare-application-files command supplies
mail's required idmapped Downloads view and its matching release removes it
after the applications stop. This does not exercise image composition or
the root application-start launcher.
Both applications deliberately carry FLATPAK_ID=mail. Each test entry invokes
the production td-secret get client and checks its captured stdout against
exact credential fixture bytes. The client also completes its normal D-Bus
descriptor receipt acknowledgement.
The test entries and control files are absent from the distribution image.

Mail retrieval must receive the portal's unavailable error before enrollment
and while the newly enrolled store is locked. After a fresh unlock it must
receive its original credential and a mail-only record; news must receive its
own distinct main credential and be refused the mail-only name. A successful
public write must become visible through a fresh mail retrieval while news
remains unchanged. The applications, broker and portal stay live across
compositor loss and replacement: fresh retrievals must refuse after each
relock, and only another token unlock permits retrieval of the written value.
The applications cannot see the persistent store or volatile release path.
Previously delivered credentials cannot be recalled; the relock oracle
covers future retrieval, not application memory erasure.

This remains a disposable initramfs and software-device test, not a
cold-disk or physical-presence claim.


### Complete store across a cold desktop boot

The additional `fido-cold-create` and `fido-cold-reopen` guests share one
fresh 256-MiB disk and retained emulator state. The guest uses td-built
btrfs-progs to create a Btrfs `@var` subvolume and mounts it at `/var` with
nosuid,nodev. Each guest starts from a fresh initramfs and `/run`; distinct
kernel boot IDs establish that the second run is a new boot. Both guests
must unmount `/var` successfully before their normal poweroff.

Creation repeats the full desktop enrollment, public write, jailed retrieval
and generation-relock sequence with the explicitly unrecoverable policy.
It persists the encrypted bundle's digest and the public assertion challenges
beside the fixture's account ledger. Application control/response files are
removed after the jailed helpers exit, before unmount. The second guest
requires the same encrypted bundle and ledger, with no file master or old
individual records. Fixture setup never initializes or writes that store.
Before any token command, fresh mail and news requests must receive the
portal's exact unavailable refusal. A fresh secure-attention unlock then
permits mail's written credential and private record, preserves news's distinct
credential, and refuses news access to the private mail name. Compositor loss
must relock later requests. Unlock and retrieval must leave the bundle intact.

The disposable virtual-token signer uses an owner-hierarchy primary with a
persisted random public template input, so retained TPM state reconstructs
its key. The reopened public key must match the first guest's key. This is a
test-only signer; production token transport and TPM sealing are unchanged.
The second guest's assertion challenge must be nonzero and differ from every
creation/proof/assertion challenge recorded by the first guest. The expected
CTAP sequence permits no automatic enrollment or assertion before attention.

The `fido-cold-recovery-create` and `fido-cold-recovery-reopen` pair repeats
that disk lifecycle on a separate fresh volume with the second-token policy.
E on the real compositor attention screen selects enrollment. Only the primary
device is initially connected; after its proof request the fixture inserts a
second virtual device while leaving the first attached. Recovery creation
must retain the same opaque user handle and exclude the primary credential.
The two COSE public keys must differ. Both creation and proof consume fresh,
nonzero challenges, retained with the primary's challenges for the cold check.
The second device is removed after enrollment before the primary's normal
unlock/write sequence. Each token must consume its exact CTAP command count.

The recovery boot reconstructs and connects only the enrolled recovery device;
no primary signer or primary HID node is created. It requires exactly one
admitted FIDO device and zero token commands during both jailed locked reads.
R on secure attention must produce exactly getInfo plus one fresh assertion,
then the same mail/news scope and relock checks as the primary-policy boot.
This checks desktop enrollment and cold recovery through the public portal;
it does not claim that a recovery token can replace a lost TPM seed.

This proves orderly cold persistence and desktop release in QEMU. The fixture
regenerates immutable account/package scaffolding, not a full installed
system deployment; it does not exercise the system image's firstboot path.
The virtual token and sealed store share an emulator, so this does not prove
independent physical devices, token presence, TPM seed recovery, changed-PCR
migration, rollback resistance or abrupt power-loss behavior. The independent
worker guests continue to cover both recovery policies.


### Full deployment QEMU lifecycle

`td-recipe-eval qemu-secret-system --tpm /absolute/path/to/swtpm` uses the
same pinned emulator and a separate `system-secret-vm-test` image. That
recipe derives the stock system recipe, adds the source-built test output
with its debug companions before object indexing, and adds one fixture
service. The only change to an existing service is ordering seat assignment
after the fixture keyboard is enumerated. The shipping system closure has
no test executable, fixture service or synthetic PCR writer.

Two fresh QEMU processes boot the same signed deployment through the normal
selector, verified kexec, read-only EROFS root and persistent Btrfs @var.
The fixture runs beneath stock td-svc after successful firstboot, without
rewriting account tables, application configuration, store setup, broker,
portal or authority services. Its initial keyboard-ready barrier precedes
stock seat assignment; all token activity waits for the normal service
readiness and jailed mail's locked refusal. Only this selected test image
and explicit command-line opt-ins admit the fixture. Its selector is explicitly provisioned with the PCR 11 policy specified in
`td-install/DESIGN.md`. Before token setup the second kernel independently
encodes its actual deployment ID and `/proc/cmdline` and checks the resulting
PCR through the TPM. The host requires exactly one selector measurement
receipt and one successful post-kexec check per boot. The fixture never
extends PCR 11; the real verified selector owns that extension. Different
phase arguments yield different expected PCR values across cold boots.
Application-secret enrollment still uses the separate synthetic PCR 7
extension, so this does not activate a measured-deployment release policy.
Authenticated firmware entry and update authorization remain unimplemented.

The creation boot requires firstboot's file-backed mail placeholder and
portal-mode configuration without an application password file. E enrolls
primary and recovery devices, U unlocks, and the public one-operation writer
queues mail/main for W and a fresh assertion. Restarting the stock mail unit
must produce fresh portal receipt acknowledgements from its jailed helper;
the store's exact credential bytes are checked separately. Restarting the
stock compositor/authority pair must relock and make a fresh mail launch
receive the locked refusal again.

The fixture checks its fixed 1280x800, 32-bit framebuffer geometry and
matches independent bitmap expectations for the visible attention menu,
enrollment, unlock and stored notices. It waits for the overlay to disappear
before a new chord, and verifies the keyboard remains enumerated. Store
publication alone is not a completion-screen barrier. Key publication
precedes test-side store admission so observation cannot steal the worker's
exclusive store lock. Only this root test reads the private framebuffer;
no application capture or consent shortcut is added.

The recovery boot connects only the enrolled second device, requires the
same encrypted bundle, mail configuration, durable identity ledger and
machine identity after stock firstboot, and proves a different kernel boot
ID. R performs exactly one fresh recovery assertion before mail retrieval.
Both boots require exact CTAP sequences and nonzero, globally fresh
challenges; each cold boot starts with no release. The runner requires one
successful ignored test, the exact supervised result line, the selected
signed deployment ID, clean QEMU exit, stock persistent shutdown and an
offline Btrfs check. The private disposable volume and TPM state live under
`TMPDIR` (the ordinary temporary directory by default), separate from the
persistent build cache. The owner removes that scratch on completion or
error. Each guest has a 600-second outer limit. This is a
full-system credential lifecycle fixture; it does not substitute for the
separate general system, Firefox, abrupt-power-loss or physical-device gates.

Adding `--powercuts` selects four boots on the same disposable deployment,
volume and TPM state. The creation boot establishes and orderly-shuts-down
the baseline above. A recovery boot queues `td-secret set --recovery`
without selecting W, verifies the live pending client, unchanged bundle and
no write assertion, then parks for a host cut. The client prompt requires root's admission acknowledgement, so the cut
follows validation of the pending target and sealed descriptor. The next
boot must preserve
the exact baseline bundle and credential, start locked, and display no
ready write when W is selected before unlocking. It then uses fresh recovery
assertions to unlock and authorize a replacement. Only after the public
client succeeds, the stored notice is visible and exact readback succeeds
does it park for the second cut. The final boot starts locked, refuses a
stale queued request, and requires fresh recovery consent before retrieving
the replacement; normal generation teardown must still relock it.

The two cut boundaries emit exact root-fixture console lines and never
return from libtest. The host requires successful marker-triggered SIGKILL
and a reaped SIGKILL status, one exact boundary line, selected deployment
identity, and no panic, fixture failure, normal result or orderly shutdown.
A marker seen only after natural exit or timeout cannot prove a cut.
Only the normal creation and final recovery guests require libtest success,
stock shutdown and offline Btrfs checks. Intermediate boots recover the dirty
volume through its normal Btrfs mount; the host does not repair it.

The challenge ledger is fsynced before each token response so freshness
evidence survives the cuts. No test-side persistent sync, fixture metadata
update, or shutdown follows the acknowledged replacement: its durability
depends on the store's existing file-fsync/rename/directory-fsync path.
Host storage and the TPM emulator remain alive across QEMU termination.
This proves those two guest-crash boundaries, not host power loss, torn
sectors, interruption inside publication, lost-success-reply reconciliation,
or durable authentication-policy rollback resistance. No shipped worker
acquires a fault switch and the shipping image still contains no fixture.


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

**Login state.** TOKEN-LOGIN.md's increment 4 adds a second hidden
root helper, `td-secret inspect-login --uid UID`, which td-authd runs
with UID 1000 only, and only when the login record's name exists
(`td-authd/DESIGN.md`, login-state amendment 1), for request `1a` from
increment 4's C3. Both helpers are one function in `lib.rs`: the private
operation startup admission above, then the UID, then the read, then
the whole result written to the stdin socket under the two-second write
timeout, so a refused admission reads nothing and a failure writes
nothing. It reads no application store and takes the same parent
requirements. It reads `/var/lib/td/login` (root:root) once through the
record store ("Login record store"), whose walk, directory check and
name lookup are the shared predicate's; it takes no lock, opens no token
or TPM device, runs no temporary cleanup and writes nothing to the
directory. Its result is exactly `1a 00` for a damaged record, or for an
enrolled one `1a 01`, the record's version byte, the slot count and each
slot's four-byte fingerprint (the first four bytes of the credential
ID's SHA-256) in canonical slot order, so at most 36 bytes at the
eight-key cap (`login_store::MAX_INSPECTION`). Every other state
(unenrolled, a damaged directory, a read that could not complete) and
every failure writes no result and exits unsuccessfully; td-authd then
answers that the state could not be read, and its next refresh runs the
directory-and-name predicate again, which decides those states without
the helper. td's writer publishes by rename, and the store's read
refuses an inode that is replaced between inspection and open, or whose
size, mtime or ctime changes while it is read, and reads an inspected
name whose inode has no links as that race too, so a race with that
writer answers no result or one whole record, never a mix of two and
never `1a 00`.

Host tests cover each state and every damaged-record kind (a link, a
second link, a directory, a socket, a wrong mode, oversized, zeroed,
truncated, empty or trailing bytes, an unknown version, another
account's record), the exact bytes of the pinned record vector and of
one through eight keys enrolled out of order, a directory left
byte-identical with its temporaries in place, read checkpoints that swap
the record, change its mode, append to it or rewrite it, a lookup that
sees an unlinked inode, a renaming writer running alongside, the exact
argv, and admission before the UID or any read. The root startup
admission itself is `operation::startup`'s, shared and not re-proved
here. td-authd's tests play the helper with child fixtures, and
`qemu-secret`'s `supervise-login` root case runs the production helper
over a damaged record through `1a`.

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

The unchanged `td-authd/src/secret_sys.rs` transport also serves the
unprivileged Claude shell launch channel described in `td-authd/DESIGN.md`.
That channel's named consumer transfers a fresh PTY master, never a
credential. Named-write intake continues to require a sealed regular file
and cannot accept that descriptor. The transport further serves the
`set-hostname` intake (`td-authd/DESIGN.md`, "Elevation operations"),
which reads its sender's credentials and pidfd and refuses every
descriptor, so no credential or descriptor reaches it. See `UNSAFE.md`
section 16.

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
and waits for the exact `TDSET02` newline greeting before transfer. It sends
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

After validating the sender, target and sealed descriptor, root returns
one byte `02` acknowledging an admitted queue slot. Until this byte is
sent, the request retains its five-second admission deadline and cannot
be selected. A nonblocking send failure or expiry refuses the request;
backpressure never starts or extends the queue lifetime. A successful
send starts the sixty-second queue deadline. The client requires exactly
`02` within a bounded five-second wait after submission before printing
the attention instructions. EOF, timeout, rejection or an early completion
byte cannot be mistaken for readiness. This acknowledges admission only;
expiry, peer loss or later refusal may still invalidate the slot.
The greeting version changes atomically with both endpoints; the prior
protocol is refused rather than interpreted as an admission receipt.

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

The shared named transport also serves td-authd's local installation intake,
specified in its design and UNSAFE.md section 16. That consumer rejects all
incoming rights and retains only sender pidfds. It adds no raw operations or
credential access. Consent tag 5 describes a system installation,
`deploy-publish`, confirmed since L5 by its approval key; secret workers
reject it, and that key cannot substitute for a token-bound secret
operation.

Consent tags 11 and 12 describe the elevation operations `deploy-rollback`
and `set-hostname` (td-authd/DESIGN.md, "Elevation operations").
td-secret's copy of the shared codec decodes tags 5, 11 and 12, so their
refusal rests on each worker's operation match: the unlock, enrollment,
credential write and login-key workers each reject all three, which a
test feeds every worker.

## Login record codec

`login_record.rs` is the first piece of
[../td-login/TOKEN-LOGIN.md](../td-login/TOKEN-LOGIN.md)'s root login
worker. TOKEN-LOGIN.md, "The login record" and "Token profile", owns the
bytes; this module implements them. The worker ("Login-key worker"
below) decodes, checks and hashes records, and builds the ones its
writes publish. It
reads no file, token or entropy: the worker supplies the record bytes,
hmac-secret outputs and the 32 random bytes of each client-data hash.

Decoding takes the expected UID and refuses an oversized input, a wrong
magic, a version outside `READS`, another UID, a count outside one through
eight, an empty or oversized credential ID, a noncanonical or off-curve
key (through `fido_p256`'s validating constructor), truncation, trailing
bytes, and duplicate or unordered credentials. A decoded record keeps
the SHA-256 of the exact bytes read as `Record::digest`; the encoding is
unique, so re-encoding hashes the same. `write_version` picks the lowest
version in `READS` that both retained deployments' read sets list, and
refuses when there is none.

Only a `Record` builds a slot, so every verifier binds that record's own
UID and ID: `Record::enroll` takes one through eight proved keys, each a
credential, public key, salt and borrowed hmac-secret output; both
consuming methods, `with_key` adding one proved key and `without`
removing a nonempty set of enrolled keys, keep the UID, ID and untouched
slots and take the version to write. Each sorts and re-applies the decode rules.
`with_key` refuses a ninth or an enrolled credential; `without` refuses an
unknown or repeated credential and the removal of every key, since the
store layer unlinks the record instead. A login derives the verifier again
and compares it in constant time. The output stays in the caller's
clearing owner; the derived verifier is a private, non-Debug, non-Clone
owner cleared on drop. Stored verifiers, keys, salts and credential IDs
are public metadata, as TOKEN-LOGIN.md says. Authorize-phase client-data
hashes take the baseline record and refuse without it; every other phase
refuses one. Key fingerprints are `fido_ctap::fingerprint`, the first four
bytes of the credential ID's SHA-256, which td-pass's `Fingerprint` also
uses, as `PORTABLE.md` requires.

`tests/login_record_vectors.py` independently derives the record bytes,
verifiers, fingerprints and every phase's client-data hash with Python's
`hashlib`, `hmac` and its own integer P-256 arithmetic; the committed
`tests/login_record_vectors.txt` is what the tests read.

## Tier marker reader

`login_tier.rs` is TOKEN-LOGIN.md's "Deployments" reader, std-only and
`forbid(unsafe_code)`: the marker's grammar, and the bounded newc reader
that finds it. The worker's module, it is compiled by td-authd through a
reviewed `#[path]` for request 19 and for request `1d`'s selectors
(`td-authd/DESIGN.md`, "Elevation operations"), and hashes with the
`engine/src/sha256.rs`
copy each crate already compiles (td-secret names `crypto`'s as
`crate::sha256`). `read` takes a held deployment directory, the ID it is
read against and the owner its files must have; `retained` takes the
volume and a selector name, `current` or `previous`, and reads the
deployment it names by that name. Any failure reads no version.
`open_volume` and `selected` are `retained`'s first steps on their own:
the volume held as a directory, and the ID one selector names through
it, exactly `../deployments/` and 64 lowercase hex digits; td-authd's
`deploy-rollback` reads both selectors with them, and reads them again
through the same descriptor before it acts. The
system recipe writes the marker into every deployment from
TOKEN-LOGIN.md increment 4's C10b, which follows the enforcement the
marker claims, so these readers find it in a td-built deployment from
C10b onward and none in one built before; the tests below build their
own archives.

Each of `manifest` and `initramfs.cpio` is opened through the
directory's `/proc/self/fd` path with `O_NOFOLLOW | O_NONBLOCK` and
admitted only as a nonempty regular file of that owner within its bound,
4096 bytes and 512 MiB, before a byte is read. The manifest must hash to
the ID and be td-boot's exact four lines; the archive is then streamed
once through a fixed 64 KiB buffer, hashed as it is parsed, and must hash
to the manifest's `initramfs.cpio` entry. Its size is read again when
the read ends, and the stream must end exactly there: a file that shrank
ends the read early and one that grew is found by one more read. Each
read, each retry of that last probe, and the verdict once the stream
ends first check the caller's give-up, a parameter (the worker's ten
seconds per deployment, td-authd's two for request 19), so a read that
finishes late reads nothing; the give-up cannot interrupt a stalled
filesystem. The newc reader takes magic `070701` only, a name
field of 2 to 4096 bytes with its NUL last and no other, at most 65536
members before `TRAILER!!!` and only NUL bytes after it; other members'
data is passed through the buffer unkept. The marker is the one member
named exactly `etc/td-login-tier`, a single-link regular file of at most
256 bytes whose bytes are exactly the grammar; a second one, another
type or a malformed one refuses.

Host tests, run in both crates, cover the grammar's every rule, archives
with and without the marker and under other spellings of its name, every
bound at and past its limit (the archive's by a sparse file), a bad
magic, bytes after the trailer, truncation at each member boundary and
inside each header, a budget spent by the last read or the end probe, a
duplicated or odd marker, a size change in both directions and the
give-up, the digest chain both ways, links, a FIFO, a directory and the
wrong owner in each file's place, and retained deployments through
fixture volumes: both markers, one missing, malformed selectors, a
deployment directory that is a link, contents that are another
deployment's and no volume at all.

## Login record store

`login_store.rs` is the second piece of the root login worker: it
reads the login state and publishes or removes the record, as
TOKEN-LOGIN.md, "The login record", specifies, for the worker's reads
and writes. It
uses only std filesystem calls, adds no `unsafe` and no syscall surface,
and takes no lock: td-authd's single operation slot serializes writers
("Login-key worker").

The walk, the directory check, the name lookup and the temporary
cleanup are the shared login-state predicate's, `login_state.rs`
(TOKEN-LOGIN.md, "The login record"): one std-only file, with
`#![forbid(unsafe_code)]`, that td-firstboot and td-authd compile
through a reviewed `#[path]` (td-authd from TOKEN-LOGIN.md increment 4's
C3) and td-login (C5). It takes the root it reads
under, answers unenrolled, enrolled (the record name exists, whatever
it holds) or unavailable with a cause, and reads no record bytes; each
refusal also carries one line saying what was refused, for firstboot's
console. The store uses it with the production directory under `/`.

`Store::open` walks the directory path (`/var/lib/td/login` in
production) from `/` one component at a time, each opened with
`O_DIRECTORY | O_NOFOLLOW` through the previous one's `/proc/self/fd`
path, so a link anywhere on the path refuses, and keeps the leaf's
descriptor. The expected owner is a parameter, root:root in production
and the test's own IDs in tests. The directory must have that UID and
GID, mode exactly 0700 and a nonzero link count, rechecked on every read
and write, so a directory removed under a held descriptor never reads
as empty. A missing component, a link or a non-directory on the path is
a damaged directory, but only while the parent's descriptor path still
names the parent (same device and inode); otherwise, and for any other
open or inspection failure, it is unreadable. Only the leaf's metadata
is checked, as for the application store.

A read looks up only `<uid>`, in decimal. A missing name is unenrolled
only if the directory's `/proc/self/fd` path then still names the held
directory, so a store that later loses or over-mounts `/proc` reads
unreadable, never unenrolled; temporary cleanup makes the same check
before it lists the directory.
The name is inspected before it is opened, so a FIFO or device is never
opened: anything but a regular, single-link, mode-0600 file of the
expected owner, or a size above `login_record::MAX_RECORD`, is a damaged
record. The exception is an inode with no links: the lookup resolves the
name and reads its metadata in separate steps, so a rename over the
record, or the unlink of its last key, between them shows the replaced,
already unlinked inode, which reads as unreadable, never damaged; two or
more links stay damage, since td's writer never hard-links. The file is
then opened `O_NOFOLLOW | O_NONBLOCK`, must be the inode inspected and
pass the same owner, mode, link and size checks through its descriptor,
is read through a `MAX_RECORD + 1` bound, and must keep its inode, size,
mtime and ctime across the read; a failure at any of these steps is a
race and reads as unreadable, and the next read shows what the name
settled on. The bytes decode with the
expected UID, and a refusal is a damaged record. `State::Enrolled` keeps
the decoded record and so its exact-bytes digest. `State::baseline` is
`Absent` or `Digest` for the two valid states and none for unavailable,
so no write starts from an unavailable state.

Publication and removal first unlink every `tmp-` entry, syncing the
directory if any went, and compare the current state with the baseline.
Publication then creates `tmp-` and 32 hex digits from 16 caller-supplied
random bytes with `O_CREAT | O_EXCL | O_NOFOLLOW`, sets mode 0600 against
the umask, admits its owner, mode and links, writes, fsyncs, compares the
baseline again, checks the name still holds its inode, renames over the
record, fsyncs the directory, rechecks the directory and checks the
record name holds that inode. Removal refuses an `Absent` baseline, then
unlinks, fsyncs the directory, rechecks it and its descriptor path, and
checks the name is gone. The outcome is `Committed`; `Rejected` before
the rename or unlink is attempted; or `Uncertain` from then on. A
rejected publication removes its temporary, best effort and only while
its name still holds the file created; the next write's cleanup removes
any it leaves. A record for another UID is rejected. The
store writes the record in the version it was built with, which the
caller chose with `write_version`; it never picks one. Private `Stage`
hooks after each step let tests inject a failure or a concurrent change
at every point; the worker's tests reach them only through test-only
`publish_at` and `remove_at`, which name each stage. Each
publication and removal stage, from an absent and a present record,
leaves a later read with the old record or the whole new one, and a
later write succeeds. Private `ReadStage` hooks after the
inspection and after the read let tests swap the inode, change its mode
or append to it, each of which reads as unreadable; a test-only
descriptor root stands for a lost or replaced `/proc`. These injected
failures model neither power loss nor durability; the power-cut guests
("Login power-cut guests") cut a guest at the same stages and read what
a cold boot finds.

## Login CTAP primitives

The third piece of the root login worker is the protocol steps
TOKEN-LOGIN.md's "Token profile" names, in `fido_ctap.rs`, `fido_pin.rs`
and `fido_transaction.rs`; the worker calls identify, the login
assertion and creation. That section's "Wire choices" owns the bytes.

`IdentifyRequest` builds the silent getAssertion over one batch of
nonempty, distinct IDs, bounded like the assertion request, and `select`
returns the chosen index, or none for an empty NO_CREDENTIALS answer. It
refuses a credential outside the batch, an omitted one in a batch of
several, UP, UV or AT set and inconsistent backup flags, and checks the
user entity, count and extension tail with the assertion parser's own
checks, now shared; it parses no signature. `Profile::parse_login` reads
the getInfo claims the notebook's `Profile::parse` reads, except that
omitted extensions or options advertise nothing rather than refusing, and
returns `LoginRefusal::NoHmacSecret`, `AlwaysUv`, `PinUnsupported` or
`PinNotSet` before the shared policy. The notebook's admission, its
diagnostics and their order are unchanged, as is its defaulted list
capacity of eight; `advertised_list_limit` reports the advertised one.
`Profile::identifiable` says whether a credential's own silent request
fits the key's ID and message limits. `login_enrollment` labels creation
`td login`, leaves unidentifiable IDs out of the exclusion list and
refuses `ListTooSmall` before any PIN. `RetriesRequest` encodes
getPINRetries and parses the count and the power-cycle state.

`Transaction::identify` refuses an empty, oversized or repeated ID before
any token I/O, skips unidentifiable IDs, packs the rest in order into
batches within both the list limit and the message size, and returns an
index into the caller's list. Its replies come through the raw exchange:
any status but success or NO_CREDENTIALS is typed as for the notebook,
and `select` decides the rest, so only a bare NO_CREDENTIALS moves to the
next batch.
`login_assertion` and `login_create` run the notebook's key-agreement,
PIN-token, creation and proof sequence, the assertion shared as
`authorized_assertion`, and query getPINRetries after key agreement and
before each prompt, which receives the step and the count. Zero ends
as `Status::PinBlocked`, whatever the power-cycle state, and otherwise a
power-cycle state as `Status::PinAuthBlocked`, with no prompt; device
statuses stay typed as for the notebook. A login assertion's
client-data hash binds the step presented with that count, which exists
only once getPINRetries has answered, so `login_assertion` takes the
credential, key and salt (`LoginAssertion`) and its prompt returns the
PIN with the hash: `Profile::login_assertion` checks the credential
before any request and sends the same key agreement, and
`KeyRequest::bind` adds the hash after the prompt. `login_create` binds
both of its hashes the same way, taking only the user handle, salt and
exclusions (`LoginCreation`): `Profile::login_enrollment` returns an
`UnboundCreation` whose key-agreement request is final and whose `bind`
adds the creation's hash after its prompt, and
`MakeRequest::login_proof` returns a `LoginProof` whose prompt
(`LoginPin::Proof`) names the credential just created, which the prove
step shows, and whose `bind` refuses the creation's hash. Its sequence is
`create_and_prove`'s, written out with the two binds; the notebook's
`create_and_prove` and `MakeRequest::proof` are unchanged but for
sharing the reply checks. The requests and their order are the
notebook's, and the committed transcripts pass unchanged. A failure is
`LoginError::Refused` or
`LoginError::Failed` around the notebook's `Error`. A login assertion
refuses signed backup flags, as a proof does.

The notebook's bytes are unchanged: its creation passes `td personal
vault`, and its committed transcripts pass as they were.
`tests/login_ctap_vectors.py` derives the identify requests and
selections, the getPINRetries requests and replies and the login creation
requests with Python's `hashlib` and `hmac` alone. It recomputes
`pin_vectors.py`'s public creation seeds, so its creation rows differ
from that file's only in the labels; the committed
`tests/login_ctap_vectors.txt` is what the tests read.

### Virtual authenticator

`fido_virtual.rs` is a test-only (`#[cfg(test)]`) CTAP 2.1
authenticator behind the transaction runner's `Channel`, so host tests
drive the production notebook and login steps end to end without a
device; nothing in the shipped program contains it. It models td's
subset: getInfo, whose versions, extensions, `clientPin` (derived from
whether a PIN is set), `alwaysUv`, `pinUvAuthToken`, PIN protocols and
message, list and ID limits a test chooses; clientPIN getPINRetries,
getKeyAgreement, getPinToken and getPinUvAuthTokenUsingPinWithPermissions
for both PIN protocols; non-resident ES256 makeCredential with
hmac-secret, an exclusion list, configurable credential-ID length and a
none or packed self attestation; and getAssertion, silent or
PIN-authorized, with hmac-secret's separate UV and non-UV secrets. It
derives its own ECDH, KDF, AES-CBC and HMAC keys from td's primitives
and decrypts and verifies every PIN hash, token authorization and salt
authentication rather than trusting the client. A
getPinUvAuthTokenUsingPinWithPermissions request for makeCredential or
getAssertion must name its RP, which the permission table marks
Required; undefined permission bits are ignored. A token authorizes
only its own RP and permissions; on a FIDO_2_1 key a legacy getPinToken
token takes the RP of its first authorized use, and a FIDO_2_0-only
key's token never binds one. A FIDO_2_1 key clears a token's
permissions, except lbw, once makeCredential's or getAssertion's
presence step (CTAP 2.1 sections 6.1.2 step 14 and 6.2.2 step 9)
collects presence, whatever follows; the excludeList presence returns
CREDENTIAL_EXCLUDED before that step, whatever its outcome, and leaves
the token. A FIDO_2_0-only key keeps its pinToken until the next
getPinToken or power cycle. hmac-secret without presence, `rk` on
getAssertion and `uv` without pinUvAuthParam are refused with the
specified option errors, and `alwaysUv` requires pinUvAuthParam only
for a getAssertion that asks for presence. Retries start at eight;
each mismatch spends one, three consecutive mismatches return
PIN AUTH BLOCKED until `power_cycle`, zero is PIN BLOCKED, which a power
cycle does not clear, and a correct PIN restores both. A default
credProtect level 3 hides its credentials from any assertion without
UV. Scripts add presence delay, denial and timeout, a wrong secret, a
stale signature (the key's previous one, over other data), a foreign
signature, a replay (the current data signed over the client-data hash
of the key's last earlier assertion that asked for presence, which
verifies exactly when the client sends that hash again) and signed
backup flags, and a test may change a key's configuration between
operations, as a key's own tool toggles `alwaysUv`. Its state is a
plain clonable struct; randomness is SHA-256 over a seed and a count,
or bytes a test injects.

It signs with `fido_p256`'s test-only ES256 signer, whose nonce follows
`pin_vectors.py`'s rule, so given that file's authenticator-side rows
it reproduces the committed key-agreement and PIN-token replies byte
for byte, and the assertion reply too once a test pins the hmac-secret
output (`Output::Fixed`), since the vectors' output is not derived from a
credential secret. It does not reproduce the makeCredential replies,
whose credential ID and key it draws itself. It does not model
attestation chains or certificates, real presence, resident credentials,
built-in UV, credProtect requested by the client, PIN setting or change,
reset or largeBlobs, and passing against it proves no hardware's
behaviour. A key is one shared, locked state, so a clone serves it from
another thread; `touching` says whether a request is inside its
scripted presence delay. This core is the authenticator logic
TOKEN-LOGIN.md's Evidence names.

A persistent key (`Virtual::persistent`, `Virtual::restore`) keeps its
`State` in a file, so a guest's key keeps its credentials across a cold
boot: the PIN, the retry count, the signature counter, the random seed
and draw count, and each credential's ID, private scalar, both
hmac-secret randoms and credProtect level. The PIN token, key agreement
and consecutive-failure count are volatile, as a power cycle clears
them, and the configuration is the test's, as a key's model is. Creating
one refuses an existing file. The state is saved after every request and
every test edit that changed it, before the reply, as a real key's flash
write precedes its reply: a sibling `.next` file is written and synced,
renamed over the file and the directory synced. Only a persistent key
copies its state to compare. A failed save answers CTAP2_ERR_OTHER and
is kept for `saved`, which the guests check. One that failed before the
rename left the file as it was, so the key's `State` goes back to it,
and the agreement key and PIN token go too, since they may come from
draws it rewinds: no rewound draw stays in use, and none reached a
reply. The volatile consecutive-failure count and a consumed injected
draw are not restored. One that failed after the rename, at the
directory sync, leaves the name holding the new state or the old with no
telling which, so the key is poisoned: it keeps the new state, answers
every later request with CTAP2_ERR_OTHER, takes no test edit, and
`saved` says so, so memory and disk never silently diverge. A test
injects either failure into the next save (`fail_next_save`). `freeze`
refuses every later save before it writes anything and calls an alarm,
which the power-cut guest uses. The file is the magic `TDVKEY01`, the
body's u32 length, the body and the SHA-256 of all before it, integers
big-endian. The body is, in order: a PIN flag byte, then if set a u8
length and the PIN; the retries (u8); the signature counter (u32); the
draw count (u64); a u8 length and the seed; the credential count (u8, at
most 32); and per credential a u16 ID length (1 to 1024), the ID, the
32-byte private scalar, an hmac-secret flag byte, then if set the
32-byte UV and non-UV randoms, and the credProtect level (u8). A file
over 64 KiB, a wrong magic, a length that disagrees with the file, a
digest mismatch, a short field, bytes past the last one, a flag other
than 0 or 1, retries above eight, an empty, oversized or repeated
credential ID, an invalid P-256 scalar or a credProtect level outside 1
to 3 is a typed `Damage`, never a panic; a guest fails naming the key
and the damage. Only a state that decodes back to itself encodes. Host
tests restore a key with its credentials, secrets, PIN and spent retries
but not its PIN AUTH BLOCKED state, check that each change is on disk
before its reply, refuse every truncation, every flipped bit and each
out-of-bounds field of a digest-valid file, and answer a failed save
with ERR_OTHER: before the rename with the file's old state and a key
that goes on, drawing its agreement key again; after it with the file's
new state and a poisoned key that refuses every request and edit;
frozen, with no write and the alarm raised.

### UHID binding

`fido_uhid.rs`, also test-only, holds the UHID event code every guest
fixture shares: `guard` requires the guest's `td.hid-fixture=1` and exact
`/case` and root before any fixture opens `/dev/uhid`; `Uhid::create`
checks that it is a root-owned, root-group mode-0600 character device
and sends CREATE2 for a USB-bus device under vendor 0x1209 with the
caller's name, product and report descriptor; `input` sends INPUT2 and
`output` returns the next queued 64-byte output report after its
leading zero report ID, passing over START, STOP, OPEN and CLOSE and
refusing anything else. Reads are nonblocking; closing the descriptor
destroys the device. The scripted HID guests' token and the desktop guest's keyboard
use it; both check `/dev/uhid`'s owner, group and mode.

`Plugged` presents a virtual key as a FIDO device with the descriptor
`Device::discover` admits, and waits until a hidraw node whose
`HID_NAME` is the caller's exists; its thread speaks CTAPHID (CTAP 2.1
section 11.2). A broadcast INIT allocates a fresh channel, answered with
the nonce, protocol 2 and the CBOR and NMSG capabilities: a CTAP2-only
key, which td's sessions are the only clients of. They allocate no other
way and send no CTAPHID_MSG; a report on any channel but the last
allocated one, or anything but INIT and CBOR, fails the fixture. A
complete CBOR request goes to the key on a helper thread; while it
works, the binding sends a KEEPALIVE each 98 ms (`KEEPALIVE_PERIOD`, the
section's 100 ms ceiling less its 2 ms poll, so only scheduling can
stretch a gap past the ceiling), UPNEEDED while the key is touching and
PROCESSING otherwise, then the reply's fragments. The key also totals
how long its scripted touches actually took (`touched`). It counts
channels, requests and each keepalive status and records the longest a
pending request went without a frame, and `remove` returns those counts
once the device and its node are gone. Removing and inserting a key
again, with `power_cycle` between, is a reinsertion: a new device and
node, and a cleared PIN AUTH BLOCKED. It needs no TPM. Its thread ends
itself after 170 seconds, inside the guest's bound.

## Login-key worker

`login-operation --uid UID` is TOKEN-LOGIN.md's root worker, in
`login_operation.rs`: session unlock, first enrollment, key addition
and key removal. td-authd supervises it on the paired `TDLA003`
requests `1b` and `1c` (`td-authd/DESIGN.md`, "Login-key operation
supervision"), but nothing in production starts it except an unlock:
the compositor sends `1b` for a first enrollment or an addition, which
a production td-authd refuses before starting anything, as it refuses
removal, and an unlock's `1b` only from its lock surface while the
login state is enrolled, so only on a machine holding a record, which
nothing in production writes (TOKEN-LOGIN.md increment 4's C7); the
compositor's PIN field sends `1c` only at a PIN step, which only that
unlock reaches.
It reaches the command line only
through `run`, keeps the unlock worker's root startup, unnamed
socketpair, descriptor inventory and bounded framing, and uses its
`10`/`11` presentation and `12`/`13` commit rounds. It neither reads nor
clears an application-store key.

Its frames, in hex: the worker sends `18` the baseline, `10` and `12`
invitations, `14` success and `15` a failure; root sends the
description, `11` and `13` acknowledgements and `16` a PIN after each
PIN step's acknowledgement. `17` stays store inspection's. A disclosure
step's `11` (TOKEN-LOGIN.md increment 5's A4) is waited for until the
operation's deadline rather than the five seconds above, since the
person reads the disclosure and types its approval key before root
acknowledges and no token I/O is open meanwhile. That step is the
first, root's own description with root's key, which the worker repeats
exactly after deriving the same step from the decoded operation; every
step it derives later carries no key.

1. After startup it requires no active swap and a zero core-dump soft
   limit, as the named-write worker does, since it will hold a PIN and an
   hmac-secret output; a refusal is INTERNAL and comes before the
   baseline. Then, before any presentation or token I/O, it reads
   `/var/lib/td/login` (root:root) once and sends the baseline: `18 00`
   unenrolled, or `18 01`, the slot count and each slot's four-byte
   fingerprint in canonical order. These fingerprints are what root's
   `begin_login` and `admit_login_step` take. An unavailable state sends
   its failure instead, so no description is ever admitted against it.
   The worker keeps the record it read, and checks against that.
2. Root's first frame is the TDCONS01 description of a login operation
   for this account. Another owner or operation, or one that
   `begin_login` refuses against the worker's own baseline or does not
   reproduce, is INTERNAL. An unlock, addition or removal against an
   unenrolled baseline is NO RECORD; an enrollment against an enrolled
   one is INTERNAL, since root was shown the record. An addition to eight
   keys has no encoding (its before count is one to seven), so root
   refuses it from the baseline's count and sends no description; any
   description it sends instead is INTERNAL, before any token I/O. The description fixes the
   deadline (below). A write that leaves a record then takes its version
   from `write_version` over the record versions the current and
   previous deployments read, and fails as VERSION when there is none,
   before any presentation; removing every key writes none. `run`'s
   read sets are the `current` and `previous` deployments' tier markers
   (increment 4's C4), read only when such a write asks for its version,
   so an unlock reads no deployment. Each is read through a held
   `/run/td-volume/td` descriptor as TOKEN-LOGIN.md, "Deployments",
   specifies: the `boot/` selector read once and required to be exactly
   `../deployments/` and 64 lowercase hex digits, `deployments/<id>`
   opened by that name, its `manifest` and `initramfs.cpio` root's
   regular files opened without following a link or waiting on a FIFO,
   the manifest hashing to the ID and the archive to the manifest, and
   the marker taken by the shared bounded newc reader,
   `login_tier.rs`, which td-authd compiles for request 19 and request
   `1d`'s selectors. A
   deployment whose marker does not verify, or a machine with no volume,
   reads no version. Every deployment built from TOKEN-LOGIN.md
   increment 4's C10b onward carries the marker, C10b following the
   enforcement it claims ("Deployments"), so `run` reads the record
   versions of each retained deployment that carries it and none from
   one built before C10b; a write still fails as VERSION while either
   retained deployment predates C10b. Production still refuses every
   write before it reaches the worker (`td-authd/DESIGN.md`,
   `login::WRITES`), so this changes no production answer until
   activation.
3. It presents root's own first step (identify, or an enrollment's
   first connect), then reads the record again: if it no longer reads as
   the baseline, an unavailable state included, it fails as RECORD
   CHANGED, before any token I/O.
4. An unlock, addition or removal identifies the key. Exactly one FIDO
   device must be connected; the worker does not wait for one (ONE KEY).
   Identify runs over the slots' credential IDs in record order with the
   identify-phase hash of the identify step and 32 fresh kernel-random
   bytes. No selection is NOT ENROLLED. Discovery must again find that
   one device, and a second session runs the selected slot's login
   assertion with its salt and key. After key agreement and
   getPINRetries the worker builds the unlock step, or for an addition or
   removal the authorize step, from the slot's fingerprint and the
   reported count, presents it, and takes one PIN frame: `16` and 4 to 63
   bytes, at most 64 bytes in all. A person types it, so its header waits
   for the operation deadline rather than a frame time; the rest then has
   one frame time. A longer header refuses before its payload is read.
   The bytes stay in clearing owners and never enter a diagnostic. Only
   then is the step's hash computed, over that exact step, so the
   signature binds the count shown; an authorize hash also binds the
   record's ID and the digest of its exact bytes. Every other frame the
   worker reads, the description and each acknowledgement, is also read
   into clearing storage, so a PIN root sends out of order is zeroed when
   the worker refuses it. The primitives verify the assertion (RP hash,
   UP, UV, the one allowed credential, the slot key's signature, no
   signed backup flags), then `Record::check` compares the slot's
   verifier in constant time and the output is dropped. A mismatch, a
   stale or foreign signature included, is FAILED. Nothing is retried.
5. A new key, each of a first enrollment's in turn or an addition's after
   its authorization, begins at a connect step: root's own first step
   for an enrollment's first key, otherwise one the worker presents. The
   person needs time to change keys, so the worker then polls discovery
   every 100 ms within the deadline until exactly one device is
   connected and it is not the device the previous session used, the
   authorizing key or the previous new key; none, or that device alone,
   waits, and several refuse as ONE KEY. A key removed and inserted again
   is a new device, which its exclusion then refuses. On that device:
   - one session creates and proves the credential: `login_create` with
     a fresh user handle and salt, excluding every enrolled credential
     and every credential created earlier in this operation. After its
     key agreement and getPINRetries the worker presents the create step
     with that count, takes its PIN and only then computes the creation's
     hash over that step; after makeCredential, the proof's key agreement
     and its own getPINRetries, the prove step names the new credential's
     fingerprint and that count, and binds the proof's hash the same way.
     CREDENTIAL_EXCLUDED is EXCLUDED, and a key whose list or message size
     cannot hold the exclusions is KEY REFUSED `05` before any PIN;
   - a second session's login assertion, presented as the repeat step,
     must reproduce the proof's hmac-secret output, compared in constant
     time, or the operation is FAILED;
   - the worker presents the probe step, then a third session's silent
     identify, under the probe-phase hash of that step, must select the
     new credential. It runs over the excluded credentials and the new
     one in record order, batched by the key's list limit as an unlock's
     identify is, so the key meets the batches an unlock of the record
     holding it would send: for an addition, exactly that record's; for a
     one-key enrollment, the new credential alone; for a two-key
     enrollment's second key, both, while the first key's probe cannot
     yet name the second's credential. Anything else is KEY REFUSED `06`:
     a key whose default credProtect level hides the credential from a
     silent assertion could never be selected by an unlock.

   Discovery must find the same device before the repeat and before the
   probe (ONE KEY). Every key is created, proved, repeated and probed
   before anything is published.
6. Every presented step is built from consent's types and admitted by
   the worker's own `admit_login_step` exactly as root admits it, with
   the baseline's fingerprints and, from the prove step to the probe,
   the created credential's fingerprint; it must answer `Next`, or `Last`
   for the operation's final step (unlock's unlock, a removal's
   authorize, the last new key's probe). The commit round runs only over
   a step admitted `Last`, and only with five seconds of the operation
   deadline left (`COMMIT_MARGIN`) when the worker sends its invitation
   and again when root's acknowledgement arrives; otherwise the worker
   reports TIMEOUT before any write. That makes it likely that a write
   and its success frame finish while root, whose deadline started
   slightly earlier, still waits; a success root has not seen by its
   deadline is uncertain to it. An unlock's record must read as the baseline before its commit
   round, and `14` follows the round. A write's commit round comes first;
   then the store must open and the record read as the baseline: a store
   that cannot be opened reports its unavailable kind, and a different
   state RECORD CHANGED, without attempting the write. Then the store
   publishes, against the baseline, `Record::enroll` (a fresh record ID),
   `with_key` or `without` in the chosen version, or unlinks the record
   when a removal leaves no key. Root must also observe a successful
   exit.

A write's outcome, never retried:

- **Committed** sends `14`.
- **Rejected** left the record name untouched. The worker re-reads it: a
  record that still reads as the baseline makes the failure FAILED, a
  local store failure such as an unremovable temporary, a failed write or
  fsync, or missing entropy; any other state, an unavailable one
  included, is RECORD CHANGED, since the record changed under the write.
  The re-read, not the store's diagnostic text, selects the kind.
- **Uncertain** means the rename or unlink was attempted. The worker
  re-reads and reports UNCERTAIN with what it found: `01` no record, `02`
  the baseline record, `03` the record this write built, `04` another
  record, or `05` an unavailable state. What `01` means depends on the
  operation: after a first enrollment the baseline was no record, so it
  is the baseline unchanged; after removing every key, which builds no
  record, it is the write's own result; after an addition or a partial
  removal it is another state. Root re-reads the state, as after any
  operation.

Root's acknowledgement of a write's commit round (`13`) is where the
write may begin. From then on, root treats any outcome but `14` followed
by a successful exit, a failure frame of any kind, a lost channel, a
failed exit or its own deadline, as UNCERTAIN, not failed, and re-reads
the login state; the frame's kind and detail only explain it. Before
`13` nothing was written. An unlock writes nothing, so its non-success
is a failure either way.

The failure frame is `15`, a kind byte, and for three kinds one detail
byte:

| Byte | Kind | Detail |
| --- | --- | --- |
| `01` | WRONG PIN | the count the key reported before this attempt |
| `02` | PIN AUTH BLOCKED: reinsert the key | |
| `03` | PIN BLOCKED | |
| `04` | NOT ENROLLED: identify found no slot | |
| `05` | ONE KEY: none or several devices, or a device other than this key's sessions' | |
| `06` | KEY REFUSED | `01` no hmac-secret, `02` `alwaysUv`, `03` no PIN support, `04` no PIN set, `05` list too small, `06` the probe did not select the new credential |
| `07` | DENIED: presence refused | |
| `08` | TIMEOUT | |
| `09` | NO RECORD | |
| `0a` | DIRECTORY DAMAGED | |
| `0b` | RECORD DAMAGED | |
| `0c` | STATE COULD NOT BE READ | |
| `0d` | RECORD CHANGED since the baseline | |
| `0e` | UNCERTAIN: a write's outcome | what the re-read found, as above |
| `0f` | FAILED: any other token, verification or store failure | |
| `10` | INTERNAL: root's frames, the channel, entropy or unprotected memory | |
| `11` | EXCLUDED: the key already holds an enrolled credential or one created earlier in this operation | |
| `12` | VERSION: no record version this build and both retained deployments read | |

Typed statuses select the kind, never diagnostic text. PIN_INVALID is
WRONG PIN only after this operation's PIN step; td never infers what
remains. PIN_AUTH_BLOCKED and PIN_BLOCKED are their kinds, and a
reported power-cycle state or zero count ends as one of them before any
PIN step, as TOKEN-LOGIN.md's table says. OPERATION_DENIED is DENIED and
CREDENTIAL_EXCLUDED is EXCLUDED; the touch and action timeouts, an
expired frame and the operation deadline are TIMEOUT; any other token,
transport or verification failure is FAILED, or TIMEOUT once the
deadline, or the open session's, has passed: a key that stalls a
session's initialization until the session deadline is TIMEOUT, though
the operation has time left. Root may be gone, so the frame is best
effort, and
none is sent after the operation deadline, which root's own deadline has
already ended.

The operation deadline counts from startup. The worker starts under the
longest, `LOGIN_LONGEST`, and narrows it once the description names the
operation to its `login_ceiling`, td-authd's (td-authd/DESIGN.md
amendment 6, reading allowance included). Each session opens with a
deadline of at most the transport's two-minute lifetime (`fido_device::MAX_LIFETIME`) and never past the
operation's, so the HID worker's own watchdog never cuts a session the
worker still waits on. While a session is open, its deadline also bounds
every wait on root, each presentation's acknowledgement and each PIN, so
a PIN cannot arrive for a session that has already expired; the wait
ends at the session deadline as TIMEOUT, and the operation deadline is
not narrowed for the waits outside any session, such as a connect
step's.

The worker holds no lock across its sessions. Each session's HID worker
takes `/run/td-fido/operation.lock` with a nonblocking `flock` on its own
open file description, so a lock the login worker held for its lifetime
would refuse its own sessions as busy; sharing it would change the
transport for nothing the operation slot does not already give. Writers
are serialized by td-authd instead. Only this worker publishes or
removes a record (firstboot removes only temporaries, before td-authd
starts). Within one td-authd, its single operation slot, shared with
the store's workers, runs one operation at a time, and its supervision
kills and reaps a cancelled or failed child before it admits another
(`td-authd/DESIGN.md`, "Private token child supervision" and its login
amendment 2). Across td-authd processes the guarantee is td-svc's, and
this design depends on it: td-authd's serving instance
(`terminal-serve`), the one that starts workers, runs only as the
`wayland` unit's `exec`, one instance, a `cgroup=service` pair-exec unit,
which a test of the system recipe's unit table pins, and the worker it
starts stays in that unit's leaf. Other units run `/bin/td-authd` for
other purposes. When either peer of the pair exits,
td-authd crashing included, td-svc writes the leaf's `cgroup.kill`,
which kills every process in it, the worker too, and starts the next
generation only once `cgroup.events` reports the leaf empty, as it does
for any launch of a pair, a restarted td-svc included
(`td-svc/DESIGN.md`, "Paired daemons and private descriptors"). A
worker past its commit acknowledgement when its td-authd dies is
therefore killed, possibly mid-write, and a new td-authd can start no
worker until it is gone, so no two login workers overlap; root re-reads
whatever the killed write left, as for any uncertain write. td-authd
launched outside that unit, by root, is outside this guarantee. The
baseline comparisons, the worker's before token I/O and before
publication and the store's before the temporary and after its fsync,
bound what such an overlap or any writer outside td's path, such as root
or someone holding the disk, can do unseen, which TOKEN-LOGIN.md does
not claim to stop: a change between the store's last comparison and the
rename is not seen.

Discovery and sessions go through a private `Devices` trait: production
uses the root USB transport, and host tests use in-process virtual keys
over a real socketpair behind scripted device nodes, playing root with
consent's own `begin_login` and `admit_login_step`, taking each prove
step's key as the created credential as td-authd will, and asserting
every frame root sees and the store afterwards. Unlock tests cover each
slot of a three-key record, whose two hashes match the identify and
exact unlock steps; a wrong PIN with a falling count and a later
success; PIN AUTH BLOCKED after three and no PIN step until reinsertion;
PIN BLOCKED and no PIN step after; a key not in the record; zero and two
devices; a tampered verifier and public key; a signature over other
data, the identify's; a replay over an earlier unlock's client-data
hash, refused in an operation drawing its own challenge and admitted
when the draws repeat, so the refusal shows the challenge is fresh; an
enrolled key later configured with `alwaysUv`, KEY REFUSED `02` at the
identify with no PIN step; no record, a damaged record and directory;
the record changed after the baseline and during the ceremony; root
never acknowledging; root cancelling at the PIN step; descriptions that
do not match the account or baseline; PIN frames outside their bounds,
none of which reaches the key; a PIN sent as the description or in place
of each acknowledgement; and a memory refusal before the baseline. Write
tests cover a one-key enrollment, whose create, prove, repeat and probe
hashes each bind their exact step and whose record its key then unlocks;
a two-key enrollment that waits through the previous key and an empty
port, excludes the first credential and publishes both; several keys at
a connect step and a key that never comes; the same key offered twice;
additions to eight, whose authorization binds the record, each new key
then unlocking, and the ninth refused before any token; removal of one
key, of several including the authorizing one, and of the last, which
unlinks the record even with no deployment marked; a credProtect default
failing the probe; an `alwaysUv` key refused as KEY REFUSED `02` before
any PIN, and one advertising it false enrolling; presence denied at a
creation and at an unlock's assertion, each after its PIN, as DENIED
with no retry spent; a list too small for the exclusions; an enrolled
key offered as the new one; a repeat with a different output; the record
changed before any token I/O and at the commit, with no write attempted;
every version refusal, empty read sets among them, with
the store's write never called; a failure injected at each publication
and removal stage, rejected as FAILED or reported UNCERTAIN with each
re-read detail, and a concurrent change rejected as RECORD CHANGED; root
cancelling at the connect, create, repeat and commit steps; a wrong PIN
while authorizing and while creating; a PIN wait ended at its session's
deadline well before the operation's, and a PIN within the session
unlocking; a key stalling a session's initialization to its deadline
(TIMEOUT) and one failing it at once (FAILED); a commit round refused
under its margin at the invitation, for a write and an unlock, and, run
alone, after a late acknowledgement, with no write attempted; each
timing test leaves the work it expects to finish tens of seconds, and
root sends a held PIN only if the worker has not ended first; a
directory that cannot be opened at the commit (its unavailable kind, no
write); and an addition's probe sending the new record's batches to a
key whose list holds two IDs. Operation tests cover the PIN frame's
bounds, its wait past a frame time, its operation deadline, and an open
session bounding every wait without narrowing the operation's. Clearing
a dropped PIN, or any frame, is by construction, which a source test
pins; safe code cannot observe freed memory.

### Login-key worker guests

`td-recipe-eval qemu-secret` runs twelve more fresh, diskless login
guests, with or without `--tpm`, since none needs a TPM: the eleven here
and `login-desktop` (below). Each boots the same kernel
and fixture init as the authority cases with `td.hid-fixture=1`, selects
one ignored test of `login_vm.rs`, a module of the worker's tests, and
holds the 180-second bound and the one-test passing summary. The test
calls the worker's own `operate` over `Physical`, as `run` does, through
a test-only wrapper that passes every call through unchanged and records
how many devices each discovery found:
`Device::discover`, `Session::open` and each session's production
`/bin/td-secret hid-worker` with its `/run/td-fido/operation.lock`,
kernel entropy and the protected-memory check, against virtual keys
`Plugged` presents through UHID. Root is the host tests' own over a
socketpair `Wire`, and every frame it sees is asserted as the host tests
assert it. The record is the production `/var/lib/td/login/1000`, the
directory made root's with mode 0700 as firstboot does. The departure
from `run` is the versions: both retained deployments read this
build's, standing for the tier markers `run` reads from the volume,
which these guests have none of, since production still refuses every
write that leaves a record; `login-changed` also runs empty read sets,
as a machine without marked deployments reads, and others that share no
version, and
departs once more, wrapping the store's write in a counter that its
refusals must leave at zero.
After each operation the lock is root's, mode 0600 and free.

A swap follows the worker, not a clock. Root's hook at the connect step
runs before its acknowledgement, so no discovery for the new key has
happened yet; the swap thread then removes the old key only after a
later discovery found it alone, and inserts the new one only after a
discovery after that found the port empty. The worker is thus seen to
wait through both, and the guest fails if either is not seen within 20
seconds, inserting the new key regardless so the worker is not left
waiting. A host test drives the same gate against a simulated worker
that starts late and requires it to see the old key, the empty port and
the new key in that order.

| Case | Proves over HID |
| --- | --- |
| `login-unlock` | each key of a two-key record unlocks over an identify and an assertion session, every CTAP request crossing the device; WRONG PIN at 8 then 7 and a later unlock at 6; a stranger NOT ENROLLED with no PIN step; no device and two devices ONE KEY with no report to either |
| `login-blocked` | three wrong PINs end PIN AUTH BLOCKED, and an unlock gets no PIN step until the device is destroyed, the key power-cycled and a new device created, which then unlocks at 5; one retry left blocks for good, across reinsertions |
| `login-enroll-one` | a one-key enrollment's three sessions publish the record, which a fresh worker then reads and the key unlocks; removing that last key unlinks it |
| `login-enroll-two` | a two-key enrollment across a gated swap at the second connect step: the worker polls with the first key alone, then with none, then takes the second; the second creation excludes the first credential; each key then unlocks |
| `login-add-remove` | an addition authorized by the enrolled key, then the same gated swap to the new key; the new key unlocks and removes the authorizing one, which then is NOT ENROLLED |
| `login-keepalive` | a three-second scripted touch on the unlock assertion: the Session waits through UPNEEDED keepalives and unlocks. Over the touch's measured length, at most one keepalive per period plus two, at least one per observed longest silence less two, and no request silent for a second |
| `login-probe` | a key whose default credProtect hides its credential from a silent assertion is KEY REFUSED `06` at the probe, and nothing is published |
| `login-refusals` | a key advertising `alwaysUv` is KEY REFUSED `02` at its one session, before any PIN step or makeCredential; presence denied at a creation, after its PIN, is DENIED with no credential made; a key advertising `alwaysUv` false enrolls and unlocks, is DENIED when it denies presence at the unlock assertion, unlocks again with its count reported 8, and once reconfigured to advertise `alwaysUv` true is KEY REFUSED `02` at the unlock's identify with no PIN step; a new key whose list cannot hold a two-key record's exclusions is KEY REFUSED `05` after the gated swap, before any PIN, the record unchanged |
| `login-verify` | the production record rewritten between operations with a slot verifier, then a public key, not the key's; a signature over other data, the identify's; and a replay, the current data signed over the client-data hash of an earlier unlock, which the worker's fresh challenge does not match: each FAILED after a right PIN, as a valid record before the signature cases unlocks |
| `login-changed` | the record replaced between the baseline frame and the description: RECORD CHANGED at the first step with no report to the key; replaced at an addition's commit round, removed at a removal's, and appearing at a first enrollment's: RECORD CHANGED after the whole ceremony, the store's write never called; VERSION before any token I/O for an addition, a removal that leaves a key and a first enrollment, under empty read sets and four pairs that share no version, the counted write never called; removing every key under empty read sets unlinks the record, the guest's one write |
| `login-eight` | seven additions over gated swaps, each authorized by the key added before it, to eight keys, each of which then unlocks; a ninth refused as INTERNAL from root's description, no report reaching the plugged key |

A record written in one guest's step is read back in its next; these
diskless guests keep nothing across boots, which the power-cut guest
below does. Every operation kind, unlock,
enrollment, addition and removal, crosses the kernel's HID path. The
acknowledgements are simulated root, the keys are software, and USB
metadata and presence are fixtures: this proves no USB controller,
YubiKey behaviour, physical presence or touch timing, and no td-authd
supervision. td-authd's host tests drive its supervision against
scripted workers, its `supervise-login` authority case runs this worker's
refusals through the real paired Session, and `login-desktop` runs its
unlock under the production td-authd.

### Login desktop guest

`login-desktop`, the twelfth login case, is TOKEN-LOGIN.md increment
4's C8 desktop guest: the paired production compositor and td-authd over
a record this worker enrolled, through a UHID keyboard and a UHID key,
with no TPM, no persistent disk and no portal.
`td-recipe-eval qemu-secret --case login-desktop` runs it alone, on KVM
as every boot oracle does; `--case NAME` selects any one guest, a TPM
guest only beside `--tpm`, but not one that opens the disk or TPM state
an earlier guest of the run leaves (`tpm-reopen`, `tpm-pcr`,
`tpm-other`, `fido-cold-reopen`, `fido-cold-recovery-reopen`), which it
refuses. Like the other secret guests it is run by
hand and is in neither `check` nor `check integration`. Its one ignored
test, `qemu_login_desktop_starts_every_generation_locked_and_unlocks_with_the_key`
in `login_vm.rs`, holds the one-test passing summary within a
300-second bound. The guest carries the production `td-authd`,
`td-firstboot`, `td-login` and `td-compositor`, which the plain command
now stages for every guest; the TPM desktop guests add the broker,
portal and jail.

The guest sets its hostname, releases the framebuffer from the kernel's
console (its vtconsole unbound, so no cursor draws), and checks the
fixed 1280x800, 32-bit output. It makes the login directory as the
worker guests do, the session's accounts and enrolled principal ledger
as firstboot leaves them, the image's bus application policy naming no
application, the compositor's runtime directories, and
gives the compositor's account the input and framebuffer nodes, as
seat setup does; the FIDO node stays root's. The record is seeded by
TOKEN-LOGIN.md's fixture rule: a one-key enrollment through this
worker's own `operate`, root simulated and both retained deployments
reading this build's version, as `login-enroll-one` does. The pair is
the fido-desktop guest's (`Pair`, `Keyboard` and `Process` from
`fido_device`'s desktop module), without its TPM measurement, store or
portal.

The host checks the display. The guest writes `TD-LOGIN-SCREEN NAME
[ARGUMENT...]` to `/dev/console` and waits on ttyS0, read without
becoming its controlling terminal, for `TD-LOGIN-SHOWN NAME`. The host
(`guest_screens.rs`, beside the boot oracles) takes the request from a
finished console line, requires the names in `LOGIN_DESKTOP_SCREENS`'
order, captures the display through QMP `screendump` until that name's
check accepts a whole 1280x800 capture, within 60 seconds, and only then
types the answer into the serial socket, so the guest acts on nothing
the host has not seen. A screen given arguments it does not take is
refused at once. A screen may hold from its answer until the next is
accepted: the host keeps capturing back to back whether or not the
guest has asked for the next screen, and every capture meanwhile, the
accepting one included, must satisfy the hold. Every screen of a
locked generation holds only the guest's magenta, black, and the
attention ground and white, which the lock surface, the attention
screens and the PIN step paint, so no client or desktop pixel shows
while locked, between steps included; `touch` holds nothing, since its
successor is the unlock, `unlocked` and `blank-unlocked` hold no pixel
of the attention ground, and `desktop`, the last, holds nothing. After
the boot, a request left in the console's last lines fails. Host tests
pin the guest's requests, in its source order, to that list, every
check to an independently drawn frame, each screen's hold, and the
judgement of holds across requests.

| Screen | The host accepts |
| --- | --- |
| `blank` | every pixel the guest's magenta, which no compositor paints; holds the lock palette (magenta, black, the attention ground and white) until the next screen |
| `blank-unlocked` | the same magenta |
| `locked` | exactly the lock surface: `TD-LOGIN-DESKTOP`, `TESTER`, `LOCKED` and `PRESS CTRL+ALT+ESC TO UNLOCK` in the chrome rows from 276, every other pixel the ground |
| `locked-damaged` | the same with `LOGIN KEY STATE UNAVAILABLE:` and `DIRECTORY DAMAGED` |
| `pin FINGERPRINT RETRIES MASKS` | exactly the unlock's PIN step: consent's eight rows for that key and count and a time line, centred in doubled Unifont, `ENTER THE PIN FOR THIS KEY` a row gap below, and that many masks |
| `touch FINGERPRINT RETRIES` | the same prompt with `TOUCH YOUR KEY` and no mask |
| `wrong-pin`, `not-enrolled`, `damaged` | exactly the attention screen's title, `WRONG PIN`, `THIS KEY IS NOT ENROLLED HERE` or the cause's two rows, and `ESC TO RETURN` |
| `unlocked` | no attention-ground pixel, the status bar's band mostly its ground, and a window's title band, focused or not, on glass |
| `desktop` | the same without the window |

The guest blanks the framebuffer before each generation, since the last
one's frame stays there, and asks in this order:

1. Enrolled: `blank`, then the pair's `locked`; the demo client
   (`/bin/td-ui-demo`, the compositor's own, as the account) maps a
   window, and `locked` again shows no pixel of it.
2. The chord's unlock with the wrong PIN: `pin` with no mask and with
   four, Enter, `wrong-pin`, the key's retries now 7, Escape, `locked`.
3. A key that holds no credential of the record in the enrolled key's
   place: the chord's `not-enrolled`, with no PIN step, Escape, `locked`.
4. The enrolled key again, its touch scripted to take five seconds: the
   chord, `pin` for 7 retries with no mask and with four, Enter,
   `touch`, and within the touch td-authd's only live child is the
   `login-operation` worker it started for the `1b`; then `unlocked`,
   the retries 8 again. td-authd's reaped children's fault count
   (`cminflt`) grew: reaping the worker, among others such as the state
   helper the compositor's `1a` runs afterwards, shows there.
5. The pair restarted: `blank`, `locked`.
6. The login directory made mode 0755: `blank`, `locked-damaged`, the
   chord's `damaged`, and a second later td-authd's reaped children's
   fault count unchanged, read before and after its children are
   counted, and no child of it alive, so no worker started and no `1b`
   came; Escape, `locked-damaged`, and a second later the same again,
   so none came on Escape or back on the lock surface. The mode is
   restored.
7. The record removed through the worker: `blank-unlocked`, then the
   pair's `desktop`.

Each key served exactly the sessions its operations need. This proves
the paired desktop's locked start, unlock and refusals against the
production compositor, authority and worker in a guest; it does not
prove a frame the captures did not sample, which the compositor's host
tests hold for every frame handed to the output, nor Escape after an
unlock's commit, which the guest cannot time since the commit round
follows the touch and success follows the commit at once. The keys and
keyboard are UHID fixtures, presence is scripted, and enrollment's root
is simulated; there is no physical-presence, USB or YubiKey claim.

### Login power-cut guests

The same command then boots one more TPM-free guest, `login-powercut`,
twelve times in order on one fresh 256 MiB raw disk, each boot naming
its phase as `td.login-cut=PHASE`, which the fixture init requires to be
exactly one listed phase. Its one ignored test,
`qemu_login_record_survives_power_cuts_inside_its_writes`, runs the
worker as the guests above do, with the record at the production
`/var/lib/td/login/1000` on a Btrfs `@var` subvolume of that disk,
mounted with production's `nosuid,nodev` and `commit=300`. Two
persistent virtual keys ("Virtual authenticator") live beside it in a
root-only fixture directory, outside the login directory. Each cut boot
replaces the worker's store write (`Context::write`) with one that
first saves a ledger, the record's old and new digests and the
publication's temporary name, durably in that fixture directory, then
calls the store's `publish_at` or `remove_at`; at the boot's stage the
hook writes `TD-LOGIN-CUT PHASE` to `/dev/console` and parks without
returning, and the host, seeing the line, SIGKILLs QEMU, as
`qemu-secret-system --powercuts` does. A boot cut after its write
commits names its phase once the worker has sent its success frame and
exited `operate`.

| Phase | The boot's write, cut at |
| --- | --- |
| `setup` | none: makes the volume, the login directory as firstboot does, the blank keys and the ledger, unmounts and powers off |
| `enroll-created`, `enroll-synced`, `enroll-renamed`, `enroll-committed` | a one-key enrollment of the first key, over no record: `Created`, `FileSynced`, `Renamed`, after the success frame |
| `add-written`, `add-attempted`, `add-synced` | the second key added by the first, across the gated swap: `Written`, `RenameAttempted`, `DirectorySynced` |
| `remove-attempted`, `remove-unlinked`, `remove-synced` | both keys removed, authorized by the first, which unlinks the record: `UnlinkAttempted`, `Unlinked`, `DirectorySynced` |
| `final` | none: checks the last cut and both keys, unmounts and powers off |

So every publication stage is cut once, over no record or over one, and
every removal stage. Each boot after setup restores both keys from the
disk, a damaged state file failing the guest by name, and requires the
ledger to name the phase before its own. Then what that cut left must
be what its stage leaves:

- before the temporary's sync (`Created`, `Written`) the old record or
  none, and at most that write's temporary;
- from the temporary's sync to the directory's (`FileSynced`,
  `RenameAttempted`, `Renamed`) the old record or none, and exactly that
  temporary, whose bytes are the whole new record;
- before the removal's directory sync (`UnlinkAttempted`, `Unlinked`)
  the old record, and no temporary;
- after the directory sync, or after the commit, the new record or none,
  and no temporary.

The record must read as enrolled with exactly the expected digest, or
as unenrolled, never unavailable, and no other name may exist. Every
key holding a credential the surviving record lists then unlocks it,
and a key holding only others is NOT ENROLLED; then the boot makes its
own write, whose start removes any leftover temporary with a directory
sync, which the next boot's exact names confirm. The final boot requires
no record and no name, and the first key holding four credentials and
the second three: each made in a boot later cut, saved before its
reply. A host test requires that each listed stage is one its write
reaches, that the publication and removal stages are each cut exactly
once, and that the ledger round-trips and refuses malformed text; the
recipe's tests require its phase list to be the guest's boots, in order.

An addition's write first requires that its swap was gated, as above,
and that the authorizing key served its two sessions. Once the ledger
is saved, every key is frozen ("Virtual authenticator"): a key save
from then until the kill is refused before it writes anything and puts
a failure line on the console, which fails the cut. No key request is
expected then, since every token session ends before the commit round.

The host kill ends QEMU, not the host: every write the guest's block
layer submitted and QEMU completed is in the host's page cache, since
the disk uses QEMU's default writeback cache, and survives. What the
guest had not yet submitted is lost; I/O submitted but not completed at
the kill may land or not. With `commit=300` an unsynced change stays in
guest memory: Btrfs commits its running transaction every 300 seconds,
longer than any boot's 180-second bound, and otherwise when something
syncs, or a fsync falls back to a full commit, or memory or space runs
short. That is what makes a missing sync visible.

The `Renamed` and `Unlinked` cuts are the controls, each paired with the
cut of the same write after its directory sync: `enroll-renamed` with
`enroll-committed`, and `remove-unlinked` with `remove-synced`. In the
first of each pair the rename or unlink has been made but the directory
not synced, and the guest requires the old state back; in the second it
requires the new one. So the store's directory sync is what made the
change durable, and a store without it would lose a committed write in
a crash. The controls rest on two premises:

- nothing syncs a file on `@var` between the change and the kill. Only
  `cut`'s console write and its park run in that window; the fixture's
  ledger and key syncs all come before the change, the store's own syncs
  before it too, and the freeze above refuses any key save after. Their
  order is what makes them harmless: a sync before the change can only
  make earlier state durable;
- the kernel's rename and unlink do not sync the Btrfs log tree. On the
  pinned 7.1 kernel a rename of an inode already logged in the running
  transaction, as the synced temporary is, only updates the log tree in
  memory (`btrfs_log_new_name`), and an unlink of an inode not logged is
  memory-only. Some kernels from about 4.20 to 5.11 synced the log on
  such a rename.

Btrfs may commit early in principle, but within this window nothing
asks it to: no sync runs, the transaction is seconds old, and the guest
holds a few kilobytes of dirty data. A red at `enroll-renamed` or
`remove-unlinked` after a kernel bump is therefore a changed premise to
investigate, not by itself proof of a store regression; it is loud,
never hidden.

The temporary's own sync is tested as well: the `FileSynced`,
`RenameAttempted` and `Renamed` cuts require the temporary to survive
whole, which it would not without that sync. What the guests cannot show
is that a published record would be torn without it, since a Btrfs
directory sync may log the renamed file's contents with its name. Nor
can a kill show that a sync reached stable media: the host keeps its
page cache and QEMU's flushes to the host file are never put to the
test, so a missing cache flush, reordered or torn sectors and host power
loss are outside it. Production mounts `@var` with the default 30-second
commit, which only narrows a window the store never relies on.

The disk and its scratch directory are removed when the run ends. Each
boot holds the 180-second bound. Setup and the final check require the
one-test passing summary and a clean exit; each cut requires exactly
one cut line, its own, a SIGKILL the host sent and saw reaped, and no
passing summary, fixture success line, failure line or panic. The keys,
root and presence are the fixtures above; this proves crash consistency
of the store's own writes on Btrfs in a guest, not firstboot's
temporary cleanup, which td-firstboot's host tests cover.

### Login system guest

`qemu-login-system` is TOKEN-LOGIN.md increment 4's C11: the login tier
on the full system, with no TPM. `td-recipe-eval qemu-login-system`
builds `system-secret-vm-test`, the production system image with the
secret fixture's unit and test binaries, and boots it sixteen times in
order on one disposable volume, each boot naming its phase as
`td.login-system=PHASE` beside `td.hid-fixture=1`; the fixture init
requires exactly one listed phase and runs the one ignored test,
`qemu_login_system_locks_unlocks_and_refuses_on_a_full_system` in
`login_system_vm.rs`, beneath the stock supervisor, then asks td-svc to
reboot through the unchanged persistent shutdown, which ends QEMU under
`-no-reboot`: QEMU's S3 wake resets the q35 chipset, after which the
guest's ACPI soft-off no longer powers the machine down. It runs by hand,
on KVM only, and is in neither `check` nor `check integration`. Every
boot is the stock firstboot, serial greeter, sshd, boot health, seat,
compositor, td-authd, terminal and the rest of the unit table; the test
adds a UHID keyboard before the seat starts, which the fixture unit's
readiness gates, and UHID keys.

The volume is made as the boot oracles make theirs, with a throwaway
signing identity, but mkfs runs beneath `td-builder userns-private`, as
an installation's does, so its files are guest root's. That is what the
worker's production read of the retained deployments needs: `Volume`
with owner 0 over `/run/td-volume/td` gives this build's version only
because the current deployment's files are root's, and the guest shows
that the same read with another owner gives none. The machine is a
direct-boot q35 with `ICH9-LPC.disable_s3=0`, so its ACPI offers S3.

The host checks the display as for login-desktop (`guest_screens.rs`),
with its `blank`, `blank-unlocked`, `pin`, `touch`, `unlocked` and
`desktop` and these: the lock surfaces carry the image's
hostname, `TD`, and each unavailable cause; the attention menu's rows;
`UPDATE CANNOT READ LOGIN KEYS`; the trusted installation prompt for a
named deployment, whose ID must be the booted manifest's as the host
computes it; `release`, the touch request the guest names last before
it completes the touch; `killed`, the lock palette or QEMU's inactive
output once a compositor is killed; and `asleep`, the desktop the guest
suspends from. Every hold of a lock screen admits the lock palette and
QEMU's own frame for no scanout, black with its grey "Display output is
not active." in the 16-pixel row from 384 within 128 pixels of the
centre, which shows between a compositor's exit and its successor's
first frame. Both touch requests hold that, and so does `killed`, until
its successor's lock surface. After `asleep` is answered the host waits
for QEMU to report the guest suspended, keeps it so for 10 seconds and
sends `system_wakeup`. From the suspension to the end of the boot every
capture must be what a lock screen's hold admits, but for the capture
taken while it slept, which is admitted only until the first capture
after the wake that differs from it, so the desktop cannot return; and
QEMU's inactive output must show at least once after the wake, so the
dead card is observed, not assumed. Every enrolled or unavailable
boot waits for the serial greeter's td-login to hold ttyS0 before it
asks for a screen, since its getty flushes the line's input first.

| Phase | The boot |
| --- | --- |
| `seed` | unenrolled, with `inspect-login` failing and the boot's cutover record naming the unenrolled state: once the serial greeter has logged in it is stopped, so no shell reads ttyS0, then `blank-unlocked`, holding no lock pixel, and `desktop`; the ordinary SSH form, root's loopback key refused; then the greeter started and logged in again and an SSH session opened as the primary account; two persistent keys enrolled through the worker with the production retained read, the first alone and the second added across a gated swap, and the record copied aside; last the cutover (TOKEN-LOGIN.md increment 5's A3): the pair restarted finds the enrolled state and the unenrolled record, and both sessions end, the record names the enforced form for this boot, no reboot guard exists, ttyS0 is root's, the greeter parks refusing, `locked`, and the enforced SSH form |
| `locked` | locked from the first frame and still locked once the terminal is ready; boot health completes and the enforced SSH form refuses root and admits the primary; unlocks with each key, relocks by `Super+l` and by the menu's `L`; three queued updates; a killed compositor; the pair restarted with the helper failing; an unlock; last, suspend to RAM |
| `damaged-CAUSE` | locked with the cause's rows; the chord shows them and no login worker lives for a second after it or after Escape, nor is one started at all; enforced SSH; then the documented recovery |
| `repaired-CAUSE` | locked as enrolled, the record the copy; enforced SSH; then the next cause's damage, if any |

The causes, each made at the end of the boot before: the login
directory mode 0755, owned by the account, or replaced by a file; the
record mode 0644, given a second link, truncated by a byte, or of an
unknown version. Each is repaired as TOKEN-LOGIN.md's "Recovery" says:
the directory restored as root:root mode 0700 with the record copy, the
mode restored, the second link removed, or the record replaced by its
copy.

Each unlock is the chord, the PIN step empty and with four masks,
Enter, the touch request with exactly one `login-operation` worker
alive under td-authd and the key waiting for its touch, the touch
request again with that worker still alive and the key still waiting,
and `release`; only then does the guest complete the held touch, so the
unlock cannot commit while the host holds lock pixels; then the
client's window and the desktop, the key's retries back at eight. The
killed compositor's step asks for `killed` as soon as the kill is sent.
The updates are
queued as the primary account with `td-authd request-update` over a
source directory it owns: a deployment whose initramfs carries no
marker and one whose marker lists another version are refused at the
menu's `I` with `UPDATE CANNOT READ LOGIN KEYS`, and a copy of the
booted deployment, whose C10b marker the guest reads first, reaches
its prompt, which Escape cancels. Each request counts in td-authd's
update backoff: after it ends, the backoff file holds the `update` row
and the account's next request is refused as backing off, and root's
fixture then removes the file rather than wait out the window
(td-authd/DESIGN.md, "Elevation operations"). `Super+l` follows a Caps
Lock, since the unlock's close discards the keyboard's first fresh
report.

A recorder, a shell wrapper bind-mounted over the `td-secret` binary,
appends each run's verb to a log and runs a copy of the binary; td-authd
starts its login worker and its state helper through that path, so a
worker however short-lived is in the log. A failing recorder also fails
`inspect-login`. Each damaged boot's chord and Escape run under a
recorder, which logs no worker. With a failing one the restarted pair
locks with `STATE COULD NOT BE READ`, whose chord starts no worker,
live or logged, while the log shows the polled helper; once it is
unmounted the compositor's polling shows the enrolled lock surface. On
`seed` a failing recorder logs no helper run but its own check: an
unenrolled machine never runs the helper.

The suspend spends the unlock's discarded report with a Caps Lock,
mounts a recorder, selects `deep` sleep and writes `mem` to
`/sys/power/state` from the unlocked desktop with the second key
plugged, no worker live or logged; the kernel's count of successful
suspensions and an uptime gap of at least 8 seconds confirm the sleep.
The chord is sent as soon as the write returns, whole in one report
(Ctrl, Alt and Escape, so one evdev frame, its modifiers before the
key), and that report is the keyboard's first input after the wake. It
starts exactly one
logged and live `login-operation` worker under td-authd, an unlock's
`1b`, which the chord sends only on a locked session (on the unlocked
desktop it opens the menu), and the compositor and authority are the
ones from before the suspend: the session was locked when the first
post-wake input was routed, in the same generation. Escape then ends
the worker. The lock surface itself cannot be seen: the wake resets the
virtio-gpu card and Linux 7.1.4's driver has no freeze or restore, so
the card stays dead and QEMU shows its inactive output, which the host
requires. The suspend is the boot's last step, since the display does
not return.

The host requires, for each boot, the phase's pass line, QEMU's exit
after the persistent shutdown marker (a capture lost after the last
screen is forgiven only when QEMU has already exited, or exits within
5 seconds once that marker is on the console), the selected current
deployment, boot health's success marker, rootcheck's login directory
marker except on a damaged directory's boots, and the serial greeter's
exact refusal line once with no greeting on every boot but `seed`,
whose greeter logs in and, once the cutover restarts it on the
enrolled record, prints the refusal line once. This proves the tier's locked boot, unlocks,
relocks and refusals on the full system in QEMU. It does not show
`Super+l` or `L` on an unavailable session, or request 19's admission
while the state could not be read. The keys and keyboard are UHID
fixtures, presence is scripted and enrollment's root is simulated; QEMU
has no lid, its S3 is not a laptop's and leaves no display to see the
resume's lock on, and there is no physical-presence, USB, firmware or
YubiKey claim.
