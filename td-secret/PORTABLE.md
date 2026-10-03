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
notebook session permits list, read and entry writes to its pinned
application instance until it locks; it grants no other application
personal-vault access. That one unlock is the consent for the session's
saves: a save presents no token, so anything able to drive the unlocked
notebook can change or delete its entries, which earlier revisions and
exports still hold. Protector changes and import require fresh
authentication bound to the exact immutable operation, independently of
the session, because a key enrolled through a reachable session would
keep reading every later revision after the lock.

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
vault ID, credential ID, canonical public verification key, role and
derivation salt. The encrypted notebook authenticates the complete protector
table, format version and revision as well as its contents. Unauthenticated metadata is only bounded input to an
unlock attempt, never authority to add or replace a protector.

Initial enrollment proves a primary and, unless the person chooses the
primary alone, a separately presented backup before publishing anything.
Every enrolled key must independently unwrap the identical vault key,
and the notebook must authenticate under it. Exclude existing credential
IDs during new credential creation and require the operator to use a
separate physical key for the backup; this cannot prove distinct
malicious or cloned hardware. Creating with the primary alone is the
explicit unrecoverability decision AGENTS.md principle 7 names: td-pass
asks for it at creation, saying that losing that key or blocking its PIN
loses the notebook, and a backup can be added later through the ordinary
addition. Nothing in the envelope records the choice; a one-slot table
is the state it leaves.

Adding a key requires fresh authorization by an existing enrolled key, then
creation and proof on the new key. The complete new protector table and
authenticated notebook publish atomically only after proof. Cancellation,
failure or power loss before publication preserves the old working vault.
An orphan credential on a token is possible after interrupted enrollment;
it grants no access without a committed wrapped vault key. A lost success
reply does not authorize replay of the operation.

Each backup is independently usable with the complete vault file on another
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

The envelope admits one through eight key slots with bounded credential
IDs and fixed salt, nonce and wrapped-key lengths. There is exactly one
primary, and any other slot is a backup. Slots have a canonical order
and duplicate credentials are refused. All lengths, counts, versions and
trailing bytes are checked before expensive operations or allocation
proportional to input. Authentication precedes plaintext decoding. Every
successful write uses a fresh random nonce and a checked increasing
revision.

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
device consumer or authorization API. Its synthetic protector inputs do not
establish enrollment, UV or a presented operation. The private ciphertext
store below supplies publication.
The future trusted backend must provide proved token output and kernel
entropy; no application receives access to the raw-key primitive. An open
snapshot pins the complete envelope digest, so revision refuses another
vault or a different retained revision before producing output. Revision
returns proposed encrypted bytes; it neither publishes nor consumes write
authorization and cannot replace the ciphertext store's baseline check.
The primitive permits competing proposals from the same snapshot; the
backend must authorize and publish at most one against that baseline.
A saved envelope needs a new open snapshot before a subsequent revision;
this primitive does not advance the browsing session automatically.

Protector changes are two further proposal primitives with the same
snapshot pinning, vault identity and exact next revision. Adding a key
keeps the vault key and every existing wrapper and inserts one backup slot.
Rotation draws a fresh vault key and rebuilds the complete table from the
supplied protectors; omitted slots are revoked. A supplied protector whose
credential is already enrolled must keep its role, public key and salt and
must unwrap the current vault key, and at least one must be retained. A
slot-only check confirms that a secret unwraps an opened envelope's vault
key without decrypting the notebook again. Neither
primitive proves a new key or authorizes the change; the lifecycle below
does both before publication.

All wire integers are unsigned big-endian. Version 2 replaces the earlier
synthetic-only version 1 prerequisite atomically; version 1 and unknown
versions are refused, with no downgrade or implicit migration. No supported
hardware vault was published by version 1. Version 2 is:

| Field | Encoding |
| --- | --- |
| Magic | Eight bytes `TDVAULT2` |
| Vault ID | 32 random bytes |
| Vault revision | Nonzero u64 |
| Slot count | u8, one through eight |
| Slots | Repeated structure below, strictly sorted by credential bytes |
| Notebook nonce | 12 fresh random bytes |
| Sealed notebook length | u32, 20 through 4 MiB + 16 |
| Sealed notebook | Ciphertext followed by the 16-byte AEAD tag |

Each slot contains role u8 (1 primary, 2 backup), credential length u16
(1 through 1024), credential bytes, a fixed 77-byte public COSE key,
32-byte hmac-secret salt, 12-byte random wrapping nonce and 48 bytes of
encrypted vault key plus tag. Exactly one slot is
primary; all other slots are backups. The maximum envelope is 4 MiB + 16 +
65 + 8 * 1196 bytes. Duplicate or unordered credentials, unknown roles,
truncation and trailing bytes are refused before cryptographic work.

The wrapping key is HKDF-SHA256 with the UV hmac-secret result as input,
vault ID as salt and ASCII `td-secret/portable/wrap/v2` as info. Its AEAD
associated data is ASCII `td-secret/portable/slot/v2`, a zero byte, vault ID,
and the slot's encoded role, credential length/bytes, canonical public COSE
key and hmac-secret salt.
The body key is HKDF-SHA256 with the vault key as input, vault ID as salt
and ASCII `td-secret/portable/body/v2` followed by a zero byte as info. Body
associated data is that same info followed by every envelope byte before the
sealed body, including the full key table, nonce and body length. Both use
the existing RFC 8439 ChaCha20-Poly1305 implementation. Independent random
96-bit nonces are appropriate for this bounded local notebook; no
high-volume encryption service is claimed.

The fixed COSE encoding admits exactly the five canonical public
EC2/ES256/P-256 parameters: kty=2, alg=-7, crv=1, and 32-byte x and y.
The safe P-256 primitive checks canonical field ranges and curve membership
before an envelope can be used. This bounded public-point check runs while
parsing each of at most eight slots, before the final table-order and
trailing-byte checks; it does not perform key agreement or signature work.
Private parameters, extra fields, alternate
encodings, off-curve points and malformed extents are refused. The typed
VerificationKey can be built from a proved enrollment's canonical COSE
bytes, but key parsing itself is not proof of enrollment.

Backend-only unlock hints expose the credential ID, verification key and
salt from a locked envelope. A borrowed iterator enumerates at most eight
hints in canonical credential order so a future adapter can select a
credential without relying on an external credential-ID database. Exact-ID
lookup uses that same iterator. They remain untrusted inputs to an attempted
assertion until the wrapped key and the entire notebook authenticate.
The backend must retain that exact envelope and hint through the attempt,
supply a fresh challenge and enforce signature/UP/UV policy before passing
the resulting secret to open. Hints confer no write or protector-change
authority. This increment stores verification keys but does not persist
assertion-counter history or implement a hardware unlock adapter. Revisions
preserve the complete key table. Changing even a valid public key invalidates
its own wrapping tag and the notebook tag for every other slot.

Decrypted notebook bytes contain entry count u32 followed by each entry's
16-byte stable ID, nonzero u64 revision, u32 title length and UTF-8 title,
then u32 body length and UTF-8 body. Duplicate IDs or exact titles are
refused. Decode is bounded before allocation and authenticates before
plaintext parsing. Empty notebooks and bodies are valid. Secret owners
implement neither Clone nor Debug and clear owned byte buffers on drop,
including decode/error paths, with the existing best-effort limitation.
Entry text fields expose read-only views and whole-buffer replacement;
replacement clears the outgoing buffers before releasing them; each clear
is followed by a `std::hint::black_box` barrier, as elsewhere in the crate.
Protector ordering sorts references, leaving secret owners in their
original vector. These measures do not prevent temporary copies, and the
barrier is best effort rather than a guarantee against elided clears.
Unicode label presentation, including bidi spoofing, needs separate UI
policy and tests; the wire format rejects control characters only.

