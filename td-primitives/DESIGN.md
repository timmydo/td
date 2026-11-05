# td-primitives: td's own cryptographic primitives

td writes its own cryptography when that makes the system smaller and easier
to audit than the library it replaces. This crate is where that code lives.
It is std only, depends on no crate, forbids `unsafe`, and contains no
assembly, so it adds nothing to `UNSAFE.md`.

## Rules

- **A primitive needs a named consumer.** Nothing is here for completeness;
  an algorithm goes when its last consumer does.
- **No choice inside.** No runtime algorithm negotiation, modes, parameters
  or backends. Callers name the exact algorithm.
- **No protocol.** Records, handshakes, certificate paths, key files and trust
  decisions belong to the caller. The crate only says whether a check held.
- **Refusals carry no reason.** `Refused` is one value, so it is not an oracle.
- **Nothing is released before it is authenticated.** `open` compares the tag
  before it decrypts and leaves a refused input as ciphertext.
- **A primitive needs a known answer it did not write,** from a source other
  than this crate (below), and each corpus states its row count, so a truncated
  file reds rather than passing on less.

## Inventory

| Module | Algorithms | Consumer need |
|---|---|---|
| `sha` | SHA-256, SHA-384, SHA-512 (FIPS 180-4) | TLS transcripts and signatures; forkable because TLS forks its transcript |
| `sha` | SHA-1, as one function and not a `Digest` | The OpenPGP v4 fingerprint, which that format defines by it. No HMAC or signature can be built on it |
| `hmac` | HMAC (RFC 2104) | TLS key schedules. HKDF and the TLS 1.2 PRF are not here: rustls derives both from it |
| `chacha20_poly1305` | ChaCha20-Poly1305 (RFC 8439) | TLS records. Poly1305 is reachable only through the AEAD |
| `aes_gcm` | AES-128-GCM and AES-256-GCM (SP 800-38D), 96-bit nonce, 128-bit tag | TLS records, where a server may offer nothing else |
| `curve25519` | X25519 (RFC 7748); Ed25519 (RFC 8032) | TLS key exchange; TLS and certificate signatures; td-subst's signing keys |
| `ec` | P-256: ECDH, ECDSA signing (RFC 6979 nonces), verification. P-384: ECDH and verification | TLS key exchange and signatures; td-test-https's identities. A TLS 1.2 server holding a P-384 certificate refuses a client that does not list that group, whichever it then negotiates |
| `rsa` | RSASSA-PKCS1-v1_5 and RSASSA-PSS verification, 2048 to 8192 bits. PSS salt the length of the hash | TLS and certificate signatures; OpenPGP v4 signatures on Flathub commits |
| `random` | The system's random bytes, from `/dev/random` | Keys, nonces and TLS randoms |
| `ct` | Equality reading every byte; best-effort erasure | Tag and secret comparison |

AES-192, other GCM nonce and tag sizes, a standalone Poly1305, a standalone
AES block API, RSA signing and decryption, compressed points, P-384 signing and
any other curve, P-521 included, are absent on purpose. Private
`bignum` and `der` serve the modules above and are not an API. td-net is the consumer these exist
for, in place of `ring`: `net/src/tls_crypto.rs` puts them behind rustls's
`CryptoProvider`, whose suites are ChaCha20-Poly1305 and AES-GCM in TLS 1.3 and
with ECDHE in TLS 1.2, over X25519, P-256 and P-384. Key-file formats, as td-subst's
and td-test-https's PKCS#8, belong to td-net too.

## What is claimed about side channels

The source claim is narrow and exact: **in the code above, no branch and no
memory address depends on a secret**, only on public lengths, on the result of
a final authentication check and on whether a candidate scalar or nonce is in
range, where a rejected value is thrown away and tells nothing of the kept one.
Two more refusals branch on a computed value: an all-zero X25519 result, which only
a low-order peer brings about and which that peer already knows, and a point at
infinity on leaving projective form, which valid inputs never reach.
An HMAC key's length counts as public, so a
caller must not key HMAC with a value whose length is secret, such as a TLS 1.2
finite-field premaster with its zeros stripped; td negotiates no such group.

