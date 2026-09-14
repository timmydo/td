# td-photo

td-photo is td's photo tool: it imports a card or folder of camera raw files
into a library of dated folders, lets the photographer cull them quickly from
the camera's own embedded previews, develops the kept ones with exposure, a
crop and a named look, and exports them. It is a small, Wayland-native
td-owned program over the shared `td-ui` toolkit, dependency-free Rust and
`std` alone, in the same position as td-editor and td-setup. This document
is the component contract and the starting point for successive agents; the
root `AGENTS.md` and `DEVELOPMENT.md` still govern changes and submission.

The reference camera is the Nikon Z 8. Its 14-bit lossless-compressed NEF is
the first and only raw format of version 1; everything below that names a
budget or a format was measured against a Z 8 file (8280x5520 samples, one
55 MB strip, a 1620x1080 baseline JPEG preview and an 8256x5504 one). Other
Nikon bodies that write the same container and codec work when their colour
matrix is in the camera table; other makers, DNG, and Nikon's High
Efficiency codec are later increments, not silent partial support.

## Status and scope

Implemented: the bounded TIFF container reader (`tiff`), the NEF reader
over it (`nef`: the raw sub-image, the embedded previews, the exposure
facts, and the maker-note white balance, black level, sensor crop and
linearization table), the Nikon Huffman decoder for every tree dcraw names
with the 14-bit lossless tree verified against a real Z 8 frame and the
others against the test encoder only, the camera table with the Z 8's
colour matrix (`camera`), the linear colour math and the sRGB transfer
(`color`), the superpixel demosaic, area resampler and headless development
pipeline (`develop`), the RGB image buffers and PPM writer and reader
(`image`), the baseline JPEG decoder for the embedded previews with its
reduced-transform scaling (`jpeg`), the thumbnail rule and the thumbnail
cache, and the command line `td-photo probe FILE`, `td-photo develop
FILE OUT.ppm`, `td-photo thumb FILE OUT.ppm` and `td-photo cache`.
Every verb reads the file through a read bounded by `MAX_FILE_BYTES` that
does not trust the length the file system reported, and `develop` and
`thumb` refuse an `OUT.ppm` (or `OUT.ppm.tmp`) that already exists rather
than replace it, publishing the finished temporary by a hard link so a
name that appeared meanwhile is not replaced either.
No window, no sidecar, no import and no look yet: the increments at the
end schedule them in order. Nothing in this crate depends
on td-ui until the window increment adds the path dependency.

The rules below define version 1; the increments identify the order of
implementation, not choices left to each implementing agent.

## Purpose and trust position

td-photo is target-zone source, built by a cargo recipe that stages sibling
trees (the td-net shape) once it depends on td-ui. It reads only the files
the user names or the folders they import from and into, its own sidecars,
and its own cache directory. It never modifies, renames or unlinks an
original: import copies, culling moves rejects into a subfolder, export
writes new files beside the roll, and every write is a temporary file
given its final name by a hard link, which cannot replace an existing
name; a rename, which can, is the fallback only on a file system without
links, after a second check. It opens no network connection and runs no
subprocess. A camera
file is untrusted input: every offset, count and dimension is checked
against a ceiling before it sizes an allocation or indexes a buffer, and a
malformed file is an error naming what was refused, never a panic or an
unbounded read.

Production code has no `unwrap`, `expect`, panics or panicking indexing;
the crate root forbids `unsafe`, and its confinement tests pin that it
declares no dependency (td-ui only, from the window increment on).

## Workflow

One window, three modes, all keyboard-first; the pointer does the same
things, and so does an agent, through the same actions (see Driving). The
modes are the photographer's order of work.

1. **Import** copies raw files from a source folder (a mounted card) into
   the library. `td-photo import SRC DEST` does the same headless.
