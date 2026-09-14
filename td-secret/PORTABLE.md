# Portable personal vault

This is the production target for `portable-vault-rolling`, owned by
td-secret. It complements the existing application credential service in
[DESIGN.md](DESIGN.md). No completed portable enrollment, hardware recovery,
host authentication, or td-pass deployment is claimed until the acceptance
evidence below exists. The existing application store and its authorization
rules remain authoritative for its consumers.

## Ownership and scope

td-secret owns the encrypted format, entry transactions, token enrollment,
unlock, protector changes, export/import, recovery and plaintext lifetimes.
td-pass owns a notebook window using td-ui and the shared editor component;
it receives entries and operation status, never an encryption key. The
notebook API expresses unlock, list, read, revision-checked save, create,
rename, delete, lock, export, add-key and replace-key operations. Device and
cryptography APIs are private to the backend, not part of that interface.

One x86-64 Linux executable must work on td and a supported foreign Wayland
desktop. This is supported use, not the application layer's development-only
host fixture. td mode connects to the admitted td authority; standalone mode
owns a private backend using the same vault implementation. No permanent
host daemon, host keyring, network service, GPG installation or TPM is a
prerequisite for the first portable profile. Service discovery may suggest a
configuration, but an existing vault never changes backend or protector
because authentication failed or a service disappeared. td mode cannot
reach the standalone device/backend path as an authorization fallback.

The portable vault has its own random identity. Unix UIDs, hostnames, store
paths, TPM state and PCRs are not cryptographic identity. Local filesystem
ownership and service authorization are separate, enforced by each adapter.
Moving a vault to a different machine changes neither its identity nor the
credential IDs and salts needed to unlock it.

## Trust and authentication

The first protector is a removable FIDO2 authenticator supporting
`hmac-secret` and PIN-based user verification, tested first on YubiKeys.
Capability negotiation precedes enrollment; neither the brand name nor an
unsigned AAGUID proves capability, physical device identity or attestation.
USB HID is the first transport; NFC and non-Linux platforms are separate
work. No token reset, PIN change or weaker-protocol retry is implicit.

The FIDO2 credential's private material remains on the authenticator. The
authenticated `hmac-secret` result reaches only the td-secret backend and
feeds domain-separated HKDF-SHA256 to produce a wrapping key. Require the
UV-protected result consistently at enrollment, proof and unlock; touch-only
and UV results must never be interchanged. A verified signature alone cannot
provide a reproducible encryption secret. PIN retries and user verification
belong to the authenticator; a password-derived vault fallback is absent.

CTAP clientPIN and hmac-secret require key agreement and encrypted protocol
messages beyond the current presence-only CTAP implementation. The current
TPM signature-verification dependency cannot enter this portable profile.
Existing CTAP/HID framing can be reused after review, but is not evidence of
PIN, extension, or physical-device interoperability. Every token operation
has a fresh operation-bound challenge, a fixed deadline, bounded messages,
explicit cancellation and no automatic retry after an uncertain result.

On td, the dedicated backend identity, exclusive token mediation and trusted
compositor/td-authd presentation enforce authorization. Authentication input
does not go through the notebook text editor. An explicitly authorized
notebook browsing session permits list/read to its pinned application
instance; it grants no other application personal-vault access. Writes and
protector changes require fresh authentication bound to the exact immutable
operation, independently of an existing browsing session.

Standalone mode trusts the host kernel, compositor and invoking account. Its
authentication adapter must identify its own prompt as host authentication;
it cannot claim td secure attention or protection against a compromised host
account. It keeps keys inside the backend, but same-UID process separation
alone is not an adversarial boundary. USB permissions, process dumps,
swap/hibernation, screen lock and suspend integration require an explicit
supported-host contract and tests before real-secret support. No automatic
sudo path or new general elevation service is added. PIN and token outputs
never enter argv, environment, logs, notebook buffers or persistent files.

## Keys and recovery

A fresh kernel-random 256-bit vault key protects the notebook. Each enrolled
token gets an independent encrypted copy of that key, protected by its own
credential-specific wrapping key. The versioned wrapping context binds the
vault ID, credential ID, role and derivation salt. The encrypted notebook
authenticates the complete protector table, format version and revision as
well as its contents. Unauthenticated metadata is only bounded input to an
unlock attempt, never authority to add or replace a protector.

