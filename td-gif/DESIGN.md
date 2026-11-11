# td-gif

A dependency-free, safe Rust GIF87a/GIF89a first-image decoder for future
inline web pictures. The byte-slice API follows td-png's shape:
`header(bytes) -> Result<Header, Error>` reads logical-screen width and
height; `decode(bytes) -> Result<Rgba8, Error>` returns owned eight-bit
straight-alpha RGBA, top-to-bottom rows with no padding. `Rgba8` has public
`width`, `height` and `data` fields. No files, environment, clocks, network,
threads, unsafe code or external crates. The caller owns fetching and
chooses a display size; this crate neither scales nor animates.

The format reference is CompuServe's GIF89a specification, including the
cover sheet's deferred-clear rule and Appendix F:
<https://www.w3.org/Graphics/GIF/spec-gif89a.txt>. Graphics Interchange Format
and GIF are CompuServe Incorporated's copyright and service mark.

## First-image semantics

The decoder reads the logical screen, global palette, intervening extensions,
then exactly one image descriptor with its optional local palette. Local
palettes override global palettes for image pixels. Rectangles must be
nonzero and fit wholly inside the logical screen. All four interlace passes
are decoded into their final rows, including short images with empty passes.

The returned canvas has the logical-screen dimensions and preserves image
left/top offsets. If that image's graphic control extension enables
transparency, the canvas starts transparent and transparent pixels are
canonical RGBA zero. Otherwise a global background colour fills the canvas;
without a global palette the uncovered area is transparent. Palette indices,
including a declared transparent index and a used background index, must
exist. This is an explicit web-picture canvas policy; no previous frame or
ambient screen is needed.

Graphic control must have its four-byte body and zero terminator, no reserved
bits, and disposal 0 through 3. Delay, user input and disposal do not affect
the first snapshot. Repeated graphic controls before the image are refused.
Comments and unknown extension labels are skipped through bounded sub-blocks;
application extensions require an eleven-byte identifier before their data.
Plain-text rendering is refused explicitly rather than silently replacing a
preceding text graphic with a later image. Pixel aspect ratio and palette
sort/colour-resolution hints do not change the returned samples.

Only the first image's complete LZW stream and its zero sub-block terminator
are required. An end code must occur after exactly the rectangle's pixels,
with no complete data byte remaining after it. Unused bits in the last byte
may be arbitrary. Later frames, extensions, the GIF trailer and any bytes
following it are not read or validated. A trailer before an image is
`Error::NoImage`; truncation of the first image is always an error.

## Bounds and LZW

The canvas has nonzero axes at most `MAX_AXIS` (16384) and at most
`MAX_IMAGE_PIXELS` (16 Mi), checked before allocation; RGBA uses at most
64 MiB. Only the output canvas is allocated. Compressed data is read across
sub-blocks directly, without concatenation or a separate index image.

`MAX_INPUT_BYTES` (64 MiB) bounds bytes examined, including skipped metadata,
through the first image. A longer slice with a complete early image is fine:
its animation tail is outside this operation. `header` reads only thirteen
bytes. `MAX_BLOCKS` (1 Mi) counts both top-level blocks and sub-blocks,
including terminators. `MAX_CODES` (32 Mi) bounds LZW codes, including repeated
clear codes which produce no pixels. Budget violations return `Error::Limit`.
Frame, palette, framing and LZW failures have distinct errors.

LZW accepts minimum code sizes 2 through 8, starts with a clear code, grows
codes from minimum+1 through 12 bits, and maintains a fixed 4096-entry prefix
and suffix dictionary. The special next-code case emits the previous string
plus its first byte. A full dictionary stays frozen until a clear code;
references into it remain valid. Clear resets code width, dictionary extent
and previous-string state without changing sub-block bit alignment.

Prefix references must go backwards and stay inside the current dictionary.
Expansion uses a fixed 4096-byte stack, checks the exact remaining pixel
budget before emitting, and refuses invalid codes, chains, missing end codes,
short output and excess output. Total expansion work is proportional to the
output pixels plus at most one final 4096-entry rejected string; independent
clear-code and framing limits bound the work that produces no pixels. No
recursive decoding, per-code allocation or unbounded dictionary exists.

## Validation

The crate joins the discovered standalone gate with its committed Cargo.lock
and all-target Clippy. Confinement tests pin the single pure source file,
no-dependency manifest, panic/unsafe policy and public budgets.

`tests/decode.rs` exercises local/global palettes, offsets, transparency,
first-frame-only tails, every short interlace pass shape, all LZW code-width
transitions, next-code expansion, clear/reset, a full dictionary with deferred
clear and continued references, one-byte sub-blocks, named errors, dimensions,
every truncation of small fixtures and deterministic mutated/random bytes.
Unit tests exercise budget boundaries without allocating maximum-size output.

Committed fixtures are generated by ImageMagick 6.9.13-5 from deterministic
RGBA formulas and compared to those formulas exactly. The random palette
fixtures traverse all code widths independently of the synthetic writer.
Their generator takes the host tool path explicitly; it is not run by tests,
not a shipped dependency and not a recipe input. WebP and AVIF are deferred.
