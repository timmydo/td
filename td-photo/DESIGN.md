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
the look in it, and the full-resolution bilinear demosaic export runs in row
bands (`develop`), the RGB image buffers and PPM writer and reader
(`image`), the baseline JPEG decoder for the embedded previews with its
reduced-transform scaling and the baseline encoder export writes through
(`jpeg`), the AV1 still-picture encoder (`av1` over `transform` and
`cdf`) and the AVIF container (`avif`) export writes through instead when
asked, the thumbnail rule and the thumbnail cache,
the library's sidecar grammar, roll rules and dating rule (`library`), the cull
and develop controller over td-ui's driven seam with its action table and scene
(`ui`), the
window over it with its thumbnail pool and control socket (`window`), and the
command line `td-photo probe FILE`, `td-photo develop FILE OUT.ppm`, `td-photo
export FILE`, `td-photo thumb FILE OUT.ppm`, `td-photo cache`, `td-photo
looks`, `td-photo import SRC DEST`, `td-photo list ROLL`, `td-photo flag
FILE`, `td-photo edit FILE`, `td-photo open [ROLL]` (a bare `td-photo` is
`open`), `td-photo --replay`, `td-photo --preview` and `td-photo --help
actions`. Every read of a camera
file is bounded by `MAX_FILE_BYTES`, of a sidecar by `MAX_SIDECAR_BYTES` and
of a look by `MAX_LOOK_BYTES`, trusting neither the length the file system
reported; `develop` and `thumb` refuse an `OUT.ppm` (or `OUT.ppm.tmp`) that
already exists rather than replace it, `export` takes the first free numbered
name and refuses a stale `STEM.jpg.tmp`, and `import` a copy that differs,
publishing the finished temporary by a hard link so a name that appeared
meanwhile is not replaced either; the sidecar is the one file td-photo
replaces, and only through its own temporary. The crate depends on
td-ui, by path, for the driven seam, the raster and bands the scene is laid out
with and the Wayland client the window runs on; the window's develop mode
lands over its own slices: the mode, its keys and the develop edits over the
seam and the socket are in, as are the developed preview and the crop drag
with its edge and corner handles and the look palette; the `export` verb and
the window's `export` action are in, `delete-rejected` with them, and the
export view with its settings and the picks' export close the export
increment but for AVIF, whose AV1 encoder, container, `format` setting
and `--format` are in and whose view buttons follow. The crate is
packaged: the `td-photo`
target recipe builds it static over the staged td-ui and td-compositor
trees, the image copies its output and links `/bin/td-photo`, and
`td-photo-test` runs the built binary's verbs over a synthetic frame
(Packaging below).

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

One window, four modes, all keyboard-first; the pointer does the same
things, and so does an agent, through the same actions (see Driving). The
modes are the photographer's order of work.

1. **Import** copies raw files from a source folder (a mounted card) into
   the library. `td-photo import SRC DEST` does the same headless.
2. **Cull** shows a roll as a grid of thumbnails made from the camera's medium
   embedded preview. The photographer walks it with the arrows, `Home`, `End`
   and the page keys, presses `p` to pick, `x` to reject and `u` to clear,
   `Return` (or the mode strip's Single) to see one photo developed to the
   area, its edits applied, and `Return`, `Escape` or Culling to go back,
   and `1` to `4` (or the filter strip) to show all,
   the picks, the rejects or the unflagged. `Delete` (`delete-rejected`)
   moves the rejects and their sidecars into the roll's `rejected/` folder,
   each file linked there before its old name is dropped, so no file is
   lost and no name is replaced. `td-photo delete-rejected ROLL` is it headless.
3. **Develop** shows one photo developed from its raw data, entered with `d`
   on the cursor's photo and left with `Escape`. `=`/`-` move exposure by a
   third of a stop and their shifted pair `+`/`_` by a tenth, `0` resets to
   camera defaults, and the crop and the look are set by the `crop` and `look`
   actions, taking the box's four fractions or a look's stem, and the crop
   also over the preview: a marquee tightens it to a sub-region, and a
   crop-adjust sub-mode toggled with `c` (the Crop button) shows the whole
   frame with the crop's rectangle over it, a press off the rectangle (or
   inside it, off its handles, when there is no crop yet) drawing a fresh
   crop over the frame and its edge, corner and interior handles growing,
   shrinking or moving it, each saved on release while the frame under it
   stays whole -- the crop's content is what the rectangle encloses -- and
   applied to the preview when the sub-mode is left. The `aspect` action locks
   the crop drag to a ratio (free, 3:2, 4:3, 1:1 or 16:9), held in the preview's
   own pixel space, so a locked marquee, corner or edge drag keeps that ratio;
   picking a ratio only arms the lock. A look palette toggled with
   `l` lists the available looks with the current one marked, and a press on a
   name picks it. Two bands above the preview carry the same controls as
   buttons: a tool band with Crop (the crop-adjust toggle, shown selected
   while it is on), Uncrop (`C`, clearing the crop), Undo, Reset, the
   exposure's `-` and `+` beside a slider over the exposure's whole range,
   and Fit and 100% (the zoom, the one in force selected), and
   a look band with None and every available look, the current one selected,
   the first nine on `F1`..`F9`. The preview fits the box until it is
   zoomed: `Z` (or the 100% button) shows the photosites around the centre
   one a pixel, the wheel or `]` and `[` step through 25, 50 and 100
   percent from the fit and back, `f` (or Fit) fits again, and a drag over
   the zoomed box pans it. A filmstrip under the preview shows the shown
   photos around the cursor's, `Left` and `Right` or a press moving along it.
   The status row names the sub-mode in force (`develop crop-adjust`, `develop
   looks`) and the zoom (`develop 100%`). The immediate snap that reshapes
   the current crop and the scaled
   preview under the tighten marquee are later slices. Every change is saved
   to the sidecar as it is made, as a step of the photo's history (below);
   there is no explicit save.
