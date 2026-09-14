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

Implemented: the bounded TIFF container reader (`tiff`), the NEF reader over it
(`nef`: the raw sub-image, the embedded previews, the exposure facts, and the
maker-note white balance, black level, sensor crop and linearization table), the
Nikon Huffman decoder for every tree dcraw names with the 14-bit lossless tree
verified against a real Z 8 frame and the others against the test encoder only,
the camera table with the Z 8's colour matrix (`camera`), the linear colour math
and the sRGB transfer (`color`), the look format with its built-in set (`look`),
the superpixel demosaic, area resampler and headless development pipeline with
the look in it (`develop`), the RGB image buffers and PPM writer and reader
(`image`), the baseline JPEG decoder for the embedded previews with its
reduced-transform scaling (`jpeg`), the thumbnail rule and the thumbnail cache,
the library's sidecar grammar, roll rules and dating rule (`library`), the cull
controller over td-ui's driven seam with its action table and scene (`ui`), the
window over it with its thumbnail pool and control socket (`window`), and the
command line `td-photo probe FILE`, `td-photo develop FILE OUT.ppm`, `td-photo
thumb FILE OUT.ppm`, `td-photo cache`, `td-photo looks`, `td-photo import SRC
DEST`, `td-photo list ROLL`, `td-photo flag FILE`, `td-photo edit FILE`,
`td-photo open ROLL`, `td-photo --replay`, `td-photo --preview` and `td-photo
--help actions`. Every read of a camera file is bounded by `MAX_FILE_BYTES`, of
a sidecar by `MAX_SIDECAR_BYTES` and of a look by `MAX_LOOK_BYTES`, trusting
neither the length the file system reported; `develop` and `thumb` refuse an
`OUT.ppm` (or `OUT.ppm.tmp`) that already exists rather than replace it, and
`import` a copy that differs, publishing the finished temporary by a hard link
so a name that appeared meanwhile is not replaced either; the sidecar is the one
file td-photo replaces, and only through its own temporary. The crate depends on
td-ui, by path, for the driven seam, the raster and bands the scene is laid out
with and the Wayland client the window runs on; the window has no develop mode
yet: the increments at the end schedule the rest in order.

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
links, after a second check. The sidecar, td-photo's own file, is the one
exception: its temporary is renamed over it, and only after the old one
was read whole and accepted. It opens no network connection and runs no
subprocess. A camera
file is untrusted input: every offset, count and dimension is checked
against a ceiling before it sizes an allocation or indexes a buffer, and a
malformed file is an error naming what was refused, never a panic or an
unbounded read.

Production code has no `unwrap`, `expect`, panics or panicking indexing;
the crate root forbids `unsafe`, and its confinement tests pin that td-ui
is its one dependency and which files name which of its modules.

## Workflow

One window, three modes, all keyboard-first; the pointer does the same
things, and so does an agent, through the same actions (see Driving). The
modes are the photographer's order of work.

1. **Import** copies raw files from a source folder (a mounted card) into
   the library. `td-photo import SRC DEST` does the same headless.
2. **Cull** shows a roll as a grid of thumbnails made from the camera's medium
   embedded preview. The photographer walks it with the arrows, `Home`, `End`
   and the page keys, presses `p` to pick, `x` to reject and `u` to clear,
   `Return` to see one photo at the medium preview's full size and `Return` or
   `Escape` to go back, and `1` to `4` (or the bar) to show all, the picks, the
   rejects or the unflagged. `Delete rejected` moves the rejects and their
   sidecars into the roll's `rejected/` folder; nothing is unlinked.
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
sides. The shape is td-ui's driven seam (td-ui/DESIGN.md, "The semantic
seam"), which td-photo is the first consumer of: a display-independent
dispatcher, a headless replay of it, and a control socket on the live
window, all speaking the toolkit's one vocabulary.