- AES is bitsliced over four blocks, with no S-box table.
- GHASH multiplies with integer multiplication, with no table of multiples of
  H, and Poly1305 takes 64-by-64 products. Both assume those instructions are
  constant time, as they are on the x86-64 CPUs td targets.
- ChaCha20 is additions, XORs and rotations.
- SHA-1 and SHA-2 are fixed sequences for a given input length.
- X25519 is a Montgomery ladder with masked swaps, in 5x51-bit limbs.
- Ed25519 signing is a fixed double-and-add-always over complete addition,
  selecting by mask. Its nonce is derived as RFC 8032 says.
- P-256 and P-384 secret operations are double-and-add-always over the complete
  formulas of Renes, Costello and Batina (eprint 2015/1060, a = -3, one formula
  for add and double), selecting by mask, with Fermat inversion and
  word-by-word Montgomery multiplication whose final subtraction is masked.
  ECDSA nonces are RFC 6979's, so a bad random source cannot leak the key.
- Verification, of every kind, and all of RSA take only public inputs and
  may branch on them. `bignum` is shared, so RSA pays for the masks it does
  not need rather than keeping a second multiplier.

What is **not** claimed:

- That the compiled binary keeps those properties. Rust gives no constant-time
  guarantee. `std::hint::black_box` around comparison results and masks
  discourages the optimizer and promises nothing. `td-secret/CRYPTO-AUDIT.md`
  shows the instruction-level inspection such a claim needs, and none has been
  done for this crate yet.
- Microarchitectural or physical resistance: power, EM, speculation, or
  operand-dependent instruction latency.
- Erasure. `ct::zero` and the `Drop` impls clear what they own, best effort.
  Registers, stack copies and moved values are not cleared, as `td-fido`'s
  `hmac.rs` already records for its own buffers. `Hmac` has no `Drop`: its two
  hash states are key-equivalent and stay in memory until overwritten, and a
  long key's tail stays in the temporary hash that shortened it. Key expansion
  uses two buffers that never grow and clears both, so it leaves no freed copy
  of the key; the round keys live in one box, cleared on drop, so moving a cipher
  copies only H.

## Decisions on what to accept

Where a specification leaves room, the crate takes the stricter reading, except
where that would refuse what real peers send. Wycheproof marks such rows
"acceptable", and `tests/vectors.rs` pins how many of each are taken or refused, so
a change to any of these reds:

- X25519 refuses an all-zero result (31 low-order keys), as RFC 8446 requires of
  TLS, and takes the rest, twist and non-canonical keys included, as RFC 7748 says.
- ECDH and ECDSA keys are uncompressed points with coordinates below p, on the
  curve; the curves have prime order, so that is the whole check. A compressed
  point is refused (one Wycheproof row on each ECDH curve).
- Ed25519 refuses S of L or more, a non-canonical key and a key of x = 0 with its
  sign bit set. It is cofactorless and takes small-order and mixed-order keys: a
  key is trusted before it is used here, and a trusted key is neither.
- ECDSA takes strict DER only, with r and s in 1 to n - 1, and a digest of at
  least 32 bytes: at e = 0 anyone can forge for any key.
- PKCS#1 builds the one encoding the hash allows and compares it, which is what
  defeats the 2006 forgery; so it refuses a DigestInfo missing its NULL.
- PSS takes only a salt the length of the hash, as TLS requires.
- RSA takes odd exponents from 3 to 33 bits and moduli from 2048 to 8192 bits.

`random` reads `/dev/random`, which since Linux 5.6 blocks only until the kernel's
generator is ready and never after. It exists in td-jail's tree and the builder
sandbox, and a read that fails refuses rather than falling back.

## Speed

Measured on a Ryzen 7 5700G at `opt-level = 3`, sealing 16 KiB records. td-net
builds at that level, so a transfer with a server that picks AES runs at about
130 MB/s rather than `ring`'s several GB/s, ahead of most network links:

| Primitive | MB/s |
|---|---|
| ChaCha20-Poly1305 | 415 |
| AES-128-GCM | 130 |
| AES-256-GCM | 109 |
| SHA-512 | 324 |
| SHA-256 | 211 |

