# td-fido: the shared FIDO2 client

td-fido is td's FIDO2 (CTAP2) client and the home of the HMAC-SHA256
td's std-only crates share. AGENTS.md principle 2 puts code two crates
need in one sibling crate: td-secret's portable vault, store enrollment
and login keys use a security key today, and `td-install/ENCRYPTION.md`
increment 8 has td-boot's selector unlock a disk with one and td-tpm
salt its sessions with this crate's P-256 and AES. Increment 8a moved
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
  owned PIN transaction over a `Channel`.
- `fido_p256`: software P-256 (ECDH and ES256 verification);
  `fido_aes`: AES-256-CBC for the PIN protocols.
- `fido_device`: root and desktop hidraw admission, the operation lock
  under `/run/td-fido` (or the desktop runtime directory), the Session
  that drives a token through a HID worker, and that worker.
- `hmac_sha256(key, parts)`: HMAC-SHA256 (RFC 2104) under `key` over
  the concatenation of `parts`, pinned to RFC 4231's vectors.
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
them as before. Per `td-install/ENCRYPTION.md` item 8, td-tpm (8a's
salted sessions), td-protector (its HMAC-SHA256) and td-boot (8b's
FIDO2 boot client) are the only other crates that may depend on it
directly; any other dependent is an amendment here and to
`FIDO_DEPENDENTS` in `builder/src/affected.rs`, whose lock preflight
refuses the rest. Crates that reach it through td-secret, as td-pass
does, are not dependents.

Four crates compile some of its files as shared source instead:
td-protector compiles `hmac.rs` for its PIN's authValue (below); as
they compiled td-secret's, td-firstboot and td-portal read td-secret's
token-protected store through `fido_cbor`, `fido_ctap`, `fido_enroll`,
`fido_hid`, and `hmac.rs` and `root.rs` through td-secret's store
crypto and store (below), and td-crypto's tests compile `fido_p256.rs`
as their test-only ES256 oracle (td-crypto/PORTABLE.md). These files
therefore reach siblings only through `super::` names those crates also
provide.

The td-secret recipe builds td-fido's rlib twice, with the shipped
profile and for the test harness, links td-secret against them, and
runs td-fido's own tests; the td-firstboot, td-portal and td-pass
recipes stage its tree or the files they compile, as the td-setup
recipe stages its tree and the td-boot and td-install recipes
`hmac.rs` for td-protector.

## Shared HMAC-SHA256

`hmac.rs` is the one SHA-256 digest, HMAC-SHA256 and one-block HKDF
(RFC 5869) the std-only crates share. It names the hash only as
`super::sha256`, so a crate that already mounts the engine's SHA-256
compiles it by path beside that mount instead of keeping a second copy.
td-fido's `crypto.rs` re-exports `hmac_sha256` as `td_fido::hmac_sha256`
and its digest, HMAC and HKDF to the CTAP code. td-secret's store crypto
compiles it the same way, as does td-protector's `pin` module beside
its own mount of the engine's SHA-256, and td-firstboot and td-portal
reach it through td-secret's `crypto.rs`. Its tests pin RFC 4231 cases 1 to 4, 6 and 7
(each whole and split at every offset), keys of 64 and 65 bytes on either
side of the block size, RFC 5869 case 1 and FIPS 180's "abc".

It zeroes the key-derived buffers it owns before returning: the
normalized key, the hashed long key, each pad, the inner hash and HKDF's
pseudorandom key. The engine's `Sha256` keeps its chaining state and
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
the authenticator. The vectors and their optional host generators live
under `tests/`.