Initial enrollment proves a primary and a separately presented backup before
publishing anything. Both must independently unwrap the identical vault key
and authenticate the notebook. Exclude existing credential IDs during new
credential creation and require the operator to use a separate physical key;
this cannot prove distinct malicious or cloned hardware. An explicit
unrecoverability profile is not part of this first production target.

Adding a key requires fresh authorization by an existing enrolled key, then
creation and proof on the new key. The complete new protector table and
authenticated notebook publish atomically only after proof. Cancellation,
failure or power loss before publication preserves the old working vault.
An orphan credential on a token is possible after interrupted enrollment;
it grants no access without a committed wrapped vault key. A lost success
reply does not authorize replay of the operation.

The backup is independently usable with the complete vault file on another
machine, with the original primary and TPM absent. It may authorize a new
primary. Do not remove the last working recovery path. Replacing a lost or
revoked key rotates the vault key, re-encrypts the current notebook and
rebuilds wrappers for every retained key, requiring their participation;
merely deleting one wrapper while keeping the vault key is not revocation.
Historical exports remain readable with protectors that could read those
exports. Neither rotation nor filesystem deletion retracts old plaintext.

Export copies the complete encrypted vault and necessary public metadata,
not a token's private credential or plaintext key. Import parses and
authenticates before adopting an existing vault, preserves its identity,
and refuses silent replacement of different or newer state. The portable
format alone does not supply synchronization, merge, or rollback protection.
A token is not a backup of the notebook bytes. Losing all enrolled keys or
all complete vault copies loses access.

## Storage and resource limits

The initial bound is 1024 entries, 512 UTF-8 bytes per nonempty title, 64 KiB
per body and 4 MiB of serialized plaintext for the whole notebook. Empty
bodies are valid. Titles reject controls and are labels, never paths. Entry
IDs are random, stable and independent of titles. Titles, contents and entry
revisions are all encrypted. Exact text bytes, including line endings, are
preserved; visual wrapping performs no data transformation.

The envelope admits two through eight key slots with bounded credential IDs
and fixed salt, nonce and wrapped-key lengths. There is exactly one primary
and at least one backup. Slots have a canonical order and duplicate
credentials are refused. All lengths, counts, versions and trailing bytes
are checked before expensive operations or allocation proportional to input.
Authentication precedes plaintext decoding. Every successful write uses a
fresh random nonce and a checked increasing revision.

Filesystem publication must pin directories and files, reject symlinks,
wrong owners, excessive permissions, nonregular files and unexpected links,
serialize cooperating operations, compare the retained baseline, and publish
through an exclusive temporary, file fsync, rename and directory fsync.
The file and lock policies must survive copying a vault to another account
without deriving cryptographic identity from ownership. No persistent
plaintext scratch, autosave, swap or editor recovery file is permitted.
Publication failure after rename can have an uncertain outcome; reread and
authenticate state before the user decides how to proceed.

Lock ends the browsing session and retires plaintext, undo, search, transfer
and render-buffer owners. System lock or authority loss cannot wait for a
dirty-document dialog. Best-effort clearing does not guarantee erasure of
compiler copies, prior clipboard recipients, screenshots or historical disk
copies. The UI design specifies the unsaved-edit behavior.

## Implementation and dependency decision

### Implemented envelope prerequisite

`src/portable.rs` supplies the private, safe envelope primitive. It is
compiled and tested by the td-secret recipe but has no public command,
device consumer, filesystem writer or authorization API. Its synthetic
protector inputs do not establish enrollment, UV or a presented operation.
The future trusted backend must provide proved token output and kernel
entropy; no application receives access to the raw-key primitive. An open
snapshot pins the complete envelope digest, so revision refuses another
vault or a different retained revision before producing output. Revision
returns proposed encrypted bytes; it neither publishes nor consumes write
authorization and cannot replace the future persistent-baseline check.
The primitive permits competing proposals from the same snapshot; the
backend must authorize and publish at most one against that baseline.
A saved envelope needs a new open snapshot before a subsequent revision;
this primitive does not advance the browsing session automatically.

All wire integers are unsigned big-endian. Version 1 is:

| Field | Encoding |
| --- | --- |
| Magic | Eight bytes `TDVAULT1` |
| Vault ID | 32 random bytes |
| Vault revision | Nonzero u64 |
| Slot count | u8, two through eight |
| Slots | Repeated structure below, strictly sorted by credential bytes |
| Notebook nonce | 12 fresh random bytes |
| Sealed notebook length | u32, 20 through 4 MiB + 16 |
| Sealed notebook | Ciphertext followed by the 16-byte AEAD tag |

