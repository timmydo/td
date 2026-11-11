# td-av1

This standalone, std-only crate owns the AV1 arithmetic shared by td's
picture codecs. It forbids unsafe code and has no dependencies. No module
opens files, reads the environment, obtains time, starts threads or reaches
network/process services.

## Primitive contract

These are arithmetic primitives, not a bitstream parser or an AVIF decoder.
The caller owns and validates geometry, coefficient ranges and buffers.
`td-photo::av1` uses them for its still-picture encoder and reexports the
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
  4:2:0 geometry.

Plane dimensions, strides and allocation extents must agree, be positive,
and fit the caller's image budget. The photo caller admits at most 16384
pixels per axis. A future hostile-bitstream consumer must establish its own
pixel/work budgets and validate these preconditions before calling the
primitives; these APIs are not an allocation or malformed-stream boundary.

The arithmetic and unit tests are shared unchanged with the photo encoder.
Its independent reconstruction hashes and optional dav1d oracle remain in
td-photo's tests. The photo design owns the encoder's tool selection,
threading, quality choices and export behavior. This document owns the
shared primitive coverage.
