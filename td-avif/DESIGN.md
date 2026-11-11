# td-avif

This standalone, std-only crate reads AVIF image-item containers and
decodes a bounded eight-bit still-picture subset through the sibling
`td-av1` crate. It forbids unsafe code and performs no ambient I/O, thread
creation or process execution. `header` returns the primary item's admitted
dimensions. `primary_image` returns those dimensions, the four-byte AV1
configuration record and the joined coded-item extents. Neither function
decodes AV1 pixels or claims that the item data is a valid AV1 stream.

## Container contract

The format reference is AOM's AVIF 1.2.0 and its HEIF/ISO-BMFF item model.
The first box must be `ftyp`, with `avif` as a major or compatible brand.
The reader handles 32-bit, extended 64-bit and end-of-parent box lengths.
It walks the complete container under one box budget and never recurses.

One `meta` supplies `hdlr` (`pict`), `pitm`, `iinf` and `iloc`, and `iprp`
with one `ipco` and one `ipma`. Primary item IDs may be 16 or 32 bits.
Duplicate singleton boxes, item IDs, associations and primary size/config
properties refuse. The primary item must be an unprotected `av01` item.
An `ispe` property supplies the bounded dimensions; `av1C` supplies a
version-one record without config OBUs or initial presentation delay.
If `pixi` is present, its channel count and depth must agree with `av1C`.
Unknown essential properties and transformative properties refuse.
Colour properties are admitted only as `nclx`;
the extraction API returns their CICP values without interpreting pixels.

`iloc` versions 0–2 support offset/base/length widths of zero, four or eight
bytes, with an explicit nonzero length width for the primary item. File
extents must lie wholly inside a declared `mdat` payload. Construction
method one reads extents from the single `idat`. External data references,
item-offset construction and indexed extents refuse. Several primary
extents are concatenated in declaration order, with a separate total-byte
bound even if the ranges repeat. No unselected item is copied or decoded.
References are limited to descriptive metadata and thumbnails; auxiliary
alpha, derived grids, premultiplied data, layers, sequences and display
transforms are unsupported and refuse instead of changing picture meaning.

## Pixel decoding

`decode` returns straight-alpha RGBA with opaque alpha. It checks container
size and AV1 configuration against the sequence header before allocating
pixels. If an `nclx` property exists, all its CICP values must equal the
sequence header's; conflicting metadata refuses. Duplicate colour properties
also refuse. `is_avif` inspects only the first file-type box for sniffing,
without claiming that the remaining container or coded image is valid.

The codec subset and its work limits are normative in `td-av1/DESIGN.md`.
In particular this is eight-bit 4:2:0, reduced-header still AV1 with a
restricted tool set, not general AVIF support. Unsupported tools, alpha,
HDR, grids, animation and display transforms return typed errors.

Presentation accepts primaries 1 or 2, transfer 1, 2 or 13, and matrix
1, 2, 5 or 6. It uses integer BT.709 for matrix 1 and BT.601 for the other
admitted matrices, including unspecified matrix 2. Full and limited range
have separate inverse coefficients at sixteen fractional bits. Chroma is
nearest-neighbor 4:2:0; RGB is rounded and clamped to eight bits. There is
no ICC, transfer-curve conversion or gamut mapping.

## Bounds

- Encoded file and joined primary data: 64 MiB each.
- Primary dimensions: 1–16384 per axis, at most 16 Mi pixels.
- Boxes across all visited levels: 4096; nesting follows the fixed schema.
- Items per table and properties: 256 each.
- Extents across the whole location table: 256.
- Associations: at most 255 per item, as the field permits, with 256 items.

Checked offsets precede every slice. Metadata vectors grow fallibly under
their counts; the joined output reserves fallibly after all extents are
validated. `header` allocates only bounded metadata, not a pixel buffer or
the joined coded image. Malformed counts cannot create unbounded loops.

Tests cover both extent methods, multiple extents, properties, truncation,
dimension/count/offset refusal, a deterministic mutation corpus and an
independently generated libavif container.

Pixel decoding first copies at most 64 MiB of coded extents. The native
codec's live buffers remain below 80 MiB, excluding that copy and the
caller's file. The coded copy is dropped before the at-most-64-MiB RGBA
allocation; the returned YUV planes occupy at most 24 MiB. RGBA reserves
fallibly. The native codec's bounded shared allocation behavior is stated
in its design document. Conversion visits each visible pixel once.

Pixel tests cover a libavif/libaom container, exact RGBA values derived
from independently decoded YUV, metadata disagreement, unsupported codec
tools, truncation, and full/limited-range matrix vectors.

Allocation helpers use the sibling `td-alloc` implementation under its
reservation and error contract (`td-alloc/DESIGN.md`).