Each slot contains role u8 (1 primary, 2 backup), credential length u16
(1 through 1024), credential bytes, 32-byte hmac-secret salt, 12-byte random wrapping
nonce and 48 bytes of encrypted vault key plus tag. Exactly one slot is
primary; all other slots are backups. The maximum envelope is 4 MiB + 16 +
65 + 8 * 1119 bytes. Duplicate or unordered credentials, unknown roles,
truncation and trailing bytes are refused before cryptographic work.

The wrapping key is HKDF-SHA256 with the UV hmac-secret result as input,
vault ID as salt and ASCII `td-secret/portable/wrap/v1` as info. Its AEAD
associated data is ASCII `td-secret/portable/slot/v1`, a zero byte, vault ID,
and the slot's encoded role, credential length/bytes and hmac-secret salt.
The body key is HKDF-SHA256 with the vault key as input, vault ID as salt
and ASCII `td-secret/portable/body/v1` followed by a zero byte as info. Body
associated data is that same info followed by every envelope byte before the
sealed body, including the full key table, nonce and body length. Both use
the existing RFC 8439 ChaCha20-Poly1305 implementation. Independent random
96-bit nonces are appropriate for this bounded local notebook; no
high-volume encryption service is claimed.

Decrypted notebook bytes contain entry count u32 followed by each entry's
16-byte stable ID, nonzero u64 revision, u32 title length and UTF-8 title,
then u32 body length and UTF-8 body. Duplicate IDs or exact titles are
refused. Decode is bounded before allocation and authenticates before
plaintext parsing. Empty notebooks and bodies are valid. Secret owners
implement neither Clone nor Debug and clear owned byte buffers on drop,
including decode/error paths, with the existing best-effort limitation.
Entry text fields expose read-only views and whole-buffer replacement;
replacement clears the outgoing buffers before releasing them. Protector
ordering sorts references, leaving secret owners in their original vector.
These measures do not prevent compiler-elided clears or temporary copies.
Unicode label presentation, including bidi spoofing, needs separate UI
policy and tests; the wire format rejects control characters only.

`tests/portable_vector.py` independently constructs the literal Rust fixture
using host OpenSSL 3.5.7 EVP and Python hashlib/hmac. These are optional
fixture-generation tools, never target inputs or test dependencies. The
ordinary suite checks the exact bytes, both keys, a single-bit flip at every byte offset, every truncation, invalid tables/entries/lengths, failed entropy,
stale snapshots and the maximum plaintext envelope. These are cryptographic
and structural tests, not physical enrollment or recovery evidence.

### Dependency-free cryptography boundary

Reuse the existing ChaCha20-Poly1305 and HKDF implementation for the envelope;
new format helpers remain safe, dependency-free Rust inside td-secret.
Extract reusable code into a td-secret library with an explicit notebook
surface when its backend exists; do not expose raw key-taking methods to
td-pass to stand in for authentication. Current application credentials do
not acquire portable-vault access by sharing code or an unlocked session.

The production credential layer remains dependency-free. Neither LibreSSL,
libfido2 nor a host crypto library may supply the missing P-256 key agreement,
PIN protocol AES or TPM-independent signature verification. Native linking,
dynamic loading, subprocess crypto and vendoring external implementations
do not evade this requirement. Host reference implementations may generate
public test vectors, but do not enter the shipped closure.

The missing primitives are being implemented in separately reviewed safe-Rust
increments. Before device integration they must have independent positive
and negative vectors, canonical field/point
validation, fixed operation schedules for secret scalars and AES, no
secret-indexed tables, and an explicit analysis of compiler/timing and
memory-erasure limits. It must not introduce a proprietary token protocol
or replace the required PIN/UV policy with touch-only authentication.
The AES prerequisite below does not supply P-256 or a working token protocol.
Any proposal to change this boundary requires a new explicit user decision.

### Implemented CTAP AES prerequisite

`src/fido_aes.rs` supplies private AES-256-CBC encryption and decryption for
one through eight 16-byte blocks. It accepts an explicit 32-byte key and
16-byte IV and transforms the caller's buffer in place. Length admission
precedes key expansion and mutation; empty, partial-block and oversized
inputs return an error without changing any input byte. The upper bound is
a local resource policy, not a universal CTAP message-size claim.

