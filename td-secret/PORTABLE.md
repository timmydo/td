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
vault ID, credential ID, canonical public verification key, role and
derivation salt. The encrypted notebook authenticates the complete protector
table, format version and revision as well as its contents. Unauthenticated metadata is only bounded input to an
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

All wire integers are unsigned big-endian. Version 2 replaces the earlier
synthetic-only version 1 prerequisite atomically; version 1 and unknown
versions are refused, with no downgrade or implicit migration. No supported
hardware vault was published by version 1. Version 2 is:

| Field | Encoding |
| --- | --- |
| Magic | Eight bytes `TDVAULT2` |
| Vault ID | 32 random bytes |
| Vault revision | Nonzero u64 |
| Slot count | u8, two through eight |
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
replacement clears the outgoing buffers before releasing them. Protector
ordering sorts references, leaving secret owners in their original vector.
These measures do not prevent compiler-elided clears or temporary copies.
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
The private protocol flow below has no hardware consumer yet.
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
and ES256 verification. The private PIN flow below consumes it; it has
no direct device, entropy or notebook consumer. Existing TPM-backed
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
return no output. Status codes currently appear in fixed diagnostic strings;
the device adapter must add typed status handling before exposing PIN retries
or recovery actions, without parsing human-readable error text.
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
