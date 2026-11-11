# td-webp

Normative contract for the shared WebP decoder. The crate is standalone,
`std` only, and forbids unsafe code. It takes borrowed bytes and returns
owned pixels or a typed error; it opens no files, creates no threads and
reads no ambient state.

## API and coverage

`header(&[u8])` reads the RIFF header and first image or extended header,
returning bounded canvas dimensions. It requires the declared RIFF extent
to exist, but does not validate compressed pixels. `decode(&[u8])` returns
straight-alpha eight-bit RGBA, top-to-bottom rows without padding.

Decoding covers VP8 key-frame (lossy) and VP8L (lossless) pictures,
both simple RIFF and VP8X containers. Lossless decoding supports all four
reversible transforms, palette packing, color caches, spatial prefix
groups, literal pixels and overlapping backward references. Lossy decoding
supports the four VP8 versions, segmentation, one to eight coefficient
partitions, every key-frame prediction mode, inverse transforms and simple
or normal filtering. VP8 dimension scaling hints are ignored. Interframes
and reserved versions/color spaces are refused.

Lossy alpha can be raw or VP8L-compressed, with all four alpha filters.
Alpha preprocessing hints are not applied. Lossy color uses bounded integer
BT.601 YUV-to-RGB conversion with nearest chroma samples; chroma edges can
differ from viewers using interpolated chroma. Lossless RGBA is exact.

Animations return only the first ANMF image, placed at its declared offset
on a transparent logical canvas. ANIM must precede it. The background-color
hint is ignored; blending into transparent canvas retains source colors,
except fully transparent blended pixels become transparent black. The
no-blend flag preserves their hidden RGB. No later frame or animation tail
is parsed. The complete declared RIFF extent must still be present.

The RIFF extent bounds every chunk, including its zero-valued padding.
Coded dimensions must agree with the canvas or animation frame rectangle;
the rectangle must fit the canvas. A second still image, duplicate alpha,
alpha beside VP8L, nested animation or misplaced VP8X is refused. Unknown
chunks and metadata are skipped by length; ICC profiles and Exif orientation
are not applied. Bytes after the declared RIFF extent are ignored. The
compressed stream must produce the complete image; unused trailing
compressed bits are ignored.

## Bounds

- At most 64 MiB of supplied bytes, 16384 pixels per axis and 16 Mi pixels.
- At most 4096 RIFF chunks across the outer container and first frame,
  and 256 spatial prefix-code groups per lossless image.
- Four distinct transforms, one of each kind, with no transform recursion.
- Auxiliary images have neither transforms nor recursive entropy images.
- A lossless stream's shared 256 Mi work budget charges each decoded
  prefix symbol, copied pixel and inverse-transform pixel, and sixteen
  units per alphabet entry when reading a prefix code. A symbol takes at
  most fifteen input bits.
- A VP8 stream permits at most 256 Mi Boolean symbols across its header,
  macroblock modes and coefficient partitions. Prediction, transformation,
  filtering and conversion run fixed work per admitted macroblock/pixel.
  A separate compressed alpha stream has its own lossless work budget;
  there are at most two compressed streams in the returned frame.
- Prefix codes use sorted symbol lists, never exponential lookup tables.
  Code lengths must describe a full tree or the specified singleton.
- Color caches have at most 2048 entries; distances must reference already
  decoded pixels and lengths must fit the remaining output.

All allocations are fallible. Dimensions bound every pixel allocation,
including auxiliary images. At most two full image buffers coexist during
palette expansion, RGBA conversion or animation placement. Each predictor, color or entropy map has at most
`ceil(width / 4) * ceil(height / 4)` pixels (including rounding at thin
image edges). Prefix tables add less than 2 MiB at the group limit. Decoder
working allocations stay below 145 MiB, excluding the caller's input.

The format references are Google's WebP RIFF container and lossless
bitstream specifications and RFC 6386 for VP8. Algorithm arithmetic uses explicit wrapping
bytes and checked slice access. Tests compare committed synthetic images
against independent libwebp RGBA, YUV and alpha output and exercise truncated containers,
entropy truncation, mutation, dimensions and allocation/work admission.

Allocation helpers use the sibling `td-alloc` implementation under its
reservation and error contract (`td-alloc/DESIGN.md`).