2. **Cull** shows a roll as a grid of thumbnails made from the camera's
   medium embedded preview. The photographer walks it with the arrows,
   presses `P` to pick, `X` to reject and `U` to clear, `Return` to see one
   photo at the medium preview's full size and `Return` again to go back,
   and filters the grid to picks, rejects, unflagged or all. `Delete
   rejected` moves the rejects and their sidecars into the roll's
   `rejected/` folder; nothing is unlinked.
3. **Develop** shows one photo developed from its raw data. `+`/`-` move
   exposure by a third of a stop and `Shift` by a tenth, `C` enters the crop
   with drag handles and aspect presets (free, 3:2, 4:3, 1:1, 16:9), `L`
   opens the look list, `0` resets, and `E` exports. Every change is saved
   to the sidecar as it is made; there is no explicit save and no undo
   stack in version 1, only reset to camera defaults.

Export renders the full-resolution raw through the same pipeline and writes
an sRGB image into the roll's `exported/` folder, never overwriting: a
second export of the same name takes a numbered suffix.

## Driving

td-photo is operated by a person at the keyboard and by an agent acting
for that person, and the design makes those one thing seen from two
sides. The shape is td-editor's, the tree's precedent for a driven UI: a
display-independent dispatcher, a headless replay of it, and a control
socket on the live window, all speaking one vocabulary.

- **One dispatcher.** Everything the window can do is an `Action`, a
  closed enum in `ui` (open a roll, select, next, previous, flag, filter,
  change mode, exposure, crop and its aspect, look, reset, export, delete
  rejected, scroll, resize, quit). `ui::Controller::dispatch(Event) ->
  Outcome` turns an input event into actions and applies them to the
  model; the keyboard bindings, the pointer hit-testing, the replay stream
  and the control socket are four adapters over that one dispatcher, and
  nothing reaches the model around it. The controller reads no file, clock
  or descriptor: its adapters feed it events, including job results, and
  carry out the requests it returns (decode this, write that).
