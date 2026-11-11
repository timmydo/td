# td-inflate

One std-only, safe Rust inflater for gzip, zlib and raw DEFLATE. The
library exposes bounded byte decoders and the existing bounded gzip file
reader. `decompress_bytes_with_limit` decodes gzip, including concatenated
members; `decompress_zlib_with_limit` decodes HTTP deflate (RFC 1950 zlib
framing, not the nonstandard raw-DEFLATE variant). Both verify framing and
checksums, refuse trailing data and stop before exceeding the supplied
output limit. Gzip checks each member's CRC, size and optional header CRC.
Zlib checks its method, window declaration, header check and Adler-32;
preset dictionaries are unsupported. Raw decoding reports its consumed
prefix or requires an exact size and complete input.

`decompress_raw_prefix_checked` uses allocation-free `RawError` values.
All raw-decoder buffers reserve fallibly before growth; fixed alphabets use
stack arrays. The older String-returning entry points adapt these errors
for existing callers and are outside the allocation-free error guarantee.

Input is bounded at 257 MiB, gzip members at 4096 and DEFLATE blocks at
2^20 per decode, shared across gzip members. Huffman lookup is bounded by
15 code bits. Output grows only after checking its bound; gzip holds the
aggregate output and at most one bounded member before appending it.
Callers supply any tighter input, output and concurrency budgets.

`src/gzip.rs` owns the sole inflater. The dependency-free control-plane
workspace, td-png and direct-rustc target recipes mount that same source,
with the engine's shared CRC/Adler module, preserving their existing build
shape. td-net uses the Cargo library for fetch-service decoding. There is
no registry dependency, unsafe code, shell or host decompression tool.
Recipe source staging and both control-plane fingerprints include the
relocated implementation. td-web, td-mail and td-news do not change.

Tests retain the original stored, fixed and dynamic Huffman, malformed
stream, checksum, output-bound, block-budget and member-budget cases.
Zlib tests cover framing, dictionary refusal, checksum, trailing bytes,
empty output and every truncated prefix. Fetch-service loopback tests
exercise negotiation and framing through the actual HTTP reader. PNG's
independent oracle suite continues to check image decoding.

Allocation helpers use the sibling `td-alloc` implementation under its
reservation and error contract (`td-alloc/DESIGN.md`).