- **One dispatcher.** Everything the window can do is an `Action`, a closed enum
  in `ui` (open a roll, the cursor moves, select, pick, reject, unflag, the four
  filters, the single view and back, scroll, quit; develop mode's actions,
  export and delete rejected join it in their increments). `ui::Controller`
  holds the model (the roll's names and sidecars, the cursor, the filter, the
  view, the scroll and the surface, and the shown list the filter admits, kept
  rather than rescanned); `action(name, fields)` and `input(Input)` apply one
  action to it and return the outcome and the `Effect`s the adapter carries out
  (`Open` this folder, `Flag` that photo). The keyboard bindings, the pointer
  hit-testing, the replay stream and the control socket are four adapters over
  that one dispatcher, and nothing reaches the model around it. The cursor is
  always among the shown or nowhere: a filter or a flag that hides it moves it
  to the first shown photo, and the single view ends when there is none. The
  controller reads no file, clock or descriptor; `main`'s `Session` is the
  adapter that reads rolls and writes sidecars for it, and implements td-ui's
  `driven::Controller`, so the toolkit's generic verbs route to it. A flag is
  set on the sidecar as the file holds it when the action arrives, not on the
  copy the model took at open, so an edit made meanwhile is kept and a sidecar
  that became malformed refuses the flag; the adapter then settles the model
  from what it wrote, or from the file when it could not, which is `refused`;
  the model changes when it is settled and not before, so a refused flag leaves
  the model, the cursor and the generation as they were unless the file itself
  had changed. The dispatch asks for every flag, even one the model thinks the
  photo has or one whose sidecar it holds as refused, since the file may differ
  from its copy: the adapter answers `ignored` when the file already holds the
  flag, refuses a sidecar it cannot read, and the model takes the file's word
  either way. A roll of more than `MAX_PHOTOS` originals, or holding more than
  `MAX_SIDECAR_TOTAL` of sidecar text between them, is `refused` rather than
  held, and the budget holds at every settle as at open: a flag that would take
  the roll past it is refused before anything is written, and a sidecar that
  grew past it meanwhile is not held, its photo shown as refused.