- **A headless verb for every durable effect.** Whatever an action does to
  files is also a command-line verb: `import`, `list`, `flag`, `edit`
  (get and set of a sidecar's values), `develop`, `export`, `thumb`,
  `looks` and `cache clear`. Verbs are the batch face: they read the same
  sidecars and write through the same publication, and an agent that does
  not need to see pixels never opens a window.
- **`--replay`: the window without a display.** `td-photo --replay [--size
  WxH] [ROLL]` reads requests on stdin and answers on stdout in the
  envelope td-editor's CONTROL.md defines: a four-byte big-endian length,
  then one tab-separated ASCII line `1 ID verb args...`, answered by `1 ID
  ok ...` or `1 ID error CODE HEX`. The same controller runs, jobs run on
  the same pool, and the frame is painted through the toolkit's raster
  into memory. The vocabulary: `state` (mode, roll, count, cursor, filter,
  the selected photo's name, flag and edit, the outstanding job count and
  the frame generation); `photo N` (one photo's facts); `action NAME
  ARGS...` for every action by its table name; `key HEX_CHORD`, `pointer
  PHASE X Y` and `wheel ROWS` for the input path through the same
  bindings; `wait-idle MS` (returns when no job is outstanding or the
  deadline passes, saying which); `wait-frame GENERATION`; `frame` (the
  last painted frame as a PPM in hex); `quit`. Error codes are stable
  (`unknown-verb`, `bad-argument`, `no-roll`, `no-photo`, `busy`,
  `refused`) and the message is hex.
- **`--control-socket PATH`: the same vocabulary on the live window,**
  served from a private (0600) Unix socket by a bounded worker in
  td-editor's `control_socket` and `control_worker` shape: one request
  per connection, a deadline per request, and a response that reflects
  the state after the turn that applied it. Pixels of the live window are
  evidence only through the compositor's capture channel, observed
  before and after as td-compositor/AUTOMATION.md prescribes; `frame`
  over the socket returns the app's own last frame for correlation.
- **Determinism.** The same requests over the same roll give the same
  `state`, and with `wait-idle` between them the same `frame`; the tests
  hold a scripted session to a pixel oracle.
- **One table.** The vocabulary lives in one table in `ui`: action name,
  key binding, replay name and argument shape, verb where one exists, and
  a help line. `tests/ui.rs` pins that every action has a keyboard or
  pointer path, a replay name and a help line, and `td-photo --help
  actions` prints the table so an agent can read it instead of guessing.

## Files

The library is folders of originals; there is no database.

- **Roll**: one folder of originals. Import files a photo under
  `DEST/YYYY/YYYY-MM-DD/NAME` by its `DateTimeOriginal`, or under
  `DEST/undated/` when the file has none. A roll is listed by name; a
  supported file is one whose extension is `nef` or `NEF` (JPEG and DNG are
  later increments).
- **Import** copies through `NAME.part` in the destination folder,
  syncs, then links it to `NAME` and unlinks `NAME.part` (the publication
  every write uses). A destination that already exists with
  the same length and identical bytes is skipped and counted; one that
  differs is reported and left alone, never overwritten. The source is
  never written.
- **Sidecar**: `NAME.ext.edit` beside the original, UTF-8 text, one
  `key value` pair per line:

  ```text
  td-photo edit 1
  flag pick
  exposure -0.33
  crop 0.1000 0.0500 0.8000 0.9000
  look classic-chrome
  ```

  `flag` is `pick` or `reject`, absent when unflagged; `exposure` is stops
  with two decimals in -5.00..=5.00; `crop` is `x y w h` as fractions of the
  oriented image with four decimals, all in 0..=1, `w` and `h` at least
  0.05; `look` is a look's file stem. A line whose key is unknown is
  preserved verbatim and rewritten in place, so a later version's keys
  survive an earlier one's edit. A sidecar over 64 KiB or 1024 lines, a
  first line other than `td-photo edit 1`, or a malformed known value is
  refused as a whole and the photo is shown with camera defaults and an
  error, never with half its edits. Writes go through `NAME.ext.edit.tmp`
  and the same link-then-unlink publication.
- **Rejected** originals move, with their sidecars, into `rejected/`
  under the roll. Export writes into `exported/`. Neither folder is
  listed as part of the roll.
- **Cache**: `$XDG_CACHE_HOME/td-photo` when that variable is absolute
  (`~/.cache/td-photo` otherwise), holding `thumbs/` and nothing else in
  version 1. The base directory is the user's and may be a symlink;
  `td-photo` and `thumbs` are the cache's own and must be real directories,
  since `cache clear` unlinks inside them, so a symlink in either place is
  refused rather than followed. A thumbnail is `thumbs/KEY-N.ppm`: `KEY` is
  the FNV-1a-64 hex of the original's resolved path, byte length and
  modification time, `N` the long edge, and the file is the PPM
  `image::write_ppm` writes, published by the link rule through a temporary
  named for the filling process (`KEY-N.ppm.PID.tmp`), so two processes
  filling one entry never contend for a name and the first to publish wins.
  The stamp is taken before the lookup and again from the open file after
  the read, and an entry is stored only if the two agree, so the bytes
  cached are the file the key describes (a stamp that differs or cannot be
  taken stores nothing and says nothing: the next run keys the file as it
  now is); an original edited or replaced, as far as length and modification
  time tell, keys itself anew. Reading back accepts only a regular file of
  that exact shape under 64 MiB with a long edge of at most `N`; a symlink,
  directory or device in an entry's place is a miss and left alone, and an
  entry of the right kind with the wrong content is unlinked so the miss
  refills it. The cache is an optimisation: whatever it cannot do (a refused
  directory, a full disk, a permission) is a note on stderr and the
  thumbnail is still written from the original. `td-photo cache clear`
  unlinks the entries and temporaries named that way (and only those) and
  nothing in the library changes; `td-photo cache path` prints the directory
  as the bytes it is. The windows between a check and the operation it
  guards are those of any program without directory descriptors: the
  directories are the user's own cache, and the only name ever unlinked is
  one of the cache's own shape.
- **Looks** are read from `$XDG_CONFIG_HOME/td-photo/looks/*.look`
  (`~/.config/td-photo/looks/` without it) on top of the built-in set the
  binary carries; a user look of the same stem shadows the built-in one.

## Decoding

All decoders take a byte slice and return an owned result or an error
naming the refused item. Nothing here reads a file, the environment or a
clock; `main` and the library adapter own I/O.

### Container (`tiff`)

A little- or big-endian classic TIFF: the 8-byte header, then IFDs. The
reader indexes entries in place and copies nothing until asked. Budgets: a
file of at most 512 MiB; at most 64 IFDs visited in one walk and at most 16
in one next-pointer chain, a repeated offset ending the walk as a cycle;
at most 4096 entries per IFD; a value's byte extent
(`count * type size`) is checked to lie inside the file before any read;
the walk visits IFD0 and the IFDs on its next-pointer chain (at most
`MAX_CHAIN`), IFD0's `SubIFDs` (the first `MAX_SUB_IFDS`, 16), its
`ExifIFD` and the one IFD of the maker note that Exif carries, and nothing
deeper: the preview IFD the maker note names is not walked, and the maker
note's reader sees the file only up to the end of the field Exif declares
for it. Unknown tags and types are skipped, not refused.

### NEF (`nef`)

The Nikon layout over the container:

- The raw sub-image is the `SubIFDs` entry whose `PhotometricInterpretation`
  is CFA (32803), with `BitsPerSample` 12 or 14, `SamplesPerPixel` 1, one
  strip, and a 2x2 `CFAPattern` whose colours are one of the four
  permutations of R, G, G, B. Axes are at most 16384 and the sample count
  at most 128 Mi (a 256 MiB `u16` buffer), checked before allocation.
- `Compression` 34713 with a maker-note linearization table (0x0096) is
  Nikon compressed; 1 is uncompressed 16-bit samples in the container's
  byte order, and any other value is refused by name. The Z 8 writes 34713.
- The maker note is the `Nikon\0` type-2 form: a ten-byte prefix, then a
  TIFF header of its own, whose byte order is its own and whose offsets
  are relative to that header; the linearization table is read in that
  order, not the container's. The reader takes
  `WB_RBLevels` (0x000C, red and blue multipliers as rationals over green),
  `BlackLevel` (0x003D, four shorts, the first used), `CropArea` (0x0045,
  left, top, width, height) and the linearization table (0x0096). Every
  one is optional: the camera table supplies black when the maker note
  does not, the full frame is the crop when 0x0045 is absent, and white
  balance falls back to the camera's daylight multipliers. The encrypted
  colour-balance block (0x0097) is not read; the unencrypted 0x000C is
  present on the Z 8 and is the as-shot balance.
- Previews: the JPEG images the container names through `JPEGIFOffset`
  and `JPEGIFByteCount` in IFD0, the IFDs on its chain and the sub-IFDs,
  in that order, each checked to lie in
  the file and to begin with the SOI marker, reported with their byte
  extents and, once the preview decoder lands, their dimensions. On the Z
  8 they are 160x120, 1620x1080 and 8256x5504 baseline 4:2:2 JPEGs.
- Exposure facts from the Exif IFD: `ExposureTime`, `FNumber`,
  `ISOSpeedRatings`, `FocalLength`, `DateTimeOriginal`, `LensModel`, and
  `Orientation` from IFD0 (1, 3, 6 and 8 are honoured; any other value is
  treated as 1 and reported).

### Nikon Huffman codec

The decoder is dcraw's `nikon_load_raw`, restated: the linearization
table's first two bytes are the version; `0x46` selects the lossless tree
family and 14-bit samples the 14-bit member (tree 5 of the six); four
shorts in the maker note's own byte order follow as the vertical
predictors for the two rows and two columns of the 2x2 phase; then a short
curve length in the same order. A version `0x44
0x20` table carries a sampled curve interpolated to the sample range and a
split row after which the tree changes (the lossy type-2 form); another
non-`0x46` version with a curve of at most 0x4001 entries carries it whole;
a `0x46` table has no curve. The bitstream is MSB-first with no marker
stuffing. Each sample is one Huffman symbol whose low nibble is the
difference length and high nibble a shift, then `length - shift` raw bits
(seven for tree 4's `0x5c`), sign-extended the JPEG lossless way after the
shift; the first two columns of a row predict
from the row two above, every other column from two to its left. The tree
tables are the six dcraw publishes, with the code lookup built as a
`1 << longest` table so a symbol costs one load.

A predicted sample outside `0..max` is counted as corrupt and clamped, as
dcraw's `derror` counts and continues; the count is part of the result and
`probe --decode` prints it with the FNV-1a-64 hash of the decoded samples.
A stream that ends before the last sample is `Truncated`, with the rows
completed, and no partial image is handed on. The 14-bit lossless tree is
pinned against the first rows of a real Z 8 frame decoded by an independent
reference, and the whole frame's hash is recorded beside the fixture for
anyone holding the file; the other trees round-trip through the test
suite's encoder only, and the design says so until a frame from such a
camera pins them.

### Embedded previews (`jpeg`)

The preview decoder is a baseline JPEG decoder: 8-bit, one or three
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

The thumbnail rule: for a long edge N, take the smallest preview whose
long edge covers N (else the largest), decode it at the coarsest covering
scale, area-resample in the encoded domain to exactly N, never enlarging,
and turn the result by the file's orientation as `develop` turns the raw,
so a portrait frame is a portrait thumbnail. The cull grid asks for N =
400, which on the Z 8 is the
1620x1080 preview at 1/4 (405x270) resampled to 400x267 in 80 ms; the
single-photo cull view asks for N = 1600, which the same rule answers
with that preview at 1/1. `td-photo thumb FILE OUT.ppm [--long-edge N]
[--cache]` is the rule headless, and `td-photo probe FILE` prints every
preview's geometry (`--decode` also its full-scale pixel hash, the number
the oracle prints for the same bytes).

## Colour and development

### Camera table (`camera`)

One entry per supported body, matched on the exact `Make` and `Model`
strings the file carries: the Adobe `ColorMatrix` from XYZ (D65) to camera
space as integers over 10000, the white level, and the default black level.
The Z 8 entry is rawspeed's (`cameras.xml`, mode `14bit-compressed`): black
1008, white 15892, matrix

```text
 11423 -4564 -1123
 -4816 12895  2119
  -210  1061  7282
```

An unknown body is refused by name rather than developed with another
body's colour; adding one is a reviewed table entry with its source.

### Colour math (`color`)

dcraw's construction, in `f32`: `cam_rgb = cam_xyz * xyz_rgb` (the sRGB
D65 primaries), each row normalized to sum to one so that a white-balanced
camera white is display white, and `rgb_cam` its inverse; a singular matrix
is refused. The daylight multipliers are the reciprocals of the rows'
pre-normalization sums, scaled to green. The transfer is the sRGB piecewise
curve through a 65536-entry `u8` table built once; the inverse is not
needed in version 1.

### Pipeline

In order, per pixel, all linear `f32` until the last step:

1. black subtraction and scaling to `0..=1` by `white - black`;
2. white balance multipliers, then a clip at 1.0 per channel (the camera
   white; this is what keeps clipped highlights neutral);
3. exposure, `2^stops`;
4. `rgb_cam` into linear sRGB primaries;
5. the look, when one is applied (below), otherwise nothing;
6. clip to `0..=1` and the sRGB transfer to 8 bits.

Steps 2 through 4 are linear and fold into one 3x3 matrix and one clip;
step 5 is a per-channel curve table over a log-spaced domain plus one
saturation step; step 6 is the table. The cost per pixel is nine multiplies,
six table loads and a few adds, which is what makes the preview redraw at
frame cadence over a whole canvas on one core and in a few milliseconds
across the pool.

### Levels and memoization

The development pipeline holds four levels per photo and recomputes only
what an edit invalidates:

| level | content | size on a Z 8 frame | lifetime |
|---|---|---|---|
| 0 | CFA samples, `u16` | 87 MiB | current photo plus prefetch, under `RAW_CACHE_BYTES` (512 MiB) |
| 1 | superpixel demosaic, camera-native linear `u16` per channel, black-subtracted, un-balanced | 65 MiB at 4128x2752 (the 8256x5504 crop halved) | current photo only |
| 2 | the crop of level 1 area-resampled to the canvas, oriented, linear `f32` | canvas x 12 bytes | current photo and canvas size |
| 3 | display pixels, XRGB, written into the frame | canvas x 4 bytes | the frame |

Exposure and look edits recompute level 3 only. A crop edit recomputes
level 2 from level 1 (tens of milliseconds across the pool) and then level
3; during a crop drag the previous level 2 is shown scaled until the
pointer settles for one frame, so the drag never waits. Resizing the
window recomputes level 2. Switching photo recomputes level 1 from the
cached level 0 and prefetches the next two and previous one level-0 frames
in the background. Level 1 is superpixel (each 2x2 CFA quad becomes one RGB
pixel: exact colour, no interpolation, a quarter of the samples), which is
the right demosaic for every on-screen size below half resolution; export
and the 100% loupe (a later increment) run a full-resolution demosaic in
row bands from level 0 so peak memory stays bounded by the band, never by
the frame.

The resampler is separable area averaging for reduction and bilinear for
enlargement, over `f32` rows, and is shared with thumbnails. The headless
`develop` verb converts the whole of level 1 to `f32` before resampling
it, one frame at a time; the window increment resamples from the `u16`
level directly so level 2 is the only `f32` buffer.

## Looks

A look is a text file, `stem.look`, UTF-8, one operation per line, applied
in file order after step 4 of the pipeline:

```text
td-photo look 1
name Classic Chrome-like
primaries 0.92 0.06 0.02  0.03 0.94 0.03  0.02 0.06 0.92
tone contrast 1.35 toe 0.0 shoulder 0.0
curve luma 0 0  0.25 0.22  0.75 0.78  1 1
saturation 0.85
monochrome 0.30 0.59 0.11
```

- `primaries` is a 3x3 matrix, row-major, each row renormalized to sum
  to one so neutral stays neutral;
- `tone` is the sigmoid `y = x^c / (x^c + k)` with `k` chosen so middle
  grey (0.1845) is fixed, `contrast` the exponent in 0.5..=3.0, `toe` and
  `shoulder` in -1..=1 skewing the curve below and above grey;
- `curve` is a monotone cubic through at most 16 points in `0..=1` for
  `r`, `g`, `b` or `luma`;
- `saturation` scales chroma about Rec. 709 luminance in 0..=2;
- `monochrome` collapses to the weighted sum, weights renormalized.

At most 16 operations, a 4 KiB file; an unknown operation or an
out-of-range value refuses the whole look by line number. The built-in set
is `contrast-boost`, `contrast-soft`, `mono` and a Fujifilm-inspired family
(`provia-like`, `velvia-like`, `astia-like`, `classic-chrome-like`,
`classic-neg-like`, `eterna-like`, `acros-like`), each authored by hand in
this format. The jssfr.de darktable styles that motivated them are built
from darktable's `primaries`, `colorcontrast`, `colorbalancergb`, `agx`
and `monochrome` modules; td-photo does not execute darktable's pipeline
and does not claim to reproduce those styles. A converter that reads a
`.dtstyle` and emits the nearest `.look` for that module subset is a later
increment and is a translation the user runs, not a runtime dependency.

## Window

The window is a `td_ui::client::App` in the shape td-editor and td-setup
use, and it is one adapter over `ui::Controller` (see Driving): its
`event` maps keys and pointer to the controller's events and its
`end_turn` feeds the controller the job results. It owns no Wayland
objects of its own, paints chrome through the
toolkit's raster and bands (the menu bar, the status row, the paged list
for looks and rolls), and paints photo pixels itself with a clipped XRGB
blitter into the same frame inside `present`'s closure, because the
toolkit's raster is fills and glyphs. When a second consumer needs an image
primitive it is promoted into `td_ui::raster` with a pixel oracle; until
then the blitter is this crate's and its confinement test pins it as the
only code that writes frame bytes outside the raster.

Performance contract: no decode, resample or file read runs on the turn
loop's thread. Workers post results; `end_turn` drains them, marks damage
and, while any job is outstanding, sets the transport wait to at most 16
ms so results appear within a frame of arriving; with nothing outstanding
the wait is the toolkit's idle wait. Thumbnails are requested for the
visible rows first, then one screen ahead and behind; a scroll that makes
a request stale drops it before it starts. A grid cell whose thumbnail is
not ready paints a neutral placeholder and its name, never blocks.

## Concurrency and memory

- One pool of `available_parallelism` workers, at most 16, started at
  window open and joined at close; jobs are `Thumbnail`, `Preview`,
  `RawDecode`, `Level1` and `Export`, each carrying a generation the pool
  compares before starting and the turn loop compares before applying.
- At most one `RawDecode` runs at a time (the codec is sequential); demosaic
  and resampling split rows into bands on one shared queue that the calling
  thread and its scoped helper threads drain together, so no thread
  outlives the call and a band count that exceeds the threads is shared out.
- Budgets are named constants the tests pin: `RAW_CACHE_BYTES` 512 MiB,
  `THUMB_CACHE_BYTES` 256 MiB in memory, `MAX_FILE_BYTES` 512 MiB,
  `MAX_RAW_SAMPLES` 128 Mi, `MAX_PREVIEW_SAMPLES` 128 Mi over a preview's
  padded component planes, `MAX_TABLE_DEFINITIONS` 32, `MAX_IFDS` 64,
  `MAX_ENTRIES` 4096, `MAX_AXIS` 16384 for raw and image axes alike,
  `MAX_IMAGE_PIXELS` 64 Mi for any one image buffer (768 MiB as `f32`
  RGB, under a 32-bit target's allocation limit), `MAX_RANGE` 32768 for
  a decoder parameter set's sample range. Eviction is least recently
  shown.
- Buffers are allocated once per size and reused: the level-2 and level-3
  buffers per canvas size, the decoder's output per raw geometry, the
  thumbnail scratch per worker.

## Invariants

- No original is ever modified, renamed or unlinked by td-photo. Import
  copies; culling moves into `rejected/`; export writes new names.
- No existing name is ever replaced: a destination is refused by name
  before any work, and the final step of every write is a hard link,
  which fails on a name that appeared meanwhile; only a file system that
  refuses links falls back to a second check and a rename.
- Pure modules (`tiff`, `nef`, `camera`, `color`, `develop`, `image`,
  `jpeg`, and later `look`, `edit`) read no file, environment, clock or
  descriptor. `main` and the library adapter own I/O.
- Every ceiling above is checked before the allocation or index it
  guards; a refused input names the item.
- The colour of an unknown body is refused, not guessed.
- The decoded sample of the 14-bit lossless tree is what dcraw decodes;
  the pinned real-frame fixture is the oracle and a change that moves it
  is a codec change.
- No `unsafe`, no dependency other than td-ui, no `include!`, no build
  script.

## Test contract

`tests/confinement.rs` pins the source inventory, that the crate root
forbids `unsafe`, that the manifest declares no dependency (td-ui only,
from the window increment), no build script and no `include!`, and that
the pure modules name no `std::fs`, `std::env`, `std::time`, `std::net` or
`std::process` path.

`tests/nef.rs` carries a synthetic NEF writer and a Nikon Huffman encoder
for all six trees, and pins: round trips of random and edge-valued frames
through every tree at 12 and 14 bits, with and without a curve and a split;
the real-frame oracle (`tests/fixtures/z8-rows.bin`, the first rows of a Z
8 strip with the header values that decode them, held to the FNV-1a-64
hash an independent Python transcription of dcraw produced, recorded in
`tests/fixtures/README.md`); a truncated stream reported with its rows;
corrupt samples counted and clamped; every container refusal (cycles,
oversize counts, out-of-file offsets, a missing raw sub-image, an unknown
compression, a non-Bayer pattern, oversize axes); and the maker-note facts
of the reference file's header values.

`tests/develop.rs` pins the colour math against hand-computed values
(neutral in, neutral out; the daylight multipliers of the Z 8 matrix; the
sRGB table's endpoints and monotonicity), the superpixel demosaic on a
known quad, the area resampler on constant and step images, and a complete
development of a synthetic frame to expected 8-bit values.

`tests/jpeg.rs` carries a synthetic baseline JPEG writer (fixed complete
DC and incomplete AC tables, byte stuffing, restart markers, 8- and
16-bit quantisers) and an in-test reference of the decoder's arithmetic
written the long way over a copy of the same literal tables, and pins:
grey and three-component round trips of every sampling shape, chroma
coarser and finer than luma, with and without restarts, at every scale,
exactly; the real Z 8 thumbnail (`tests/fixtures/z8-thumb.jpg`) held
exactly to the coefficient hash and the four pixel hashes the oracle
recorded; the scale and thumbnail rules by value; and every refusal by
name (a non-JPEG, a cut stream, an early EOI, every unsupported frame
type and the hierarchical and arithmetic markers, a second frame,
precision, component layout and sampling factor, axes zero or past the
ceilings and planes past the sample budget, a missing, over-full,
all-ones or zero-valued table and more definitions than the budget, a
scan that is not the whole frame or lists it out of order, a missing,
wrong or out-of-sequence restart marker and an unread byte before one, a
code no table holds, a reserved AC symbol, a predictor past 16 bits, a
run or a ZRL past the block, and anything but EOI after the scan).

From the window increment on, `tests/ui.rs` drives `ui::Controller`
in-process (the action table's completeness, scripted sessions held to
`state` and pixel oracles through the replay path) and
`tests/control_process.rs` runs the built binary against the native
compositor harness the sibling crates use, with the observe, capture,
observe rule for pixel evidence.

The builder discovers the crate by existing; its gate runs `cargo test` and
all-target Clippy.

## Independently landable increments

1. Crate and codec: this document, the container and NEF readers, the
   Nikon Huffman decoder with the real-frame oracle, the camera table,
   colour math, superpixel demosaic, resampler, and `probe` and `develop`
   headless. Landed with this document.
2. Previews: the baseline JPEG decoder with reduced-IDCT scaling and its
   oracle, the thumbnail rule and cache, `td-photo thumb FILE OUT.ppm`,
   `td-photo cache`, and preview facts and hashes in `probe`. Landed.
3. Library: rolls, the sidecar reader and writer, `import`, `list`,
   `flag` and `edit` headless, with the never-overwrite and never-unlink
   oracles.
4. Window, cull mode: the td-ui dependency, `ui::Controller` with its
   action table and `--help actions`, `--replay` with the whole
   vocabulary over the cull actions, `--preview`, the worker pool, the
   grid with flags, filters and the single-photo view, `tests/ui.rs`, and
   `tests/control_process.rs` under the native compositor harness the
   sibling crates use.
5. Window, develop mode: levels 0 through 3 with their memoization,
   exposure, crop with the drag contract, the look list and the look
   format with the built-in set, the develop actions in the table and
   the replay, and `--control-socket` on the live window.
6. Export: banded full-resolution bilinear demosaic, the JPEG encoder,
   `exported/` naming, and `td-photo export`.
7. Packaging: the cargo recipe staging td-ui, the image entry, and the
   recipe check that develops the synthetic frame in the built artifact.
8. Later: the 100% loupe from level 0, DNG and JPEG rolls, the Nikon High
   Efficiency codec, a better full-resolution demosaic, highlight
   reconstruction, the `.dtstyle` translator, and ratings.