This is the block-cipher prerequisite for
[CTAP PIN/UV protocols](https://fidoalliance.org/specs/fido-v2.2-ps-20250714/fido-client-to-authenticator-protocol-v2.2-ps-20250714.html#pinProto1).
It adds no padding, IV generation, integrity check, protocol negotiation,
device I/O, command or application API. The future protocol adapter must
enforce the selected protocol's message lengths, IV rules, authentication,
signature checks and plaintext lifetime. CBC is not authenticated encryption
and must not replace the portable envelope's ChaCha20-Poly1305.

AES follows [FIPS 197](https://doi.org/10.6028/NIST.FIPS.197-upd1).
The S-box and inverse use a fixed exponentiation chain in GF(2^8), with
eight fixed mask-and-shift steps per multiplication. No secret byte indexes
a table or selects a branch. The key schedule's branches depend only on the
public round index; rounds, row permutations and column transforms use
fixed layouts. CBC iteration depends on the admitted public message length.
The implementation deliberately favors a small arithmetic surface over
bulk-encryption speed. No AES-NI, foreign crypto library or raw surface is
introduced.

The 240-byte expanded schedule has a private heap owner without Clone or
Debug. Its destructor clears the schedule; expansion clears its rolling
word buffer. Each clear is followed by `std::hint::black_box` as a
best-effort optimizer barrier. This does not guarantee erasure: temporary
register/stack copies, allocator behavior, aborts and compiler transformations
remain outside that claim. The caller retains and must retire the input
key and any decrypted buffer. Fixed source operation schedules are not a
Rust language guarantee of constant time, a complete CPU side-channel
analysis, or cryptographic certification. PIN/hmac-secret integration must
review generated code for the actual shipped compiler/options as an
acceptance gate before admitting a hardware consumer.

Tests compare both directions against the NIST AES-256 block and
[CBC example](https://csrc.nist.gov/CSRC/media/Projects/Cryptographic-Standards-and-Guidelines/documents/examples/AES_ModesA_All.pdf),
check all 256 S-box values, and refuse every invalid length through the
first two blocks beyond the bound. Ten independent OpenSSL 3.5.7 fixtures
cover every admitted block count, zero and nonzero IVs, and all-zero/all-one
inputs. `tests/aes_vectors.py` regenerates the public literals in
`tests/aes_vectors.txt`; it is an optional host fixture tool and never a
build or test dependency. Host and td-built tests consume only those
committed literals. These are primitive tests, not PIN or YubiKey evidence.

## Independently landable increments

1. This contract, td-pass notebook/host scope, bounded authenticated portable
   envelope and primary/backup oracles using synthetic key material; no
   device, public unlock or UI.
2. Reviewed cryptographic backend and PIN/hmac-secret protocol, with
   independent vectors and failure tests, then real primary/backup token
   enrollment, unlock and protector replacement without TPM access.
3. Checked persistent store and reusable td-secret notebook API, host
   authentication adapter and td-authority integration. Prove identical
   operations across backends and explicit mode admission.
4. Shared td-ui/editor components and the td-pass notebook, including
   confidential buffer lifecycle and clipboard tests.
5. Source-built artifact and image integration, same-artifact foreign-host
   acceptance, independent recovery, migration and hardware evidence.

## Acceptance evidence

Require literal independently generated envelope vectors, both keys opening
the same entries, wrong-key and every authenticated-field tamper refusal,
truncation/length/count/duplicate/version refusals, and bounded maximum-size
roundtrips. Test no partial output or state mutation after any refusal.

Exercise the complete primary/backup lifecycle across process restart,
different UIDs and machines, with no TPM. Add-key and replace-key tests cut
power before and after publication; new-key proof failure preserves the old
vault. Old keys cannot open the rotated current vault, while historical-copy
limits are stated exactly. A missing token, wrong PIN, exhausted retries,
cancel, unplug, stalled transport and uncertain response never downgrade or
silently retry. Fixtures must not reset or enroll an operator's devices.

Run the identical production td-pass ELF on td and a named foreign Wayland
distro, recording its digest, architecture, kernel baseline and static or
otherwise portable runtime closure. Require source-built provenance, frame
pointers and the deterministic debug companion on td. Observe actual UI
selection/edit/save/copy/paste/lock, not just headless model state. Record
hardware model/firmware, both independently usable tokens and fresh-machine
restore. Mock or software-token results alone cannot label the target ready
for the user's only copy of real credentials.
