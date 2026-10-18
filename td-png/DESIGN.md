# td-png

A bounded PNG decoder, std only, held to an independent oracle's hashes
rather than a tolerance, and an eight-bit RGB encoder held to that
decoder. It is a crate of its own, beside td-jpeg, so td-agent can show
the PNG images a conversation holds (a pasted screenshot, a tool's
output), the compositor can write its screenshots, and any later td
program can read and write PNG the same way. Nothing here reads a file,
the environment or a clock.

The inflater and the CRC are the engine's (`engine/src/gzip.rs`'s raw
DEFLATE reader and `engine/src/crc32.rs`), mounted by `#[path]` as
td-util mounts them rather than copied: the one inflater td trusts for
source preparation is the one that reads image data. `gzip.rs` carries a
file entry point td-png never calls; td-png's own source opens nothing.
The DEFLATE encoder is td-util's (`td-util/src/deflate.rs`, its gzip
applet's), mounted the same way; its gzip framing is unused here.

## Public surface

- `header(bytes) -> Result<Header, Error>`: width, height, depth,
  colour type and interlacing, from a checked `IHDR`, without decoding;
  it refuses everything `decode` would refuse from the header alone
  (axes, the pixel budget, the inflated budget), so a consumer budgets
  with it before it decodes.
- `decode(bytes) -> Result<Rgba8, Error>`: the whole image.
- `Rgba8`: eight-bit RGBA, rows top to bottom, no padding, alpha
  straight (not premultiplied); its fields public, `pixel` checking them
  rather than trusting them.
- `Colour`, `Header`, and `Error`, each refusal by name.
- `encode_rgb8(width, height, rgb) -> Result<Vec<u8>, EncodeError>`:
  packed eight-bit RGB rows, top to bottom, as a whole PNG stream;
  `EncodeError` names an axis of zero or past PNG's 2^31 - 1 (`Axis`)
  and pixels other than three bytes each (`Length`).
- The bounds: `MAX_AXIS` (65536, long enough for a full-page screenshot;
  the pixel budget bounds the rest), `MAX_IMAGE_PIXELS` (64 Mi),
  `MAX_INFLATED_BYTES` (256 MiB, filter bytes included, so a 16-bit RGBA
  frame past 32 Mi pixels is refused) and `MAX_CHUNKS` (65536). The
  compressed image data may be at most twice its inflated size plus 64
  KiB (a fixed-Huffman block of literals runs up to an eighth past its
  rows, and dynamic blocks resend their tables, so only a stream no
  encoder writes is refused); past that it is `Error::Budget`.

## The decoder

Every colour type at every depth the specification pairs it with: grey
at 1, 2, 4, 8 and 16 bits, RGB at 8 and 16, palette at 1, 2, 4 and 8,
grey with alpha and RGBA at 8 and 16; the five row filters; Adam7, its
empty passes contributing no bytes; and `tRNS` for grey, RGB and palette
images. Output is always eight-bit RGBA: a sample below eight bits
becomes `v * 255 / (2^d - 1)`, a 16-bit sample `(v * 255 + 32767) /
65535` (nearest), grey is replicated, a `tRNS` key has its bits above
the image depth masked off (as the specification asks of a decoder), is
compared at that depth before scaling and makes its pixels' alpha 0, and
a palette entry's alpha is its `tRNS` byte or 255. That expansion is a
contract shared with `tests/fixtures/png_ref.py`, an independent decoder
written from the specification over Python's zlib, so a decode is held
to the oracle's hash exactly.

It is strict about what reaches the pixels and reads the rest as libpng
reads it, so an image every viewer shows is not refused where libpng
would show it. (That parity includes libpng's losses: a palette's `tRNS`
with a bad CRC is skipped, and its image shows opaque.) Strict: a chunk
type must be four ASCII letters and a length at most 2^31 - 1; a
critical chunk's CRC must match and an unknown critical chunk is
refused. `IHDR` must come first, once, be 13 bytes and name a depth and
colour type the specification pairs, compression and filter method 0 and
interlace 0 or 1. An indexed image's `PLTE` comes once, before its data,
a whole number of entries and at least one; an index past it is refused.
The `IDAT` chunks must be consecutive; `IEND` must be empty, and what
follows it is not read. The zlib header must name DEFLATE with a window
of at most 32 KiB, pass its check and carry no preset dictionary; the
stream must inflate to exactly the size `IHDR` implies (one filter byte
and the packed row for every row of every non-empty pass), be followed
by its Adler-32 and nothing else, and every row filter must be 0 to 4.
libpng warns and decodes a stream that runs long or has bytes after its
Adler-32; td-png refuses both, the one place it is stricter, since the
extra is image data it cannot account for. Each refusal is a named
`Error`; the inflater's own complaints arrive as `Error::Inflate` with
its words.

