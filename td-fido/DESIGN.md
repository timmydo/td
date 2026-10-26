# td-fido: the shared FIDO2 client

td-fido is td's FIDO2 (CTAP2) client and the home of the HMAC-SHA256
td's std-only crates share. AGENTS.md principle 2 puts code two crates
need in one sibling crate: td-secret's portable vault, store enrollment
and login keys use a security key today, td-tpm salts and encrypts its
PIN-authorized sessions with this crate's P-256, AES and HMAC-SHA256,
and `td-install/ENCRYPTION.md` increment 8 has td-boot's selector unlock
a disk with one. Increment 8a moved
the code here from td-secret with no behaviour change; td-secret is its
first consumer. It is pure `std`, depends on no crate, compiles the
engine's SHA-256 as shared source, and forbids `unsafe`; it adds no
syscall surface to `UNSAFE.md`. Its hidraw transport is std file I/O
and its report descriptor a sysfs read, so it needs no hidraw ioctl.

## Scope

The modules keep the names they had in td-secret, whose DESIGN.md and
PORTABLE.md specify each one's protocol and bounds ("FIDO2 protocol
prerequisites" onward):

- `fido_hid`: the 64-byte CTAP HID report framing and the typed
  initialization transaction.
- `fido_cbor`: CTAP's bounded canonical CBOR.
- `fido_ctap`: the getAssertion and silent identify codecs, credential
  fingerprints and DER signatures; `fido_enroll`: getInfo negotiation
  and makeCredential with proof of possession.
- `fido_pin`: PIN protocols one and two, the PIN and pinUvAuthToken
  flows, hmac-secret and the login profile; `fido_transaction`: one
  owned PIN transaction over a `Channel`. Since increment 8b the login
  profile's admission requires getInfo's versions to include
  `FIDO_2_1`, after its typed refusals (`LoginRefusal::NoFido21`), and
  `Transaction::probe` is TOKEN-LOGIN.md's no-PIN probe: the
  PIN-authorized hmac-secret assertion without its pinUvAuthParam and
  protocol, verified with signed backup flags refused as every login
  assertion's are, whose output must differ from the PIN's
  (`LoginRefusal::PinOptional`); any status, NO_CREDENTIALS included, is
  refused, since the identify probe has already selected the credential
  without UV. `FIDO_2_1_PRE` alone is not `FIDO_2_1`. CTAP
  2.0's hmac-secret holds one secret, so a key that has no `FIDO_2_1`,
  or claims it over one secret, would give the PIN's output without the
  PIN. The notebook's admission is unchanged.
- `same(a, b)`: equality of two secrets comparing every byte, which the
  repeat and probe comparisons share.
- `fido_disk`: the disk protector's flows (td-install/ENCRYPTION.md,
  "FIDO2 protectors"), since increment 8b: its client-data hash, the
  keyslot passphrase (HKDF-SHA256 through `hkdf_expand`), `release` and
  `create`, over `fido_transaction::Devices`, the device access
  td-secret's login worker shares, and a `Person` that answers PIN steps
  and is told when the key waits for a touch. Creation is the login
  creation under the `td disk` labels (`Transaction::disk_create`).
  `authorize` is `release` under the authorize phase in two presented
  steps, for a login operation's disk proof (td-authd/DESIGN.md
  amendment 10): the identify hash binds the caller's identify step, and
  the assertion's binds the step its `Steps` presents for the selected
  token and the key's PIN retries, which exist only once the key has
  answered. `create_stepped` is `create` in presented steps, for the
  upgrade's key ceremonies (td-authd/DESIGN.md amendment 10, tag 17):
  its `CreateSteps` gives the create step's description once the key
  reports its retries, the prove and repeat steps' once it also names
  the new credential, and the probe step's, which both probes bind;
  each phase's hash binds its own step, and every check is `create`'s,
  which is `create_stepped` with one description for every phase. It
  holds no header and no policy; td-boot's selector and td-install's
  storage worker are its consumers.
  Its vectors, `tests/disk_vectors.txt`, come from
  `tests/disk_vectors.py`, which computes them with Python's hashlib and
  hmac alone.
- `fido_p256`: software P-256 (ECDH and ES256 verification), its ECDH
  `SecretScalar::agree` public for td-tpm's session salt; `fido_aes`:
  AES-256-CBC for the PIN protocols, and AES-128 in CFB mode, over the
  same round function, for td-tpm's parameter encryption, pinned to
  FIPS 197 and SP 800-38A; and AES-128 key unwrap (RFC 3394) over the
  inverse cipher, for td-netd's WPA2 group keys, pinned to RFC 3394's
  vectors, bounded to 16 to 1024 bytes and zeroing its output when the
  integrity check fails.
- `fido_device`: root and desktop hidraw admission, the operation lock
  under `/run/td-fido` (or the desktop runtime directory), the Session
  that drives a token through a HID worker, and that worker.
- `hmac_sha256(key, parts)`: HMAC-SHA256 (RFC 2104) under `key` over
  the concatenation of `parts`, pinned to RFC 4231's vectors, and
  `hkdf_expand(ikm, salt, info_parts, okm)`: HKDF-SHA256 (RFC 5869)
  filling up to `HKDF_MAX_OKM`, 8160 bytes.
- `root.rs`: the root console's admission, every user ID 0, which
  `fido_device::require_root` re-exports. td-secret's store compiles
  the same file by path for its own admission, with its refusal text.

The crate owns protocol and transport, never policy or persistence: no
store, record, release or authorization lives here. Two seams keep it
so:

