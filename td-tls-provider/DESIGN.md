# td-tls-provider: td-primitives behind rustls

This crate is the one implementation of rustls's `CryptoProvider` that td's
TLS consumers share. rustls is the TLS engine; every cryptographic operation
it asks for is td-primitives (`td-primitives/DESIGN.md`). It is a crate of its
own, not a module of td-net, because it is the designated Rustls provider
bridge for td-crypto, which does not use it yet (`td-crypto/DESIGN.md`), and
AGENTS.md principle 2 puts code two crates
need in one `td-*` crate rather than a copy per consumer. That placement is
the repository user's decision.

## What it provides

- `provider()`: a `CryptoProvider` value, and `shared()`, the same in an
  `Arc`, for building configurations. The crate never installs it as
  rustls's process default; a consumer that wants a default installs it
  itself, as td-net's multicall does before any applet runs.
- `p256_to_pkcs8`: the one P-256 PKCS#8 shape the key provider loads (RFC 5958
  around RFC 5915, with the public point, as ring and OpenSSL write it).

The provider carries, in this order:

- cipher suites: TLS 1.3 ChaCha20-Poly1305, AES-128-GCM and AES-256-GCM,
  then TLS 1.2 ECDHE with ChaCha20-Poly1305, AES-128-GCM and AES-256-GCM,
  ECDSA before RSA at each cipher. ChaCha20-Poly1305 leads on every CPU;
- key-exchange groups: X25519, P-256, P-384;
- signature verification: ECDSA over P-256 and P-384 with SHA-256 and
  SHA-384, Ed25519, RSA-PSS and PKCS#1 v1.5 with SHA-256, SHA-384 and
  SHA-512, PKCS#1 also under its OID without the NULL parameters, which
  certificates in the wild carry. TLS 1.3 holds each scheme to its first
  listed verifier, so to its own curve;
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
`provider()` and may narrow its suites, groups or verifiers. td-net takes it
whole. A consumer behind a facade of its own, as td-crypto's is
(`td-crypto/DESIGN.md`), keeps these parts private and never re-exports them.

## Admission

The crate depends on exactly two things: rustls 0.23.46 with `custom-provider`,
`std` and `tls12` and no built-in backend, and local td-primitives. It is
admitted beside td-crypto and td-mta in `builder/src/crypto_policy.rs`, which
pins its manifest, lock, rustls line and selected graph, and its legs run
offline through `td-builder gate-crates crypto-cargo`. ring sits in the lock
through rustls-webpki's weak feature edge and is never selected. No roster
crate outside the admission may depend on it. The crate has no `unsafe` and no
build script.

## Tests

Both ends of every in-memory connection in the tests run this provider, so
an error made the same way on both would still connect. The tests therefore
also hold it to answers it did not compute: records written by ring, RFC
7748 and RFC 5903 shared secrets, HKDF, PRF and HMAC values from Python's
`hmac`, and signatures made with big integers by `tests/signature_answers.py`,
which ring verified. `tests/fixtures/account-ca` is a local CA and the server
identity it issued. td-net's tests read both: its fetch tests the CA, including
`expired.pem`, which only they use, and its OpenPGP tests a row of
`tests/signature_answers.txt`. Changing either file therefore runs td-net's
tests too.