4. **Export**, with `e` on the cursor's photo in the grid or in develop
   mode, renders the full-resolution raw through the same pipeline and
   writes an sRGB JPEG into the roll's `exported/` folder, never
   overwriting: a second export of the same name takes a numbered suffix
   (Files, below). The status row says what came of it. `td-photo export
   FILE` is it headless. The export view, entered with `E` (or the mode
   strip's Export) and left with `Escape`, shows the settings every export
   is written with -- the JPEG quality on a slider, the long edge the
   image is shrunk to on a row of buttons (Full, 1024, 2048, 4096) --
   kept in the user's settings file between sessions (Files, Settings),
   and its Export picks button (`C-e` anywhere) exports every pick of the
   roll with them, the status row counting them as they land.
   `td-photo export-picks ROLL` is that headless, the settings on its
   command line.

## Driving

td-photo is operated by a person at the keyboard and by an agent acting
for that person, and the design makes those one thing seen from two
sides. The shape is td-ui's driven seam (td-ui/DESIGN.md, "The semantic
seam"), which td-photo is the first consumer of: a display-independent
dispatcher, a headless replay of it, and a control socket on the live
window, all speaking the toolkit's one vocabulary.

- **One dispatcher.** Everything the window can do is an `Action`, a closed enum
  in `ui` (open a roll, choose one, the cursor moves, select, pick, reject,
  unflag, the four filters, the single view and back, scroll, quit, enter
  develop and its exposure nudges and absolute `exposure`, look and the
  look shortcuts `look-1`..`look-9`, crop and `uncrop`, crop-adjust, aspect,
  the look palette (`looks`), reset, undo and the history's step toggle and
  delete, the zoom (`zoom-fit`, `zoom-100`, `zoom-in`, `zoom-out`), export,
  the export view (`export-mode`), its settings (`quality`, `long-edge`),
  the picks' export (`export-picks`), and delete rejected).
  `ui::Controller` holds the model (the roll's names and sidecars, the cursor,
  the filter, the view, the scroll and the surface, and the shown list the
  filter admits, kept rather than rescanned); `action(name, fields)` and
  `input(Input)` apply one
  action to it and return the outcome and the `Effect`s the adapter carries out
  (`Open` this folder, `List` it for the chooser, `Flag` that photo, `Expose`
  by a delta, `Edit` a crop or look, `Reset` to camera defaults, `Undo` the
  last step, `StepToggle` and `StepDelete` a step, `Export` that photo,
  `ExportPicks` every pick, `Settings` the export settings).
  The keyboard bindings, the pointer hit-testing, the replay stream and the
  control socket are four adapters over that one dispatcher, and nothing
  reaches the model around it. The cursor is
  always among the shown or nowhere: a filter or a flag that hides it moves it
  to the first shown photo, and the single view ends when there is none, as
  develop mode does, both falling back to the cull grid. The
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
- **Three modes, and a strip that names what is in view.** The controller
  is in `cull`, `develop` or `export`; `develop` is a view of one photo,
  the cursor's, its history pane beside it, entered
  with the `develop` action (`d`) and left with `grid` (`Escape`), which
  returns to the cull grid; `export` is the roll's export view (below),
  entered with `export-mode` (`E`) and left the same way. The mode strip
  (td-ui's `chrome::Buttons`, the window's first band: Roll Selection,
  Culling, Single, Develop, Export)
  shows which is in view (cull's grid or single view apart), the roll
  chooser while it is open and none before a roll,
  and a press on it is the one target every mode shares (`press_mode`):
  Roll Selection is `choose`, Culling closes the chooser or leaves
  develop whole (its palette, crop-adjust and any drag dropped, as `grid`
  does once they are down) or the export view to the grid, Single does
  the same to the single view of the cursor's photo (what `view`,
  `Return`, toggles from the grid), Develop closes the chooser and
  develops the cursor's photo, and Export closes the chooser and shows
  the export view. The
  one in view is `ignored`, as is a disabled button: Culling and Export
  before a roll, Single and Develop before a photo (`mode_states`);
  `filter_states` disables
  the filter strip in develop, in the export view and under the chooser,
  where the filters are not the mode's, its buttons inert. The develop
  edits are that mode's alone and are `ignored` in cull and export; the
  cull filters, `delete-rejected` and the single-view toggle are cull's
  and are `ignored` in develop and export. Exposure is a
  delta, `expose-in`/`expose-out` a third of a stop and
  `expose-in-fine`/`expose-out-fine` a tenth, added by the adapter to the
  file's exposure as it stands (concurrency-correct as a flag is, not the
  model's copy) and clamped to `MAX_EXPOSURE`, so a step that clamps to no
  change writes nothing and is `ignored`, unless a file changed meanwhile
  makes the settle a change, as it does for a flag; `look` and `crop` are
  absolute values the adapter sets,
  their grammar judged in the dispatch so a stem that is not a look or a box
  under the minimum edge or outside the image is `bad-argument` before any
  write; `reset` clears the develop keys, and the history with them, and
  keeps the flag. Losing the cursor,
  when a flag hides the last shown photo, drops develop back to the cull grid as
  it ends the single view.
- **The history pane.** Every develop edit is a step of the photo's sidecar
  history (Files, Sidecar; a run of edits to one key is the one step it
  began), and develop mode shows that history in a pane at
  the area's left (td-ui's `chrome::List`, `PANE_W` (216) reference pixels
  wide, above a band of three buttons: Toggle, Delete, Undo, a td-ui strip
  on the pane's width, wrapping where it is short), the steps
  oldest first as `KEY VALUE` (`-` a clear; a crop as `x,y wxh` in whole
  percents, `ui::step_label`, so it fits the row), a step that is off
  dimmed, one selected. The selection is the model's (`state` reports the
  step count and the selected index): the newest step when a photo is
  developed, when the cursor moves to another photo, and whenever a settle
  brings a longer history (a step just added) or one as long whose last
  step differs (taken up by a run of its key), else the one selected,
  clamped as steps go; none without a step, and dropped on leaving develop. In
  develop `Up` and `Down` walk the selection (the rows are the history's, not
  the grid's; `Left` and `Right` still move the cursor), `Ignored` at an end
  or with no step, and a press on a shown step selects it (a crop drag in
  progress owns the pointer wherever it goes, so its release over the pane
  ends it). `undo` (`z`) asks for the last step back, `step-toggle` (`t`) for
  the selected step off or back on, `step-delete` (`Backspace`) for it
  deleted; the pane's buttons ask the same. Each is a develop edit like the
  others: `Ignored` in cull, and with no selection for the step actions; asked
  of the adapter, which applies it to the sidecar as the file holds it
  (`Undo`, `StepToggle`, `StepDelete` effects through the one `edit` path a
  flag takes), settles the model from what it wrote and answers `ignored` when
  the file had nothing to take or no such step, `changed` when it did. The
  pane and its buttons are the frame's: a selection move, a step toggled and
  the list itself are witnessed by the scene, so the replay `frame` and
  `--preview` carry them.
- **The develop controls.** Two bands lead the develop region, above the
  preview: the tool band (`Layout::tool_band`, the region's first rows)
  with `TOOL_BUTTONS` -- Crop, Uncrop, Undo, Reset, `-`, `+`, Fit, 100%
  (`TOOL_COUNT`, 8) -- a td-ui
  `chrome::Buttons` strip on the region's width, wrapping to more rows
  where the width is short, and after them the exposure slider (td-ui's
  `chrome::Slider`, `EXPOSURE_STEPS` (100) steps of a tenth of a stop from
  -5.00 to +5.00, `ui::exposure_value` placing the sidecar's exposure on
  it to the nearest step and `ui::exposure_at` reading a step back) from
  the last button to a cell short of the region's right on that row when
  the travel gives every step its own column, else on a row of its own
  across the region; and the look band (`Layout::look_band`, the rows
  under it) with `NO_LOOK` (None) then a button per available look, in
  the order the session reported them, a strip wrapping the same way. The
  tool band is as many rows as its strip takes and the look band as many
  as its strip takes while the view under both keeps a name row and a box
  a thumbnail tall (`ROW + FILM_H`), one row at least (`Layout::with_looks`
  lays the look band for the session's looks, the layout the controller
  uses; `Layout::new` for none), and the view is under them, so a narrow
  window wraps its buttons rather than cuts them and a short one keeps
  its preview; a button the surface cannot hold whole, or on a row cut at
  the region's foot or for the view's room, is not laid (those looks are
  the palette's and the keys'), and the slider needs every tool button,
  the room td-ui asks and a column of travel per step (td-ui's `travel`
  contract: with fewer, a press on the knob's own centre would read as
  another step), on the buttons' row or its own.
- **The zoom.** The develop box fits the whole image until it is zoomed:
  the model's `zoom` is `None` (the fit) or a percent of full resolution
  (a photosite is `zoom / 100` pixels) around a `centre`, ten-thousandths
  of the oriented, cropped image on each axis (`develop::CENTRE_UNIT`),
  the middle until a pan moves it. `zoom-100` (`Z`, the 100% button) and
  `zoom-fit` (`f`, Fit) set it; `zoom-in` (`]`) and `zoom-out` (`[`) walk
  `develop::ZOOM_STEPS` (25, 50, 100) from the fit's own percent
  (`develop::fit_zoom`: the lesser axis ratio of the box to the extent,
  floored and half at most, since the fit never enlarges the
  half-resolution level 1, so an image the box holds whole fits at 50 and
  its one step in is 100), in to the first step past the zoom in force,
  out to the last short of it and past the fit, else the fit, `ignored`
  past either end (`ui::zoom_step`); the wheel in develop does the same,
  up a step in and down a step out, one a turn whatever its size (the
  event carries no position, so it is the mode's wherever the pointer
  is). The ladder needs the
  photo's extent, a fact the window reports from the levels it holds
  (`set_zoom_extent`, `None` until a develop of the photo lands, no more
  moving the generation than the fit does), so a step before it lands is
  `ignored`; `zoom-100` and `zoom-fit` need it not. The zoom is develop's
  alone (`ignored` in cull), kept across photos, dropped to the fit with
  the centre at the middle when develop is left, when a roll opens and
  when crop-adjust is entered, whose canvas is the whole frame (the zoom
  actions, the wheel and the two buttons are off while it is on, Fit not
  selected either; under the look palette the wheel is inert too, the
  grid it would scroll being out of view); a zoom the same as the one in
  force is `ignored`. A change is a frame change before the develop
  lands: the status row names the zoom (`develop 100%`), Fit is selected
  while fitting and 100% at 100 (`tool_selected`), and `state` reports it
  (`fit`, or `PERCENT@X,Y` with the centre); a crop drag in progress is
  dropped, the canvas it was taken against being gone. Zoomed, the box shows a
  window of the image, not a canvas a crop maps onto, so the pointer pans
  instead of cropping: a press in the box anchors the pan (`Ignored`, as
  arming a marquee is), a move shifts the held image by the pointer's
  travel (a frame change; `pan_shift`, which the window blits by, the box
  clipping it), and the release moves the centre against the travel at
  `100 / zoom` photosites a pixel as a fraction of the extent, so what was
  under the pointer stays under it, then the develop at the moved centre
  follows; the centre is held where the window lies inside the image, half
  the box's photosites from either edge and the middle where the window
  covers the axis (`clamp_centre`: on a zoom change, before a release's
  travel is applied, and when the extent or the surface changes, the
  centre a photo switch or a resize left outside the room being brought
  in without a bump, since the develop clamps its window the same way and
  no pixel moves), so a pan has no dead travel at an edge, and with the
  extent not yet known the release moves nothing. A pan in progress owns
  the pointer as a crop drag does, and is dropped as one is: with the
  photo, the mode, the surface or the zoom. A click moves nothing; a
  release with travel is a frame change whether or not the centre moved:
  the pointer's shift ends and the window blits the held image by the
  held window's offset from the wanted one instead (Window), so the image
  stays where the pointer left it until the develop lands.
  Crop toggles crop-adjust and is selected while it is on; Uncrop (`C`,
  the `uncrop` action) clears the crop through the same `Edit` the `crop`
  action makes; Undo and Reset ask what `z` and `0` do; `-` and `+` nudge
  the exposure a third of a stop as `-` and `=` do; a look button sets that
  look (None clears it) as the `look` action does, and `F1`..`F9` (the
  `look-1`..`look-9` actions) pick the first nine, `Ignored` past the
  list. Uncrop is enabled only with a crop, Undo and Reset only with a
  step; a press on a disabled button, on a band's chrome, and a move or a
  release over a band are `Ignored`, and never start a crop drag. The
  `exposure` action takes the sidecar's own spelling (`-1.25`), bad-argument
  otherwise, and sets it through an `Edit` like a look. The slider: a
  press on it moves the knob to the pointer's step and starts a drag that
  follows the pointer's column wherever it goes, each step it crosses a
  frame change and no write; its release commits the step it rests on as
  the `exposure` action would, or writes nothing when that is the step the
  exposure in force rounds to (then a frame change only if the drag had
  moved the knob; a nudge's off-step exposure stays as it is). A release
  that commits is a frame change on its own when the knob had moved, since
  the knob paints the held value again from there whatever becomes of the
  write. A photo switch, a removal, the chooser opening, a resize or leaving
  develop drops the drag as it drops a crop's, and a drag in progress, a
  crop's or the slider's, owns the pointer over the bands and the pane. All
  of it is `Ignored` in cull. The status row names the sub-mode after the
  mode: `develop crop-adjust` or `develop looks`.
- **The filmstrip.** Under the develop view, at the foot of the region, a band
  `FILM_H` (a thumbnail and its padding) tall (`Layout::film_band`) carries
  the shown photos in thumbnail boxes (`Layout::film_boxes`: `THUMB_WIDTH` by
  `THUMB_HEIGHT` from a cell in, a cell between, as many as the band holds
  whole), the cursor's box outlined in its padding and kept centred as the
  ends allow, so `Left` and `Right` walk the strip as they walk the shown, and
  the roll of picks is the strip under the picks filter. The band is laid only
  when it can hold a box whole and the view above it keeps a name row and a
  box at least a thumbnail tall; a shorter or narrower surface keeps the foot
  for the view. A press on a box selects its photo as `select` does (`Ignored`
  on the cursor's own; an open look palette closes with it, as on any photo
  switch); the band's other pixels, and a move or release over it, are inert,
  and a crop drag in progress owns the pointer over it. The strip's boxes are
  the window's `visible` in develop, as the grid's cells are in cull: the
  window blits the thumbnails it holds into them and repaints the badges over
  them, `--preview` the same, and the chooser withholds the boxes as it
  withholds the grid's; `wanted` in develop is the strip's boxes, then as many
  shown after them, then before, so a cursor move finds its neighbours made,
  and none without a strip (the wants stand under the chooser, as the grid's
  do).
- **Export is the roll's, not develop's.** `export` (`e`) asks for the
  cursor photo's export in any mode; the dispatch is `changed` with the
  effect and moves nothing in the model. The adapter reads the sidecar as the
  file holds it when the action arrives (a refused sidecar refuses the action
  before anything is read) and runs `export_file`, the verb's own runner: the
  replay on the request, answering `changed` with the JPEG written or
  `refused` with the reason on stderr; the window through its pool (Window),
  answering `changed` as the job is queued, `wait-idle` waiting for it. What
  came of it is the status row's export note (`exporting NAME`, `exported
  NAME.jpg`, `export of NAME failed`, `export of NAME refused`): the adapter's
  report through `set_export`, a fact absent from `state` like the job count,
  but one the row shows, so a note that differs from the one held moves the
  generation and the same note again does not; it is cleared when a roll
  opens, and the row's note is the open roll's: an export of a roll opened
  before that finishes after the switch is noted on stderr instead. Every
  export is written with the export settings in force (below), read from
  the model when the request is made.
- **The export view.** `export-mode` (`E`, the strip's Export) shows,
  over the area in place of the grid, the settings every export is
  written with and the picks' export; it needs a roll (`no-roll`), is
  `ignored` when it is in view, leaves develop whole as Culling does and
  ends the single view, and `grid` (`Escape`) returns to the grid; a
  roll opening leaves it too. It is the roll's, not the cursor's: the
  cursor's moves still walk the shown (`Left`, `Right`, the pages), a
  cursor lost to a flag does not end it, and the wheel and `scroll`,
  `view` and the grid's own actions are `ignored`. The settings are the
  `format` (`jpeg` or `avif`, JPEG at first; the window exports the
  file's format while the view's text still says JPEG, since the view's
  format row and the `format` action are the next landing), the
  `quality` (1 to 100,
  `jpeg::QUALITY`, 92, at first; the JPEG tables' scale or the AV1 step)
  and the `long-edge` (`full`, or 1 to `settings::MAX_LONG_EDGE`, 16384,
  pixels on the long side, the source's own size at first) every export
  is shrunk to, never enlarged; `state` reports the quality and the long
  edge as its last two fields and all three are kept across rolls and
  modes. The quality and the long edge are each set by their action
  in any mode (`quality N`, `long-edge N|full`, `bad-argument` outside
  its grammar): the value in force is `ignored` with nothing asked,
  else the dispatch is `changed` with a `Settings` effect carrying the
  whole settings, the adapter writes them to the user's file (Files,
  Settings) and settles the model through `set_settings`, a change and
  a bump when they differ from those held, and a file that cannot be
  written is `refused` with the reason on stderr, the model as it was.
  The view (`Layout::export_panel`, down the area a cell in from each
  side) is a text row counting the picks and naming where they go, a
  row naming the quality, the quality slider (td-ui's `chrome::Slider`,
  `QUALITY_STEPS` (99) steps, a quality each, across the width when the
  travel gives every step its own column, else none), a row naming the
  long edge, the long-edge strip (`LONG_EDGES`: Full, 1024, 2048, 4096,
  the setting's selected, none when the action set another size,
  wrapping on the width) and the Export picks button, enabled while the
  model holds a pick; a control the area cannot hold whole is not laid.
  The slider drags as the exposure slider does: a press moves the knob
  to the pointer's step and starts a drag that owns the pointer wherever
  it goes, each step crossed a frame change and no write, and the
  release commits the step it rests on as `quality` would, or writes
  nothing when it is the setting's own step; a long-edge button sets
  that edge as `long-edge` would; the view's other pixels, the filter
  strip and a move or release are inert. Leaving the view drops a drag.
  `export-picks` (`C-e`, the button) asks for every pick's export in any
  mode with a roll: as `delete-rejected` asks, the files say which
  photos are picks, so the dispatch asks whenever a roll is open with
  an `ExportPicks` effect and the adapter reads the roll's picks as
  their sidecars flag them (`read_picks`), `ignored` when they hold
  none. Each pick is an export as `export` makes one, with the settings
  in force, in the roll's order: the replay runs them in turn on the
  request, `changed` when all landed and `refused` when any failed
  (each reason on stderr), the note set once; the window queues them to
  its pool behind any export already asked for, `changed` as they are
  queued, and counts them as they land (`Batch`, the roll's: a pick
  whose sidecar is refused between the listing and its request is
  counted failed at once; one refused at the listing is no pick, as it
  is no reject to `delete-rejected`): the note is `exporting pick N of
  M` while any is outstanding, then `exported K of M picks` with `, F
  failed` when any did, an export of the batch never noted on its own
  but on stderr when it failed; a second batch of the same roll asked
  for while one runs queues behind it and the counts add up, while a
  roll opening leaves the batch to finish on stderr (its note the
  batch's, as a single export's is) and the next batch is the new
  roll's own, the old tally dropped with it (its landings counted
  nowhere, a failure still noted).
- **Delete rejected is the files'.** `delete-rejected` (`Delete`) is the
  cull grid's action, as the filters are, so develop mode ignores it; with
  a roll open the dispatch asks whenever it is called, since the files, not
  the model's copies, say which photos are rejects. The adapter runs the
  verb's mover over the roll as it stands (Files, Rejected) and takes the
  photos it moved out of the model through `remove`: the shown list is
  recomputed, the cursor keeps its photo when that stays and otherwise its
  position among the shown, clamped to the end, or leaves when none is
  shown, ending the single view and the drag (a crop's or the slider's),
  crop-adjust, aspect lock and look palette that were the moved photo's,
  as a filter that hides it does. The reply is `changed` when any moved,
  `ignored` when the files hold no reject, and `refused`, each reason on stderr,
  when one was kept or moved without its sidecar, the ones that moved taken out
  all the same, so the model holds what the roll lists. The window keeps a moved
  name's thumbnail and cached level 0 until they are evicted, as it does
  for a file replaced in place.
- **The roll chooser is the finder.** `choose` (`o`) opens td-ui's shared
  directory finder (td-ui/DESIGN.md, "Shared directory finder") over the
  area, in place of the grid or the view, whatever mode is open behind it;
  the action closes an open one (the key cannot, since every key is the
  finder's then; `Escape` does). The dispatch asks the adapter to `List`
  the open roll's parent with the roll selected, or its working directory
  when none is open; path semantics are the adapter's: `list` makes the
  path absolute (so an ascent has a parent and the model holds one
  spelling), takes the parent when asked (the root's parent being the
  root, a name that is not text selecting nothing) and reads the folder
  (`list_folder`: its subfolders, a link followed only to learn it is one
  and marked `link`, and its originals marked `original` and disabled, for
  what the folder is rather than as targets, folders first and each
  sorted, at most `finder::ENTRIES` listed entries, what is not listed
  not counted, and `finder::LISTING_BYTES` of their text before the
  listing is cut short; an entry gone between
  the read and its type, or a name that is not text, left out), and
  installs it through `set_listing`, which opens the chooser (refused when
  the area cannot hold a finder, so nothing opens) or replaces its
  listing, a change either way; a folder that cannot be read is `refused`
  with the reason on stderr and, when a chooser is open, in its status row
  through `note_listing`, which fits the reason to the finder's bound
  keeping its tail (the reason follows the path), blanks control
  characters, and leaves the generation alone for the note it already
  shows. While it is open every chord is the finder's (`Up`, `Down`,
  `PageUp`, `PageDown`, `Home`, `End`, `Return` to descend, `Backspace` to
  edit the filter or with none to ascend, `M-Up` or `^` to ascend filter or
  none (the caret is the ascent's, so a caret in a name cannot be filtered
  by; the rest of the name can), `C-Return` to open the folder in view as
  the roll, `Escape` to close, any other single printable character, a
  plain space among them, for the filter; anything else `ignored`), so a
  letter filters rather than flags; a held key repeats by the chooser's
  own rule (`chooser_repeats`: the moves, a typed character but the
  caret, and `Backspace` while there is a filter to edit, since a held
  one on an empty filter, or a held `M-Up` or `^`, would run up the tree;
  the window asks it again before each repeat it delivers and stops the
  repeat the rule no longer allows), and every press and wheel is the
  finder's too, so the filter strip's buttons and the cells under it are
  no targets (the mode strip's are, being every mode's); of the actions
  only `choose` (closing it), `open`, `quit` and `scroll` (its wheel)
  reach through, the rest is `ignored`. A descent asks for the folder
  under the cursor and an ascent for this folder's parent with it
  selected (nothing
  above the root), each `changed` without moving the generation, as `open`
  is, since the frame changes when the adapter installs the listing or
  notes the refusal; opening the folder in view is the same `Open` effect
  the `open` action makes, so the roll opens as it always does, or is
  refused as it always is, the chooser gone either way; a roll opening
  closes it too, and a resize lays it out again over the new area, closing
  it when the area can no longer hold it. `state` reports the listed
  folder's path in hex as its last field (`-` when closed): the chooser's
  own selection and filter are the finder's, read back through `text`.
  Its boxes, the develop preview and the overlays are withheld while it is
  open (`visible` is empty and `develop_box` is `None`), so the window
  blits nothing over it, and the status row carries the prompt, short
  enough for the default width whole (Return enter, Backspace or ^ up,
  C-Return open here, Escape cancel, type to filter: 97 of the 98 cells
  the default width paints, so a longer prompt reds the read-back rather
  than eliding) in place of the roll's facts, since the finder paints no
  affordance of its own. The chords it names are spelled as td-ui's
  keymap spells them, pinned by translating each through the keymap.
- **A headless verb for every durable effect.** Whatever an action does to
  files is also a command-line verb: `import`, `list`, `flag`, `edit`
  (get and set of a sidecar's values), `delete-rejected`, `develop`,
  `export`, `export-picks` (both taking `--format`, `--quality` and
  `--long-edge` on the command line, at their defaults without; the
  settings file is the window's), `thumb`, `looks` and `cache clear`.
  Verbs are the batch face: they read the same sidecars and write them
  the same way, and an agent that does not need to see pixels never
  opens a window.
- **`--replay`: the window without a display.** `td-photo --replay [--size WxH]
  [ROLL]` reads requests on stdin and answers on stdout through td-ui's replay
  runner and `driven::request`, in the envelope td-ui/DESIGN.md defines: a
  four-byte big-endian length, then one tab-separated ASCII line `1 ID verb
  args...`, answered by `1 ID ok ...` or `1 ID error CODE HEX`. The same
  controller runs and the frame is painted through the toolkit's raster into
  memory. The vocabulary is the seam's: `state`, `actions`, `action NAME
  ARGS...` for every action by its table name, `key HEX_CHORD`, `held PREFIX`,
  `pointer PHASE X Y`, `wheel ROWS COLUMNS`, `resize W H SCALE`, `focus`,
  `tick`, `text` (the
  scene read back as a cell grid), `frame` (the frame's size and digest) and
  `frame-page` (its pixels in pages); and td-photo's own `photo N` (the Nth
  shown photo's name in hex, flag, exposure, crop, look, sidecar state and, for
  a refused sidecar, the reason in hex) and `wait-idle MS`: `ok idle` once no
  job is outstanding, the wants are computed for the model as it stands and
  the frame the compositor acknowledged (its frame callback) is the model's,
  `ok busy` at the deadline, `MS` at most `MAX_WAIT_MS` (4,000, under the
  transport's five-second deadline per request, so the reply is written before
  the connection expires); the replay, with nothing outstanding, is idle at
  once. `state` is the mode (`cull` or `develop`), the roll's path in hex, the
  photo count, the shown count, the cursor's position among the shown, the
  filter, the view (`grid` or `single`), then the photo under the cursor (its
  name in hex, since the envelope is ASCII and a file name need not be, then
  flag, exposure, crop, look, sidecar state), the outstanding job count as the
  window last reported it (the turn before), the frame generation, the
  chooser's listed folder in hex, then the history's step count and the
  selected step's index, then the zoom (`fit`, or the percent and the
  centre as `PERCENT@X,Y`, `fit` in cull too), then the export quality
  and the long edge (`full` or the pixels); `-` stands for a roll, a
  photo, a chooser or a selection that is absent. The mode is `cull`,
  `develop` or `export`. The generation
  moves on a change and on nothing else: not on a step at an end, a filter, view
  or size already set, a refused open, or a refused flag that leaves the file as
  the model held it; a settle that brings a file changed meanwhile is a change.
  A crop set over the develop preview is witnessed by the frame, not `state`:
  the generation moves exactly when the painted outline does. The tighten
  marquee arms and rubber-bands as before (a zero-edge, invisible rectangle
  changes nothing, a visible one and any press or release that removes a painted
  one are changes), committing the sub-region as the `crop` action does. The
  crop-adjust sub-mode toggled with `c` paints the crop's rectangle and its
  eight handles: entering or leaving it, and a handle drag that moves the
  rectangle, are changes, while grabbing a handle at the rectangle it already
  shows is not; its release commits the resized or moved crop, or clears it
  when the rectangle covers the whole frame. A press on the canvas off the
  rectangle -- or inside it, off its handles, when the crop is the whole
  frame, whose interior moves nothing -- draws a fresh crop instead: the
  press paints nothing (the crop stays shown until the marquee has size),
  the drag rubber-bands the marquee as the crop's rectangle, handles and all
  (`crop_adjust_rect`; it is not the tighten marquee `crop_drag` reports), and
  the release commits the marquee's fractions of the whole frame the sub-mode
  shows as an absolute crop, replacing the old one rather than tightening it,
  `None` when it covers the frame; a click or a sub-minimum marquee commits
  nothing and the crop's rectangle returns; a release that commits is a frame
  change when the rectangle it leaves differs from the one shown, whatever
  becomes of the write. Leaving the sub-mode drops a drag in progress. The
  sub-mode's canvas is the uncropped frame's reported fit alone, never the box:
  the window reports a fit only for an image developed at the crop the mode
  wants, so while the uncropped frame is still being made the box shows the
  cropped one with no overlay, no press draws or grabs, and the overlay
  appears with the frame it maps onto (toggling or escaping the sub-mode
  drops the fit held, so a press in that turn finds none). The `aspect` lock
  shapes the drag geometry, not the frame directly: a locked drag moves the
  outlined rectangle, already a change, while arming a ratio paints
  nothing. A look palette, toggled with `l`, lists the available looks over the
  develop box with the current one marked (on a panel of the box's rows across
  the develop region, `look_panel`; a row a name each, down a column and on
  beside it in the next, each column as wide as its longest name, so a list
  taller than the panel is not cut at its foot; a column that starts within
  the panel is cut at its right as a long name is, one past it neither
  painted nor a target; `look_rows`, which the painter and the pointer
  share): opening or closing it over a non-empty
  list is a change (the status row names the sub-mode whether or not there
  is a box to list into, as it names crop-adjust), over an empty list
  nothing. A press on a name picks that look through the same edit the `look`
  action makes, so the mark follows the edit's settle; the palette stays open.
  So the aspect lock, the crop-adjust and look-palette sub-mode flags and the
  developed image's fitted rectangle for the drag's canvas (the window's report)
  are none of them `state` fields -- all facts like the job count, so they never
  move the generation on their own. The available look list (reported once when
  a session opens) is a fact too, but the look band paints it, so a list that
  differs from the one held moves the generation when develop is in view, as an
  export note does. A flag the adapter wrote answers `changed` whether or not
  the model moved, since the file did. Error codes are stable (`no-roll`,
  `no-photo`, `bad-argument`, `refused`, and the transport's `protocol` and
  `limit`); a refusal's reason goes to stderr, since the line carries the code.
  `action quit` answers `quit` and the runner keeps answering; the window closes
  on it.
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
  action either binds a key, takes a typed argument (`look` a stem, `aspect` a
  ratio, `quality` and `long-edge` a size) or is reached by the pointer
  (`select` by a press on a cell, `scroll` by the wheel, `crop` by a
  marquee or a crop-adjust handle over the develop preview) or is the
  agent's (`open`; `choose`, `o`, is the person's way to one).
  `td-photo --help actions` prints the table so an agent can read it
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
  letters, digits, `-`, `_` and `.`, not starting with `.` and not the bare
  `-`, the clear wherever a look is set. A key is 1 to 32
  bytes of lowercase ASCII letters, digits and `-`, starting with a letter, and
  a value is one or more characters with no control character and no space at
  either end: the grammar a later version's keys must keep. A line whose key is
  unknown is preserved verbatim and rewritten in place, so a later version's
  keys survive an earlier one's edit; a known key given twice, a blank line, or
  a line that is not `key value` is a fault. The develop history follows the
  lines as `step-N on|off KEY VALUE` (`N` from 1 in order without a leading
  zero, `KEY` a develop key, `VALUE` in its grammar or `-` for a clear):

  ```text
  step-1 on exposure -0.33
  step-2 off look mono
  step-3 on crop 0.1000 0.0500 0.8000 0.9000
  ```

  The develop keys are the history's summary: the steps that are on, folded
  in order with the last word on each key winning, rewritten from it on
  every change, so a reader that knows only the keys sees the settings in
  force. A file with a history is read by it, its summary rewritten where it
  disagrees; one without and with develop keys (a file from before there
  was a history) seeds one step per key in the file's order, written at
  its next rewrite (and counted toward the roll's sidecar budget as it
  will be written), so every edit from then on is a step, or takes up
  the last as one does. The history is always written after the lines: a
  file with lines after its steps is read all the same and rewritten
  with them before. Setting a develop key to a value is a step: when the
  last step is on and sets the same key to a value, that step takes the
  new value (a run of exposure nudges, of crops or of looks is one step,
  undone as one, its row in the pane showing the value in force);
  otherwise a step is added, so a step that is off, or one of another
  key, is never taken up. The last step is the file's, whichever session
  or `edit` wrote it: a run continues across them, as the pane shows it.
  A clear is a step of its own, never taken up and never taking a value
  up, so undoing an uncrop brings the crop back. A value the key already
  holds is no step; the flag is a cull decision, never a step; and a
  history of `MAX_STEPS` (128) refuses a further step until one goes
  (its last step still takes a value up). The `step-` key prefix is the
  history's: a `step-` line that is not a step is a fault, not an
  unknown line kept.
  `undo` takes the last step back, a step may be turned off (its
  key falls back to the earlier step's value, or clears) or deleted (the
  later ones closing up), and `reset` clears the history with the keys. A
  `step-N` out of sequence or past the ceiling, or a step that is not the
  shape above, is a fault (`Error::Step`), as is a known key given twice, a
  blank line, or a line that is not `key value`. A sidecar over 64 KiB or 1024
  lines, a first line other than `td-photo edit 1`, or a malformed known value
  is refused as a whole and the photo is shown with camera defaults and an
  error, never with half its edits; `list` shows the error, and `flag` and
  `edit` refuse to rewrite it. A name at the sidecar's place that is not a
  regular file (a link, a fifo, a folder) is refused the same way, checked by
  name before the open, with the window between the two that the Cache bullet
  names. Writes go through `NAME.ext.edit.tmp`, synced and renamed into place
  (`replace_own`): the sidecar is td-photo's own file, one of the two it
  replaces (the export settings are the other), and the temporary
  is created exclusively, so a stale one is reported rather than reused or
  removed; an edit that would take the sidecar past the ceilings above is
  refused before anything is written, so what td-photo writes it reads.
- **Rejected** originals move, with their sidecars, into `rejected/` under the
  roll: `delete-rejected` lists the roll, reads each sidecar and moves the
  photos whose sidecar says `flag reject` (a sidecar the reader refuses cannot
  say so, and that photo stays). The folder is made when the first reject is
  found, so a roll without one is left as it was, and checked by name as
  `exported/` is, the roll itself under that name (a bind mount) refused with a
  link. Each file moves by the publication rule: linked to its name in
  `rejected/`, which fails on a name that appeared meanwhile, then its old name
  dropped, so the file has a name throughout and nothing existing is replaced; a
  file system without links falls back to the check and rename `publish` uses; a
  file linked whose old name could not be dropped is reported and has both
  names, the roll listing it still, until the next ask finds the name there the
  file's own (the same inode on the same device, with two names to it) and
  finishes the move by dropping the old name. Both destinations, `rejected/NAME`
  and `rejected/NAME.ext.edit`, are refused by name before either moves (a name
  that cannot be looked up refuses too, and so does a stale `NAME.ext.edit.tmp`,
  the sign of an interrupted write the user should look at) and the photo is
  kept with why; then the original moves, then its sidecar, and a sidecar that
  could not follow is reported with where its original went, the photo moved
  without it. The verb prints `moved NAME`, `moved NAME without its sidecar:
  WHY` or `kept NAME: WHY` per reject and fails after the rest when any was kept
  or split, as `import` does; the window's adapter notes the same line on
  stderr, so a kept photo, still whole in the roll, is told apart from a split
  one. Neither that folder nor `exported/` is listed as part of the roll.
- **Export** writes `exported/STEM.jpg` beside the roll, `STEM` the
  original's name without its extension, or `STEM-2.jpg`, `STEM-3.jpg` and
  so on: the first free number from 2 when the plain name is taken, at most
  `MAX_EXPORT_NAMES` (1000) tried, and a gap left by a removed export is
  filled before a new number is taken. The folder is created, once the raw
  is decoded and the export planned, when absent; anything else at its
  name, a link included, is refused. The stream is developed and coded a
  band at a time into a fresh `exported/STEM.jpg.tmp`, created exclusively
  (a stale one is reported, not reused or removed), synced, then linked to
  the first free name, the same publication as `develop`'s: a name that
  appears between the check and the link is skipped for the next, never
  replaced. The export takes the sidecar's exposure, crop and look as the
  file holds them; a sidecar the reader refuses, or a look it cannot find,
  refuses the export before anything is written, since developing at camera
  defaults would silently drop the edits. It is written at the export
  settings' quality and shrunk to their long edge (Colour and development,
  Export).
- **Settings**: `$XDG_CONFIG_HOME/td-photo/export` (`~/.config/td-photo/
  export` when that variable is not absolute), the export settings the
  window keeps between sessions, UTF-8 text in the sidecar's shape
  (`settings::Settings`):

  ```text
  td-photo export 1
  format jpeg
  quality 92
  long-edge 2048
  ```

  `format` is `jpeg` or `avif`, `quality` is 1 to 100 and `long-edge` 1 to
  `MAX_LONG_EDGE` (16384) or `full`, each at most once and at its default
  (JPEG, 92, `full`) when left out; a file past
  `settings::MAX_BYTES` (4096), not UTF-8, without the header, with a line
  that is not `key value`, a key this version does not know (the file is
  td-photo's own, so an unknown key is another version's) or a value
  outside its grammar is refused whole: the session starts at the
  defaults with the reason on stderr, and the next change replaces the
  file; anything but a regular file at its name (a fifo, a folder, a
  link) is refused by name before the open, so a session cannot block on
  it, and before a write. It is read once when a session starts (the
  replay and the window alike) and written, whole, on every change the
  export view or the `quality` and `long-edge` actions make, through
  `export.tmp` as the sidecar is written (`replace_own`; the folder made
  as needed; a stale temporary is reported, not reused or removed, and
  refuses the change): the last write wins. The verbs never read it;
  `export` and `export-picks` take `--format`, `--quality` and
  `--long-edge` on the command line.
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
thumbnail as it is; the single view shows no thumbnail but the raw
developed to its box, as develop does (Window). `td-photo thumb FILE
OUT.ppm [--long-edge N]
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
3; a crop set over the preview outlines its rectangle and commits the crop
on release, which reruns level 2 as any crop edit does, so no develop runs
mid-drag. The tighten marquee outlines over the cropped preview; the
crop-adjust sub-mode develops the uncropped frame instead (crop `None`, the
byte-identical path), so its handles can grow the crop past its current
edges. Entering and leaving the sub-mode flip the crop and rerun level 2;
a handle release commits the crop but the develop stays uncropped (the crop
is still `None`), so it reruns nothing, and the cropped result appears when
the sub-mode is left. Showing the previous level 2
scaled under the outline while the drag is live, so the crop previews before
release, is a later slice.
Resizing the window recomputes level 2. Switching photo recomputes level 1
from the cached level 0, or decodes when the photo is no longer cached. The
window holds these levels and this memoization from increment 5(d)'s second
slice, described here; its first slice developed the preview correctly but
reran the whole pipeline on each edit. Its third slice prefetches the
neighbouring level-0 frames in the background (Concurrency and memory), so
a cursor move to a neighbour reruns level 1, not the codec, as a return to
a recently shown photo does; the raw cache holds the current photo, those
recently shown and the neighbours decoded ahead, evicting the least
recently shown under `RAW_CACHE_BYTES` (a prefetch never evicts: it is
taken only into room). Level 1 is superpixel
(each 2x2 CFA quad becomes one RGB
pixel: exact colour, no interpolation, a quarter of the samples), which is
the right demosaic for every on-screen size below half resolution; export
runs the full-resolution demosaic
in row bands from level 0 so peak memory stays bounded by the band, never
by the frame (Export, below), and a zoom past `HALF_ZOOM` (50) runs it on
the box's window alone. A zoomed level 2 is the window of the image the
box shows at the zoom (`develop::viewport`: `box / (2 * zoom / 100)`
level-1 pixels on each axis, rounded up, cut to the crop's oriented
rectangle and moved inside it, centred as near the centre as the edges
allow, then mapped back through the orientation as the crop is, so a
window at half zoom is level 1 as it is), resampled to the window at the
zoom, no more than the box (the rounding up can overshoot it by a pixel,
left out rather than resampled away, so 100% is a photosite a pixel on an
odd box too), and oriented: from level 1 through half zoom
(`zoom_level2`), where level 1 has every pixel the box shows, and from
level 0 past it (`zoom_level2_full`, the window doubled onto the sensor
crop and cut to the photosites the target shows, through `bilinear`),
where it has fewer; then level 3 as the fit's is. The zoom is
a percent of the oriented, cropped image at full resolution
(`develop::extent`, two photosites a level-1 pixel), and the fit's own
percent is `fit_zoom`, the lesser axis ratio of the box to that extent,
floored.

The resampler is separable area averaging for reduction and bilinear for
enlargement, over `f32` rows, and is shared with thumbnails. It reads the
`u16` level 1 directly, each sample scaled to `f32` as it is read (the same
value the whole-frame conversion would give, in the same order), so level 2
is the only retained `f32` image buffer, for the headless verb and the
window alike (the resampler's middle pass and the orient hold transient
`f32` buffers).

### Export

`td-photo export` develops from level 0 at full resolution, one band of
output rows at a time, straight into the JPEG encoder, so the frame is
never held as RGB: level 0 (the decoded CFA frame, 87 MiB on a Z 8) plus
one band is the peak (an AVIF adds a few copies of its coded stream at
the end, the tiles, the frame OBU and the file, since the container
names the stream's length). `develop::export_geometry` plans
it: the user crop's
fractions of the oriented frame are mapped back through the inverse of the
orientation to a `Region` of the sensor crop by the mapping `level2`
applies to level 1 (the same rounding at twice the scale, so the export
and the preview select the same region to within a level-1 pixel), the
whole crop without one, and the output's axes are the region's turned by
the orientation. `develop::export_band` makes output rows `first..first
+ rows`: the sensor region those rows come from (the band's rows upright,
the mirrored rows for a half turn, a run of columns for a quarter turn,
the inverse of `orient`'s per-pixel map applied to a band) through
`develop::bilinear`, turned, then `level3` at the sidecar's exposure and
look. The bands concatenate to the same bytes whatever their size, and an
uncropped export turned by `orient` is the turned export; the tests pin
both. `main` feeds `EXPORT_BAND_ROWS` (64) rows a band, a multiple of the
encoder's block row and enough of them that a band's transform spreads
over the pool.

A long edge under the export's shrinks it: `image::shrunk` sizes the
output (the longer axis to the edge, the other to the nearest pixel of
the same ratio, one at least; the source's own size when the edge covers
it, since an export is never enlarged), and `image::Shrink` is the area
resample fed the same source bands of `EXPORT_BAND_ROWS` rows, giving
back the output rows each band completes for the encoder, so the peak
stays level 0, one source band and the rows in progress whatever the
ratio (a source row lies in at most two output rows; `main::Output` is
the encoder the settings' format picks, fed and drained the same way,
the export named `STEM.jpg` or `STEM.avif` by it). Each output pixel
is the mean of the source box it covers, a source pixel on the box's
edge weighted by the part of it inside, in whole arithmetic (`span`: the
shares of an output pixel over the source pixels it covers, in units of
`1 / out`, summing to the source axis; with the axes swapped, the output
rows a source row lies in) rounded half up, so the rows are the same
bytes whatever the bands and a shrink to the same size is the identity;
the tests pin the weights, a flat frame, a checker's mean, every band
size giving the same rows, the rows coming out as they complete with at
most two in progress, and the refusals (an axis past its source or zero,
a band of another width or past the foot). The encoder is opened at the
output's size and the settings' quality.

The bilinear demosaic makes one pixel per photosite: its own channel the
sample as it is, each other channel the rounded mean of that channel's
neighbours in the 3x3 around it (on a Bayer grid the four axial neighbours
or the four diagonals at a red or blue site, a facing pair at a green
one); a neighbour outside the sensor crop is left out of the mean, so an
edge pixel averages the neighbours it has. It is black-subtracted and
scaled like `superpixel`, a level 1 at full resolution, and a region of it
is that window of the whole, read at the sensor's CFA phase, not the
crop's. A better full-resolution demosaic is a later increment.

The encoder (`jpeg::Encoder`) is baseline: JFIF, YCbCr 4:4:4 (one block
per component per MCU, so a band of eight rows is a row of MCUs and the
right edge and bottom pad by replicating the last column and row), the
T.81 Annex K quantisation tables scaled at the settings' quality
(`QUALITY`, 92, at first) the way every encoder since IJG scales them
(`5000 / q` below 50, `200 - 2q` from 50 up, entries held to 1..=255)
and the Annex K.3 Huffman tables, written into the stream, no restart
intervals. The colour transform is the JFIF one the
decoder inverts; the forward transform is the decoder's `T8` basis
transposed, in `f32`, rows then columns; coefficients are held to the
categories the tables carry (the DC to -1024..=1023, each AC to
-1023..=1023), and a symbol no table carries would be refused rather than
coded as a bare run of bits; the last byte is padded with ones. It is fed
rows in any number at a time and drained of bytes as they are made, so the
stream is written as the frame is developed; the transform and
quantisation of a band run across the pool through `develop::bands`, the
entropy coding is sequential, and the bytes are the same whatever the band
or thread count. The decoder is its oracle: a ramp at `QUALITY` round-trips
within 12 of every 8-bit value and noise at quality 100 within the colour
transform's rounding, a grey frame stays exactly grey, a black frame at
quality 100 (the largest DC category through coder and decoder) exactly
black, and the standard tables are pinned complete (every baseline DC
category and AC run/size once) with no code of all ones, which is what the
decoder refuses; the clamp is pinned by a unit test over `forward`.

The AVIF export codes the frame with the crate's own AV1 encoder
(`av1::Encoder`, over `transform` and `cdf`), pure `std` like the rest:
a still picture (`still_picture` and `reduced_still_picture_header`
set), main profile, 8-bit 4:2:0 at full range in BT.601 (`Y = (77R +
150G + 29B + 128) >> 8`, the chroma the mean of each 2x2 offset by 128),
one key frame of intra blocks. The tools are the subset a decoder must
carry and an encoder can verify: 64-pixel superblocks always split,
then square blocks of 32, 16 or 8 (a partition of `NONE` or `SPLIT`
alone, the frame's edges splitting as the spec forces them; no 64-wide
block is ever a leaf, since there is no 64-point transform here), one
transform the block's size (`TX_MODE_LARGEST`, the reduced transform
set, so a block's type is the one its mode implies, `DCT_DCT` at 32),
the thirteen intra modes without
an angle delta, filter, palette or chroma-from-luma, no loop filter,
CDEF, restoration, superres or film grain, and the symbol adaptation the
spec runs by default. The quality maps to `qindex` as `255 - ((q - 1) *
254 + 49) / 99`, the dead-zone quantiser rounds the DC at half a step and
each AC at three eighths, and a block's mode is chosen by rate-distortion
(`(D << 7) + (R * rdmult) >> 9`, `rdmult` from the DC step as libaom sets
it, the rate the coder's own cost of its symbols under the tile's live
probabilities): the modes are screened by the SAD of their predictions,
the best three (two for chroma, one mode for both planes) coded in full,
and a superblock's partition is decided by trials that save and restore
the contexts and pixels they touch before the chosen tree is emitted, so
the decoder's state is the encoder's exactly. The frame is tiled
uniformly by its size alone, so the bytes are the same whatever the
thread count, as the JPEG encoder's are: as many tile columns as the
width allows while the uniform column stays four superblocks wide (the
last column is the remainder), within the tile columns the level the
picture size sets allows (Annex A.3), and the fewest rows the tile area
needs, which is none up to level 6; past it a column the cap left wide
and rounded up to whole superblocks can pass the tile area, and the
rows then split again. The tiles of a superblock row are coded in
their own coders over `develop::bands`, so the threads spread over the
columns; the tile sizes are written in four bytes, and the sequence
header claims the least level whose picture size, tile count and tile
columns admit the frame. `av1::av1c` and the colour constants are the
configuration the container repeats, from the same values the sequence
header writes. The transforms are the spec's integer butterflies from
generated stage tables, so the reconstruction the encoder keeps is what
any conforming decoder shows; `tests/av1.rs` holds that to dav1d when
one is named (`TD_TEST_DAV1D`), and holds three small streams to the
hashes of bytes dav1d decoded, so a change to any emitted symbol is
verified against the decoder before the hash moves. It is fed rows like
the JPEG encoder and codes a superblock row when one is complete, so
the export band rules hold; a frame is at most 16384 on an axis. Angle
deltas, a better distortion measure, straight-line transforms and
per-tile scratch buffers in place of the per-trial allocations are the
performance and fidelity follow-ups; a 24-megapixel export takes tens
of seconds on one thread and a few on eight.

The AVIF (`avif::file`) is the least HEIF file the format asks for and
every reader expects: `ftyp` with the `avif` brand and `mif1`, `miaf`
and, to level 5.1, the AVIF Baseline profile `MA1B` compatible (the
Advanced profile names the high profile, which the stream is not, so
past Baseline no profile is claimed); `meta`
(version 0) holding `hdlr` `pict`, `pitm`
naming item 1, `iloc` (version 0, four-byte offset and length, no base
offset) with the item's one extent at its absolute offset in the file,
`iinf` with one `infe` (version 2) of type `av01`, and `iprp` whose
`ipco` carries `ispe` (the frame's size), `pixi` (three channels of
eight bits), `av1C` (`av1::av1c`: the profile and level the sequence
header claims, 8-bit 4:2:0, no configuration OBUs since the item's data
carries the sequence header) and `colr` `nclx` (the colour `av1`
declares, full range), associated to the item by `ipma` with `av1C`
essential; then `mdat` holding the sequence header and frame OBUs, no
temporal delimiter. The meta box's length is the same whatever the
offset, so it is written twice, the second time with the offset the
layout gives; sizes and offsets are four bytes, which a frame of at most
`MAX_AXIS` square keeps under. The unit tests hold every box to its
bytes, the profile brand to the level and the extent to the data;
`tests/av1.rs` reads the extent back and decodes it with
dav1d; ffmpeg 6.1's demuxer read the samples' size, range and colour as
written and its libdav1d and libaom decoders agreed on them out of band.

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
`velvia-like`, `astia-like`, `classic-chrome-like`, `reala-ace-like`,
`pro-neg-hi-like`, `pro-neg-std-like`, `classic-neg-like`, `eterna-like`,
`eterna-bleach-bypass-like`, `acros-like`), each authored by hand in this
format and carried as a constant of `look` (no `include_str!`). With `mono`
standing for Monochrome, the built-in set covers the twelve simulations of
the jssfr.de darktable style set that motivated the family (Nostalgic Neg is
not in that set). Those styles are built from darktable's `primaries`,
`colorcontrast`, `colorbalancergb`, `agx` and `monochrome` modules; td-photo
does not execute darktable's pipeline and does not claim to reproduce those
styles. A converter that reads a `.dtstyle` and emits the nearest `.look` for
that module subset is a later increment and is a translation the user runs,
not a runtime dependency. The looks are listed sorted by stem, so with more
than nine the `F1`..`F9` shortcuts reach the first nine of that order and the
band and the palette the rest (the palette laying a list taller than its
panel in columns).

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
model per request: the mode strip and the filter strip (each a td-ui
`chrome::Buttons`, the first band Roll Selection, Culling, Single and Develop
with the one in view selected, the second All, Picks, Rejects and Unflagged
with the
active filter selected, a button the mode cannot use disabled, see Driving:
the buttons keep their places whichever is active, the selection styled rather
than marked in the text; each strip wraps to more rows on a surface too
narrow for its buttons on one, the filter strip under the mode strip's rows
and the area under both), the grid or the single view (or the roll chooser's
finder over the area while one is open, see Driving), and the status row
(`chrome::Status`: the roll's folder, the counts, the filter, the photo under
the cursor with its flag, `(sidecar refused)` when it was, `single` in that
view, `develop` in develop mode). A grid cell is `CELL_W` by `CELL_H` (176 by
152) reference pixels at the surface's scale: a 160 by 120 thumbnail box under
`CELL_PAD` (8) of padding, a `P` or `X` badge at its corner
for a flagged photo, and the name under it, a reject's dimmed; the cursor's cell
wears a two-pixel selected frame. Cells fill whole rows from the top-left, as
many columns as the width holds and as many rows as the height between the bands
holds, at least one of each, so a surface too small for a cell clips one rather
than shows none, and the grid scrolls by rows, keeping the cursor's row shown.
The single view shows the name, the facts and the largest 3:2 box under them,
the cursor's photo developed into the box as develop's is (its sidecar's
crop, exposure and look applied, at the fit: the zoom, the crop drag and
the bands are develop's), so `develop_box` is that box there and the
window's develop path, its memo and the neighbours' prefetch serve both
views; develop mode shows the name row and the box (no facts row: the
bands take the
room) of the cursor's photo in the area right of the history pane
(`Layout::develop_region`), under the tool and look bands (`Layout::tools`,
`Layout::look_buttons`, see Driving) and above the filmstrip
(`Layout::film_band`, its boxes `Layout::film_boxes`; `Layout::develop_view` the
rest), its status marked `develop`, with the developed preview blitted into that
box once it is made, and the pane at the area's left: the history list
(`Layout::history`, a `chrome::List` over the pane's width, less the band
its buttons take) and, on the band under it, the Toggle, Delete and Undo
buttons (`Layout::history_buttons`, a `chrome::Buttons` strip on the pane's
width, wrapping), Toggle and Delete enabled with a selection and Undo with a
step; a surface too short for a row has neither. The box geometry
(`Layout::box_in`, the single view's `preview_box` over the area and develop's
`develop_box` over the develop view) is one function the scene, the window and
`--preview` share, so the placeholder and the image land in one place (the
single view's box and develop's differ, so a switch between them is a
level-2 rerun from the held level 1, and until it lands the window blits
the image held for the other box, centred and clipped to the one in view,
as it blits a develop held at the other crop). A press
on a mode button changes the mode, on a filter button sets the filter, on a cell
selects it, in develop on a history step or a filmstrip box selects it, on a
pane, tool or look button asks what it says and on the slider starts its drag
(see Driving), and in the single view anywhere in the area between the bands
returns to the grid; the status row, a strip's margin and the gap between two
buttons, the pane's chrome, and anything off the surface, are not targets, and
the bands are hit-tested last painted first, so on a surface too short for them
the status row covers the strips' buttons as it covers their pixels. A box whose
thumbnail is not held is a neutral placeholder, which is what `frame` digests
either way; the develop box and the single view's hold the developed preview
once it is made. The export view has no box and no thumbnail: `visible` is
empty and `develop_box` is `None` there, as under the chooser, and the
wants stand as the grid's, so the grid is ready when the view returns to
it.

While Alt is held every button shows its chord under its caption in
td-ui's hint face (`Buttons::emit_hinted`, `Button::emit_hinted`): the
mode strip's `o`, `Escape`, `Return` and `d`, the filter strip's `1`
through `4`,
the tool band's `c`, `C`, `z`, `0`, `-`, `=`, `f` and `Z`, the look band's `F1`
through `F9` under the first nine looks (`None` has none, and a tenth
look none), and the pane's `t`, `Backspace` and `z`; each is the chord
of the action the button presses, read from `BINDINGS`, so the two
cannot drift. The hints are shown from the keyboard's `Held` report
(`Input::Held`, Alt with anything) until the roles change, the focus
leaves (`Input::Focus(false)`) or a new keymap arrives (the window hands
the model no roles held, the keyboard's baseline); while the roll
chooser is open no chord is shown, since it owns the keyboard and a
chord would not do what the button does. `Controller::hints` is the
fact, a change a new generation, absent from `state` (the `held` verb
drives it, the frame witnesses it).

`td-photo open [ROLL] [--control-socket PATH]` runs the window (`window`), a
`td_ui::client::App` in the shape td-setup's is, and one adapter over the same
`Session` the replay drives (see Driving). A bare `td-photo` is `open` with no
roll: the window is what td-photo is and the verbs are its batch face, so the
usage is behind `--help`; without a compositor the refusal names what was
resolved, the display path or the inherited `WAYLAND_SOCKET` descriptor, or
why no endpoint could be made, and points at `--help`. The adapter: `event`
maps the compositor's
configure to a resize, a key to its chord (a held move or page repeats through
the toolkit's repeat; a flag, a filter, a view or quit fires once), a
modifier change with no key under it to `Input::Held` (the hints), a left
button's press, motion and release to the pointer path (the crop drag reads
the motion and release, not the press alone) and the wheel's frames to
`scroll` (in develop, the zoom's step), and `end_turn` takes the
pool's results, asks for the thumbnails the
model wants and the neighbours to decode ahead (Concurrency and memory),
reports the developed image's fitted rectangle within the develop
box as the crop drag's canvas (`set_preview_fit`, a fact that no more moves the
generation than the job count does) and the cursor photo's oriented, cropped
extent at full resolution from the level 1 it holds current for it, the zoom
ladder's and a pan's measure (`set_zoom_extent`, a fact the same way, `None`
until a develop of the photo lands), serves the socket and hands the exports
the session queued this turn to the pool, each with the photo's cached level 0
when the memo holds it, keyed by the request's own path (so a roll opened in
the same turn never lends a like-named photo's frame) and held weakly, so the
queue keeps no frame the raw cache has let go. An export that finishes sets
the status row's note (`exported NAME.jpg`, or `export of NAME failed`, the
worker having noted the reason on stderr), a new generation, and the frame it
decoded joins the raw cache, when its roll is still the held one; for a roll
no longer held the note goes to stderr and nothing is cached. The exports
asked for in the closing turn reach the pool as the window finishes,
whichever way it closed, so none is lost to a `quit` or a close request. While crop-adjust
is on it asks the pool for the develop uncropped (the preview's crop `None`,
the byte-identical path), so the handles overlay the whole frame and can grow
the crop; the cropped result returns when the sub-mode is left. Zoomed, it
asks for the develop at the model's zoom and centre (the preview's `zoom`,
so another zoom or centre is another develop), and blits the held image
shifted by a pan in progress (`pan_shift`) and, once released, by the held
window's offset from the wanted one (`held_shift`: the centres' difference
as photosites at the zoom, both centres held inside the image by the
model's rule (`ui::clamped_centre`) first, while the held develop differs
from the wanted only by its centre, exposure or look), clipped to the
box, until the release's develop lands. A held develop that differs from
the wanted one by a centre the model normalized alone -- a photo switch
lands the zoomed develop before its extent is known, then the clamp moves
the centre to the window it already shows -- is the wanted one, neither
shifted nor asked for again.
It owns no Wayland objects of its own. A frame is presented whenever the
generation submitted is not the model's: the scene through the toolkit's raster,
then each held thumbnail centred in its box through `ui::blit`, a clipped XRGB
blitter into the same frame inside `present`'s closure, because the toolkit's
raster is fills and glyphs, clipped to the grid's area so a box that runs under
the status band on a short surface leaves the band alone, then the flag badges
again (`Controller::badges`, the scene's badge draws alone, painted within the
grid's area as the blits are, so over the scene's own frame they change
nothing), since a thumbnail covers the corner the scene painted its badge in.
In develop mode the crop overlay is painted over the preview last, clipped to
the develop box, the overlay the model derives from the reported fit: a
tighten marquee while a drag is in progress, or, in crop-adjust, the crop's
rectangle with eight handle marks at its corners and edge midpoints. The
generation on screen is the one the compositor
acknowledged with its frame callback, which is what `wait-idle` waits for; a
generation is submitted once. When a second consumer needs an image primitive it
is promoted into `td_ui::raster` with a pixel oracle; until then the blitter is
this crate's, in `ui` where its pixel oracle is, and the confinement test pins
that the window writes frame bytes through nothing else. `td-photo --preview WxH
[ROLL] [--develop [POSITION] [--zoom] | --single [POSITION]]` is the same
frame without a display,
its thumbnails and, with `--develop`, the developed preview of the photo at
`POSITION` (the cursor's, the first, by default) made on the calling thread,
with `--zoom` at 100% around the centre (the `zoom-100` action, so the
frame is the window's at that zoom), or with `--single [POSITION]` in its
place the cull single view of that photo: the oracle the native test holds
a capture to.

Performance contract: no decode or resample runs on the turn loop's thread. The
roll's listing and its sidecars' reads and writes do, as they do in the replay,
bounded by `MAX_PHOTOS` and `MAX_SIDECAR_TOTAL`; a worker for them is a later
increment's if a roll shows the need. Workers post results; `end_turn` drains
them, marks damage and, while
any job is outstanding, sets the transport wait to at most 16 ms so results
appear within a frame of arriving; with nothing outstanding the wait is the
toolkit's idle wait. Thumbnails are requested for the rows on screen first, then
the screen below and the one above (in develop, the filmstrip's boxes, then the
shown after them, then before); a scroll that makes a request stale drops it
before it starts. A thumbnail is asked for at the box's width by the rule `thumb
--cache` applies, so the disk cache is the verb's exactly, and shrunk in memory
to the box for a shape taller than it (`develop::shrink`). A grid cell whose
thumbnail is not ready paints a neutral placeholder and its name, never blocks.

## Concurrency and memory

- One pool of `available_parallelism` workers, at most 16
  (`develop::MAX_THREADS`), started at window open and joined at close, over one
  queue the turn loop replaces under its lock whenever the model's generation
  moves, so a request the model no longer wants is dropped before it starts;
  jobs are `Thumbnail`, `Preview` — the develop preview, at most one at a
  time, run from the level an edit invalidates (`Decode`, `Level1`, `Level2` or
  `Level3`, planned from the window's memo, so an exposure or look edit reruns
  level 3 alone and a resize level 2) — and `Export`, the verb's runner over
  the request the session read on the dispatch, with the cached level 0 when
  the window held one at submission. Exports are not wants: a replacement of
  the wants leaves them queued, they run in order one at a time beside the
  develop (a thumbnail and the develop are taken first), and a closing pool
  hands out the exports alone until none is left queued or in flight, so a
  `quit` waits for the exports asked for rather than dropping them. A
  thumbnail in the running set and the develop in its in-flight slot stay
  outstanding until the turn loop collects the result; an export, queued or in
  its slot, until the worker sends the result, which it does under the queue's
  lock as it leaves the slot and wakes the workers waiting for it, so the
  window never counts an export it holds the result of nor misses one whose
  result is unsent, and a closing pool, with no turn loop to collect, still
  drains its exports. The worker notes a failed export's reason on stderr, so
  one that fails after the window closed is reported. The count never reads
  zero with a result made and not yet sent. A finished thumbnail is
  kept whichever wants asked for it, since it is the file's; the later jobs'
  results the turn loop
  compares before applying. Thumbnails are held in memory by name for the roll
  that is open at the surface's scale (a job is keyed by roll, name and scale,
  so another roll's file of the same name is another thumbnail; the held set is
  let go when either changes), `THUMB_CACHE_BYTES` between them, one that could
  not be made charged its name and slot alone, the least recently shown that is
  not on screen evicted first; an eviction recomputes the wants, so a thumbnail
  let go while still wanted off screen is asked for again.
- At most one develop, and so one raw decode of its own, runs at a time (the
  codec is sequential): the pool hands a worker the develop only when none is
  in flight. Beside it, at most one prefetch: the level-0 decode of a shown
  neighbour of the cursor in develop or the single view
  (`Controller::neighbours`, `PREFETCH_DEPTH` (2)
  either side, next before previous, nearer first), the lowest class of work,
  handed out only when no thumbnail, develop or export is pending and none
  is being prefetched or waiting to be collected, so it is never taken ahead
  of what is asked for (a decode once begun runs to its end, so a develop
  asked for meanwhile takes another worker), and never of the photo the
  develop or the export in flight is decoding (that frame is cached as it
  lands); asked for only while the pool has a worker to spare
  (`MIN_PREFETCH_WORKERS`, 2: on one worker it would hold the develop up)
  and the raw cache has room for a frame the size of its largest without an
  eviction. It is not a job: the count and `wait-idle` leave it out, since
  nothing waits for it, though the turn loop polls at the frame rate while
  one is queued, in flight or uncollected, as it does for a job, so the next
  neighbour follows without an event; its frame joins the cache at the
  turn's collect (before the wants are replanned, so a cursor move finds it)
  when its roll is still held and there is still room, as the least recently
  shown, never having been (so a develop's decode evicts it before a frame
  that was), never evicting a frame shown; a neighbour that could not decode
  is not asked for again while the memo lives, and one the cache had no room
  for not until the room it needs is there (`Memo::prefetched`,
  `prefetchable`). A develop that would decode the photo being prefetched
  waits for that decode rather than running it twice: the slot is held until
  the window collects the frame (so a plan made between the decode's end
  and its collection waits too), the collect drops the waiting plan and
  holds the next prefetch back until the wants are replaced, and the window
  replans the develop from the cache in the same turn, so a worker takes it
  first. The deference is the queue's, not the kernel's: the crate forbids
  `unsafe`, so no thread is reniced (`setpriority` would be a new UNSAFE.md
  surface), and a prefetch on one worker shares the cores with the
  develop's bands on the others. A closing pool waits for a prefetch in
  flight as it waits for a develop. The headless `export` verb runs on the
  calling thread, each band's
  demosaic, pipeline and transform split across scoped threads as below.
  Demosaic and resampling split rows into bands on one shared queue that the
  calling thread and its scoped helper threads drain together, so no thread
  outlives the call and a band count that exceeds the threads is shared out.
- The window memoizes the current photo's level 1 and its level 2 for the box's
  long edge, and caches level-0 frames (charged by their samples and key) under
  `RAW_CACHE_BYTES`, evicting the least recently shown, so a develop reruns only
  what its edit invalidated and a return to a recently shown photo skips the
  codec. A zoomed develop of the current photo starts at its level 1
  (`Start::Zoom`, with the cached level 0 beside it past half zoom, which
  needs the frame; evicted, the decode starts over and caches it again),
  its zoomed level 2 transient -- made and shown, remade on the next zoom,
  pan, crop or resize -- while the fit level 2 stays current, so back to
  the fit is level 3 alone; a zoomed develop that decodes or starts at
  level 1 makes the fit levels and the zoomed frame together. Each plan is
  keyed by the photo, so it never supplies another photo's
  levels: a result from a previous roll is dropped, and one whose photo is no
  longer the cursor's does not become the current levels (its decoded level 0 is
  still cached), so a develop that finished after a switch never evicts the
  photo the model moved to. When a develop completes the pool drops its own
  queued plan too, so the turn loop replans from the merged memo before any
  worker takes a plan made against the old one. The memo and the raw cache are
  let go when the roll or scale changes, as the thumbnails are. `--preview`
  keeps no memo: it develops the whole preview on the calling thread, so it
  stays the window's oracle.
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

- No original is ever modified or lost by td-photo, and no name of one is
  dropped but by culling's move into `rejected/`, under the roll it is in
  and by the same name, once the file has its name there. Import copies;
  export writes new names.
- No existing name but a sidecar's is ever replaced: a destination is
  refused by name before any work, and the final step of every write is a
  hard link, which fails on a name that appeared meanwhile; only a file
  system that refuses links falls back to a second check and a rename.
  The sidecar is td-photo's own file, read whole and accepted before its
  temporary is renamed over it. Culling's move is the same rule over the
  original and its sidecar: linked to the name in `rejected/`, then the
  old name dropped.
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
rule, that export develops in bands into the encoder through a temporary of
its own name and the encoder spreads its transform through `develop`'s
bands, and the budgets by value.

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
known quad, the area resampler on constant and step images, a complete
development of a synthetic frame to expected 8-bit values, and the export
path: the bilinear demosaic keeping a sample and averaging each other
channel over the neighbours a pixel has (a constant-per-channel frame, and
one bright site seen by its neighbours with the rounded means at the edge),
a region being that window of the whole at the sensor's CFA phase, its
refusals (a region past the crop, empty or overflowing, levels, a short
buffer), the export geometry mapping the crop through every orientation to
pinned regions and axes, and the export bands concatenating to the whole at
every band size and orientation, with and without a crop, the uncropped
export turned by `orient` being the turned export, and a band past the end
or of no rows refused; and the zoom geometry: the extent and the fit zoom
following the crop and the orientation (the fit half at most), the viewport
holding the box at each zoom, rounded up and cut to the level, its target
no more than the box (an odd box at 100 its own size) and following the
window's axes when turned, centred and clamped to the crop's edges and
refusing a zoom off the ladder, an empty box, a degenerate crop or a bad
level, a zoomed level 2 at half zoom being that window of the whole
frame's level 2 at every orientation with and without a crop (the inverse
mapping pinned against the orient it inverts), a bad level or a window
past it refused, and a zoom past half being the doubled window's bilinear
level 1 oriented, an odd box's region cut to its photosites exactly, a
window past the crop refused.

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
run or a ZRL past the block, and anything but EOI after the scan); and the
encoder against the decoder (Export, above): ramps of every partial-block
shape at `QUALITY` and noise at quality 100 round-tripping within their
tolerances, a grey frame staying grey, the stream compressing, the bytes
identical however the rows are fed and on any thread count, the axes
and rows refused by name, and the headers held to their literal bytes (the
frame and scan segments whole, each table's counts, the luma quantiser's
first zigzag entries and the chroma's last at `QUALITY`); the standard
tables' completeness is an inline unit test in `jpeg`.

`tests/av1.rs` pins the AV1 encoder: the geometry by value (the mode-info
and superblock grids, the tile column starts the width and the level
allow, the rows the area past level 6, the rounding and a caller ask
for, the level of each size and grid, the axes refused, the quality to
`qindex` map monotone with its ends); a range of shapes, whole and partial
superblocks of every edge, one pixel, a frame wider than a tile, tile
columns and two and five tile rows on one and two threads, each coding
to a stream whose reconstruction is the encoder's own and, with
`TD_TEST_DAV1D` naming a dav1d binary, decoded by it to the same bytes
exactly; three streams held to the hashes recorded under that decode,
the same bytes on one thread and three; the stream's sequence header
and the frame header's opening field by field, the quality buying luma
PSNR and costing bytes; the rows refused by name; and the container's
extent read back as the stream and decoded. The transforms' unit
tests hold every kernel to the real transform it approximates and to
orthogonality, the scans to their anti-diagonals, the quantiser tables to
their ends; the coder's unit tests round-trip symbols and bits through a
transcription of the spec's decoder, pin the adaptation, the bit writer,
`leb128` and the OBU framing, the trailing-bit rule (spec 8.2.4, which
libaom's decoder enforces), the end-of-block classes and the predictors
by value; `avif`'s hold every box to its bytes.

`tests/nef.rs`'s third command case runs `export` with `--long-edge`,
`--quality` and `--format` over a temporary roll (the export shrunk with
its ratio kept, an edge past the source and `full` the source's own size,
a lower quality fewer bytes at the same size and the shrunk frame's mean
the full one's within the codings' error, an AVIF named by its format
with the shrunk size in its `ispe` and, with `TD_TEST_DAV1D` alone, its
luma the JPEG export's within the codings' error (the gate holds the
streams by hash in `tests/av1.rs`), a lower quality fewer bytes, each
bad setting refused by name before anything is written) and
`export-picks` (no pick a count of none, the picks in name order with
the rejects and the unflagged left out, a line each and the count, the
picks' AVIFs named by the format, a pick that fails keeping the others
coming and failing the run after the rest, a bad setting, no ROLL and a
file for ROLL refused first), and a taller frame shrunk over several
source bands
decoding to the shrink of the full-size export within the codings'
error. `tests/nef.rs`'s second command case runs `export`
over a temporary roll:
the JPEG in `exported/` decoding to the in-process export of the same frame
within the round trip's tolerance, the second and third exports numbered
and a gap filled, the sidecar's exposure brightening, `mono` greying and a
crop selecting the pinned axes, a refused sidecar and an unknown look
refusing before anything is written, a stale temporary reported and left, a
non-original, a stray argument and a missing FILE refused by name, a turned
frame exporting turned, a file at `exported/` refusing, and the originals
unchanged.

Unit tests in `settings` pin the file's grammar: the text reading back, a
key left out at its default, every refusal by name, and the actions'
words; in `image` the shrink's sizes, shares, means, bands and refusals
as above; and in `main` the batch note's counts.

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
unlinking nothing; and `delete-rejected` moving a reject and its sidecar into
`rejected/` byte for byte (an unknown line carried), keeping one whose name
there is taken with why and on a second run again, leaving a pick, a refused
sidecar that says reject and a photo without one where they are, with as many
files under the roll as before, saying and making nothing for a roll whose
only rejects are a refused sidecar's and a link's, keeping a photo whose
original's name is taken or whose sidecar temporary is stale, finishing a move
interrupted after the link, refusing a link at `rejected/` by name before
anything moves, and a missing roll or argument. A unit test in `main` drives
the mover over an injected file move that fails the sidecar's leg: the photo
reported as moved without it, the rest going on, and the real mover refusing
a taken name and finishing its own interrupted move.

`tests/ui.rs` holds the action table to `driven::check` and to its alignment
with `Action`, and the error codes to the code grammar; drives `ui::Controller`
in-process (the state before a roll and after, walking with every step and page,
`select`, the filters and the cursor they keep or move, the single view and back
by action and by key, unbound keys, `quit`, scrolling by action and by wheel
clamped to the roll and revealed by the cursor, resizes good and bad, the
pointer on the filter strip, the mode strip (its transitions from the grid,
the single view and back by Single and Culling, the chooser over the grid,
the single view and develop, and develop with its palette, crop-adjust or a
marquee up, to the grid or the single view, the disabled buttons and the
one in view ignored, a change bumping once and a chooser request never, the
states it reports; on 400 wide the strip wraps to two rows, Export on the
second, and the tests at that width lay the filter strip and the area under
them), the export view (refused before a roll, shown by `E`, the strip and
the action, its mode word, status row, states, withheld boxes, disabled and
inert filters, the grid's actions ignored, its text read back, left by
Escape and every other mode button and entered from each, the chooser over
it, a lost cursor keeping it and a roll opening leaving it; the settings
set through `Settings` effects in any mode and settled, the value in force
ignored, each bad value refused, `state` reporting them and keeping them
across rolls and modes, the panel painting a size no button has; the
long-edge buttons and Export picks asking what they say, the button
disabled without a pick while the action and `C-e` still ask from any mode,
the slider's press, drag and release as the exposure slider's, a
long-edge press mid-drag the drag's, the drag dropped on leaving, `C-e`
under the button while Alt is held, and a short surface laying none of the
controls), a gap and a margin, a cell, the status row (on a
surface too short
for the bands too), off the surface and past the last photo, a resize to the
size it has, an empty roll and one past `MAX_PHOTOS`; the effects a flag
change asks
for, the model unchanged until they are settled and its sidecar text after, the
flag the file already holds ignored, an unknown line kept, a refused sidecar
never rewritten, a flag that hides the photo under a filter and ends the single
view, a settle that brings nothing new leaving the generation and one that
differs moving it, the sidecar budget at open and at settle, and delete rejected
asked for by verb and by `Delete` whenever a roll is open and moving nothing
itself, ignored in develop mode, and `remove` taking the moved photos out with
the cursor keeping its photo or its position, clamped to the end, the single
view staying on the photo that takes a reject's place or leaving with the cursor
when none is shown, and names not held changing nothing; the roll chooser
asking for the roll's parent with the roll selected or the working directory,
open only once a listing is installed, its finder in the text with the prompt
whole in the status row and no boxes to blit, a letter filtering rather than
flagging, a descent and an ascent as `List` effects moving no generation, a
refused listing noted, the same note again no change, a long note keeping its
tail and a control character blanked, an original listed disabled, the
window's actions behind it but `choose`, `open`, `quit` and `scroll`, the
chooser's repeat rule, `M-Up` and `^` ascending over a filter and never
repeating, every chord it names made by the keymap from its key, a plain
space filtering, `C-Return` opening the folder in view as an `Open` effect,
`Escape`, a roll opening and an area too small closing it, the root and a
relative roll asking for their parent by their own
path, the pointer, the wheel and a resize its, and develop's box, handles and
palette withheld under it and back when it closes); reads the scene back
as text (the strips with their buttons in place under every filter, the names
and badges by row, the status line, the single view, the empty and filtered-out
messages, a scale of 2) and holds its frame digest to equality and to change;
shows the chords under the buttons while Alt is held (each band's in its
order, the look band's `F1`..`F3` after `None`, marks the text read-back
passes over, the frame changed and the plain one back on release or focus
loss, Control alone showing none, none under the open chooser, the
strips' in the cull grid too);
and runs the built binary's `--replay` over a temporary roll through the seam's
verbs, writing a pick through the sidecar, refusing to flag a refused one,
keeping an edit made meanwhile and refusing a sidecar that became malformed,
reporting a stale temporary and settling the model from the file with the cursor
and the generation unmoved, a name that is not ASCII in hex, answering after
`quit`, choosing a roll (the chooser listing the roll's parent with the roll
selected and no file that is no original, a descent into a folder removed
meanwhile refused with the reason in the finder's row, the folder kept, an
ascent selecting the folder left, `C-Return` opening the folder in view with
the keys the window's again, a linked folder marked `link`, a link to a file,
a dangling link and a name that is not text left out, a folder of more
entries than the finder holds cut short and one of as many with sidecars
besides whole), and refusing a roll that is not there, a
bad size and a stray argument
before the session starts, and deleting the rejects: the rejects and their
sidecars in `rejected/` when the reply is and the model holding what is left, a
second ask `ignored`, a reject added since the roll opened moving and `changed`
though the model never held it, and a mixed batch, one moved and one whose name
there is taken kept, `refused` with the reason on stderr and the model holding
what the roll lists; holds the window's helpers (the boxes on screen and the
wants in order under a scroll, a filter and the single view, the job count as a
fact and a touch as a change, which keys repeat, the blitter's pixels centred,
clipped to the surface, the box and the area, and in BGRX order, the badges
painted back over a thumbnail that covered one and changing nothing over the
scene's own frame, on a tall surface and on one too short for a cell where only
the area's clip keeps them off the status band, and the in-memory shrink by the
thumbnail rule), `wait-idle` over the replay idle at once with its argument
judged, `--preview` equal to the seam's frame of the empty window and of a roll
and refused for a bad size or roll, `develop_box` the preview box only in
develop mode, the crop set over the develop preview (a marquee armed,
rubber-banded and committed as a sub-region of the current crop, a click, a
sub-minimum marquee and an off-canvas press refused, the develop box the
fallback canvas when no fit is reported (in crop-adjust no canvas at all: no
overlay and no drag until the uncropped fit is reported); the crop-adjust
sub-mode toggled and
escaped in layers, the crop mapped onto the canvas, a corner handle growing it,
the interior handle moving it, an edge handle clamped to the minimum and grown
to clear the crop, and the sub-mode and its handles witnessed by the frame not
`state`; the fresh crop drawn in crop-adjust (from a press inside a full-frame
crop and off a partial one, a point paint-free and the drag handled unlike the
tighten marquee's frame, committed as the whole frame's fractions and
replacing the old crop, a click and a sub-minimum marquee committing nothing,
the whole frame clearing it, the lock shaping it, the sub-mode's leaving
dropping it, an off-canvas press inert); the `aspect` lock -- a locked
corner drag mapping the ratio in the
canvas's pixel space (so a 3:2 lock on a 4:3 canvas is a 9:8 fraction box), a
grab without moving neither reshaping nor committing, a corner, edge and
tighten-marquee drag holding the ratio, a one-to-one lock keeping a square in
pixels, a locked edge clamped to the minimum, switching back to free, the lock
dropped on a photo switch, and a bad ratio token or wrong mode refused); the
look palette (toggled only in develop and escaping in layers; a `set_looks`
fact, and the status row naming the sub-mode over a boxless surface too;
the current look marked and picked by a press on its name (a press off the names
or with no box picking nothing; the built-in set on a 640 by 480 surface going
on in a second column, a column of long names cut at the panel's right and
the next not laid); mutually exclusive with crop-adjust and dropped
on a photo switch; a touch behind the open palette not bumping the generation);
the develop controls (the bands' geometry, the buttons enabled as the
photo's crop and steps allow and inert otherwise, each button's effect and
`C`'s, `exposure` accepted and refused, the bands' chrome and a move or
release over them inert, no facts row, a narrow surface laying no slider;
the zoom: `zoom_step`'s ladder from any fit, the actions refused in cull,
the fit and a step without the extent `ignored`, 100% a change reported in
`state`, the status row and the selected button, the ladder walked in and
out by action, key and wheel once the extent is reported (a turn a step,
none with no rows), an image held whole with no step in, the zoom kept
across photos and dropped on leaving develop and by crop-adjust (which
holds the actions, the wheel and the buttons off, Fit unselected; the
wheel inert under the palette), the Fit and 100% buttons, a zoom mid crop
drag dropping the drag, the four chords; the pan: a
press in the fitted box the marquee's and in the zoomed box the pan's, a
move shifting the frame and `pan_shift` saying by how much, the release
moving the centre against the travel at the zoom's photosites a pixel, a
click, a stray move or release and a press off the box inert, the centre
clamped to the window's room at either edge and held across a zoom, a
photo switch dropping the pan, the extent unknown moving nothing, a
reported extent or a resize bringing the centre into the room without a
bump, and the centre the middle again once develop is left;
the slider's mapping at its ends and between, a press on the knob's own
step and a release there writing nothing, a jump, a drag and its release
committing, a drag back to the value in force releasing without a write,
the last column the last step and a drag off the edge staying there, a
photo switch dropping the drag; the look band's buttons and `F1`..`F9`
picking and clearing, a key past the list ignored, the band read back and
the palette's status word); the filmstrip (its band and boxes at 800 by 600
and at scale two, the develop box above it, the cursor centred as the ends
allow and its box outlined, `visible` the strip's boxes and `wanted` around
them, a press selecting and recentring, the cursor's own box and the band's
chrome inert, a crop drag released over it the crop's, the badges over the
boxes following the flags, the palette closing on a strip press, the band
withheld a row short of the room it needs and laid with it (a box at least a
thumbnail tall above), a region too narrow for a box laying none and a cell
wider one, fewer shown than boxes, an even box count, the wants bounded on a
roll of twenty, and the chooser withholding the boxes but not the wants); the
`--preview --develop` of a roll whose embedded previews are flat JPEGs
blitting each into its strip box (`control_process.rs`), the native
compositor leg holding the live frame to it, and `--preview --develop
--zoom` of a frame dark but for a square at its centre showing the square
its own size where the fit shrank it, `--zoom` refused without `--develop`;
the export action (asking for the cursor photo in any mode by verb and by
`e`, refused without a roll or a photo, not repeating on a held key, the
dispatch moving nothing; the note set, shown at the row's end, a new generation
when it differs and none when it is the same, absent from `state` but for the
generation, and cleared by an open); the binary's `--replay` exporting a
synthesized decodable NEF on the request (the JPEG there and the row noting it
when the reply is, the second export numbered, the job count zero, a frame that
cannot be decoded `refused` with the row saying it failed, a refused sidecar
`refused` before anything is read, and nothing written for either), the
binary's `--replay` under an `XDG_CONFIG_HOME` of its own keeping the export
settings (the defaults with no file, a change writing the file's exact text
and settling, the same value ignored and a bad one `bad-argument`, the
export view's text, `export-picks` ignored with no pick and, with two picks
one of which cannot be decoded, `refused` with the row counting one of two
and one failed and the export shrunk to the long edge in force, from the
view and from the grid by `C-e`; the next session reading the file back and
the empty configuration every other replay session runs under not, a
refused file leaving the defaults with the reason on stderr and the next
change replacing it, a format the file holds kept through a change to
another key, a folder at its name refused by name with the session
starting, and a change refused when the file cannot be written, the model
as it was), and `open`
refusing a bad socket path, a second roll or a stray flag before it looks for a
display and leaving no socket behind when the display is not there;
`src/window.rs`'s own tests hold `wait_ms`'s grammar, the envelope's ID, the
held shift (the centres' difference as pixels at the zoom, nothing for a
centre the model normalized), the
charge of a held entry, the queue's replacement skipping what runs and the
pool's count staying outstanding until a result is collected, the pool running
one develop at a time and dropping its queued plan when one finishes, a
prefetch taken after every wanted job and one at a time, not counted, the
develop that would decode its photo waiting, dropped as the frame is collected
and taken first once replanned, a prefetched frame joining the cache only into
room and never evicting, evicted before a frame shown, refused for good when
it could not decode and until the room is there when it had none, the memo
planning each develop from what it holds (level 3 for an exposure or look edit,
level 2 for a resize, level 1 for a cached photo, else a decode; a zoom of the
current photo from its level 1 with the cached frame, without it through half
zoom alone and past half a decode, a zoomed result leaving the fit levels
current, and a zoom of a cached photo not current from level 1), untouched by a
failed develop, caching but not becoming current for a develop that finishes off
the cursor, the raw cache evicting the least recently shown under
`RAW_CACHE_BYTES`, an export keyed by its own path as the memo keys it, and the
exports surviving a replacement of the wants, running in order one at a time
after a thumbnail and the develop, draining alone from a closing queue once none
is in flight, coming back from the pool with why one failed and leaving the
count as the result is sent, running one after another on collection alone, and
drained by a pool closing with no window to collect. `tests/control_process.rs`
runs the built binary under the native compositor harness the sibling crates use
(`ready` builds the compositor; the case is ignored without it): the window on a
roll of originals no decoder accepts, so the frame is the scene's, mapped with
its app id, idle over the socket, its state, its captured tile equal to
`--preview` of the roll at the tile's size under the observe, capture, observe
rule, a pick over the socket written through the sidecar and shown, a click from
the seat on the second cell and `End` from the seat each moving the cursor
(repeated until one lands, each the same cell however many land; the pointer
then parked in the desktop bar, since the seat draws the client's cursor over
the tile), `first` over the socket restoring the frame, and `quit` closing the
window with its socket gone. A second native case, on a synthesized decodable
NEF, develops the cursor photo over the socket and holds the captured tile to
`--preview --develop` of the roll before and after an exposure edit, zooms
to 100% over the socket (the tile `--preview --develop --zoom`, the box
changed) and back to the fit (the tile the fitted frame again), then
exports over the socket: with the sidecar naming a look no file provides the
export fails and the row says so with nothing written, and with the look cleared
`wait-idle` waits for the pool's export, the JPEG is in `exported/` and the row
names it, and an export asked for and quit at once is written, numbered, by the
time the process has exited; a headless case (no compositor) holds `--preview
--develop` to a develop box that carries a developed image and changes with the
sidecar's exposure, and `--preview --single` to the single view's box the
same way (`--zoom` refused there, and the two flags together).

The builder discovers the crate by existing; its gate runs `cargo test` and
all-target Clippy.

## Packaging

The target recipe `td-photo` (`recipes/src/recipes/td-photo.rs`) builds the
crate with cargo on the source-built toolchain, staging the `td-ui` and
`td-compositor` trees beside it so the toolkit's `#[path]` mounts and
embedded notices resolve as the sources name them, links the binary fully
static and splits its debug companion: the td-editor and td-taskmgr shape,
with a lock that lists only td-photo and td-ui. The system image
(`recipes/src/recipes/system-x86-64.rs`) copies the complete recipe output,
companion included, into the immutable root and links `/bin/td-photo` to
it; the tool is run from the terminal and receives no authority, socket or
credential of its own. Every retained file of the three trees moves the
`td-photo-source` row of `seed/seed-digests.txt`; DESIGN.md is excluded
from staging and from the hash.

The realized-output check `td-photo-test` requires and asserts the static
binary, runs `--help` and an empty `--replay` on the target, then compiles
`recipes/src/fixtures/td_photo_synth.rs` with the target rustc, static as
the direct td recipes are: the uncompressed synthetic writer of
`tests/support/synth_nef.rs` restated over literal tags, so it compiles
alone. The fixture writes one 64 by 48 Z 8 frame into a roll, and the built
td-photo probes it with the raw strip decoded, develops it at a 16-pixel
long edge with an exposure and the built-in `mono` look, writes its sidecar
through `edit` and lists the roll. That is target-artifact coverage of the
binary and its pipeline; the window, the thumbnails and the Wayland client
stay with the crate's own tests under the native harness on the host
preflight. A td-photo, td-ui or td-compositor edit selects this check in
`td-builder affected-checks` beside the crate's cargo gate.

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
5. Develop mode, in landings. (a) The look format with the built-in set
   (`look`), the look in the headless pipeline, `td-photo looks` and
   `develop --look`. Landed. (b) The develop model: the `cull`/`develop`
   mode, the develop actions in the table (enter and leave, exposure in and
   out coarse and fine, `look`, `crop` by fractions, `reset`), their gating
   to the mode, exposure as a file-relative delta and `look` and `crop`
   absolute, over `--replay` and the control socket, with `tests/ui.rs`.
   Landed. (c) The develop pipeline as reusable levels in `develop`: level
   2 (the `u16` level 1 resampled to the canvas directly, then oriented,
   the one `f32` buffer) and level 3 (the per-pixel tail), and
   `RAW_CACHE_BYTES`, with `render` their composition. Landed. (d) The
   developed preview in the develop view over the window, in two slices.
   First: the developed preview blitted into the develop box from a
   `Preview` pool job off the turn thread, the develop box geometry shared
   by the scene, the window and `--preview`, `--preview --develop` of a
   developed frame and the native test; this slice re-develops from the raw
   on each edit, correct but not yet incremental. Landed. Second: the level
   memoization and the level-0 cache, so an exposure or look edit reruns
   level 3 alone, a resize or crop edit level 2, and a
   photo switch level 1 from the cached level 0; the pool plans each develop
   from the window's memo and the raw cache evicts the least recently shown
   under `RAW_CACHE_BYTES`. Landed. Third: the neighbouring level-0 frames
   decoded ahead as the pool's lowest class of work, one at a time into the
   cache's room, so an arrow key in develop reruns level 1, not the codec.
   Landed. (e) First: the user crop applied to level 2 (the crop
   of level 1), mapped through the inverse of the orientation so an
   uncropped develop is byte-identical, in the window preview, `--preview`
   and the headless verb's `--crop`. Landed. Second: the crop drag over the
   develop preview — a marquee the pointer arms, rubber-bands and commits on
   release as a sub-region of the current crop, through the same `Edit` the
   `crop` action makes, the window reporting the developed image's fitted
   rectangle as the drag's canvas. Landed. Third: the crop drag handles — a
   crop-adjust sub-mode toggled with `c` that develops the frame uncropped
   and overlays the crop's rectangle with edge, corner and interior handles;
   grabbing one and dragging resizes or moves the crop and commits it on
   release through the same `Edit`, clearing the crop when the rectangle
   covers the whole frame. Landed. Fourth: the aspect presets — the `aspect`
   action locks the crop drag to free, 3:2, 4:3, 1:1 or 16:9, a transient
   crop-tool setting (not a sidecar key). The ratio is a pixel ratio held in
   the reported canvas's pixel space, so a locked marquee, corner or edge drag
   keeps it; picking a ratio only arms the lock. The immediate snap that would
   reshape the current crop the moment a ratio is picked is deferred with the
   scaled preview under the tighten marquee: the window now reports a fit
   only for the frame the mode wants (8(d)), so a snap would wait for the
   uncropped fit rather than read a stale cropped aspect, but it is still
   a later slice. Landed. Fifth: a
   look palette — the `looks` action (`l`) toggles a list of the available
   looks (built-in and user stems, a `set_looks` fact) over the develop box
   with the current one marked. It is a frame-witnessed sub-mode, mutually
   exclusive with crop-adjust. Landed. Sixth: picking from the palette — a
   press on a name sets that look through the same `Edit` the `look` action
   makes, so the mark rides the edit's settle and the palette stays open; a
   press off the names, or on a surface too small for the box, picks nothing.
   Landed. The live scaled preview under the marquee remains a later slice.
6. Export, in landings. (a) The banded full-resolution bilinear demosaic
   and the export geometry in `develop`, the JPEG encoder in `jpeg`, the
   `exported/` naming, and `td-photo export FILE` headless. Landed. (b) The
   `export` action in the window: the `Export` pool job over the raw cache,
   running the verb's runner off the turn thread, its outcome the status
   row's note; the replay running it on the request. Landed. (c)
   `delete-rejected`, the action (`Delete`) and its verb that move rejects
   and their sidecars into `rejected/` by the publication rule, the names
   there refused first, the model taking the moved photos out. Landed.
   (d) The roll chooser: `choose` (`o`) over td-ui's shared directory
   finder, the `List` effect and `list_folder`, the chooser owning the
   keys, the pointer and the area while it is open, `C-Return` opening the
   folder in view through the `open` path, and a bare `td-photo` opening
   the window. Landed. (e) The mode strip and the filter strip over
   td-ui's bezelled button strip, the mode strip a target in every mode.
   Landed. (f) The export view: `export-mode` (`E`, the strip's Export)
   over the area, the quality and long-edge settings (`settings`, the
   user's `export` file, the `quality` and `long-edge` actions and the
   `Settings` effect), the area shrink (`image::Shrink`) in the
   export runner, `export-picks` (`C-e`, the button, the `ExportPicks`
   effect, the batch note) and `td-photo export-picks ROLL`. Landed.
   (g) AVIF, in landings: the AV1 still-picture encoder in pure `std`
   (`transform`, `cdf`, `av1`), held to dav1d. Landed. The HEIF
   container (`avif`), the export settings' `format` and `--format` on
   the export verbs. Landed. Then the view's format row, its buttons
   and the `format` action. Planned.
7. Packaging: the cargo recipe staging td-ui, the image entry, and the
   recipe check that develops the synthetic frame in the built artifact.
   Landed.
8. The develop history, in landings. (a) The sidecar history: `step-N` lines
   with the develop keys their summary, seeded from a file without one, `undo`,
   a step toggled or deleted, `reset` clearing it, and `edit FILE undo`; the
   history pane at the develop view's left with its Toggle, Delete and Undo
   buttons, `Up` and `Down` walking its selection in develop, the `undo` (`z`),
   `step-toggle` (`t`) and `step-delete` (`Backspace`) actions and their
   effects, and the step count and selection in `state`. Landed; a run of
   edits to one key folding into its step landed after. (b) The develop
   controls: a tool strip above the preview with the crop, uncrop, undo and
   reset buttons and the exposure in a td-ui slider (`chrome::Slider`) beside
   its step buttons, a look strip with the looks on `F1`..`F9`, and the status
   row naming the crop sub-mode. Landed. (c) A filmstrip of the shown photos
   under the preview, `Left` and `Right` or a press moving between them. Landed.
   (d) The crop tool: a press off the crop in crop-adjust (or off its handles,
   with no crop) draws a fresh crop over the whole frame the sub-mode develops,
   its handles adjust it, and leaving the sub-mode applies it, so the crop is
   chosen and adjusted over its own content before the preview crops. Landed.
   (e) The zoom: Fit and 100% on the tool band, `Z`, `f`, `]` and `[`, the
   wheel stepping 25, 50 and 100 percent from the fit, a pan over the
   zoomed box, the zoomed level 2 from level 1 through half zoom and from
   level 0 past it, and `--preview --zoom`. Landed.
9. Later: DNG and JPEG rolls, the Nikon High
   Efficiency codec, a better full-resolution demosaic, highlight
   reconstruction, the `.dtstyle` translator, and ratings.
