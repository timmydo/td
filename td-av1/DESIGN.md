# td-av1

This standalone, std-only crate owns the AV1 arithmetic shared by td's
picture codecs, a still-picture encoder and a bounded still-picture decoder. It forbids unsafe code and depends only on the sibling `td-alloc` crate. No module
opens files, reads the environment, obtains time, starts threads itself or reaches
network/process services.

## Primitive contract

The arithmetic modules are not a bitstream parser or an AVIF decoder.
The caller owns and validates geometry, coefficient ranges and buffers.
`intra` uses them for its still-picture encoder; td-photo reexports the
module names for its existing consumers.

- `transform` supplies AV1 integer inverse DCT/ADST, the paired forward
  transforms, quantization tables and coefficient scans. Sizes are square
  4 through 64 and 2:1 rectangles through 32; a 64-point transform codes
  only its lower 32 frequencies. Callers supply sixteen-bit bounded
  inverse coefficients and eight-bit-image forward residuals. Fixed arrays
  and literal butterfly indices hold each stage's shape. Slice operations
  outside an admitted shape do not extend buffers.
- `cdf` holds the default cumulative probability tables for the encoder's
  supported intra coding tools. It performs no decoding or allocation.
- `deblock` filters caller-owned eight-bit 4:2:0 planes at the supplied
  luma vertical/horizontal and chroma levels. The callback supplies the
  transform size at a mode-info location. This interface covers intra
  square/half blocks, with no segmentation, filter deltas or sharpness.
- `cdef` filters those planes under one strength set, reading the deblocked
  samples and the supplied skip map. Its source copies are bounded by the
  caller's admitted plane buffers. It covers one CDEF preset and eight-bit
  4:2:0 geometry. It reserves all source copies before modifying any plane,
  returning `TryReserveError` if a copy cannot be allocated.

Plane dimensions, strides and allocation extents must agree, be positive,
and fit the caller's image budget. The photo caller admits at most 16384
pixels per axis. The decoder establishes pixel/work budgets, geometry and coefficient bounds
before calling the primitives. The primitive APIs themselves are not an
allocation or malformed-stream boundary.

The arithmetic and unit tests are shared unchanged with the photo encoder.
Its independent reconstruction hashes and optional dav1d oracle remain in
td-photo's tests. The photo design owns its export behavior and worker pool. This document
owns the shared coding and arithmetic contracts.

## Encoder

The caller chooses a `Runner` type. Its `run` starts jobs from the end of
the supplied vector: a job can wait for jobs after it. It runs every job
exactly once and joins them before returning. `InOrder` runs that order on
the caller's thread; td-photo's `PhotoRunner` uses its existing bounded
worker pool. The encoder caps concurrency at 16. Geometry and quality are
validated at construction; RGB rows are trusted caller-owned data, not an
untrusted compressed stream. This API has the encoder's existing infallible
buffer allocations and is separate from the decoder's hostile-input boundary.

