# Shared legacy character decoding

`td-charset` is a std-only, safe Rust library for decoding untrusted text.
It depends on no application, locale, system iconv, runtime codec or external
crate. Its algorithms follow the [WHATWG Encoding Standard](https://encoding.spec.whatwg.org/).
The exact mapping and label data, license, hashes and offline regeneration
contract live in [data/README.md](data/README.md).

## Supported encodings and labels

The decoder supports UTF-8, UTF-16LE/BE, every WHATWG single-byte encoding,
Shift_JIS, EUC-KR, GBK/gb18030, replacement and x-user-defined. Single-byte
coverage includes windows-1252, ISO-8859-2 through -8, -8-I, -10, -13 through
-16, the other Windows pages, IBM866, KOI8-R/U and Macintosh pages.
WHATWG maps ISO-8859-1 and ASCII labels to windows-1252, ISO-8859-9 to
windows-1254 and ISO-8859-11 to windows-874. ISO-8859-12 has no encoding.
Strict ASCII and strict ISO-8859-1 are not aliases for this behavior.
ISO-8859-8 and -8-I remain distinct identities for callers that need layout
policy, though their scalar mappings are identical.

`Encoding::for_label` trims ASCII tab, LF, FF, CR and space and compares
ASCII case insensitively. It returns None for unsupported or unknown labels,
including Big5, EUC-JP and ISO-2022-JP. Replacement labels are recognized;
the replacement decoder emits one error replacement for any nonempty stream.
There is no claim to implement every WHATWG encoding or an encoder.

## Streaming and resource ownership

`Decoder::push` accepts one byte and returns zero through four scalar values
in a fixed `Decoded` batch. The caller drains the batch before feeding more
bytes. This makes chunk boundaries irrelevant and bounds each call to four
pending-byte transitions, each using direct table access or a binary search
of the 207 GB18030 ranges. State retains at most three unfinished bytes
between calls. No decoder allocation, I/O, recursion or retained output occurs.
`finish` emits one replacement for an unfinished sequence and closes the
stream; repeated finishes are inert and subsequent pushes return Finished.
Decoders can be copied as pure checkpoints; callers own admission and output.

Undefined mappings and malformed input produce U+FFFD and set the batch's
`had_errors`; a literal U+FFFD does not set it. Recovery preserves ASCII
syntax bytes after invalid multibyte leads. GB18030 replays buffered digits
and a third byte on invalid continuation, but consumes the incomplete prefix
as one error at EOF, as the standard requires. GBK uses the gb18030 decoder,
including four-byte mappings, the 7457 exception and the disallowed pointer
gap. Shift_JIS includes halfwidth Katakana, U+0080 and the EUDC private range;
EUC-KR includes the Windows extensions in the WHATWG index.

`decode` is a convenience whole-input wrapper with a caller-chosen maximum
UTF-8 output length. It uses fallible allocation and returns OutputLimit for
length, arithmetic or allocation refusal, publishing no partial string.
The limit bounds string length, not allocator capacity or RSS; callers with
strict resident-memory budgets use the allocation-free streaming API.
CPU work is linear in supplied bytes, with fixed table bounds. Label lookup
uses a fixed roster and linear whitespace trimming; callers bound metadata.
No API normalizes Unicode, removes controls, sanitizes HTML or strips BOMs
implicitly. Decoding is not a security/content validation boundary.

## Charset sniffing

`sniff::sniff` chooses a UTF-8/UTF-16 BOM, then a supported declared charset
label, then an opt-in HTML meta declaration, then the caller's explicit
fallback, in that order. It returns the encoding, source and BOM skip count.
The declared argument is a label extracted by the caller's HTTP/MIME parser,
not a full Content-Type field. Unsupported declarations allow later choices.
Only a recognized BOM contributes bytes to skip; `Selection::body` applies
the skip to the same complete byte stream before raw decoding.

The HTML scanner reads at most the first 1024 bytes without allocation.
It follows HTML's byte-prescan rules for comments, skipping other tags and
their quoted attributes, case-insensitive attribute names, first duplicate
attributes, `charset`, and `content` gated by `http-equiv=content-type`.
An unfinished tag or value at the scan limit supplies no encoding. UTF-16
meta labels become UTF-8; x-user-defined becomes windows-1252. This is the
HTML meta prescan subset, not a full HTML tokenizer or the browser's whole
encoding algorithm: XML declarations, parent context, overrides, locale and
statistical detection are outside its contract. In particular the prescan
does not interpret script raw text. It does not guess UTF-8 from validity
or infer an East Asian charset from arbitrary bytes.

Call sniff after collecting min(1024, input length) bytes for HTML, or at
least three bytes/EOF otherwise; it owns no streaming prefix buffer. A
partial prefix can miss a BOM or declaration. Plain mail/news text must not
opt into HTML scanning merely because it contains a tag-like string.

## Consumer and build boundaries

This library is independently consumable by td-web, td-mail and td-news.
It currently changes no consumer and enters no distribution recipe.
Adoption must stage this crate in the consumer's recipe and respect that
consumer's deadlines, work budgets and charset policy. In particular
`td-mime`'s strict ASCII/Latin-1 semantics and admitted labels cannot be
replaced by WHATWG aliases without an explicit policy change.
The existing `td-encoding` crate continues to own hex/base64 byte encodings.

The standalone gate runs tests and all-target Clippy. Tests cover published
language vectors, all single-byte and two-byte mapping entries, every
GB18030 four-byte pointer, all two-byte input combinations of the multibyte
decoders, error recovery, EOF, BOM behavior, output limits and bounded HTML
metadata cases. Offline reproduction authenticates the upstream JSON and
compares every generated table and label declaration. These tests do not
claim full browser or mail-client integration.
