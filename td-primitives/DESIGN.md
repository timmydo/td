# td-primitives: td's own cryptographic primitives

td writes its own cryptography when that makes the system smaller and easier
to audit than the library it replaces. This crate is where that code lives.
Its library is std only and depends on no crate, and it contains no assembly.
Its one `unsafe` surface, `UNSAFE.md` §26, is calling CPU instructions a runtime
check found.

## Rules

- **A primitive needs a named consumer.** Nothing is here for completeness;
  an algorithm goes when its last consumer does.
- **No choice inside.** No runtime algorithm negotiation, modes or parameters.
  Callers name the exact algorithm. Which instructions run it is not theirs to
  choose: the CPU decides, the bytes are the same, and tests hold them so.
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

## CPU instructions

| Module | Replaces, when the CPU has it | Instructions | Asked |
|---|---|---|---|
| `aes_ni` | Bitsliced AES and integer GHASH | AES-NI and PCLMULQDQ, with SSSE3 and SSE4.1 | Once per key |
| `chacha_avx2` | One ChaCha20 block at a time | AVX2 | Once per message |
| `sha_ni` | SHA-256's compression | The SHA extensions, with SSSE3 and SSE4.1 | Once per block |

`accel.rs` holds all of the crate's `unsafe`. Key expansion stays software on every
path, so AES has one schedule to check, and the hardware path loads the result.
Each software path stays, because a CPU or hypervisor may not offer the
instructions, and neither it nor its corpora may lapse. VAES, wide PCLMULQDQ and
AVX-512 are absent: they are more code for a speed well past the links td uses.
SHA-1, SHA-384, SHA-512, Poly1305 and the public-key algorithms are plain Rust on
every CPU. `-C target-cpu` is not used, so a binary still runs on any x86-64.

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

- Which backend runs depends on the CPU and nothing secret, and each of the
  statements below holds on whichever does.
- With its instructions, AES is `AESENC` and `AESENCLAST`, and GHASH is
  `PCLMULQDQ` over powers of H, with no table indexed by a secret. This assumes
  those instructions' timing does not depend on their data, which is what they
  were introduced for and which td has not measured.
- Without them AES is bitsliced over four blocks, with no S-box table.
- Without them GHASH multiplies with integer multiplication, with no table of
  multiples of H. Poly1305 always takes 64-by-64 products. Both assume those
  instructions are constant time, as they are on the x86-64 CPUs td targets.
- ChaCha20 is additions, XORs and rotations, by register or by word.
- SHA-1 and SHA-2 are fixed sequences for a given input length, by the SHA
  extensions or not.
- X25519 is a Montgomery ladder with masked swaps, in 5x51-bit limbs.
- Ed25519 signing is a fixed double-and-add-always over complete addition,
  selecting by mask. Its nonce is derived as RFC 8032 says.
- P-256 and P-384 secret operations take fixed four-bit windows over the
  complete formulas of Renes, Costello and Batina (eprint 2015/1060, a = -3, one
  formula for add and double), reading every table entry and selecting by mask,
  with Fermat inversion and word-by-word Montgomery multiplication whose final
  subtraction is masked. The table holds multiples of a public point. ECDSA
  nonces are RFC 6979's, so a bad random source cannot leak the key.
- Verification, of every kind, and all of RSA take only public inputs and
  may branch on them. ECDSA verification and secret scalars share one windowed
  routine rather than have a faster public one beside it. `bignum` is shared, so RSA
  pays for the masks it does not need rather than keeping a second multiplier.

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
  of the key; the round keys live in one box, cleared on drop, so moving a
  software cipher copies only H and a hardware one only a pointer. The hardware
  paths are the same bargain made more often: each call copies round keys, H and
  its powers, ChaCha20's key, or SHA-256's state and block into registers, and
  where those run out onto the stack, and leaves them there.

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

Measured on a Ryzen 7 5700G at `opt-level = 3`, sealing 16 KiB records, with
the instructions and without. td-net builds at that level. The last column is `ring`
on the same machine, measured before td-net left it:

| Primitive | MB/s | Software path, MB/s | `ring`, MB/s |
|---|---|---|---|
| AES-128-GCM | 4650 | 138 | 5522 |
| AES-256-GCM | 4000 | 114 | |
| ChaCha20-Poly1305 | 1820 | 500 | 2435 |
| SHA-256 | 1900 | 280 | |
| SHA-512 | 445 | same | |

| Operation | µs |
|---|---|
| X25519 | 49 |
| P-256 ECDH | 191 |
| P-256 sign | 208 |
| P-256 verify | 249 |
| P-384 ECDH | 500 |
| Ed25519 sign | 91 |
| Ed25519 verify | 164 |
| RSA-2048-size PSS verify | 37 |

td-net offers ChaCha20-Poly1305 first, on every CPU: AES is faster where the
instructions are, but both are ahead of a network link, and one order is what
its tests freeze.
A crate compiled as a dependency takes the consuming build's `opt-level`, so a
consumer that moves bulk data must not build at `opt-level = "s"`.

## Provenance

`aes_gcm` translates BearSSL's `aes_ct64` and `ghash_ctmul64`, with its S-box
being Boyar and Peralta's circuit (eprint 2009/191). One difference is
deliberate: round keys are stored fully expanded rather than compressed, which
costs memory and drops a routine. Poly1305's limbs and finish follow the 64-bit
path of OpenSSL's `poly1305.c`. `THIRD-PARTY.md` names both. `aes_ni`'s GHASH
follows Gueron and Kounavis's carry-less multiplication paper for Intel, with its
reduction and bit reflection, and folds eight blocks before each reduction as their
aggregated form does. The rest is written from the specifications named in the
inventory and the vendor's instruction documentation.

## Tests and how to regenerate them

| Corpus | Source | Regenerate |
|---|---|---|
| `tests/wycheproof/` | Wycheproof at the commit `THIRD-PARTY.md` names | `python3 -I wycheproof_import.py DIR`, from `tests/`, over the downloaded JSON |
| `tests/digest_vectors.txt` | Python's `hashlib` and `hmac` | `python3 -I digest_vectors.py > digest_vectors.txt`, from `tests/` |
| `tests/aead_long.txt` | A naive reference in `aead_long.py`, written for this purpose and sharing no code with the crate: table AES, bit-at-a-time GHASH, big-integer Poly1305. Its rows were also checked against OpenSSL in review. Wycheproof's longest message is 513 bytes, and these reach TLS records and pass 2^16 counter blocks | `python3 -I aead_long.py > aead_long.txt`, from `tests/` |
| `tests/edge_vectors.txt` | Python big integers in `edge_vectors.py`: P-256 and P-384 points with a coordinate that fits again after adding p, a P-256 signature over SHA-384, a 2049-bit RSA key for PSS's short encoded message and an 8192-bit key at the top of the range, signatures made here | `python3 -I edge_vectors.py > edge_vectors.txt`, from `tests/` |
| Inline | FIPS 180, FIPS 197, RFC 4231, RFC 5903, RFC 6979, RFC 7748, RFC 8032, RFC 8439 including A.3's reduction edges, and a few values from Python big integers, each marked where it stands | none |

`tests/vectors.rs` goes through the public API, so on a given CPU it reaches
only one backend. Unit tests beside each backend therefore run the corpora on
every one this machine has, the software path always: the AES-GCM and
ChaCha20-Poly1305 Wycheproof files and `aead_long.txt`, and `digest_vectors.txt`'s
SHA-256 rows. AES-GCM's backends also agree on a list of lengths around their
batch sizes, with each of those below 300 as AAD, open each other's records, and
continue GHASH across a split at each of those up to 576. Both ciphers' counters
are checked against one block at a time through a 32-bit wrap inside a batch.
Scalar multiplication is checked against one bit at a time. Each `detect` is
compared with the kernel's `/proc/cpuinfo`, so one that wrongly says no shows up
rather than quietly leaving a backend untested. On a CPU without the
instructions, those tests have only the software path to run. A scratch copy with
`target_arch` renamed compiled only the software paths and passed the crate's suite
when this landed; that run is a record in the landing commit, as nothing gates it.

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
RFC 8439's AEAD vector, a carry that reaches its top limb only through a full
high limb, which survived those and `aead_long.txt`, and counters and length fields truncated to 16 bits or
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