`tests/portable_vector.py` independently constructs the literal Rust fixture
using host OpenSSL 3.5.7 EVP and Python hashlib/hmac. These are optional
fixture-generation tools, never target inputs or test dependencies. The
ordinary suite checks the exact bytes, both keys, a single-bit flip at every byte offset, every truncation, invalid tables/entries/lengths, failed entropy,
stale snapshots and the exact maximum envelope. Key-specific cases cover
persisted hints after restart and revision, malformed COSE and curve points,
valid-point substitutions against both wrapper and body authentication,
and refusal of old/unknown versions. These are cryptographic
and structural tests, not physical enrollment or recovery evidence.

### Implemented ciphertext publication prerequisite

`src/portable_store.rs`, a private child of the envelope module, owns short
exclusive filesystem transactions. It accepts an already opened, trusted
mode-0700 directory and an independently supplied owner UID. The future td
and standalone adapters must acquire that descriptor through their admitted
path traversal and durably create the directory when needed; this primitive
neither resolves user paths nor infers authority from filesystem ownership.
The adapter runs as that owner and preserves mode-0600 owner bits in its
umask; this primitive does not change process or file ownership. If a newly
created lock fails admission, it attempts to remove only that same inode.
Existing or replaced invalid locks are preserved and refused.
It introduces no command, plaintext writer, hardware consumer or application
API. It is not yet a usable vault backend.

The directory contains an empty mode-0600 `lock` and mode-0600 `vault`.
Both must be regular single-link files owned by the supplied UID. Symlinks,
wrong metadata and oversized or malformed envelopes are refused. Operations
are relative to the pinned directory through Linux procfs. Lock acquisition
is nonblocking. Retirement explicitly unlocks before closing the descriptor,
so an inherited open-file description cannot extend a normally completed
transaction. Abrupt exit still relies on descriptor closure; forked children
must exec or close their inherited descriptors. Lock pathname
identity is rechecked, and a replaced lock is refused. An unavailable lock
mechanism fails closed. A newly created lock and its directory are synced;
existing locks need no repeated sync because locking state is volatile.
Successful vault publication always syncs the directory. The caller drops
the read transaction before token
presentation, retains its snapshot, then opens a new transaction to publish.

A snapshot retains the directory descriptor, the committed inode and exact
ciphertext. Publication consumes that snapshot and the transaction. It
requires the same directory and baseline inode/bytes, or continued absence
for initial creation. New envelopes start at revision one; subsequent ones
must retain the vault identity and advance exactly one revision. These are
structural checks, not authentication: the future backend must authenticate
the retained bytes, prove both initial protectors and authorize the exact
proposed operation before invoking publication. Raw parsed envelopes confer
no such authority. The store neither merges nor automatically retries.

Publication creates an exclusive random mode-0600 temporary, writes only
ciphertext, syncs the file, rechecks the baseline and temporary inode, renames
it over `vault`, and syncs the directory. Errors before the rename attempt
preserve the old committed bytes. Once rename is attempted, every failure
has a typed uncertain outcome with its fixed operation reason, including a
rename error that might have been reported after taking effect. The caller
must reload and authenticate before deciding what to do. Temporary
collisions refuse without replacing anything. A crashed writer can leave
orphan ciphertext; it is never adopted or automatically deleted. Ordinary
errors before the rename attempt try to remove only their own retained
temporary; uncertain attempts may leave orphan ciphertext.
Cooperating writers serialize; a malicious process with the same owner or
root can interfere between checks, and is outside this filesystem boundary.
This does not prevent restoration of an older valid vault from disk.

Tests reopen created and revised ciphertext with each synthetic protector,
reject competing writers and changed baselines, exercise descriptor pinning,
metadata/size/lock refusals, inject failures around publication, and abruptly
exit owned subprocesses before and after rename. Process-exit tests establish
restart behavior, not power-loss durability on every filesystem. Physical
YubiKey recovery and Guix path/device/session integration remain outstanding.

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
The manual hardware diagnostic below is the first private protocol consumer.
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

### Implemented P-256 prerequisite