- **A headless verb for every durable effect.** Whatever an action does to
  files is also a command-line verb: `import`, `list`, `flag`, `edit`
  (get and set of a sidecar's values), `develop`, `export`, `thumb`,
  `looks` and `cache clear`. Verbs are the batch face: they read the same
  sidecars and write them the same way, and an agent that does not need
  to see pixels never opens a window.
- **`--replay`: the window without a display.** `td-photo --replay [--size WxH]
  [ROLL]` reads requests on stdin and answers on stdout through td-ui's replay
  runner and `driven::request`, in the envelope td-ui/DESIGN.md defines: a
  four-byte big-endian length, then one tab-separated ASCII line `1 ID verb
  args...`, answered by `1 ID ok ...` or `1 ID error CODE HEX`. The same
  controller runs and the frame is painted through the toolkit's raster into
  memory. The vocabulary is the seam's: `state`, `actions`, `action NAME
  ARGS...` for every action by its table name, `key HEX_CHORD`, `pointer PHASE X
  Y`, `wheel ROWS COLUMNS`, `resize W H SCALE`, `focus`, `tick`, `text` (the
  scene read back as a cell grid), `frame` (the frame's size and digest) and
  `frame-page` (its pixels in pages); and td-photo's own `photo N` (the Nth
  shown photo's name in hex, flag, exposure, crop, look, sidecar state and, for
  a refused sidecar, the reason in hex) and `wait-idle MS`: `ok idle` once no
  job is outstanding, the wants are computed for the model as it stands and
  the frame the compositor acknowledged (its frame callback) is the model's,
  `ok busy` at the deadline, `MS` at most `MAX_WAIT_MS` (4,000, under the
  transport's five-second deadline per request, so the reply is written before
  the connection expires); the replay, with nothing outstanding, is idle at
  once. `state` is `cull`, the roll's path in hex, the
  photo count, the shown count, the cursor's position among the shown, the
  filter, the view (`grid` or `single`), then the photo under the cursor (its
  name in hex, since the envelope is ASCII and a file name need not be, then
  flag, exposure, crop, look, sidecar state), the outstanding job count as the
  window last reported it (the turn before) and the frame generation, `-` for
  what is absent. The generation moves on a change and
  on nothing else: not on a step at an end, a filter, view or size already set,
  a refused open, or a refused flag that leaves the file as the model held it; a
  settle that brings a file changed meanwhile is a change. A flag the adapter
  wrote answers `changed` whether or not the model moved, since the file did.
  Error codes are stable (`no-roll`, `no-photo`, `bad-argument`, `refused`, and
  the transport's `protocol` and `limit`); a refusal's reason goes to stderr,
  since the line carries the code. `action quit` answers `quit` and the runner
  keeps answering; the window closes on it.
- **`--control-socket PATH`: the same vocabulary on the live window,** served
  from a private (0600) Unix socket by td-ui's bounded worker over
  `driven::Payload`, bound before the display is connected so a bad path fails
  before a window appears: one request per connection, a deadline per request,
  at most `CONTROL_JOBS_PER_TURN` (8) requests admitted per turn, and a response
  that reflects the state after the turn that applied it; a `wait-idle` not yet
  idle is held, `MAX_WAITERS` (7) at most, one of the worker's eight connections
  kept free so that an eighth is admitted and answered `limit` at once, and
  answered when idle or at its deadline (`wait-idle 0` while busy is `busy` at
  once); `action quit` closes the window a grace (`QUIT_GRACE_MS`, 500 ms of the
  turn clock, enough for the seam's largest reply, a `frame-page` of 256 KiB in
  hex, at the pace the worker writes) after its reply, admitting nothing more
  meanwhile and answering the waits it holds, since the toolkit's worker drops a
  reply still in flight when it closes. Pixels of the live window are evidence
  only through the
  compositor's capture channel, observed before and after as
  td-compositor/AUTOMATION.md prescribes; `frame` over the socket paints the
  scene as the seam paints it, fills and glyphs, so the thumbnails the window
  blits are not in its digest until an image primitive is promoted into the
  toolkit (see Window).
- **Determinism.** The same requests over the same roll give the same
  `state`, and with `wait-idle` between them the same `frame`; the tests
  hold a scripted session to a pixel oracle.
- **One table.** The vocabulary lives in `ui::BINDINGS`, td-ui `Binding` rows:
  action name, the key it binds, argument shape and a help line, held to the
  seam's grammar by `driven::check` in `tests/ui.rs`, which also pins that every
  action either binds a key or takes an argument and is reached by the pointer
  (`select` by a press on a cell, `scroll` by the wheel) or is the agent's
  (`open`). `td-photo --help actions` prints the table so an agent can read it
  instead of guessing.

## Files

The library is folders of originals; there is no database.

- **Roll**: one folder of originals. Import files a photo under
  `DEST/YYYY/YYYY-MM-DD/NAME` by its `DateTimeOriginal`, read from the Exif IFD
  alone so a file that is not a whole NEF still dates itself, or under
  `DEST/undated/` when it has none or the value is not exactly `YYYY:MM:DD
  HH:MM:SS` naming a calendar date and a time of day (a camera whose clock was
  never set writes blanks in that shape). Import looks eight folders deep under
  the source (a card keeps its photos a few folders down) and follows no link
  under it; the source itself may be one, as a mounted card often is. A roll is
  listed by name, regular files only, and a folder of more than 100,000 entries
  is refused; a supported file is one whose extension is `nef` or `NEF` (JPEG
  and DNG are later increments), and `NAME.part`, a dotfile or a name with a
  control character in it is not one.
- **Import** copies through `NAME.part` in the destination folder, syncs, then
  links it to `NAME` and unlinks `NAME.part` (the publication every write but
  the sidecar's uses, with the same fallback on a file system without links). A
  destination that already exists with the same length and identical bytes,
  compared a piece at a time, is skipped and counted; one that differs, is not a
  regular file or cannot be read, a `NAME.part` left by an earlier run, and a
  `NAME` that appears between the check and the link are each reported as a
  conflict with why, left alone, never overwritten; a source that cannot be read
  is reported as unread; and the run fails once the rest is done, so a script
  notices and nothing is lost. The source is never written.
- **Sidecar**: `NAME.ext.edit` beside the original, UTF-8 text, one
  `key value` pair per line:

  ```text
  td-photo edit 1
  flag pick
  exposure -0.33
  crop 0.1000 0.0500 0.8000 0.9000
  look classic-chrome-like
  ```

  `flag` is `pick` or `reject`, absent when unflagged; `exposure` is stops with
  two decimals in -5.00..=5.00, `-0.00` not a spelling of zero; `crop` is `x y w
  h` as fractions of the oriented image with four decimals, all in 0..=1, `w`
  and `h` at least 0.05; `look` is a look's file stem, 1 to 64 bytes of ASCII
  letters, digits, `-`, `_` and `.`, not starting with `.`. A key is 1 to 32
  bytes of lowercase ASCII letters, digits and `-`, starting with a letter, and
  a value is one or more characters with no control character and no space at
  either end: the grammar a later version's keys must keep. A line whose key is
  unknown is preserved verbatim and rewritten in place, so a later version's
  keys survive an earlier one's edit; a known key given twice, a blank line, or
  a line that is not `key value` is a fault. A sidecar over 64 KiB or 1024
  lines, a first line other than `td-photo edit 1`, or a malformed known value
  is refused as a whole and the photo is shown with camera defaults and an
  error, never with half its edits; `list` shows the error, and `flag` and
  `edit` refuse to rewrite it. A name at the sidecar's place that is not a
  regular file (a link, a fifo, a folder) is refused the same way, checked by
  name before the open, with the window between the two that the Cache bullet
  names. Writes go through `NAME.ext.edit.tmp`, synced and renamed into place:
  the sidecar is td-photo's own file and the one it replaces, and the temporary
  is created exclusively, so a stale one is reported rather than reused or
  removed; an edit that would take the sidecar past the ceilings above is
  refused before anything is written, so what td-photo writes it reads.
- **Rejected** originals move, with their sidecars, into `rejected/`
  under the roll. Export writes into `exported/`. Neither folder is
  listed as part of the roll.
- **Cache**: `$XDG_CACHE_HOME/td-photo` when that variable is absolute
  (`~/.cache/td-photo` otherwise), holding `thumbs/` and nothing else in version
  1. The base directory is the user's and may be a symlink; `td-photo` and
  `thumbs` are the cache's own and must be real directories, since `cache clear`
  unlinks inside them, so a symlink in either place is refused rather than
  followed. A thumbnail is `thumbs/KEY-N.ppm`: `KEY` is the FNV-1a-64 hex of the
  original's resolved path, byte length and modification time, `N` the long
  edge, and the file is the PPM `image::write_ppm` writes, published by the link
  rule through a temporary named for the filling process (`KEY-N.ppm.PID.tmp`),
  so two processes filling one entry never contend for a name and the first to
  publish wins. The stamp is taken before the lookup and again from the open
  file after the read, and an entry is stored only if the two agree, so the
  bytes cached are the file the key describes (a stamp that differs or cannot be
  taken stores nothing and says nothing: the next run keys the file as it now
  is); an original edited or replaced, as far as length and modification time
  tell, keys itself anew. Reading back accepts only a regular file of that exact
  shape under 64 MiB with a long edge of at most `N`; a symlink, directory or
  device in an entry's place is a miss and left alone, and an entry of the right
  kind with the wrong content is unlinked so the miss refills it. The cache is
  an optimisation: whatever it cannot do (a refused directory, a full disk, a
  permission) is a note on stderr and the thumbnail is still written from the
  original; the window fills and reads it the same way, at `THUMB_WIDTH` by the
  surface's scale for the long edge, so a roll culled once opens from the cache.
  `td-photo cache clear`
  unlinks the entries and temporaries named that way (and only those) and
  nothing in the library changes; `td-photo cache path` prints the directory as
  the bytes it is. The windows between a check and the operation it guards are
  those of any program without directory descriptors: the directories are the
  user's own cache and configuration, and the only name ever unlinked is one
  of the cache's own shape.
- **Looks** are read from `$XDG_CONFIG_HOME/td-photo/looks/*.look`
  (`~/.config/td-photo/looks/` without it) on top of the built-in set the binary
  carries; a user look of the same stem shadows the built-in one. A link in the
  directory is followed, since a configuration directory is often linked from
  elsewhere, and a link to nothing is an unreadable look of the user's, not an
  absent one; a fifo or a folder at a look's name is refused before it is
  opened, with the window between that check and the open the one above; a
  directory that cannot be resolved or read is reported on stderr and the
  built-in set is what there is.

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
`THUMB_WIDTH` (160) at the surface's scale, which on the Z 8 is its 160x120
thumbnail as it is; the single-photo view's preview is the develop
increment's and asks for N = 1600, which the same rule answers with the
1620x1080 preview at 1/1. `td-photo thumb FILE OUT.ppm [--long-edge N]
[--cache]` is the rule headless (N defaults to 400: on the Z 8 the
1620x1080 preview at 1/4, 405x270, resampled to 400x267 in 80 ms), and
`td-photo probe FILE` prints every
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

Steps 3 and 4 fold into one 3x3 matrix; step 5 is the look's operations in file
order, each a 3x3 matrix, a curve as a table over a log-spaced domain or a
luminance mix (Looks, below); step 6 is the table. The cost per pixel without a
look is twelve multiplies (three for the white balance, nine for the matrix), a
clip, three table loads and a few adds; a look adds nine multiplies per matrix,
six loads and a few multiplies per tone or three-channel curve, and a few
multiplies (one division for a luminance curve) per mix or luminance step; which
is what makes the preview redraw at frame cadence over a whole canvas on one
core and in a few milliseconds across the pool.

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

A look is a text file, `stem.look`, one operation per line, applied in file
order after step 4 of the pipeline. A file over 4 KiB is refused before it is
read as text; a byte that is not UTF-8 or a control character other than the
newline is refused by the line it is on; a first line other than `td-photo look
1` is refused as such, with no line; then, by line, a blank or indented line, an
unknown word, a number outside `-?D+(.D+)?` (at most 24 characters), a second or
empty or over-long `name` (1 to 64 characters, for the look list), or a value
outside its range below. `look` parses the bytes and builds the tables; `main`
reads the files.

```text
td-photo look 1
name Classic Chrome-like
primaries 0.92 0.06 0.02  0.03 0.94 0.03  0.02 0.06 0.92
tone contrast 1.35 toe 0.0 shoulder 0.0
curve luma 0 0  0.25 0.22  0.75 0.78  1 1
saturation 0.85
monochrome 0.30 0.59 0.11
```

- `primaries` is a 3x3 matrix, row-major, each row summing above zero and
  divided by its sum so neutral stays neutral, its entries then within -4..=4
  (checked after the division, so a row's gain on a channel is at most 12
  whatever scale it was written at);
- `tone` is the sigmoid `y = (1 + k) x^c / (x^c + k)` with `k = g^c (1 - g) /
  (g - g^c)` for middle grey `g` (0.1845), so black, grey and white are fixed
  and `contrast` (`c`, in 0.5..=3.0; 1 is the identity) steepens or flattens the
  curve about grey; then `toe` and `shoulder` (each in -1..=1) reshape the
  halves about the same fixed points: below grey `y' = g (y / g)^p`, above grey
  `y' = 1 - (1 - g) u^q` for `u = (1 - y) / (1 - g)`, the exponents easing
  quadratically from 1 at grey to `2^toe` at black and `2^shoulder` at white (`1
  + (2^t - 1) v^2` for `v` the distance from grey as a share of the half), so a
  positive toe deepens the shadows and is flat at black, a positive shoulder
  lifts the highlights into white and is flat there, a negative one does the
  reverse, the join at grey is smooth whatever the two are (slope 1 on both
  sides), and each half is monotone throughout the ranges;
- `curve` is a monotone cubic (Fritsch and Carlson's tangents) through 2 to 16
  points in `0..=1` with strictly ascending `x`, holding the first and last
  value outside them, for one of `r`, `g`, `b`, or `luma`, which moves the
  pixel's Rec. 709 luminance `Y` to `f(Y)` with its chroma scaled by `f(Y) / Y`
  when that darkens it and kept when it brightens it, `rgb' = f(Y) + (rgb - Y)
  min(f(Y) / Y, 1)`: a highlight keeps its hue as it rolls off, a lifted black
  takes black to the grey `f(0)` and a near-black hue to nearly that grey rather
  than to a vivid colour, and the rule is continuous, with no case at zero;
- `saturation` scales chroma about Rec. 709 luminance, `rgb' = Y + s (rgb -
  Y)`, in 0..=2;
- `monochrome` collapses to the weighted sum of non-negative weights summing
  above zero, renormalized.

`tone` and `curve` clamp their input to `0..=1` (exposure can have taken a
channel past 1 before them) and are tabulated once when the look is parsed, in
f64, over a log-spaced domain: 16 octaves below 1 with 128 nodes each, keyed by
the float's exponent and top mantissa bits, linear between nodes and from zero
to the first, so a pixel costs two loads and a few multiplies per channel and no
call. The table is the curve's resolution: it passes its points to within the
node interval, and a feature narrower than that (two knots closer than 1/128 of
an octave, under one percent of the value) is smoothed over the interval. A
table that is not finite refuses the look; with the knots the grammar admits (a
secant near 1e22 beside one near 1e-22) f64 keeps every tangent finite, so that
refusal is a guard. Matrices and mixes are applied as they are. Every operation
is bounded (a matrix's gain on a channel by 12, a saturation's by 3, a
per-channel curve's and a mix's by 1, and a luminance curve's output by 1 plus
twice the pixel's largest channel), so sixteen of them keep any pixel the
pipeline can produce finite in f32, and step 6 clips.

At most 16 operations, a 4 KiB file. The built-in set is `contrast-boost`,
`contrast-soft`, `mono` and a Fujifilm-inspired family (`provia-like`,
`velvia-like`, `astia-like`, `classic-chrome-like`, `classic-neg-like`,
`eterna-like`, `acros-like`), each authored by hand in this format and carried
as a constant of `look` (no `include_str!`). The jssfr.de darktable styles that
motivated them are built from darktable's `primaries`, `colorcontrast`,
`colorbalancergb`, `agx` and `monochrome` modules; td-photo does not execute
darktable's pipeline and does not claim to reproduce those styles. A converter
that reads a `.dtstyle` and emits the nearest `.look` for that module subset is
a later increment and is a translation the user runs, not a runtime dependency.

`td-photo looks` lists every look, one per line, tab-separated: the stem, `user`
or `built-in`, and the name (`-` without one) or `error` and why a user file is
refused (a `.look` whose stem the sidecar grammar cannot hold is listed quoted
and escaped, so a tab or a newline in a file name is still one record); the
user's directory is the one Files names, a user look shadowing the built-in of
its stem, and a directory that cannot be resolved (no absolute `XDG_CONFIG_HOME`
or `HOME`) or read, or that holds more than `MAX_ENTRIES` entries, is reported
on stderr and the built-in set listed. A link in the directory is followed, and
a link to nothing is the user's file, unreadable, not a stem the user has no
look for. `td-photo looks STEM` prints that look's text, so a built-in is copied
into the user's directory to start from. `td-photo develop FILE OUT.ppm --look
STEM` develops with it, resolved the same way: a user look that cannot be read
or does not parse refuses the develop by file and line and does not fall back to
the built-in it shadows, and a stem outside the sidecar's look grammar is
refused before anything is read.

## Window

The scene is `ui::Scene`, a td-ui `Composition` the controller builds over its
model per request: the filter bar (`chrome::Bar`, the active filter in brackets
and the others padded to its width, so the headers keep their places), the grid
or the single view, and the status row (`chrome::Status`: the roll's folder, the
counts, the filter, the photo under the cursor with its flag, `(sidecar
refused)` when it was, `single` in that view). A grid cell is `CELL_W` by
`CELL_H` (176 by 152) reference pixels at the surface's scale: a 160 by 120
thumbnail box under `CELL_PAD` (8) of padding, a `P` or `X` badge at its corner
for a flagged photo, and the name under it, a reject's dimmed; the cursor's cell
wears a two-pixel selected frame. Cells fill whole rows from the top-left, as
many columns as the width holds and as many rows as the height between the bands
holds, at least one of each, so a surface too small for a cell clips one rather
than shows none, and the grid scrolls by rows, keeping the cursor's row shown.
The single view shows the name, the facts and the largest 3:2 box under them. A
press on a bar header sets the filter, on a cell selects it, and in the single
view anywhere in the area between the bands returns to the grid; the status row,
and anything off the surface, is not a target, and the bands are hit-tested last
painted first, so on a surface too short for both the status row covers the
bar's headers as it covers their pixels. A box whose thumbnail is not held is a
neutral placeholder, which is what `frame` digests either way; the single view's
box stays one until develop mode's preview.

`td-photo open [ROLL] [--control-socket PATH]` runs the window (`window`), a
`td_ui::client::App` in the shape td-setup's is, and one adapter over the same
`Session` the replay drives (see Driving): `event` maps the compositor's
configure to a resize, a key to its chord (a held move or page repeats through
the toolkit's repeat; a flag, a filter, a view or quit fires once), a left press
to the pointer path and the wheel's frames to `scroll`, and `end_turn` takes the
pool's results, asks for the thumbnails the model wants and serves the socket.
It owns no Wayland objects of its own. A frame is presented whenever the
generation submitted is not the model's: the scene through the toolkit's raster,
then each held thumbnail centred in its box through `ui::blit`, a clipped XRGB
blitter into the same frame inside `present`'s closure, because the toolkit's
raster is fills and glyphs, clipped to the grid's area so a box that runs under
the status band on a short surface leaves the band alone, then the flag badges
again (`Controller::badges`, the scene's badge draws alone, painted within the
grid's area as the blits are, so over the scene's own frame they change
nothing), since a thumbnail covers the corner the scene painted its badge in.
The generation on screen is the one the compositor
acknowledged with its frame callback, which is what `wait-idle` waits for; a
generation is submitted once. When a second consumer needs an image primitive it
is promoted into `td_ui::raster` with a pixel oracle; until then the blitter is
this crate's, in `ui` where its pixel oracle is, and the confinement test pins
that the window writes frame bytes through nothing else. `td-photo --preview WxH
[ROLL]` is the same frame without a display, its thumbnails made on the calling
thread: the oracle the native test holds a capture to.

Performance contract: no decode or resample runs on the turn loop's thread. The
roll's listing and its sidecars' reads and writes do, as they do in the replay,
bounded by `MAX_PHOTOS` and `MAX_SIDECAR_TOTAL`; a worker for them is a later
increment's if a roll shows the need. Workers post results; `end_turn` drains
them, marks damage and, while
any job is outstanding, sets the transport wait to at most 16 ms so results
appear within a frame of arriving; with nothing outstanding the wait is the
toolkit's idle wait. Thumbnails are requested for the rows on screen first, then
the screen below and the one above; a scroll that makes a request stale drops it
before it starts. A thumbnail is asked for at the box's width by the rule `thumb
--cache` applies, so the disk cache is the verb's exactly, and shrunk in memory
to the box for a shape taller than it (`develop::shrink`). A grid cell whose
thumbnail is not ready paints a neutral placeholder and its name, never blocks.

## Concurrency and memory

- One pool of `available_parallelism` workers, at most 16
  (`develop::MAX_THREADS`), started at window open and joined at close, over one
  queue the turn loop replaces under its lock whenever the model's generation
  moves, so a request the model no longer wants is dropped before it starts;
  jobs are `Thumbnail` now, and `Preview`, `RawDecode`, `Level1` and `Export` in
  their increments. A job stays outstanding, in the pool's running set, until
  the turn loop collects its result, so the job count never reads zero with a
  thumbnail made and not yet held. A finished thumbnail is kept whichever wants
  asked for it, since it is the file's; the later jobs' results the turn loop
  compares before applying. Thumbnails are held in memory by name for the roll
  that is open at the surface's scale (a job is keyed by roll, name and scale,
  so another roll's file of the same name is another thumbnail; the held set is
  let go when either changes), `THUMB_CACHE_BYTES` between them, one that could
  not be made charged its name and slot alone, the least recently shown that is
  not on screen evicted first; an eviction recomputes the wants, so a thumbnail
  let go while still wanted off screen is asked for again.
- At most one `RawDecode` runs at a time (the codec is sequential); demosaic
  and resampling split rows into bands on one shared queue that the calling
  thread and its scoped helper threads drain together, so no thread
  outlives the call and a band count that exceeds the threads is shared out.
- Budgets are named constants the tests pin: `RAW_CACHE_BYTES` 512 MiB,
  `THUMB_CACHE_BYTES` 256 MiB in memory, `MAX_FILE_BYTES` 512 MiB,
  `MAX_RAW_SAMPLES` 128 Mi, `MAX_PREVIEW_SAMPLES` 128 Mi over a preview's padded
  component planes, `MAX_TABLE_DEFINITIONS` 32, `MAX_IFDS` 64, `MAX_ENTRIES`
  4096, `MAX_PHOTOS` 100,000 originals in an open roll and `MAX_SIDECAR_TOTAL`
  64 MiB of sidecar text between them, `MAX_WAIT_MS` 4,000 ms for one
  `wait-idle`, `CONTROL_JOBS_PER_TURN` 8 requests a turn, `MAX_AXIS` 16384 for
  raw and image axes alike, `MAX_IMAGE_PIXELS` 64 Mi for any one image buffer
  (768 MiB as `f32` RGB, under a 32-bit target's allocation limit), `MAX_RANGE`
  32768 for a decoder parameter set's sample range. Eviction is least recently
  shown.
- Buffers are allocated once per size and reused: the level-2 and level-3
  buffers per canvas size, the decoder's output per raw geometry.

## Invariants

- No original is ever modified, renamed or unlinked by td-photo. Import
  copies; culling moves into `rejected/`; export writes new names.
- No existing name but a sidecar's is ever replaced: a destination is
  refused by name before any work, and the final step of every write is a
  hard link, which fails on a name that appeared meanwhile; only a file
  system that refuses links falls back to a second check and a rename.
  The sidecar is td-photo's own file, read whole and accepted before its
  temporary is renamed over it.
- Pure modules (`tiff`, `nef`, `camera`, `color`, `develop`, `image`,
  `jpeg`, `library`, `look`) read no file, environment, clock or descriptor.
  `main` and the library adapter own I/O.
- Every ceiling above is checked before the allocation or index it
  guards; a refused input names the item.
- The colour of an unknown body is refused, not guessed.
- The decoded sample of the 14-bit lossless tree is what dcraw decodes;
  the pinned real-frame fixture is the oracle and a change that moves it
  is a codec change.
- No `unsafe`, no dependency other than td-ui, no `include!`, no build
  script.

## Test contract

`tests/confinement.rs` pins the source inventory, that the crate root forbids
`unsafe`, that the manifest declares td-ui as its one dependency, the native
case and the trusted test root (the window binds its control socket under the
harness's directory in `/tmp`, and td-ui's socket refuses an ancestor the gate's
rootless namespace shows as owned by no one, so the gate's cargo-test runs take
the builder's caller-owned sticky root, as td-editor's do), no build script and
no `include!`, that the pure modules name no
`std::fs`, `std::env`, `std::time`, `std::net` or `std::process` path and the
window no file, which files name which toolkit modules, that photo pixels reach
a frame through `ui::blit` alone and the verb and the window share one thumbnail
rule, and the budgets by value.

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

`tests/look.rs` holds the look format to its refusals by line (the header, a
blank line, an unknown word, a second or over-long name, every arity and range,
a curve out of order or over the point budget, the operation budget, the size
ceiling, and a control character or a byte that is not UTF-8 on its line) and to
the number grammar; the arithmetic to hand-computed values: the identity tone
and curve within the tables' precision, the fixed points and monotonicity of
sampled contrasts, what a toe and a shoulder do to their side of grey, a curve
through its points and flat outside them, the luminance rule darkening,
brightening and lifting black, saturation at zero and two, a mix and a matrix
renormalized at any scale, sixteen of the widest operations and the steepest
admitted knots staying finite, and the file order; every built-in parsing,
within both budgets, keeping a neutral ramp neutral, monotone and pinned at
black and white; middle grey through the whole pipeline with a tone at the plain
value and a mix making a colour grey; and runs the built binary: `looks` over a
user directory with a shadowing, a nameless, a refused, a linked and a stray
file, a stem with a tab, a folder and a link to nothing at a built-in's stem,
without a resolvable directory, and with a stem, and `develop --look` refusing a
bad stem, an unknown one, an unparseable user file and a link to nothing before
reading anything. `tests/nef.rs`'s command case develops its synthetic frame
with `--look mono` to a grey image.

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

`tests/library.rs` holds the sidecar grammar to its refusals by name and to
in-place rewriting around an unknown line, the canonical value spellings, and
the roll and dating rules with a two-IFD TIFF carrying only a capture time; and
runs the built binary over a temporary library: an import dated and undated,
eight folders deep and not nine, through a linked source and past a linked
folder and file, skipped when identical, refused as a conflict when a copy
differs, a folder stands in its place or a `NAME.part` is in the way, with the
card unchanged and no temporary left; `list` with each filter; and `flag` and
`edit` writing through the sidecar, keeping an unknown line, refusing a bad
value before writing, refusing a malformed sidecar, a stale temporary, a linked
sidecar, a sidecar past either ceiling and an edit that would take one past, and
unlinking nothing.

`tests/ui.rs` holds the action table to `driven::check` and to its alignment
with `Action`, and the error codes to the code grammar; drives `ui::Controller`
in-process (the state before a roll and after, walking with every step and page,
`select`, the filters and the cursor they keep or move, the single view and back
by action and by key, unbound keys, `quit`, scrolling by action and by wheel
clamped to the roll and revealed by the cursor, resizes good and bad, the
pointer on the bar, a cell, the status row (on a surface too short for both
bands too), off the surface and past the last photo, a resize to the size it
has, an empty roll and one past `MAX_PHOTOS`; the effects a flag change asks
for, the model unchanged until they are settled and its sidecar text after, the
flag the file already holds ignored, an unknown line kept, a refused sidecar
never rewritten, a flag that hides the photo under a filter and ends the single
view, a settle that brings nothing new leaving the generation and one that
differs moving it, and the sidecar budget at open and at settle); reads the
scene back as text (the bar with its headers in place under every filter, the
names and badges by row, the status line, the single view, the empty and
filtered-out messages, a scale of 2) and holds its frame digest to equality and
to change; and runs the built binary's `--replay` over a temporary roll through
the seam's verbs, writing a pick through the sidecar, refusing to flag a refused
one, keeping an edit made meanwhile and refusing a sidecar that became
malformed, reporting a stale temporary and settling the model from the file with
the cursor and the generation unmoved, a name that is not ASCII in hex,
answering after `quit`, and refusing a roll that is not there, a bad size and a
stray argument before the session starts; holds the window's helpers (the boxes
on screen and the wants in order under a scroll, a filter and the single view,
the job count as a fact and a touch as a change, which keys repeat, the
blitter's pixels centred, clipped to the surface, the box and the area, and in
BGRX order, the badges painted back over a thumbnail that covered one and
changing nothing over the scene's own frame, on a tall surface and on one too
short for a cell where only the area's clip keeps them off the status band, and
the in-memory shrink by the thumbnail rule), `wait-idle` over the replay idle at
once with its argument
judged, `--preview` equal to the seam's frame of the empty window and of a roll
and refused for a bad size or roll, and `open` refusing a bad socket path, a
second roll or a stray flag before it looks for a display and leaving no socket
behind when the display is not there; `src/window.rs`'s own tests hold
`wait_ms`'s grammar, the envelope's ID, the charge of a held entry, the queue's
replacement skipping what runs and the pool's count staying outstanding until a
result is collected. `tests/control_process.rs` runs the built binary under the
native compositor harness the sibling crates use (`ready`
builds the compositor; the case is ignored without it): the window on a roll of
originals no decoder accepts, so the frame is the scene's, mapped with its app
id, idle over the socket, its state, its captured tile equal to `--preview` of
the roll at the tile's size under the observe, capture, observe rule, a pick
over the socket written through the sidecar and shown, a click from the seat on
the second cell and `End` from the seat each moving the cursor (repeated until
one lands, each the same cell however many land; the pointer then parked in the
desktop bar, since the seat draws the client's cursor over the tile), `first`
over the socket restoring the frame, and `quit` closing the window with its
socket gone.

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
   oracles. Landed.
4. Window, cull mode, in two landings. (a) The td-ui dependency,
   `ui::Controller` over the driven seam with its action table and
   `--help actions`, `--replay` with the seam's vocabulary over the cull
   actions, the grid with flags, filters and the single-photo view as a
   scene, and `tests/ui.rs`. Landed. (b) The window itself as a
   `td_ui::client::App`, the worker pool and the thumbnails blitted into
   the grid, `wait-idle`, `--preview`, `--control-socket`, and
   `tests/control_process.rs` under the native compositor harness the
   sibling crates use. Landed.
5. Develop mode, in two landings. (a) The look format with the built-in
   set (`look`), the look in the headless pipeline, `td-photo looks` and
   `develop --look`. Landed. (b) Levels 0 through 3 with their
   memoization, exposure, crop with the drag contract, the look list, and
   the develop actions in the table, the replay and the control socket.
6. Export: banded full-resolution bilinear demosaic, the JPEG encoder,
   `exported/` naming, and `td-photo export`; and `delete-rejected`, the
   action and its verb that move rejects and their sidecars into
   `rejected/`.
7. Packaging: the cargo recipe staging td-ui, the image entry, and the
   recipe check that develops the synthetic frame in the built artifact.
8. Later: the 100% loupe from level 0, DNG and JPEG rolls, the Nikon High
   Efficiency codec, a better full-resolution demosaic, highlight
   reconstruction, the `.dtstyle` translator, and ratings.
