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

Decoding currently covers still lossless VP8L, both simple RIFF and VP8X
containers. All four reversible transforms, palette packing, the color
cache, spatial prefix groups, literal pixels and overlapping backward
references are supported. VP8 lossy images, separate alpha chunks and
animation return `Unsupported`. Header inspection also recognizes VP8
key frames. This is not complete WebP support and no browser advertises it.

The RIFF extent bounds every chunk, including its zero-valued padding.
The image dimensions must agree with the extended canvas. A second image
or misplaced VP8X is refused. Unknown chunks and metadata are skipped by
length; ICC profiles and Exif orientation are not applied. Bytes after the
declared RIFF extent are ignored. The compressed stream must produce the
complete image; unused trailing compressed bits are ignored.

## Bounds

- At most 64 MiB of supplied bytes, 16384 pixels per axis and 16 Mi pixels.
- At most 4096 RIFF chunks and 256 spatial prefix-code groups per image.
- Four distinct transforms, one of each kind, with no transform recursion.
- Auxiliary images have neither transforms nor recursive entropy images.
- A shared 256 Mi work budget charges each decoded prefix symbol, copied
  pixel and inverse-transform pixel, and sixteen units per alphabet entry
  when reading a prefix code. A symbol takes at most fifteen input bits.
- Prefix codes use sorted symbol lists, never exponential lookup tables.
  Code lengths must describe a full tree or the specified singleton.
- Color caches have at most 2048 entries; distances must reference already
  decoded pixels and lengths must fit the remaining output.

All allocations are fallible. Dimensions bound every pixel allocation,
including auxiliary images. At most two full image buffers coexist during
palette expansion or RGBA conversion. Each predictor, color or entropy map has at most
`ceil(width / 4) * ceil(height / 4)` pixels (including rounding at thin
image edges). Prefix tables add less than 2 MiB at the group limit. Decoder
working allocations stay below 145 MiB, excluding the caller's input.

The format references are Google's WebP RIFF container and lossless
bitstream specifications. Algorithm arithmetic uses explicit wrapping
bytes and checked slice access. Tests compare committed synthetic images
against independent libwebp RGBA output and exercise truncated containers,
entropy truncation, mutation, dimensions and allocation/work admission.
