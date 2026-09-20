# Manual token diagnostic: generated-code inspection

This is the AES/P-256 consumer acceptance record required by `PORTABLE.md`.
It covers the source-built `td-secret check-portable-token` diagnostic,
not a host Cargo build, another compiler/target, or a future vault consumer.
The inspection found no secret-dependent branch or address in the admitted
AES and P-256 secret arithmetic paths described below. This is a bounded
manual code inspection, not a constant-time proof, timing certification,
physical side-channel analysis or evidence of YubiKey interoperability.

## Artifact and reproduction

The recipe is `recipes/src/recipes/td-secret.rs`, built with
`target/release/td-recipe-eval build-run td-secret`. Its target compiler is
the source-built Rust 1.96.0 at
`/td/store/r9xqzb6pzpyv55h5sg8lbrm3wa4hpmqp-rust-toolchain-1.96.0`.
The inspected output is
`/td/store/bajphw0vdxzac9c76bsrpz2kyyjygb90-td-secret-0.1`:

| File | SHA-256 |
| --- | --- |
| `bin/td-secret` | `cbe4a4c3080ae4df0344ce6ede09dd5c53c3f04a501b1d592b46ce4bbcfe02a8` |
| `lib/debug/bin/td-secret.debug` | `cffeb99fc1c89386240fd973b52cc24c2c4d4edd8433781bfc78d5fda296f38a` |

The ciphertext-publication prerequisite has no production caller yet. Its
rebuilt diagnostic has byte-identical `.text`, `.rodata`, `.data.rel.ro`,
`.got` and `.rela.plt` sections to the previously inspected
`/td/store/i5g7hkscmlxzq5703c54rw78wk22slc3-td-secret-0.1` artifact.
The arithmetic, outlined callees and addresses below are unchanged; file
hashes change with debug information and the resulting build ID. This
comparison does not certify a future storage or notebook consumer.

On this host the same output bytes are available beneath
`~/.td/build-daemon/ladder-shared-v1/build-cache/store/`; `/td/store` is
the canonical path inside the build sandbox.

The production invocation uses edition 2021, `-C opt-level=s`, target
`x86_64-unknown-linux-gnu`, `-C target-feature=+crt-static`,
`-C relocation-model=static`, `-C panic=abort`,
`-Cforce-frame-pointers=yes`, `-Cdebuginfo=line-tables-only`, `-Cstrip=none`,
SHA-1 linker build IDs and the recipe's `/td-build-root` and `/td-build`
path remappings. It links with the declared source-built GCC 14.3.0,
binutils 2.44 and glibc 2.41; no new compiler option or dependency was added.
The exact store paths and link arguments are emitted by `build-run`.

Use the debug companion's `nm -S -C` symbols to identify code ranges, and
`objdump -d -Mintel --start-address=... --stop-address=...` on the runtime
ELF to read the actual executable instructions. Analysis tools run on the
host and are not recipe inputs. The core span is `[0x424cd4, 0x4276bd)`;
follow the outlined AES key-substitution helper at `[0x432f44, 0x432f95)`
as well. The table below uses half-open address ranges for this binary.
Check artifact hashes before using these addresses.

## Inspected paths

| Path | Runtime range | Observation |
| --- | --- | --- |
| AES row/substitution/column helpers | `424cd4–424efe` | Fixed offsets and public byte/column iteration; S-box calls arithmetic inversion. |
| AES key expansion | `424efe–425109` | Round-index branches, fixed word positions, 240-byte schedule. |
| AES CBC decrypt/encrypt | `425109–425422` | Public length admission and block iteration; fixed rounds and key-byte positions. |
| AES inversion | `425422–425685` | Eight-step multiply loops with masks/shifts/XOR; no lookup indexed by a secret. |
| Outlined AES word substitution | `432f44–432f95` | Four bytes, arithmetic inversion and affine rotations/XOR; only fixed loop exit. |
| P-256 Montgomery product/add-product | `425685–4257dc` | Four limbs; native multiply/add-with-carry, fixed carry propagation. |
| Scalar admission/public key/ECDH | `4257dc–425aab` | Scalar validity and allocation/result branches outside secret arithmetic. |
| Residue decode/encode/subtract/double | `425aab–425e54` | Fixed limb loops and masks; decode errors concern public canonical inputs. |
| Point addition | `425e54–426395` | Computes exceptional cases and selects with masks; no secret-directed branch. |
| Affine conversion | `426395–426606` | Infinity admission then fixed 256-bit exponentiation; masked selection. |
| Point selection | `426606–4266fa` | Reads both candidates, combines with AND/OR for each of three four-limb coordinates. |
| Point doubling | `4266fa–426b0c` | Fixed arithmetic and mask selection for infinity. |
| Infinity/generator construction | `426b0c–426b62`, `426cd0–426d3c` | Fixed constants and offsets. |
| Scalar multiplication | `426b62–426cd0` | 32 bytes × 8 bits; always doubles/adds; `bt`/`sbb` produces the selection mask. |
| Modular reduction | `426d3c–426dd3` | Four-limb subtract, borrow mask and both-candidate reads. |
| Public key admission/signature verification | `426dd3–4276bd` | Public validity branches; shared arithmetic above and fixed exponentiation. |

The AES iterator-size helper at `49d2fc–49d30d` only subtracts slice
pointers and shifts by two. Other calls leaving these arithmetic ranges
serve allocation, deallocation or buffer clearing, with public sizes;
allocation failures are outside the arithmetic claim. The AES schedule's
240-byte clear and selected P-256 work-buffer clears remain in this binary.
This does not extend the best-effort erasure claim in `PORTABLE.md` to
temporary register/stack copies or every intermediate field element.

P-256 affine conversion branches on infinity. Admitted nonzero ECDH scalars
and admitted nonidentity prime-order points cannot produce infinity;
signature verification uses public inputs and may legitimately reject it.
Scalar sampling/admission may reject zero or out-of-range random candidates.
No claim hides those outcomes, public lengths, allocation failures, or
protocol/PIN admission. CPU instruction latency and microarchitectural
behavior are not established by an instruction-list review.

## Acceptance boundary

The diagnostic's repeat-secret comparison was also inspected: it reads and
XOR/OR-folds all 32 bytes before branching on the equality result. Public
length admission and the final success/failure branch are outside that loop.

The source-built tests passed 249 cases with 30 existing VM/device cases
and one explicitly invoked subprocess helper ignored. Independent AES/P-256/PIN vectors and full diagnostic transcripts
test mathematical and protocol results separately from this inspection.
No physical key was used to produce this record.

Repeat inspection when crypto code, consumer integration, compiler, target,
optimization/link settings, or emitted arithmetic changes. A changed binary
hash requires accounting for the change and checking the generated code;
this document is not an automatic allowlist or a runtime attestation check.
The commit review record owns reviewer identities and dispositions. Guix
user device admission, hardware model/firmware evidence, independent backup
proofs and a durable vault lifecycle remain separate deliverables.
