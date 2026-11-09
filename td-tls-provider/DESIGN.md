# td-tls-provider: td-primitives behind rustls

This crate is the one implementation of rustls's `CryptoProvider` that td's
TLS consumers share. rustls is the TLS engine; every cryptographic operation
it asks for is td-primitives (`td-primitives/DESIGN.md`). It is a crate of its
own, not a module of td-net, because it is the designated Rustls provider
bridge for td-crypto, whose library assembles its TLS policy from it
(`td-crypto/DESIGN.md`), and AGENTS.md principle 2 puts
code two crates need in one `td-*` crate rather than a copy per consumer. That
placement is the repository user's decision.

## What it provides

- `provider()`: a `CryptoProvider` value, and `shared()`, the same in an
  `Arc`, for building configurations. The crate never installs it as
  rustls's process default; a consumer that wants a default installs it
  itself, as td-net's multicall does before any applet runs.
- `p256_to_pkcs8`: the one P-256 PKCS#8 shape the key provider loads (RFC 5958
  around RFC 5915, with the public point, as ring and OpenSSL write it).
- building blocks a consumer assembles its own provider from, beside
  `provider()`'s suites, groups, entropy and key provider. They do not change
  `provider()`, and they encode no consumer's order or mapping:
  - `verifiers`: individual `&dyn SignatureVerificationAlgorithm` statics, as
    rustls's own `webpki_algs` are. `ECDSA_P256_SHA256` through
    `ECDSA_P521_SHA512` cover P-256, P-384 and P-521 each with SHA-256,
    SHA-384 and SHA-512, and take the key in SEC 1's uncompressed or
    compressed form, through td-primitives' `uncompressed`; the hybrid form
    and the point at infinity are refused. A certificate may hold either
    form, and td-crypto admits compressed private anchor keys
    (`td-crypto/DESIGN.md`, "Trust-store admission", and its
    `certificate_algorithms::canonical_key`).
    `ED25519`, `RSA_PSS_*`, `RSA_PKCS1_*` and `RSA_PKCS1_*_BARE` (PKCS#1
    under its OID without the NULL parameters) are `provider()`'s own
    statics;
  - `tls12_ecdsa`: `provider()`'s three TLS 1.2 ECDHE-ECDSA suites with
    rustls's own providers' server scheme list, which also names
    ECDSA_NISTP521_SHA512. The key provider holds only P-256, so the list
    changes no handshake it serves.

The provider carries, in this order:

- cipher suites: TLS 1.3 ChaCha20-Poly1305, AES-128-GCM and AES-256-GCM,
  then TLS 1.2 ECDHE with ChaCha20-Poly1305, AES-128-GCM and AES-256-GCM,
  ECDSA before RSA at each cipher. ChaCha20-Poly1305 leads on every CPU;
- key-exchange groups: X25519, P-256, P-384;
- signature verification: ECDSA over P-256 and P-384 with SHA-256 and
  SHA-384, Ed25519, RSA-PSS and PKCS#1 v1.5 with SHA-256, SHA-384 and
  SHA-512, PKCS#1 also under its OID without the NULL parameters, which
  certificates in the wild carry. ECDSA takes only the uncompressed form.
  TLS 1.3 holds each scheme to its first listed verifier, so to its own
  curve;
- entropy from td-primitives' `random` (`/dev/random`), which refuses
  rather than falls back;
- a key provider that loads only that P-256 PKCS#8 shape, whose public half
  must be the secret's, and signs only ECDSA-P256-SHA256, so a certificate
  its key does not match is refused when rustls loads the pair.

Records follow rustls's own providers' layouts. A key or IV of the wrong
length makes a cipher that seals and opens nothing, and a record that
authenticates but carries more than 16384 bytes of plaintext is refused.
HKDF and the TLS 1.2 PRF are rustls's, derived from the HMAC here. Traffic
secrets are not extractable.

## What it does not decide

The crate makes no protocol choice rustls would not already refuse. Versions,
roles, trust anchors, names, ALPN, resumption and every other configuration
policy belong to the consumer, which builds its configurations from
`provider()` and the building blocks above, and may narrow or reorder its
suites, groups or verifiers. td-net takes `provider()` whole; td-crypto
selects its policy's suites and groups from it and assembles its own
certificate algorithm order and scheme mapping from `verifiers`. A consumer behind a facade
of its own, as td-crypto's is (`td-crypto/DESIGN.md`), keeps these parts
private and never re-exports them.

## Admission

The crate depends on exactly two things: rustls 0.23.46 with `custom-provider`,
`std` and `tls12` and no built-in backend, and local td-primitives. It is
admitted beside td-crypto and td-mta in `builder/src/crypto_policy.rs`, which
pins its manifest, lock, rustls line and selected graph (td-crypto's rustls
line is the same, and its graph contains this crate's), and its legs run
offline through `td-builder gate-crates crypto-cargo`. ring sits in the lock
through rustls-webpki's weak feature edge and is never selected. No roster
crate outside the admission (td-crypto and, through it, td-mta) may depend
on it. The crate has no `unsafe` and no
build script.

## Tests

Both ends of every in-memory connection in the tests run this provider, so
an error made the same way on both would still connect. The tests therefore
also hold it to answers it did not compute: records written by ring, RFC
7748 and RFC 5903 shared secrets, HKDF, PRF and HMAC values from Python's
`hmac`, and signatures made with big integers by `tests/signature_answers.py`:
one row per verifier, a signature under the small-order Ed25519 identity key,
which verification takes for any message, and `REFUSED_` rows no verifier may
take (ECDSA r or s out of range or not strict DER, RSA moduli outside 2048
to 8192 bits and exponents of 1, even or over 33 bits, Ed25519 S of L or
more). ring verified the signed rows it has the algorithm for, and AWS-LC
gave the same answer for every row when it was td-crypto's backend;
td-crypto's tests hold its policy's verifiers to these rows. The tests pin each
`verifiers` export's name and identifiers, that its Ed25519 and RSA exports
are `provider()`'s statics and its ECDSA ones are not, and that each takes its
rows with the key in either point form and refuses the other y, the hybrid
form and every other row; `provider()`'s ECDSA verifiers refuse the
compressed form.
`tests/fixtures/account-ca` is a local CA and the server identity it issued.
td-net's tests read both: its fetch tests the CA, including `expired.pem`,
which only they use, and its OpenPGP tests a row of
`tests/signature_answers.txt`; td-crypto's tests read the signature rows.
Changing either file therefore runs td-net's tests too, and any change here
runs td-crypto's and td-mta's legs, since td-crypto depends on this crate.