`intra::Encoder<R>` is a pure std still-picture encoder. Its coding
contract is:
a still picture (`still_picture` and `reduced_still_picture_header`
set), main profile, 8-bit 4:2:0 at full range in BT.601 (`Y = (77R +
150G + 29B + 128) >> 8`, the chroma the mean of each 2x2 offset by 128),
one key frame of intra blocks. The tools are the subset a decoder must
carry and an encoder can verify: square blocks of 64, 32, 16, 8 or
4 and the halves of a 32, a 16 or an 8 (a partition of `NONE`,
`HORZ`, `VERT` or `SPLIT`; a node whose lower or right half starts
past the frame's edge is split, where the spec would allow it the
halving inside too; of the blocks under 8x8 only the last along each
4-pixel side of its 8x8 carries chroma, the 8x8's, `HasChroma`), one
transform the block's size (`TX_MODE_LARGEST`, the reduced transform
set: a luma block with no side of 32 or more may signal any of the
four DCT and ADST pairs, chroma's is the one its mode implies,
`DCT_DCT` with a side of 32 or 64; a 64-point transform codes only
its lower 32 frequencies each way, the spec zeroing the rest, and a
64x64's chroma is never predicted from luma, the spec's CfL stopping
at 32), the
thirteen intra modes, the directional ones with their angle deltas
in blocks of 8x8 and more (three degrees a step, to nine either way)
and the intra edge filter (the corner and each edge a directional
angle reads smoothed by a strength its block's size, its distance from
the edge's normal and a smooth neighbour select, the smallest blocks'
edges upsampled near the normal; chroma's neighbour is the block with
chroma past its 8x8's corner), without filter intra or palette
and, for chroma, its prediction from the block's luma (each plane's alpha
the least-squares fit in eighths, it and its neighbours tried by
squared error), deblocking and CDEF but no restoration, superres or
film grain, and the symbol adaptation the spec runs by default. The
quality maps to `qindex` as `255 - ((q - 1) * 254 + 49) / 99`, the dead-zone
quantiser rounds the DC at half a step and each AC at three eighths,
a trellis then walks each transform's levels from the end of block
back (libaom's `av1_optimize_txb` in kind) keeping each or dropping it
by one, by its squared error in the transform domain and its rate in
the context the later levels give it, the end of block's symbols and
the zeros it moves over counted and libaom's intra plane multipliers
(17/16 luma, 13/16 chroma) on the rate, and a block's mode is chosen by
rate-distortion (`(16 D << 7) + (R * rdmult) >> 9`, `rdmult` from the DC
step as libaom sets it and `D` the squared error, sixteen times it the
scale libaom's distortion and `rdmult` share, the rate the coder's own
cost of its symbols under the superblock row's probabilities, below):
the modes are
screened by the 4x4 Hadamard SATD of their predictions plus their mode
symbols' rate under sixteen times the root of the multiplier (the two
closest directional modes of luma and the closest of chroma screened
again at their six other angles, the predictor making each row or
column one two-tap blend of an edge slice at a fixed step, two entries
a pixel on an upsampled edge; the screen reads the edges unfiltered, an
estimate a full trial then predicts exactly, which saved two thirds
of the filter's time for about 0.1%),
the best three, two under 8x8 (for chroma, one mode for both planes,
the closest two and the
prediction from luma if it is not one of them, since the screen ranks
it below what coding it measures) coded in full and compared with
their mode's rate, then the chosen luma mode coded under the other
transform types when it left any coefficient, and a superblock's
partition is decided by trials that save and restore the contexts and
pixels they touch before the chosen tree is emitted, so the decoder's
state is the encoder's exactly: each node whole, then split, then
halved either way, each trial cut short once it costs more than the
best so far. The halvings, which cost as much again as the node whole,
are tried only where the split did not beat it whole by 8%, and a
direction only where its halves hold the source luma's variance within
nine tenths of the other's as tightly; at 16x16 and 32x32 on the
bench's noisy synthetic picture that kept five sixths of what trying
both gained for two fifths of the trials. A 64x64 is tried whole only
where its source luma's variance is within twice the square of the DC
step over 64, which on a real photo tried a quarter to seven tenths of
them from 0.3 to 3 bits a pixel and nearly all below, for nearly all of
what trying every one gained, and it is never halved (0.1% more at low
rates for 6% more instructions), nor its luma screened at the
directional modes' other angles, which gained nothing there.
The frame is tiled
uniformly by its size alone, so the bytes are the same whatever the
thread count, as the JPEG encoder's are, and by the fewest tiles the
spec allows (Annex A.3), since each tile's edges cut the prediction
and contexts across them and restart the probabilities: the columns a
tile's 4096-pixel width needs (the last column is the remainder), then
the rows the tile area needs, which split again where a column rounded
up to whole superblocks passes the area. A tile's superblock rows are
searched in a wavefront (libaom's row-mt): a row's superblock waits
until the row above has searched the one past it, whose pixels it
reads above and to the right, and takes the contexts and the last row
of pixels that row left over it; the row's rates are under its own
probabilities, the row above's after its second superblock adapted by
the row's own symbols, and the tile's one coder writes the rows'
decisions in order under the real ones, so no search waits on the
coder, though a finished row waits its turn to write. The
rows come in batches of four a thread, every tile's rows over
the caller's `Runner` in dependency order, so the row a row waits for is always under
way. The tile sizes are written in four bytes, and the sequence
header claims the least level whose picture size, tile count and tile
columns admit the frame. `intra::av1c` and the colour constants are the
configuration the container repeats, from the same values the sequence
header writes. The transforms are the spec's integer butterflies, each
a table of stages that `butterfly!` expands to straight code over fixed
arrays, at the square sizes of 4 to 64 and the 2:1 rectangles of 4 to
32 (the
rectangles' rows scaled by the root of a half as the spec's inverse
does, the forward by the root of two as libaom's), and the deblocking
filter (`deblock`) is the spec's edge loop over 4x4 units: an edge's
length is the smaller extent across it of the transforms beside it (4,
8 or 14 luma taps, 4 or 6 chroma), no intra
block counts as skipped, and the level, one for every plane and
direction, is libaom's fit of the AC step for a key frame
(`LPF_PICK_FROM_Q`) half again, which measured better on a photo than
the fit at every rate and as well as double it but for RGB fidelity.
CDEF (`cdef`, spec 7.15 in dav1d's form) follows it: each 8x8 of luma
with coefficients is filtered along the direction its pixels show, and
its 4x4s of chroma with it, every tap reading the deblocked frame and
none outside the mode-info grid; one strength set serves the frame
(`cdef_bits` 0), libaom's fits of the AC step for an intra frame
(`av1_pick_cdef_from_qp`) with chroma's halved, which measured better on
a photo than the fit (whose chroma cost RGB fidelity at mid rates) or no
chroma filtering. Prediction reads the unfiltered pixels, so the filters
run once the frame is coded, over the superblock-padded planes a decoder
filters, and only on a reconstruction the caller asked to keep, which is
then what any conforming decoder shows; `td-photo/tests/av1.rs` holds that to
the `dav1d` on `PATH` when there is one (a run without one says so on
stderr and skips that leg, as the gate's sandbox does), and holds six
small streams and their reconstructions to the hashes of bytes dav1d
decoded, so a change to any emitted symbol or filtered pixel is
verified against the decoder before the hash moves. It is fed rows
like the JPEG encoder and codes a batch of superblock rows when one is
complete, holding the batch's planes, a byte and a half a pixel
(the photo export caller); a frame is at most 16384 on an axis. A block's prediction edges
are read once per plane and every mode is predicted from them into the
row's scratch, which also holds the transform's working blocks and each
plane's trial and best coding and only grows over the row, so a block
allocates only the levels its leaf keeps. A 24-megapixel export takes
about a minute on one thread and ten to fourteen seconds on eight
(`td-photo/tests/av1.rs`'s `bench`, a noisy synthetic picture the blocks under
8x8 cost half again on and gain nothing; a photo they cost a third
again). Against libaom's all-intra speed 6 on a real 2048-pixel photo
the encoder needs about 6% fewer bits for the same luma PSNR from 0.3 to
3 bits a pixel (3% fewer for RGB) and about 3.5% fewer below that
(4.5% for RGB; libaom leaves CDEF off there). One tile instead of the
eight columns the wavefront replaced gained 1.4% on that photo at mid
rates and 3.2% below (libaom's own ablation prices eight columns at 1%
and 4%) in the same time on eight threads. Coding 64x64s whole gained
0.3% at mid rates and 1.3% below for 9% more instructions (capping
libaom's partitions at 32 costs its speed 4 1.5% below and changes
nothing at its speed 6). The partitions not coded here (the
halves of a 64, the three- and four-way ones) and transforms smaller
than their block are the compression follow-up.
Measured before the trellis priced the zeros an end of block passes
over: zeroing a transform's few levels outright bought luma about 2%
but cost RGB 3% at low rates, a chroma weight trading one for the
other, and coding more luma modes in full bought under 1% on luma for
half again the time.

## Decoder

`header(&[u8])` reads the first sequence header under the input/OBU budget
and returns its dimensions, AV1 configuration record and CICP values.
`decode(&[u8])` reads one complete picture and returns eight-bit Y, U and V
planes, chroma half-sized with odd dimensions rounded up. Both use typed
errors; unsupported tools refuse. Container parsing, auxiliary alpha and
YUV-to-RGB presentation belong to the caller. This is an initial AV1
still-picture subset, not a complete AV1 or AVIF implementation.

The admitted stream has main profile, eight-bit colour 4:2:0, one reduced
still-picture sequence header and one frame OBU. Temporal delimiter and
padding OBUs are allowed; other OBU forms refuse. Geometry uses 64-pixel
superblocks, uniform tiles, square blocks down to 4x4 and horizontal or
vertical halves of 8/16/32-pixel blocks. All thirteen intra modes, angle
deltas, intra edge filtering, recursive intra prediction and chroma from
luma are supported. Transforms fill their block and use the reduced
transform set's four DCT/ADST pairs; identity transforms currently refuse.
The decoder uses adaptive or fixed CDFs as the header requests. It reads
coefficients and signs with the normative symbol decoder, validates the
terminating one and zero padding, and bounds Golomb lengths to the spec's
twenty bits. Dequantization bounds each inverse input to sixteen bits.

Deblocking permits zero sharpness and intra-reference deltas, including
updates; mode deltas are parsed but do not affect intra blocks. The filter
receives effective levels and skips the whole frame only when every plane
has zero strength; delta-adjusted luma zero does not suppress chroma.
Original base luma 0/0 disables deblocking globally; otherwise reference
deltas apply even to an individual zero base luma direction. A chroma
plane whose original base strength is zero remains disabled (AV1 7.14). CDEF may
be disabled or use one strength preset. Quantizer deltas, segmentation,
quantization matrices, lossless coding, screen content, other partitions,
smaller transform selection, non-reduced transform sets, restoration,
superresolution, film grain, monochrome and higher bit depths refuse.
The display metadata is returned without colour conversion.

The encoder and decoder share prediction edges, contexts, integer
transforms, dequantization and reconstruction filters. The decoder invokes
no encoder search or thread runner. It retains one tile's band buffers and
reuses block/coefficient scratch across the tile; whole-frame reconstruction
is needed because filtering follows intra prediction.

Decoder bounds:

- Encoded input: 64 MiB; at most 1024 OBUs.
- Axis: 1–16384; visible pixels: 16 Mi; padded luma plane: 20 Mi pixels.
- Tile geometry: at most 64 columns and 64 rows, with AV1 tile width/area
  constraints. Partition recursion has at most five levels.
- Entropy work: 64 Mi symbols across all tiles. Each symbol searches at
  most sixteen CDF entries. Raw header/padding reads are input-linear.
- A coded transform has at most 1024 coefficients; an inverse block has
  at most 4096 samples. Golomb lengths and dequantization keep arithmetic
  bounded, including hostile coefficient magnitudes.

Every decoder-owned buffer reserves fallibly, including tile contexts,
geometry, reconstruction copies and crops. Allocation failure returns
`DecodeError::Limit` without allocating an error message. The encoder also
propagates failures from these shared helpers as `intra::Error::Allocation`;
its other allocations remain outside the decoder guarantee. The bounds
limit simultaneously live codec buffers to less than 80 MiB at the largest
admitted picture, excluding caller-owned input and allocator bookkeeping.
`header` does not allocate pixel planes. Unit tests fail each reservation
in representative single-tile and tiled decodes, then verify recovery and
unchanged pixels. This exercises reservation failures without a global
allocator override; it cannot simulate a kernel OOM kill.

Fixtures include td-encoded pictures independently decoded by dav1d 1.5.1
and libaom 3.8.0 pictures decoded by dav1d. Tests pin exact YUV planes,
tile edges, odd/thin geometry, truncation, symbol admission and a
deterministic mutation corpus. The codec tests require no external tool.

Allocation helpers use the sibling `td-alloc` implementation under its
reservation and error contract (`td-alloc/DESIGN.md`).
