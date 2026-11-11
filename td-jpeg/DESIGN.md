# td-jpeg

A bounded baseline and progressive JPEG decoder, std only, held to an independent
oracle's hashes rather than a tolerance, and the baseline encoder that
shares its basis and zigzag order. td-photo decodes its camera's
embedded previews with it (`td-photo/DESIGN.md`, "Embedded previews")
and exports through the encoder; td-agent shows the pictures a
conversation holds with the decoder. Nothing here reads a file, the
environment or a clock, or starts a thread.

## Public surface

- `header(bytes) -> Result<Header, Error>`: the frame's width, height,
  components and luma sampling, read without decoding.
- `decode(bytes, Scale) -> Result<Rgb8, Error>`, and `decode_with`, which
  also hands each block's coefficients to a sink for the oracle tests:
  the frame at `Scale::Full`, `Half`, `Quarter` or `Eighth`;
  `Scale::covering(width, height, long_edge)` and `scaled(axis, scale)`.
- `Rgb8`: eight-bit interleaved RGB, rows top to bottom, no padding, its
  fields public so a pipeline fills one in place, every accessor
  checking the axes against the buffer; `Rgb8::new`, `is_consistent`
  and `pixel`.
- `Error`, each refusal by name.
- The bounds: `MAX_AXIS` (16384), `MAX_IMAGE_PIXELS` (64 Mi),
  `MAX_PREVIEW_SAMPLES` (128 Mi), `MAX_TABLE_DEFINITIONS` (32) and the
  encoder's `MAX_EXIF` (65527).
- `Encoder` (`new(width, height, quality)`, `with_exif(width,
  height, quality, tiff)`, `encode_rows(rows, runner)`, `take`,
  `finish(runner)`), `MAX_EXIF` and `EncodeError`, each refusal by name.
- `Runner`, what runs the encoder's independent pieces, and `InOrder`,
  which runs them in turn on the caller's thread.

## The decoder

The baseline path accepts: 8-bit, one or three
components, horizontal and vertical sampling factors 1 or 2, Huffman DC/AC
tables and restart intervals, one interleaved scan, component planes of at
most 128 Mi samples together with their block padding
(`MAX_PREVIEW_SAMPLES`, what a decode allocates), axes within `MAX_AXIS`,
pixels within `MAX_IMAGE_PIXELS` and at most 32 table definitions
(`MAX_TABLE_DEFINITIONS`), refusing arithmetic, lossless,
hierarchical (a DHP, EXP or DAC marker anywhere before the scan), 12-bit and
sequential multi-scan streams by name. SOF1 at 8 bits and 16-bit quantisers in an SOF0
stream are read as baseline; a lone component's sampling factors are
ignored, one block per MCU, as T.81 A.2.2 codes it and libjpeg reads it. It
decodes at 1/1, 1/2, 1/4 or 1/8 scale by a reduced inverse DCT over the
first N coefficients of each axis (`Scale::covering` picks the coarsest
scale whose long edge still covers what was asked).

Its arithmetic is a contract shared with `tests/fixtures/jpeg_ref.py`, an
independent transcription of T.81, so a decode is held to the oracle's
hash and not to a tolerance: the transform is separable `f64` over
literal constants, rows then columns, sums in ascending frequency; a
sample is `floor(v + 128 + 0.5)` clamped to 0..=255; chroma is replicated
to the luma grid, not interpolated; and the JFIF constants (1.402,
0.344136, 0.714136, 1.772) are applied with `floor(x + 0.5)`.

It is strict where leniency would hide corruption, and the oracle refuses
the same streams: a Huffman table with an over-full tree or a code of all
ones (T.81 C.2, which is what tells padding from a symbol), a DC category
past 11 or an AC size past 10, a DC predictor outside 16 bits, an AC
symbol without magnitude other than EOB and ZRL, a run or a ZRL past the
block, a scan that lists the frame's components out of order, a restart
marker out of sequence or preceded by more than a byte's padding, and a
scan followed by anything but EOI (a second scan, an unread byte,
nothing) are each refused by name; what follows EOI is not read. A stream
that ends early decodes to its end on zero bits and is reported as
truncated after the MCU row it ran out in, so a cut file is an error and
not a partial picture.

