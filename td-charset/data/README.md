# WHATWG mapping data

The upstream files are data from the WHATWG Encoding Standard, revision
`be17ac5c49881336538acfc7b40dba3c8009144b`:

- [encodings.json](https://raw.githubusercontent.com/whatwg/encoding/be17ac5c49881336538acfc7b40dba3c8009144b/encodings.json),
  8913 bytes, SHA-256
  `078212b3697f60b81225b6671bd9da2604497abff0aa8e96e4d46605c02ac9e7`.
- [indexes.json](https://raw.githubusercontent.com/whatwg/encoding/be17ac5c49881336538acfc7b40dba3c8009144b/indexes.json),
  530213 bytes, SHA-256
  `b7c8961095f0b4cae8f4a16a5011e7c84047a60c1277e28ee8df9e56074ee509`.

The hashes were computed from these upstream downloads and cross-checked
against the copies served at `https://encoding.spec.whatwg.org/`. No external
code is imported. The complete upstream license is in `LICENSE-WHATWG`;
source incorporations use its BSD 3-Clause terms. The JSON documents are
cold generator/test inputs, not runtime tables or network build inputs.

`*.bin` holds little-endian u32 scalar values, with `0xffffffff` for an
unassigned index entry. `gb18030-ranges.bin` holds alternating u32 pointers
and scalar offsets. All 27 single-byte indexes, JIS0208, EUC-KR, GB18030 and
GB18030 ranges are retained; unused multibyte indexes in the source JSON are
not generated. ISO-8859-8-I shares ISO-8859-8's mapping while retaining its
own label identity. `src/labels.rs` derives from the same pinned labels.

From the repository root, regenerate entirely offline:

```text
cargo run --offline --manifest-path td-charset/Cargo.toml --example generate
cargo fmt --manifest-path td-charset/Cargo.toml
cargo test --offline --manifest-path td-charset/Cargo.toml
```

The generator authenticates both complete inputs before writing anything.
The reproduction test compares every binary byte and the generated Rust
without formatting whitespace. Normal builds do not run the generator.
Updating these data requires reviewing the upstream diff, moving the two
length/hash pins in `tools/generate.rs` and this document, regenerating,
and running the conformance tests in the same commit.
