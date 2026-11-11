# AV1 decoder fixtures

All pictures are synthetic. The `.yuv` files are raw eight-bit 4:2:0
planes decoded with host dav1d 1.5.1; no external decoder is a build or
runtime dependency. The `.obu` files include a temporal delimiter.

The td encoder fixtures use seed 37 and the same generators in
`src/intra/decode.rs`'s reconstruction test: single (1×1, quality 75),
ramp (35×27, 92), solid64 (64×64, 30), noise (127×71, 75), edges
(65×129, 97), tiles (143×137, 65, two tile columns and rows), vertical
(1×127, 50), and horizontal (127×1, 95).

`libaom` is a 6×4 constant-plane image (Y=160, U=100, V=180), encoded
by libavif 1.0.4/libaom 3.8.0 at speed 10, quality 75, CICP 1/13/6,
full range, with `reduced-tx-type-set=1`.

`libaom-noise-1`, `-2`, `-4`, `-5` are respectively 8×8, 16×16,
65×67 and 4×4 random-plane images from Python Random seed 51, quality
41, 42, 44 and 45. Each case consumes its predecessor's random bytes;
case zero is constant 4×4 and case three constant 24×17. Same encoder,
speed and colour parameters, with reduced transforms and restoration,
CDEF, rectangular, asymmetric and one-to-four partitions disabled.
These are four admitted cases from a 40-case differential run: all 37
admitted pictures matched dav1d's planes; three used unsupported tools.

`chroma-delta` retains `noise`'s entropy payload but changes its frame
filter header: base luma levels 1/1, U/V levels 32/32 and an updated intra
reference delta of -1. Effective luma levels are zero while chroma remains
30/30. dav1d 1.5.1 produced the expected planes. This guards against
skipping chroma filtering when only the effective luma levels are zero.

`direction-delta` uses base luma 19/0, U/V 28/16 and intra delta +63,
which activates the initially zero horizontal level and yields 63 in
every plane/direction. `disabled-delta` uses base luma 0/0 and delta +63;
the whole deblock operation remains disabled. Both retain `noise`'s
entropy data and have independent dav1d expected planes.

`zero-chroma-delta` uses base luma 19/43, U/V 0/5 and intra delta +18.
Its effective levels are 37/63, 0/23: the zero-base U plane remains
disabled under the plane-admission rule, unlike a zero luma direction.
Its YUV is independently decoded by dav1d 1.5.1.