What it does not do, so a consumer can say so: CMYK and
YCCK (four components) are refused; application segments are skipped,
so an EXIF orientation is not applied and an Adobe transform not read.

### Progressive streams

SOF2 uses the same `header`, `decode`, `decode_with`, scales, RGB layout,
transform and colour conversion. The baseline encoder remains baseline.
The progressive decoder implements Huffman DC first/refinement and AC
spectral selection/refinement, including signed correction bits, newly
nonzero coefficients, ZRL, EOB runs and restart intervals (T.81 Annex G,
<https://www.w3.org/Graphics/JPEG/itu-t81.pdf>). DC scans may interleave
components; an AC scan has one component. Noninterleaved scans traverse
only real component blocks, while interleaved DC scans include MCU padding.
The callback receives final dequantised blocks once, in the baseline MCU,
component, block order, including padding.

Each component captures its quantiser on its first scan. A later DQT may
serve a component not yet started, without changing earlier components.
Per-coefficient history rejects repeated first scans, AC before DC,
refinement before first transmission, skipped refinement bits, and
inconsistent overlapping bands. Spectral ranges, component order and
selectors are checked. Successive approximation is at most 13; quantised
coefficients must fit signed 16 bits, so dequantisation fits signed 32 bits.
EOB runs may not cross a restart boundary or scan end. EOI requires DC for
every component; unsent AC bands and remaining low bits are zero, allowing
intentionally abbreviated progressive quality. Truncation is still an
error, never a returned partial image. Arithmetic/hierarchical markers and
new frames after a scan are refused.

Both entry points cap input at `MAX_INPUT_BYTES` (64 MiB) and marker segments
at `MAX_SEGMENTS` (4096), including skipped metadata. The table-definition
budget remains 32 across the entire progressive stream. Progressive-only
limits are `MAX_SCANS` (256), `MAX_PROGRESSIVE_COEFFICIENTS` (64 Mi
coefficients, 128 MiB), and `MAX_PROGRESSIVE_WORK` (256 Mi coefficient
visits, charged before each scan from its block count and spectral width).
All limit failures use `Error::Limit`, except the existing table and frame
bounds which retain their errors. Component planes are bounded as before;
coefficients are released before allocating the final RGB buffer. Peak
bulk allocation is at most 192 MiB for coefficients plus sample planes,
or 256 MiB for sample planes plus RGB, with bounded table/scratch overhead.
The smaller coefficient cap also preserves td-web's PNG-based decode budget:
its admitted output is at most 16 Mi pixels, so samples plus RGB need at most
112 MiB, and the RGB-to-RGBA conversion at most 112 MiB after decode
has released its planes. The coefficient/plane phase is the larger bound,
192 MiB, equal to the PNG decoding allowance. The four scales reduce sample
storage; they do not reduce the progressive coefficient-storage requirement.
There is no incremental rendering API and no file, thread or other ambient
access.

## The encoder

`Encoder` is baseline: JFIF, YCbCr 4:4:4 (one block per component per
MCU, so a band of eight rows is a row of MCUs and the right edge and
bottom pad by replicating the last column and row), the T.81 Annex K
quantisation tables scaled at the caller's quality the way every encoder
since IJG scales them (`5000 / q` below 50, `200 - 2q` from 50 up,
entries held to 1..=255) and the Annex K.3 Huffman tables, written into
the stream, no restart intervals. `with_exif` adds an Exif APP1
(`Exif\0\0` then the caller's TIFF, which the encoder does not read)
right after the JFIF APP0, as libjpeg's writers place it, 1 to
`MAX_EXIF` bytes so the segment's length fits its sixteen bits and
the segment is never a bare identifier, which some readers call
corrupt; the
rest of the stream is as without it. The colour transform is the JFIF one
the decoder inverts; the forward transform is the decoder's basis
transposed, in `f32`, rows then columns; coefficients are held to the
categories the tables carry (the DC to -1024..=1023, each AC to
-1023..=1023), and a symbol no table carries would be refused rather
than coded as a bare run of bits; the last byte is padded with ones. It
is fed rows in any number at a time and drained of bytes as they are
made, so a frame of any size is coded a band at a time; the transform
and quantisation of a band run as a `Runner`'s pieces (`InOrder` on the
caller's thread, or a pool the caller owns: td-jpeg starts no thread),
the entropy coding is sequential, and the bytes are the same whatever
the band or thread count. The decoder is its oracle: a ramp at quality
92 round-trips within 12 of every 8-bit value and noise at quality 100
within the colour transform's rounding, a grey frame stays exactly grey,
a black frame at quality 100 (the largest DC category through coder and
decoder) exactly black, and the standard tables are pinned complete
(every baseline DC category and AC run/size once) with no code of all
ones, which is what the decoder refuses; the clamp is pinned by a unit
test over `forward`.

## Test contract

`tests/decode.rs` carries a synthetic baseline JPEG writer (fixed
complete DC and incomplete AC tables, byte stuffing, restart markers, 8-
and 16-bit quantisers) and an in-test reference of the decoder's
arithmetic written the long way over a copy of the same literal tables,
and pins: grey and three-component round trips of every sampling shape,
chroma coarser and finer than luma, with and without restarts, at every
scale, exactly; the real Z 8 thumbnail (`tests/fixtures/z8-thumb.jpg`)
held exactly to the coefficient hash and the four pixel hashes the
oracle recorded; the scale rule by value; and every refusal by name (a
non-JPEG, a cut stream, an early EOI, every unsupported frame type and
the hierarchical and arithmetic markers, a second frame, precision,
component layout and sampling factor, axes zero or past the ceilings and
planes past the sample budget, a missing, over-full, all-ones or
zero-valued table and more definitions than the budget, a scan that is
not the whole frame or lists it out of order, a missing, wrong or
out-of-sequence restart marker and an unread byte before one, a code no
table holds, a reserved AC symbol, a predictor past 16 bits, a run or a
ZRL past the block, and anything but EOI after the scan).

`tests/encode.rs` holds the encoder to the decoder: the round trips
above over sizes from 1x1 to ones that are no multiple of a block, a
smooth frame a small fraction of its raw bytes, the stream fed in bands
of any size and run on one thread or a pool of up to sixteen equal to
the stream fed whole in order, each refusal by name (an axis of zero or
past `MAX_AXIS`, rows not the width, rows past the height, a finish
short of it, no Exif or Exif past `MAX_EXIF`), the segments before
the scan by
their bytes, and with Exif the APP1 (`Exif\0\0` and the TIFF) right
after the APP0, the rest of the stream and its pixels as without it,
and `MAX_EXIF` filling one segment. Its unit
tests pin the standard tables complete and prefix-free, the quality
scaling at its ends, the clamp, and the magnitude coding.

`tests/buffer.rs` holds `Rgb8` to its bounds: no zero axis, none past
`MAX_AXIS`, no frame past `MAX_IMAGE_PIXELS` (`MAX_AXIS` squared is
refused), and `is_consistent` and `pixel` refusing a buffer whose public
fields disagree, overflow or are out of range.

`tests/progressive.rs` compares libjpeg-turbo coefficient-preserving
progressive transcodes against baseline at every scale: grey, 4:4:4,
4:2:0, 4:2:2, vertical subsampling and chroma finer than luma, with and
without restarts. It also checks callback coefficients/order, every
truncation of the grey fixtures and sampled cuts of the colour fixtures,
deterministic mutations, progression errors and scan/segment limits.
The fixture generator and provenance live under `tests/fixtures/progressive`.

`tests/confinement.rs` pins the two pure decoder source files that opens nothing and
names no thread, a manifest of no dependencies, the bounds above, and
the fixture (its length and markers) and the hashes its README records,
and the oracle's contract stated in `src/lib.rs`.
