# td-jpeg

A bounded baseline JPEG decoder, std only, held to an independent
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
  `MAX_PREVIEW_SAMPLES` (128 Mi) and `MAX_TABLE_DEFINITIONS` (32).
- `Encoder` (`new(width, height, quality)`, `encode_rows(rows, runner)`,
  `take`, `finish(runner)`) and `EncodeError`, each refusal by name.
- `Runner`, what runs the encoder's independent pieces, and `InOrder`,
  which runs them in turn on the caller's thread.

## The decoder

The decoder is a baseline JPEG decoder: 8-bit, one or three
components, horizontal and vertical sampling factors 1 or 2, Huffman DC/AC
tables and restart intervals, one interleaved scan, component planes of at
most 128 Mi samples together with their block padding
(`MAX_PREVIEW_SAMPLES`, what a decode allocates), axes within `MAX_AXIS`,
pixels within `MAX_IMAGE_PIXELS` and at most 32 table definitions
(`MAX_TABLE_DEFINITIONS`), refusing progressive, arithmetic, lossless,
hierarchical (a DHP, EXP or DAC marker anywhere before the scan), 12-bit and
multi-scan streams by name. SOF1 at 8 bits and 16-bit quantisers in an SOF0
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

What it does not do, so a consumer can say so: progressive streams,
common on the web, are refused by name rather than decoded; CMYK and
YCCK (four components) are refused; application segments are skipped,
so an EXIF orientation is not applied and an Adobe transform not read.

## The encoder

`Encoder` is baseline: JFIF, YCbCr 4:4:4 (one block per component per
MCU, so a band of eight rows is a row of MCUs and the right edge and
bottom pad by replicating the last column and row), the T.81 Annex K
quantisation tables scaled at the caller's quality the way every encoder
since IJG scales them (`5000 / q` below 50, `200 - 2q` from 50 up,
entries held to 1..=255) and the Annex K.3 Huffman tables, written into
the stream, no restart intervals. The colour transform is the JFIF one
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
short of it), and the segments before the scan by their bytes. Its unit
tests pin the standard tables complete and prefix-free, the quality
scaling at its ends, the clamp, and the magnitude coding.

`tests/buffer.rs` holds `Rgb8` to its bounds: no zero axis, none past
`MAX_AXIS`, no frame past `MAX_IMAGE_PIXELS` (`MAX_AXIS` squared is
refused), and `is_consistent` and `pixel` refusing a buffer whose public
fields disagree, overflow or are out of range.

`tests/confinement.rs` pins one pure source file that opens nothing and
names no thread, a manifest of no dependencies, the bounds above, and
the fixture (its length and markers) and the hashes its README records,
and the oracle's contract stated in `src/lib.rs`.