AES-GCM is slow because it uses no AES-NI or PCLMULQDQ: reaching either needs
`unsafe` and an `UNSAFE.md` amendment. For the same reason `ring`, which has
them, measured about 5500 MB/s for AES-128-GCM on the same machine.
A crate compiled as a dependency takes the consuming build's `opt-level`, so a
consumer that moves bulk data must not build at `opt-level = "s"`.

## Provenance

`aes_gcm` translates BearSSL's `aes_ct64` and `ghash_ctmul64`, with its S-box
being Boyar and Peralta's circuit (eprint 2009/191). One difference is
deliberate: round keys are stored fully expanded rather than compressed, which
costs memory and drops a routine. Poly1305's limbs and finish follow
poly1305-donna-64. `THIRD-PARTY.md` names both. The rest is written from the
specifications named in the inventory.

## Tests and how to regenerate them

| Corpus | Source | Regenerate |
|---|---|---|
| `tests/wycheproof/` | Wycheproof at the commit `THIRD-PARTY.md` names | `python3 -I wycheproof_import.py DIR`, from `tests/`, over the downloaded JSON |
| `tests/digest_vectors.txt` | Python's `hashlib` and `hmac` | `python3 -I digest_vectors.py > digest_vectors.txt`, from `tests/` |
| `tests/aead_long.txt` | A naive reference in `aead_long.py`, written for this purpose and sharing no code with the crate: table AES, bit-at-a-time GHASH, big-integer Poly1305. Its rows were also checked against OpenSSL in review. Wycheproof's longest message is 513 bytes, and these reach TLS records and pass 2^16 counter blocks | `python3 -I aead_long.py > aead_long.txt`, from `tests/` |
| `tests/edge_vectors.txt` | Python big integers in `edge_vectors.py`: P-256 and P-384 points with a coordinate that fits again after adding p, a P-256 signature over SHA-384, a 2049-bit RSA key for PSS's short encoded message and an 8192-bit key at the top of the range, signatures made here | `python3 -I edge_vectors.py > edge_vectors.txt`, from `tests/` |
| Inline | FIPS 180, FIPS 197, RFC 4231, RFC 5903, RFC 6979, RFC 7748, RFC 8032, RFC 8439 including A.3's reduction edges, and a few values from Python big integers, each marked where it stands | none |

The importer drops what the API cannot express and prints the counts: of
Wycheproof's AES-GCM rows 133 are kept and 183 dropped (other nonce, tag and key
sizes), and of ChaCha20-Poly1305's 316 and 9. Wycheproof has no P-256 with
SHA-384 file, so `edge_vectors.txt` holds a signature for that pairing, and the
P-256 with SHA-512 rows pin the same truncation. The S-box is also checked against FIPS 197's algebraic
definition, for all 256 values in every lane.

A passing corpus is only evidence if it can fail. A change to a primitive runs
a mutation of the changed line against the suite and records in its commit
message that the suite went red, or why that mutant is equivalent. Erasure is
the exception, since safe Rust cannot observe it. Gaps found this way include
Poly1305's final subtraction and top-limb carry, which survived Wycheproof and
RFC 8439's AEAD vector, and counters and length fields truncated to 16 bits or
fewer, which survived every corpus until `aead_long.txt`. Wider counters, and
AES-GCM bit lengths cut at 24 bits or more, still survive it: those lengths need
more data than a suite should hold and more than a TLS record can carry.

Before td-net left `ring`, this crate was run against it: every Wycheproof
signature row, and random inputs through every algorithm, with signatures
cross-verified and bytes compared where the algorithm is deterministic. There
was no disagreement in 585,540 comparisons. `ring` is not a dependency, so
that run is a record in the landing commit, not a test to repeat.

## Relation to other crates

- `td-crypto` is a separate private Rustls/AWS-LC closure for mail serving. It
  does not use this crate today; its design names this crate as where owned
  primitives would go.
- `td-fido` keeps its own AES, P-256 and HMAC-SHA256 for the disk and login
  path, `td-netd` its SHA-1, and the engine its SHA-256. Moving them here is
  possible now that this crate exists, and is a separate change because each
  sits on the boot or login path.