- `fido_ctap::Es256Verifier` verifies an ES256 signature over a digest
  under a public-only key. `AssertionRequest::verify` and
  `EnrollmentProof::verify` take one; td-secret's is its TPM
  (`Client::verify_es256`, td-secret/DESIGN.md).
- `fido_device::Program` names the program a Session starts as its HID
  worker and the two verbs that run `worker` and `desktop_worker`. The
  consumer serves the role from its own command line
  (td-secret/DESIGN.md names td-secret's).

## Consumers

td-secret depends on td-fido by path, `td-fido = { path = "../td-fido"
}`, and imports its modules under their old names, so its own
modules, and the files td-firstboot and td-portal share with it, name
them as before. td-tpm depends on it the same way for its salted
sessions (td-tpm/DESIGN.md). Per `td-install/ENCRYPTION.md` item 8,
td-protector (its HMAC-SHA256), td-boot and td-install are the only
other crates that may depend on it directly. Since increment 8b td-boot does, for its
selector's FIDO2 client: `fido_disk` over root-admitted devices, each
session's worker td-boot's own `fido-worker` verb (`Program::ROOT`),
whose desktop verb it never builds or dispatches. Since increment 8e
td-install does too, for its storage worker's disk proof
(td-install/DESIGN.md, "Storage operation worker"): `fido_disk::authorize`
the same way, each session's worker td-install's own `fido-worker` verb;
any other
dependent is an amendment here and to `FIDO_DEPENDENTS` in
`builder/src/affected.rs`, whose lock preflight refuses the rest. Crates
that reach it only through td-secret or td-tpm, as td-pass does, are
not dependents.

Five crates compile some of its files as shared source instead:
td-protector compiles `hmac.rs` for its PIN's authValue and its lockout
token's HKDF (below); td-netd compiles `fido_aes.rs` for the key unwrap,
its recipe staging the file beside its own sources; as
they compiled td-secret's, td-firstboot and td-portal read td-secret's
token-protected store through `fido_cbor`, `fido_ctap`, `fido_enroll`,
`fido_hid`, and `hmac.rs` and `root.rs` through td-secret's store
crypto and store (below), and td-crypto's tests compile `fido_p256.rs`
as their test-only ES256 oracle (td-crypto/PORTABLE.md). These files
therefore reach siblings only through `super::` names those crates also
provide.

The td-secret recipe builds td-fido's rlib twice, with the shipped
profile and for the test harness, links td-secret and td-tpm against
them, and runs td-fido's and td-tpm's own tests. The td-boot, td-install
and td-firstboot recipes stage its files and build its rlib before
td-tpm's, td-boot's recipe also passing it to the binary by `--extern` (td-boot's and td-install's staging also serves td-protector's
`hmac.rs`); the td-setup, td-portal and td-pass recipes stage its tree
for cargo.

## Shared HMAC-SHA256

`hmac.rs` is the one SHA-256 digest, HMAC-SHA256 and HKDF (RFC 5869)
the std-only crates share: `hkdf` gives one 32-byte block, and
`hkdf_expand` fills an output of any length up to RFC 5869's 255
blocks, its info the concatenation of parts, refusing a longer output
and leaving it untouched. td-protector's lockout token takes 64 bytes
from it. It names the hash only as
`super::sha256`, so a crate that already mounts the engine's SHA-256
compiles it by path beside that mount instead of keeping a second copy.
td-fido's `crypto.rs` re-exports `hmac_sha256` as `td_fido::hmac_sha256`
and its digest, HMAC and HKDF to the CTAP code. td-secret's store crypto
compiles it the same way, as does td-protector's crate root beside its
own mount of the engine's SHA-256, for its PIN's authValue and its
lockout token, and td-firstboot and td-portal
reach it through td-secret's `crypto.rs`. Its tests pin RFC 4231 cases 1 to 4, 6 and 7
(each whole and split at every offset), keys of 64 and 65 bytes on either
side of the block size, RFC 5869 case 1 for `hkdf`, its cases 1 to 3
at their whole lengths for `hkdf_expand` (the info split at every
offset, one block agreeing with `hkdf`, and the length bound) and FIPS
180's "abc".

It zeroes the key-derived buffers it owns before returning: the
normalized key, the hashed long key, each pad, the inner hash and HKDF's
pseudorandom key and output blocks. The engine's `Sha256` keeps its chaining state and
block buffer private and consumes itself in `finalize`, so the
key-derived midstates inside it are not cleared. Zeroing is best effort
in safe Rust, as td-tpm's `zero` is. td-tpm keeps its own SHA-256 for
now.

## Test support

The virtual authenticator, `fido_virtual.rs`, and the committed CTAP
transcripts, `fido_fixtures.rs` (the vector rows, a scripted channel and
the re-executed worker fixture), are test-only. td-secret's tests drive
its login, vault and guest flows with them, so td-secret compiles both
files by path under `cfg(test)`; they name only public items through
`crate::` paths both crates resolve and carry no tests of their own. The
authenticator signs, which production never does, so it signs through
`crate::p256_signer`: this crate's test build of `fido_p256`, and in
td-secret a test-only copy of `fido_p256.rs` compiled by path. Its own
tests, `fido_virtual_tests.rs`, are included only here, as a child of
the authenticator. Its CTAP 2.0 mode (`Config::one_secret`) keeps one
hmac-secret for assertions with and without UV whatever versions it
claims, so the no-PIN probe's refusal is testable. The vectors and
their optional host generators live under `tests/`.