Read as libpng reads it: an ancillary chunk whose CRC fails is skipped;
a `tRNS` of the wrong length for its colour type, in an image with an
alpha channel, after the image data, after another, or for a palette
before its `PLTE`, empty or longer than it, is skipped; a `PLTE` in a
grey image, and a suggested palette of any length in an RGB one, are
skipped; and an indexed image's palette longer than 2^depth entries is
cut to them, since no index past them can occur.

What it does not do, so a consumer can say so: no colour management
(`gAMA`, `cHRM`, `sRGB`, `iCCP` and every other ancillary chunk are
skipped, the samples taken as sRGB); an animated PNG decodes to its
default image; and the image data is gathered and inflated whole, so
memory peaks at the inflated rows plus the RGBA frame (the compressed
stream is dropped first). At the budgets that is about 512 MiB and a
second for a frame 8192 pixels square, from a compressed stream a few
hundred KiB long, so a consumer showing several images budgets each with
`header` first. A 3840x2160 RGBA frame decodes in about 170 ms in a
release build.

## The encoder

`encode.rs` writes what the decoder reads most simply: colour type 2 at
depth 8, not interlaced, no ancillary chunk, so the stream depends only
on the pixels. Each row takes the filter whose bytes sum smallest as
signed values, libpng's heuristic, which picks Sub or Up for the flat
fields and gradients of a desktop. The filtered rows go through
td-util's encoder at its first level (fixed Huffman or stored blocks
over short hash chains, no dynamic tables) inside a zlib header of
`78 01` and an Adler-32, cut into `IDAT`s of one MiB. A 1920x1080
desktop encodes in about 170 ms at the compositor's `opt-level = "s"`,
to about a sixteenth of its RGB bytes; noise falls back to stored
blocks, a few bytes a block past its size. It refuses only what PNG
cannot name; a frame past the decoder's budgets encodes, and the
decoder refuses it. Paeth's predictor and Adler-32 live here and the
decoder uses them. `encode.rs` reaches only the crate root's `deflate`
and `crc32`, so a consumer mounts it beside those two and carries no
decoder, as the compositor does for Print's screenshots
(td-compositor/DESIGN.md, "Screenshots"); one that runs its tests also
mounts the engine's `gzip.rs` under `cfg(test)`, which `deflate.rs`'s
own tests read back through.

## Test contract

`tests/decode.rs` holds every file of the synthetic suite
(`tests/fixtures/suite`, written by `png_ref.py make`: every colour type
and depth, plain and interlaced, `tRNS` variants with a masked key and
16-bit near misses, and 1x1 and 3x1 interlaced frames with empty passes)
to the oracle's hash, and `shapes.png`, which librsvg wrote through
libpng from a drawing made for the suite, the same way. It checks `tRNS`
keys clear exactly their pixels and not a 16-bit near miss; builds
streams of its own (stored DEFLATE blocks, a bitwise CRC) to read
skipped ancillary chunks, image data split anywhere over consecutive
`IDAT`s and bytes after `IEND`, and each libpng-lenient case above
against the image it must still give, and to refuse by name: a non-PNG,
a cut stream, a bad CRC, a malformed chunk type or length, the chunk
budget, an unknown critical chunk, each chunk out of place, each
malformed chunk, every undefined depth and colour type and both method
bytes, axes zero, past `MAX_AXIS`, past the pixel budget and past the
inflated budget (by `header` too), compressed data past its budget, a
missing palette or an index past it, no image data, each refused zlib
header, a malformed DEFLATE stream, data short or long or followed by
bytes, a bad Adler-32 and an undefined filter. Every prefix of a real
stream is refused, and each of the first 600 bytes of three files' image
data flipped (the CRC redone, so the change reaches the inflater, the
filters and the expansion) decodes or is refused without a panic.

`tests/encode.rs` holds every stream the encoder writes to the decoder:
one pixel, single rows and columns, odd sizes, a frame of gradient,
checkerboard, flat and noise bands, a desktop that must shrink fiftyfold
and noise that must spill into exactly two consecutive `IDAT`s, each
decoded to its own pixels at alpha 255 with the header it names; the
same pixels give the same bytes, one 4x3 stream is pinned byte for
byte (read by `png_ref.py` and Python's zlib when pinned, so a change to
either encoder is seen), and each refusal is named. Its unit tests hold
Adler-32 to RFC 1950's value and a byte-at-a-time reduction and Paeth's
observable tie order, and `lib.rs`'s holds each filter to the decoder's
`unfilter`. A 1080p desktop's stream was also read by `png_ref.py` and
Python's zlib when the encoder landed.

`tests/confinement.rs` pins two source files of td-png's own that open
nothing, the two engine mounts and td-util's encoder and nothing else,
the encoder reaching only `crate::deflate` and `crate::crc32` (and no
`super::` outside its tests), a manifest of no
dependencies, the bounds above, the fixture inventory, `shapes.png`'s
length, every hash the decode test holds being the README's, and the
expansion contract stated in `src/lib.rs`.