`src/fido_p256.rs` supplies private P-256 public-key derivation, raw ECDH
and ES256 verification. The private PIN flow below consumes it through the
manual hardware diagnostic; there is no notebook consumer. Existing TPM-backed
application assertions are unchanged. The
curve is fixed to secp256r1/P-256 from
[Standards for Efficient Cryptography 2 (SEC 2)](https://www.secg.org/sec2-v2.pdf).
Operations follow [Standards for Efficient Cryptography 1 (SEC 1)](https://www.secg.org/sec1-v2.pdf)
and [FIPS 186-5](https://doi.org/10.6028/NIST.FIPS.186-5).

Private-scalar admission takes ownership of exactly 32 big-endian bytes,
accepts only `1 <= d < n`, and clears its owner on rejection or drop. The
future protocol adapter must sample fresh kernel randomness with rejection
of invalid candidates; reducing random bytes modulo n is not key generation.
Fill the candidate's heap allocation directly and clear it on entropy
failure; do not copy a plaintext stack array into the box.
Public-key admission accepts two fixed 32-byte coordinates, rejects either
coordinate at or above p, and checks `y^2 = x^3 - 3x + b`. It cannot encode
infinity. For this fixed curve, cofactor one and prime group order make an
admitted finite on-curve point a member of the required subgroup. Arbitrary
curves, compressed-point parsing and noncanonical reduction are absent.

ECDH returns the fixed-width big-endian x-coordinate in a secret owner,
including leading zero bytes. This raw result is not a vault key or an
authenticated peer identity. The CTAP adapter must apply the negotiated
protocol's KDF and authenticate its messages. ES256 verification accepts an
already computed SHA-256 digest and fixed-width r/s values, requires both in
`1..n`, computes `u1*G + u2*Q`, rejects infinity, and compares x modulo n
with r. Both high- and low-s forms are valid. DER, COSE, challenge, RP ID,
UP/UV, authenticator data and extension admission remain adapter duties;
the primitive cannot confer authorization by itself.

Field and scalar arithmetic use different instantiations of a private
Montgomery-residue type with four little-endian u64 words, radix `B=2^64`
and `R=B^4`. Residues stay below their fixed modulus. Addition tracks the
carry above R; subtraction conditionally adds the modulus with masks. Each
Montgomery multiplication performs four operand-scanning iterations and a
single final conditional subtraction. At each iteration the shifted
accumulator is below `m+b < 2m`, where b is the second operand. Before
shifting, the two product additions are below `B*(m+b) < 2*B*m`; six words
retain the carry and the extra word is at most one. Each u128 product/add
fits because `(B-1)^2 + (B-1) + (B-1) = B^2-1`. Both moduli exceed R/2,
so one subtraction also suffices for reducing a 256-bit digest or affine x
modulo n. Inversion uses a fixed 256-bit square/multiply schedule for the
public exponent `m-2`; internal zero inversion maps to zero, and consumers
reject zero denominators or signature scalars before using it.

Jacobian point arithmetic implements the
[Explicit-Formulas Database](https://www.hyperelliptic.org/EFD/g1p/auto-shortw-jacobian-3.html)
`dbl-2001-b` and `add-2007-bl` equations. Addition always computes and masks
the equality, inverse and infinity cases; doubling canonicalizes infinity
with a mask. Scalar multiplication always visits 256 bits, doubles, adds,
and selects without a secret-dependent table or source branch. Allocation
and admission/result errors are outside that arithmetic loop. For admitted
ECDH inputs, its final point cannot be infinity. Verification inputs and
its validity result are public.

SecretScalar and SharedSecret have private heap-backed byte owners without
Clone or Debug. Their drops, the Montgomery accumulator, conversion buffers,
and selected point work buffers use clears followed by `black_box` barriers.
Arithmetic residues and points are Copy values: intermediate field elements,
inversion state, prior accumulator copies and registers are not all cleared.
This is the same best-effort erasure boundary as AES, not comprehensive
memory erasure. `black_box` also discourages replacing masks with branches;
it does not guarantee constant-time code. The actual consumer binary needs
the shipped-compiler inspection gate before hardware integration. No
cross-compiler timing, physical side-channel or cryptographic certification
claim follows from these source-level schedules or test vectors.

The ordinary and source-built suites consume `tests/p256_vectors.txt`:
25 NIST ECCCDH cases, 15 NIST ES256 cases (three valid, twelve invalid),
12 NIST public-key cases (four valid, four off-curve rejections, and four
overwide rows refused by the fixed-width boundary before the primitive),
16 OpenSSL boundary and patterned-scalar ECDH cases, one leading-zero ECDH
case, 68 Python integer arithmetic cases across
p and n, an OpenSSL-verified valid signature with verification x above n,
and three OpenSSL-verified signatures with digests n, n+1 and 2^256-1.
Tests also pin opposite-s acceptance, scalar/coordinate range refusal,
infinity rejection, equal/inverse points and scaled Jacobian coordinates.
These are primitive oracles, not physical token or user-verification proof.

`tests/p256_vectors.py` is an optional offline Python 3.12/OpenSSL 3.5.7
fixture generator. It imports no td implementation, checks the archive
SHA-256 values below, and never enters the build/test/runtime dependency
closure. Its NIST inputs are public CAVP data, not CAVP validation:

- [ECCCDH archive](https://csrc.nist.gov/CSRC/media/Projects/Cryptographic-Algorithm-Validation-Program/documents/components/ecccdhtestvectors.zip):
  `5fff092551f2d72e89a3d9362711878708f9a14b502f0dfae819649105b0ea39`.
- [ECDSA archive](https://csrc.nist.gov/CSRC/media/Projects/Cryptographic-Algorithm-Validation-Program/documents/dss/186-4ecdsatestvectors.zip):
  `fe47cc92b4cee418236125c9ffbcd9bb01c8c34e74a4ba195d954bcb72824752`.

### Implemented owned transaction runner

`src/fido_transaction.rs` sequences one portable assertion or one creation
plus a fresh proof through an owned, private `Channel` interface. Each
transaction fetches getInfo, negotiates the existing PIN profile, exchanges
key agreement and PIN-token requests, then verifies the signed hmac-secret
result. Enrollment uses separate creation and proof PIN inputs and retires
each immediately through the codec. The channel is consumed and dropped on
success as well as every error; there is no command replay, PIN retry,
protocol fallback, reset, PIN change or persistent publication.

The backend must admit one device/channel before constructing a transaction.
`Channel::check` and `exchange` must share the original absolute deadline and
one-way revocation state; Drop must retire the worker. The runner checks
before and after wire exchanges, PIN collection and each entropy callback,
and after cryptographic transitions before accepting their results. A late
PIN, message or verified output is dropped. Callback failure is a typed local
error without its private diagnostic text. A callback that blocks cannot be
preempted by this synchronous runner: the trusted prompt/entropy owner must
observe its lifetime, and accepted results remain bounded by the checks.
Enrollment spans seven exchanges, two PIN prompts and two presence waits
under that one deadline. The Session limit is 120 seconds; the consumer must
budget for the complete ceremony without extending it between phases.

The backend supplies fresh operation-bound client-data hashes, the pinned
credential key and salt, kernel entropy, and trusted or explicitly host-owned
PIN collection. These are not caller-controlled application parameters.
Enrollment refuses equal creation/proof hashes before any device request;
that inequality alone does not prove freshness or bind consent. Both ordinary
assertions and enrollment proofs refuse signed BE/BS flags. Returned metadata
still needs backend counter comparison/persistence and current authorization
before release or vault publication. One creation/proof is not the repeated
primary/backup enrollment and recovery ceremony.
The runner's input hashes, user ID and salt are ordinary copied arrays;
their copies are outside the codec's best-effort clearing claim. They contain
no PIN, private scalar, PIN token or derived vault secret.

Nonzero status bytes are classified before response-body parsing. The typed
surface preserves PIN-invalid, PIN-blocked, PIN-auth-invalid,
PIN-auth-blocked, PIN-not-set/required/policy, denied, cancelled,
no-credential, credential-excluded and timeout responses, plus unknown codes
as `Other(byte)`.
These are untrusted device claims, never proof of user verification. Local
cancel/expiry takes precedence over a late device status; uncertain transport
failure remains distinct from a closed channel. `Transport` also includes
CTAPHID ERROR frames, including channel-busy; it does not establish whether
the authenticator processed a request. Classification follows
[CTAP 2.2 status codes](https://fidoalliance.org/specs/fido-v2.2-ps-20250714/fido-client-to-authenticator-protocol-v2.2-ps-20250714.html#message-encoding).
It grants no retry or recovery authority and does not infer a PIN-attempt
count from a status.
Capability-policy refusals at getInfo remain opaque `Protocol` diagnostics;
they are not typed PIN-setup or recovery instructions. Some named wire
statuses cannot arise from a conforming device in this admitted flow; their names
classify received bytes, not reachable recovery actions. No consumer may
parse those diagnostics to choose an action. Typed capability refusal and
any setup guidance require a later consumer-facing API increment.

The concrete Session binding also serves the manual diagnostic below.
Ordinary and td-built tests run
all four independent PIN transcripts, including creation and proof, through
real child/socket HID framing with keepalives. They also cover every nonzero
status, failures at each enrollment exchange, local interruption, late
results, callback errors and signed backup-flag refusal. There is no
notebook entry point. The
actual consumer binary's generated-code inspection remains mandatory before
hardware admission; these fixtures do not satisfy that gate or establish
physical YubiKey interoperability. Guix device access, authority/prompt
integration and durable primary/backup vault lifecycle remain prerequisites.

### Manual hardware diagnostic

`td-secret check-portable-token --create-test-credential` explicitly requests
creation of one nonresident `td.invalid` test credential. It requires a
root console, exactly one device admitted by the existing root-owned 0600
USB policy, no active swap, zero core-dump soft limit, and a controlling
terminal. It disables the diagnostic process's dumpability and checks that
setting before reading a PIN. The re-executed HID worker relays encrypted
PIN/hmac-secret frames and may be dumpable; it receives no plaintext PIN.
The command changes no device permissions, invokes no elevation tool,
and does not provide unprivileged Guix device admission.

The terminal identifies the operation as host authentication, with no td
secure-attention claim. Three separate PIN prompts authorize creation,
enrollment proof and a second assertion on a newly opened channel. All
three phases share one absolute two-minute deadline, fresh kernel entropy,
and distinct domain-separated challenges. The second assertion must recover
the identical 32-byte secret with a fresh UP/UV signature. Counters may both
be zero; otherwise the second must increase. The output is only a success
message or bounded diagnostic, never a PIN, credential, key, or secret.

The command retains credential metadata and both proved secrets in clearing
owners only until comparison. It saves no file and publishes no vault.
The token may retain an orphan after failure or completion; this command
never deletes credentials, resets the key, changes its PIN, retries a
command, or promises to undo credential creation. The explicit CLI flag is
required before discovery or input. Automated fixtures never invoke this
command on an operator's hardware.

`pin_terminal.rs` opens `/dev/tty` without following its leaf symlink, with
nonblocking I/O. It disables echo and line/signal processing before showing
the PIN prompt, flushes pending input, accepts only the codec's 4–63-byte
ASCII profile, and supports backspace and Ctrl+C/Ctrl+D/Ctrl+Z cancellation. It
polls the terminal with 50 ms waits and never sends input through stdout,
argv, environment or a file. Success and ordinary errors restore and verify
the original terminal mode; Drop makes another attempt after failure.
A restoration failure also emits an explicit terminal-settings diagnostic
on stderr. Busy nonblocking output queues refuse the operation. Drivers
that normalize the requested terminal mode instead of preserving it are
refused. Mode ioctls may wait for pending output to drain: the deadline
refuses late results but does not guarantee wall-clock-bounded teardown.
Abrupt termination can leave echo disabled. Outside a PIN prompt, ordinary
terminal signals may terminate the process, leaving the existing worker
watchdog to retire blocked USB I/O. No portable lock/suspend integration or
real-secret host support follows from this diagnostic.

The [generated-code acceptance record](CRYPTO-AUDIT.md) identifies the
inspected source-built diagnostic. A host Cargo build is not that record.
Physical model/firmware, independent primary/backup proofs and Guix access
still require separate acceptance evidence.

`tests/token_check_vectors.txt` supplies four independent complete command
transcripts, generated by the optional host tool `tests/pin_vectors.py`
with `--manual-check`. Tests reject signed wrong-secret and stale-counter
responses and stop without retry at every exchange. Real pseudo-terminal
fixtures check echo suppression, cancellation, timeout, invalid input,
mode restoration and flushing pasted input beyond the PIN terminator.

### Implemented PIN-authorized hmac-secret assertion flow

`src/fido_pin.rs` implements a private, safe-Rust protocol flow for an
already enrolled ES256 credential. It owns PIN handling, key agreement,
PIN-token decryption, request authentication, signature verification and
one-salt hmac-secret output. The enrollment codec below reuses this PIN
exchange. It has no device, timer, prompt, persistent writer or public
notebook API. It does not change the existing
TPM-backed application assertion path. Response parsing is shared with that
path; software verification uses the private P-256 implementation. Its
private verification context retains a never-sent presence-only request
for parser reuse; only the PIN-authorized request is exposed to transport.

The flow follows [CTAP 2.2 sections 6.5 and 12.7](https://fidoalliance.org/specs/fido-v2.2-ps-20250714/fido-client-to-authenticator-protocol-v2.2-ps-20250714.html#authenticatorClientPIN).
Capability admission requires FIDO_2_0, FIDO_2_1 or FIDO_2_2, hmac-secret,
a configured client PIN, user presence, and a non-platform authenticator. It refuses a
forced PIN change and noMcGaPermissionsWithClientPin. Unknown capabilities
are not identity evidence. Protocol 2 is preferred when advertised; otherwise
protocol 1 must be advertised. Duplicate, empty or malformed protocol lists
are refused. There is no retry with another protocol after any failure.
The first PIN input profile accepts 4 through 63 printable ASCII bytes,
including spaces, without trimming or rewriting. This subset is already
NFC. Existing non-ASCII PINs are unsupported until a separately reviewed
normalization surface exists; this flow never changes a token's PIN.

Before PIN processing, the backend pins the enrolled public key, single
credential ID, fresh operation-bound client-data hash and 32-byte vault
salt. KeyRequest, PinRequest and HmacRequest each consume themselves on
success or failure. The backend must retain one device/channel, enforce the
presented operation and fixed deadline, and drop all pending state on
cancel, disconnect, lock, suspend or authority loss. Borrowed request bytes
are for one transport submission; these types cannot stop a malicious or
incorrect caller from copying or retransmitting them. No token I/O or
transport-retry authority is supplied by the codec.

The shared USB transport now offers an opt-in cancellation handle across
startup and all exchanges, without renewing the deadline or replaying USB
reports. See `DESIGN.md` under USB token transport for its socket polling,
worker teardown and final consumer-check contract. The manual diagnostic
connects the private portable flow to that transport. A notebook owner must route
cancel, lock, suspend and authority loss to the handle, drop idle sessions and
pending PIN state, and check authorization before accepting a result.
td keeps root-only device admission. Standalone mode uses the transport's
explicit desktop admission, which the host's device policy grants; see
`DESIGN.md` under USB token transport. Guix acceptance of that policy,
including a denied open, remains hardware evidence.

Key agreement requires exactly the public EC2/-25/P-256 COSE parameters,
canonical 32-byte coordinates and curve membership. The entropy callback
must fill from kernel randomness directly into the candidate allocation.
Invalid scalars are rejected, with at most eight candidates; entropy errors
clear the candidate and stop immediately. One fresh ephemeral key belongs
to this PIN/extension transaction and is never retained across operations.
Raw ECDH is immediately derived and retired. Protocol 1 uses SHA-256, zero
IV AES-256-CBC and 16-byte truncated HMAC-SHA256. Protocol 2 uses separate
32-byte HKDF-SHA256 outputs with the standard CTAP2 AES/HMAC labels, a fresh
16-byte IV for each encryption prepended to ciphertext, and full HMAC-SHA256.
The two encryption steps request independent IVs; callback failure retires
the pending state without producing a request.

Tokens advertising pinUvAuthToken use subcommand 9 with only getAssertion
permission and the fixed td.invalid RP ID. Others use legacy subcommand 5,
which grants broader authenticator-side default permissions, but this codec
consumes the returned token for exactly the pinned assertion. It accepts
16 or 32 plaintext token bytes under protocol 1 and exactly 32 under
protocol 2. The decrypted token is retired immediately after authenticating
the pinned client-data hash. It is not proof of user verification by itself.
No raw token accessor, reset, changePIN or setPIN path exists. The creation
flow below has its own operation type and permission.

The assertion requests presence, the PIN authorization and one encrypted,
authenticated salt. Built-in uv is absent because ClientPIN supplies UV.
Protocol 2 is explicitly named inside hmac-secret; protocol 1 uses its
specified extension default. All command and response lengths include the
command/status byte and respect the token limit and local CTAP ceiling.
The final response must pass the shared allow-list, RP, presence, counter,
DER and authenticator-data checks, carry UV, and verify against the pinned
credential key over authenticatorData plus the exact client-data hash.
Only then is the signed hmac-secret ciphertext decrypted. Counter admission
is structural only; comparison and persistence against the enrolled record
are backend duties. Missing extensions,
wrong output lengths, malformed CBOR and signatures, and CTAP error statuses
return no output. The codec alone reports status bytes in diagnostic strings.
The transaction runner above classifies them before invoking the codec; no
consumer may parse human-readable error text to choose retries or recovery
actions.
The backend-only output owner contains exactly 32 bytes
plus assertion metadata; it is not an application release or store write.
The standard unauthenticated clientPIN response has no independent MAC; the
final signed assertion is required even when token decryption succeeds.
The transport and authenticator remain trust boundaries; this does not claim
resistance to a malicious authenticator or PIN-channel interception.

PIN, token, key and output allocations have no Clone or Debug and clear on
drop with black_box barriers. PIN hashes and returned KDF/MAC work arrays
also clear, including the HMAC normalized key and HKDF extract output.
Other hash/HMAC internals, arithmetic temporaries, copies,
registers and allocator behavior remain outside the best-effort erasure
claim. The eventual hardware consumer still requires the shipped-compiler
assembly inspection gate described above. Fixtures do not establish physical
user verification, token interoperability or production readiness.

`tests/pin_vectors.py` independently constructs four public full transcripts
with Python hashlib/hmac and OpenSSL 3.5.7 P-256/AES. It verifies every fixture
signature with OpenSSL, including signed negative UV/presence/RP/extension
cases. The ordinary and td-built suites consume only committed literals in
`tests/pin_vectors.txt`; regeneration is optional, offline and never part of
the dependency closure. Tests compare exact request bytes, KDFs and output,
and refuse signed-byte mutations, truncations, malformed negotiation and
key/token responses, size violations, and failed entropy at each stage.
Negative-policy tests first verify each fixture signature independently of
the policy parser, then assert its exact refusal reason.

### Implemented portable creation and proof codec

The same private `src/fido_pin.rs` now supplies PIN-authorized
makeCredential followed by a separate PIN-authorized hmac-secret proof.
It shares key agreement, PIN-token admission and authentication with the
assertion flow through typed operation state. Scoped tokens request only
makeCredential permission (0x01) for creation, then only getAssertion
permission (0x02) in a new proof transaction. Legacy subcommand 5 remains
selected only by capability, before any attempt. Each token is retired
immediately after authenticating its one pinned client-data hash.

The backend supplies a fresh creation hash, opaque 32-byte user handle and
all existing credential IDs. The codec accepts zero through eight excluded
IDs, bounded by advertised list/ID limits and the complete encoded request
size. Empty and duplicate IDs are refused; an empty list is omitted from
the wire. No ID is dropped, shortened or split across requests to fit a
token. The full creation command is size-checked before PIN processing.
When getInfo advertises creation algorithms, the list must be nonempty,
well-typed and duplicate-free. Unknown text credential types are ignored
for selection, never treated as public-key. Enrollment requires ES256
with the public-key type in that list;
an absent list permits an ES256 request whose response still has to match.
An advertised list without ES256 does not forbid an existing ES256
assertion. The other PIN capability requirements remain unchanged.

The fixed request uses td.invalid, generic personal-vault display labels,
ES256, rk=false, hmac-secret=true and the selected PIN protocol and
authentication parameter. User presence is left at its required true
default for CTAP2.0 compatibility; built-in uv is absent. It requests no
enterprise attestation, discoverable credential, credential deletion or
PIN administration. The backend must present each immutable operation,
retain its device/channel and deadline, and supply independent fresh
entropy for creation and proof. The codec has no transport and cannot
establish those obligations or enforce physically distinct tokens.

The creation response is canonical CBOR bounded by the local HID/CBOR
ceiling, including its status byte. The token's advertised maxMsgSize
limits commands it receives, not attestation-bearing creation responses.
A present epAtt field must be boolean false; unsolicited enterprise
attestation is refused. This cannot undo identifying bytes already sent
by a token. Require the fixed RP,
UP, UV, attested data, extensions and neither backup flag; admit only a
nonempty bounded ID that is not excluded. The public COSE key has exactly
EC2/ES256/P-256's five public parameters, canonical 32-byte coordinates
and validated curve membership. Parse the complete extension map and
require hmac-secret=true. The AAGUID must match getInfo, except that none
attestation may anonymize it to zero. None attestation permits an omitted
or empty statement; other nonempty bounded format names require a map,
whose contents (including an empty map) are not verified.
Attestation statements and AAGUIDs are not authenticated identity evidence;
no certificate parser, trust root or attestation verification is added.
The flags and hmac-secret confirmation in this response are likewise
untrusted until the subsequent proof establishes the usable credential.

Creation returns only a pending proof state, with no candidate credential
or output accessor. The proof must have a different, fresh operation-bound
challenge and the exact vault salt. Its state owns the candidate ID/key and
carries that identity through key agreement, PIN authorization and the
signed assertion. Only successful software ES256 verification of UP, UV
and the hmac-secret ciphertext, plus refusal of both signed backup flags,
produces an EnrolledCredential. It retains the exact ID, canonical public
COSE key, salt, 32-byte hmac-secret output owner and verified assertion
metadata. The opaque creation user handle is retired; it is not an
authenticated account identity or the output secret. The
unsigned creation counter is not a persisted baseline; the verified proof
counter is available for the future backend to retain and compare.

This is one proof of usable key possession and a UV secret, not a complete
primary/backup enrollment transaction or a physical hardware result. The
future backend must repeat salt recovery with independent exchanges, prove
both wrappers open the same notebook, transfer the proved public verification
keys into the version-2 envelope, and publish only the complete proved
protector table. The envelope now authenticates those keys; the hardware
adapter and persistent publication remain prerequisites for usable unlock.
Cancellation or refusal consumes pending state and produces no enrolled
credential; a token may still contain an orphan after uncertain creation.
No operation is automatically retried and no vault bytes are published.

The extended public Python/OpenSSL fixtures cover both PIN protocols and
scoped/legacy commands, independent creation/proof ECDH exchanges, exact
creation requests with and without exclusions, none/packed creation
responses and signed proof output. Proof cases reuse the existing assertion
cryptographic oracle with a new credential ID; each creation exchange uses
different scalar/peer/token/IV material from its corresponding proof. The original assertion rows are
unchanged. Tests reject response truncations, wrong flags/RP/AAGUID,
excluded IDs, malformed/off-curve/private COSE keys, missing or false
hmac-secret confirmation, stale proof hashes, wrong proof keys, signed
backup flags, message/list/ID limits and failure at each PIN boundary.
Additional response tests cover none with an empty statement, a non-map
packed statement, epAtt admission, and attestation-bearing responses above
the command limit up to the exact local response ceiling.
These remain offline codec tests, not YubiKey interoperability evidence.

### Implemented vault lifecycle backend

`src/portable_lifecycle.rs`, a private child of the envelope module,
composes the envelope, the ciphertext store and the transaction runner into
creation, unlock, save, add-key and key replacement. It has no command,
prompt, device admission, path policy or notebook API; those are adapter
duties. It is not yet reachable from any entry point.

Token access is one private interface. Each call admits one fresh channel,
presents one key and returns only a verified result: an enrollment's
credential ID, canonical public key, salt, UV secret and counter, or an
assertion's UV secret and counter. Every call names its operation purpose
and the role of the key to present, and an assertion also names its
enrolled credential, so a prompt can identify the exact token. The
production adapter drives the transaction runner over a backend-supplied
channel, PIN prompt and entropy source and passes that presentation to
both; an unavailable channel sends nothing and prompts for nothing.
Synthetic token implementations exist only in tests. Purpose labels
describe the operation and never contain notebook content.

Every client-data hash is SHA-256 over `td-secret/portable/operation/v1`
and a zero byte, the purpose and phase bytes, a u32-length-prefixed
operation binding, and 32 fresh bytes. Creation, proof, repeat and
authorization phases are distinct. Bindings after creation start with the
vault ID and baseline revision. A save has no client-data hash: it
presents no token. Purpose byte 4, a save's former authorization, is
not reused. Replacement adds an order-independent digest of the revoked
credential set. Adding a key binds only the vault and revision, since
the new key does not exist when the existing key authorizes it.

Each newly enrolled key is created and proved in one transaction, then
asserted again on a new channel. The repeat must recover the identical
secret with an advancing counter, except that two zero counters are
accepted. Creation refuses an existing vault before contacting a token,
enrolls the primary, then, unless the caller asked for the primary
alone, enrolls the backup with the primary excluded. Every enrolled key
must open the proposed envelope to the identical vault key before
revision one is published against the absent baseline. Six PIN prompts
cover the two-key ceremony and three the primary alone. Any refusal
publishes nothing.

Unlock reads the committed envelope, uses the caller-selected credential's
hint, and requires an assertion that opens the whole envelope. The caller
chooses which enrolled key to present; this increment does not probe a
token for its credentials, so a different token is refused only after its
PIN. A session holds the authenticated baseline, the opened vault key and
notebook, the identity of the directory it was opened from, the key that
unlocked it, which authorizes adding a key, and the verified counters
observed during the session. Counters are not
persisted, so they detect only regression within a session, not across
restarts or copies. An operation checks its counters as it goes and
records them only after it commits.

Proving a proposal decrypts its notebook once, with the first key; every
other key need only unwrap the identical vault key from its slot. An
authorization likewise unwraps only its slot and compares the key with the
session's.

Every write first refuses, before any token, a directory other than the
session's, by device and inode, so a copy holding identical bytes cannot
receive the session's writes. It then takes a store snapshot and refuses,
still before any token, when its bytes differ from the session's
authenticated baseline; that vault changed under another writer and
requires lock and unlock. Identical bytes at a new file, as a synchronizing
tool's rename leaves them, are accepted. The write publishes against that
snapshot, so the store's baseline check refuses a change made while a
token is being presented.

A save revises with the session's vault key, presents no token, and
proves its proposal by opening it with that same key before publication:
the revision copies every slot unchanged and the body tag authenticates
the whole table. The owner's lock is checked once more before
publication. A refusal leaves the vault and the session's contents
unchanged, and the session's next write takes a new snapshot. An
uncertain publication is reported as such: if the new bytes were
committed, every later write is refused until lock and unlock; if not,
the session continues.

Adding a key is refused at eight keys before any token. The session key
authorizes it, the new key is enrolled with every existing credential
excluded and repeated, and both keys must open the proposal to the
unchanged vault key. Key replacement revokes a named set of keys together
and enrolls one replacement. The set must be nonempty, distinct, enrolled,
and leave at least one key retained; these checks happen before any token.
Every retained key, and no revoked key, gives a fresh assertion bound to
the set. Revoking a set, rather than one key at a time, lets the owner
remove a stolen key together with any key its thief added, and recover
when several keys are lost at once. The replacement is enrolled with every
credential, including the revoked ones, excluded. It becomes primary when
the primary is revoked and a backup otherwise. The cost is that every
enrolled key, with its PIN, holds full authority: one key alone may revoke
all the others and enroll a key of its holder's choosing. A thief holding
any key and its PIN can therefore lock the owner out of the current file.
The owner's remedies are to revoke first and to keep exported copies, which
the revoked keys still open. Rotation must produce one
new vault key, different from the old one, that every listed key opens, and
no revoked credential may remain. A session whose key was revoked continues
under the replacement.

Revocation protects the current vault file only. Historical copies stay
readable by the keys they enrolled. Nothing persists a revision floor:
anyone able to write the vault directory can restore a pre-rotation copy.
The retained keys still open it, unlock accepts it, and later saves would
encrypt new contents under a vault key a revoked token can unwrap. Restoring
a copy from before a key was added likewise puts later saves under that
copy's keys alone, perhaps a primary without a backup. The portable format
provides no rollback protection, as stated above.

Tests drive every operation through a simulated multi-token bench with
real P-256 public keys and per-credential secrets. They cover:

- the operator presenting the wrong token or the same token twice;
- mismatched repeat secrets during creation, key addition and replacement;
- stalled counters during creation and on a later key addition;
- token refusal and unavailability on unlock and creation;
- saves that present no token, each sealed afresh, and the adding key
  being the one that unlocked;
- writes from a stale session and to another location holding the same
  vault, refused before any token;
- a vault replaced, or a store locked, while a key addition's token is
  presented, refused at publication, followed by a successful save;
- the eight-key limit and invalid revocation sets;
- revocation of the primary, of the session key, and of a stolen key
  together with the key its thief added;
- the role and credential named at each presentation.

Uncertain publication is exercised by the store's own fault tests; the
lifecycle handles every publication failure the same way. The
production adapter returns exactly the four public transcripts' credentials
and outputs, and passes the presentation to its channel and prompt. This is
not physical YubiKey, PIN-prompt, Guix access or recovery evidence.

### Implemented notebook entry API and encrypted export and import

`src/portable_notebook.rs` expresses the notebook surface over an
unlocked session: list, read, create, edit, rename and delete. Listing
yields each entry's stable ID, revision and title in stored order; reading
returns one entry. Every change names the entry revision its caller last
read. A stale revision, a missing entry, an empty, oversized,
control-bearing or duplicate title, an oversized body, the 1024-entry limit
and the 4 MiB plaintext bound are refused with typed entry errors before any
token is presented. Renaming an entry to its own title is not a duplicate.
Entry revisions are relative to the session's notebook; a vault changed by
another writer is refused by the lifecycle's own pre-token check.
Caller text enters a change in a clearing owner, so a refused change clears
it on drop as an accepted one clears its entry.

Create draws a fresh random 16-byte ID and starts at entry revision one.
Edit and rename advance the revision by exactly one; rename keeps the body.
Delete removes the entry. Each accepted change builds the complete next
notebook and runs one save under the session's unlock, presenting no
token. A refused or failed save leaves the session's entry at its base
revision with its old contents. Committed changes report the entry ID
and its new revision, or none after deletion. The API never
writes plaintext outside the session; body bytes are UTF-8 and stored
exactly.

Export is the session's authenticated baseline ciphertext, unchanged. After
an uncertain publication the store may already hold a newer revision, so
the session refuses export until lock and unlock; its reads still show its
last authenticated notebook. The caller chooses where to write it. An
adapter that reads a copy for import must bound the read to the maximum
envelope size plus one byte, as the store does. Import requires a
location holding no vault, parses the copy, and asks the selected
enrolled key for a fresh assertion bound to the vault ID, revision and
the digest of the imported bytes. The whole copy must open before the
store places it. The store's adopt path accepts any revision of an
authenticated copy, but only against an absent baseline; it never
replaces a committed vault, even with its successor. The imported vault
keeps its identity and revision, and the session continues from it.
Import is therefore restore onto a fresh location, not synchronization
or merge.

Tests cover each entry operation and read the final notebook through the
other key after unlock. They cover every typed refusal for each change kind
before any token, that no change presents one, a save refused by a lock
keeping the base entry, and both notebook bounds, reached by creation,
edit and rename. Export is refused after an uncertain publication.
Import tests refuse an unenrolled key and an undecodable copy before
any token, and refuse a tampered body or a
tampered slot salt, and a wrongly presented token, without writing. A
fresh-location restore with only the backup is followed by replacement
of the lost primary. Store tests cover adoption into an absent baseline
and its refusal over a committed vault.

### Implemented standalone host adapter

`src/portable_host.rs` supplies the standalone mode's host pieces; the
notebook process composes them with its own host-authentication prompt.

- **Process.** Protection requires the transport's desktop identity:
  unchanged user and group IDs and a nonzero uid. It makes the process
  non-dumpable, and the core-dump soft limit must be zero, the manual
  diagnostic's policy. Swap is admitted when every active device keeps
  its pages in memory: a zram device without a writeback device, whose
  `backing_dev` reads `none`, or is absent in a kernel built without
  writeback while the zram attribute `comp_algorithm` is present. Any
  other active swap (a partition, a swap file, a zram device that writes
  back, or one whose attributes cannot be read) can put PINs, keys and
  entry text on storage, where they outlive the process, and lets
  hibernation write all of memory there. `Host::open` then opens nothing
  and returns a `SwapRisk` naming every such device. The window explains
  that risk and how to avoid it, and opens only if the person accepts
  it, Cancel being the default; the acceptance covers those devices for
  that process alone and is never stored. Opening a token, and a save,
  which opens none, require the evidence protection returns and recheck
  the core limit and swap first, refusing a storage device that was not
  accepted, since the notebook process is long-lived. Swap enabled while
  a vault key is already held is not detected until the next
  presentation or save.
- **Location.** The vault directory is `$XDG_DATA_HOME/td-pass`, or
  `$HOME/.local/share/td-pass` when that variable is unset, empty or
  relative, as the XDG base directory specification requires. The adapter
  walks that absolute path from `/` one component at a time, opening each
  directory through the descriptor of the one before it without following
  a link, so the directory it checks is the one it holds; no pathname is
  checked and then reopened. Every directory on the walk, `/` included,
  must be owned by the account or root and writable by no other user or
  group unless sticky, so no other account can rename the vault, or a
  directory above it, aside and later restore an older one. The path may
  pass through links, but only links the account or root owns, since
  another account may create one in a sticky directory; at most 40 are
  followed, an absolute target restarts the walk at `/`, and `..` steps
  back to the held parent. Missing directories are created with mode 0700
  and their parent synced at once. Every held directory is synced on every
  open, which also completes the record of a creation whose sync failed or
  was interrupted; a directory on a filesystem that cannot sync one, as a
  read-only mount, is passed over; the store's own sync fails there too,
  so nothing is published through it. Group write is refused even for the
  account's own private group: on a host whose umask 002 leaves
  `~/.local/share` at 0775, the adapter refuses until the account runs
  `chmod g-w` on it. The check reads mode bits only: an ACL granting
  another account write is not seen, so a host that grants one has
  stepped outside this policy. Inside a user namespace that shows `/` or
  another directory on the walk under an unmapped owner, the adapter
  fails closed. The vault directory is opened without following a link
  and must be owned by the account with mode 0700. One this adapter
  creates is set to exactly 0700, clearing a setgid bit inherited from
  its parent, and an account-owned 2700 directory left by an interrupted
  creation is repaired the same way. The store then applies its own
  descriptor-relative policy.
- **Tokens.** Each presentation discovers tokens under desktop admission
  and opens one bounded transport session that the caller's cancellation
  handle ends, so lock, suspend and authority loss can stop it. Discovery
  reads node metadata only and requires exactly one connected FIDO node;
  more than one is reported as several, and none as unavailable. The
  kernel decides at the bounded worker's open whether the host grants the
  account that node, and a refusal is reported as a host-policy denial.
  Unprotected memory or a changed identity is a host refusal. These typed
  reasons reach the caller unchanged, before any PIN prompt, so a
  permission error stays distinct from a missing or unsupported token. Any
  other transport failure, including a busy lock, is reported as
  unavailable.
- **Entropy.** The adapter opens `/dev/urandom`, requires character device
  1:9, and reads directly into the caller's buffer.

Tests cover the location rules, private creation of the directory and its
ancestors, refusal of a foreign owner, a wider mode, a final link and a
regular file as typed policy refusals, and acceptance of a linked prefix.
They refuse a group- or world-writable parent or higher ancestor, also
when reached through a link, and a parent owned by another account. They
accept a sticky one, and repair a setgid inheritance and an interrupted
one. They follow a relative link and a `..` step to the directory they
name, and refuse a link loop, a relative path and an absolute link target
outside the walk's base. They cover the ownership and link-owner rule
tables, the token choice table, and the entropy device's refusal of
another character device and a regular file. The directory tests start
the walk at their scratch root's parent, since a build sandbox may show
unmapped owners above it; links owned by another account cannot be made
without privilege, so that rule is tested as a table. Neither the rename
races the walk closes nor the durability retry is observable in a test.
The lifecycle adapter's test passes each typed open failure through,
without a prompt, for both enrollment and assertion. Process
protection is tested where it is set, in an owned child. Real desktop
discovery, denial, cancellation and hotplug remain hardware evidence on
the supported host.

### Implemented notebook library API

td-secret is a library crate with a binary that only calls its `run`.
Its one other public module is `pass` (`src/portable_pass.rs`), the
notebook API td-pass links for standalone mode; nothing else in the
crate is public, `pass` re-exports nothing, and a confinement test pins
both.

- **Host.** `Host::open` applies the host adapter's process protection
  and admits the account's vault directory, refusing a host that cannot
  keep PINs and keys out of dumps; swap that can put them on storage is
  returned as a `SwapRisk`, opening nothing until the caller passes it
  back accepted. `keys` lists the enrolled keys, or none when no vault
  exists. `create`, `unlock`, `import`, `apply`, `add_key` and
  `replace_keys` are the lifecycle and notebook operations above, over
  the production token adapter. Choosing standalone mode is the
  caller's: on td the notebook uses the admitted service, and no failed
  service request may lead to a `Host`.
- **Keys.** A `Key` is an enrolled key's role and public credential
  identity, with a `Fingerprint`: the first four bytes of the
  credential's SHA-256, so a person can tell backups apart. It holds no
  secret. `keys_of` reads the keys of an exported copy without
  authenticating it, so a person can choose the key to import with. A
  caller reads a copy to at most `MAX_COPY` bytes plus one and refuses
  one that reaches the extra byte.
- **Vault.** An unlocked `Vault` lends entry summaries and entries (the
  title and the stored body bytes), names its revision, its keys and the
  key that unlocked it, which authorizes adding a key, and exports its
  authenticated ciphertext. Dropping it is the lock. `Host::apply` saves
  a change under that unlock and takes no `Prompt`.
- **Changes.** `Change` carries owned text and clears it on drop,
  whether it is applied, refused or never sent; applying moves the text
  into the entry API's clearing owners.
- **Prompt.** The caller supplies the host authentication adapter's
  `Prompt`. Before each token is opened, `present` names the operation
  label, the role and, for an enrolled key, its fingerprint; it returns
  once the person has connected that one key. Creation therefore asks
  for the primary and then, when the caller asked for one, the separate
  backup. `pin` returns the PIN for the presented key, as the
  transaction's PIN owner zeroes it; its `PinUse` is `Authorize` for
  every enrolled-key assertion, which `Request::operation` names. An
  error from either is the person declining, and the operation ends as
  cancelled; a PIN outside the profile is refused as such. Neither is
  retried.
- **Cancel.** A `Cancel` ends an operation from another thread, as a
  lock, suspend or authority loss does: the token session in flight,
  including its startup, which reports cancellation, not a missing key;
  a presentation not yet asked for; and a publication not yet begun. The
  lifecycle checks the token adapter's revocation once more after the
  last token exchange, before every publication, so a key change,
  creation or import cancelled then publishes nothing; a save, which
  has no token exchange, checks the `Cancel` itself before publishing.
  A revocation after that check cannot stop the publication already
  begun, which then reports its result; a creation or import that
  returns its `Vault` so is the caller's to drop when it has locked. An
  unlock cancelled before it returns drops the session. A presentation
  or startup the cancel
  interrupts reports cancellation, whatever startup then failed with. A
  `Cancel` stays cancelled, so each cancellable operation takes a fresh
  one.
- **Failures.** A `Failure` names the stale-entry, uncertain-publication
  and cancelled cases and displays a reason. Neither its text nor its
  `Debug` carries a PIN, entry content or a token's protocol detail.
- **Worker.** A presentation re-executes the running binary as the
  desktop token worker. `worker` answers exactly that argument vector,
  after the program name as `run` takes it, so a linking program calls
  it first and exits with its result.

Tests cover every failure's text and `Debug`, including that a protocol
detail is not shown, and each failure predicate. They also cover the
fingerprint, a presentation's request, the refusal of an undecodable
copy, the worker's argument vector, the change conversion and the
library's public surface. The lifecycle's tests add a revocation after
the last token exchange: the token is asked, and neither a key change
nor a creation publishes; a save under a revocation asks no token and
does not publish. The operations themselves are the lifecycle's,
tested above with synthetic tokens. Their production adapter path, from
`present` to a real token, remains hardware evidence.

### Implemented host lock and sleep events

`src/portable_events.rs` watches the host's screen lock and sleep for
standalone mode through logind on the system bus (elogind on Guix
System); `pass::HostEvents` hands them to the notebook, which locks
without asking about unsaved changes.

- **Connection.** The system bus is `DBUS_SYSTEM_BUS_ADDRESS` when it
  names one absolute `unix:path=` address, else
  `/run/dbus/system_bus_socket`; any other address is refused. The
  connect waits at most ten seconds, on a thread of its own. The watcher
  authenticates with SASL EXTERNAL as the process's uid and negotiates
  descriptor passing. Setup and every later method call have a
  ten-second deadline, so `HostEvents::watch` can take twenty seconds;
  waiting for a signal has none. Dropping `HostEvents` shuts the
  connection down, which ends the watching thread and releases a delay
  it holds.
- **Subscription.** It first subscribes to the bus's `NameOwnerChanged`
  for logind, then learns logind's unique name with `GetNameOwner`, so a
  restart in between is seen. It asks `GetSessionByPID` with pid 0,
  which logind answers from the caller's bus credentials, so neither a
  PID namespace nor a reused pid misleads it, and subscribes to the
  manager's `PrepareForSleep` and that session's `Lock`. The session is
  the one the process belongs to, whoever's it is. A signal counts only
  when its sender is the unique name learned (the bus itself for
  `NameOwnerChanged`) and its path, interface and member are the watched
  ones; the match rules are not trusted alone. A reply counts only from
  the bus for the bus's calls and from logind's unique name for
  logind's, or as an error from either.
- - **Other accounts.** Any account may send this connection a signal. A
  frame's length is read from its header without the codec's ceilings: a
  frame past 16 KiB is read through and passed over, one carrying more
  than one descriptor has them closed at once and is passed over, and an
  undecodable one is passed over; none of them ends the watch. Only a
  length past the protocol's own 128 MiB maximum, or a byte order it
  does not name, which the bus never sends, does. Every frame but a
  watched signal or the awaited reply is dropped as it is read, also
  while a call waits for its reply, so none holds memory; that wait is
  bounded by its deadline.
- **Sleep delay.** It asks logind for a `delay` inhibitor of `sleep`.
  When `PrepareForSleep(true)` arrives the held inhibitor goes to the
  caller inside `Suspend`; the caller drops it once it has locked, and
  logind's own maximum delay bounds the wait either way. After
  `PrepareForSleep(false)` the watcher takes a new one. A refusal, by
  logind (polkit decides) or by the bus, is not an error: sleep is then
  not delayed. `delays_sleep` says whether logind granted one when
  watching began; a refusal after a later wake shows only in that
  suspend's `SleepDelay::held`, and the notebook may then lock only once
  the machine wakes. Watched signals read while a call waits for its
  reply are kept, in order.
- **Loss.** logind's name changing owner, the bus closing, or a read
  failing ends the watch with `Lost` and its reason, and nothing is
  reported after it. The caller locks then and goes on with its own Lock
  only.
- **Scope.** Only logind's signals are seen. A screen locker the
  compositor runs directly, as Sway's `swaylock` alone, sends none; the
  supported Sway setup locks through `loginctl lock-session`, for
  example from swayidle's `lock` event, and the Guix acceptance records
  that setup. Hibernation is sleep to logind and is reported the same
  way, so the notebook locks, dropping its key, before the image is
  written. The image goes to whatever swap is active then, which may be
  storage swap a hibernation hook turns on after td-pass checked; that
  lock, not the swap policy, keeps the key out of it, and an unlocked
  notebook that misses it is the gap the unwatched host warning names.
- **Descriptors.** The watcher is td-secret's second descriptor receiver
  (`UNSAFE.md` §15). It calls the shared receive, adoption and disposal
  functions once each. An owning guard closes every descriptor a frame
  carries; only the one an `Inhibit` reply names, as its one `h`
  argument, is taken. A test pins `client.rs` and this file as the
  crate's only receivers.

Scripted-bus tests over a socket pair cover the authentication and call
sequence with its exact match rules; a granted delay held until dropped,
also when the watcher drops or is stopped; another sender's or another
session's lock and another sender's sleep ignored, each before an event
of another kind; owner changes of another name or from another sender
ignored; a lock that arrives while a new delay is asked for kept among
many unwatched signals; a stray descriptor, a frame with two descriptors
and an oversized frame passed over with their descriptors closed; an
inhibitor reply that does not name its descriptor refused and the
descriptor closed; a refused inhibitor; logind's restart, also during
setup, and the bus closing as loss; refused authentication, descriptor
negotiation and session lookup; a reply from another than logind; a
reply of the wrong type; a call passing over many other frames, and
frames past the codec's header and body ceilings, to its reply; the
length rule's bound and byte orders; the address rules; and the sole
descriptor sites. `HostEvents` is tested to end after loss. Deadline
expiry is not tested: it would take ten seconds. A real logind and
elogind, on the supported host, remain acceptance evidence.

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
