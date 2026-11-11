# td-avif

This standalone, std-only crate reads AVIF image-item containers. It has
no dependencies, forbids unsafe code, and performs no ambient I/O, thread
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
Colour properties are admitted only as `nclx`; this extraction API leaves
pixel colour interpretation to a future decoder.

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
