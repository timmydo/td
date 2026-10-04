# td-ui

td-ui is the dependency-free Rust toolkit that td-owned graphical programs
share. Today it carries the pinned Unifont face and wire codec, the XKB
keyboard translation, repeat policy and pointer decoding, the clipped XRGB
raster with its palette and scrollbar geometry, the Wayland client
connection over its own raw descriptor transport, and the client over it
(object table, registry, one toplevel surface with its buffers and frame
callback, the seat with its keyboard and pointer, the clipboard's data
device with its offers and sources, the pointer image and the turn loop),
all moved out of td-editor, with the chrome bands (`chrome`: the menu bar
and its panel, the wrapped text block, the tab strip and the status row)
that td-editor draws, and the paged list `List` built on the panel's row
painter, the single-line text entry `TextEntry`, the button strip
`Buttons` and the slider `Slider`, the widgets no scene drew, and the
terminal's reusable half: the VT parser and model, its renderer, the
keyboard chord encoder, the terminfo compiler and the PTY with its
threads. td-editor is its first consumer; the installer front end
`td-setup` (its welcome page landed), td-portal's file chooser, td-photo,
the terminal `td-term` and the disk usage analyzer `td-dua` follow.
This document is the component contract and the starting point for
successive agents; the root `AGENTS.md` and `DEVELOPMENT.md` still govern
changes and submission.

## Status and scope

The crate exists and carries, unchanged in behaviour, what has moved out of
td-editor so far: the bounded XKB text-v1 keymap compiler (`keyboard`,
`xkb`), the explicit-clock held-key and repeat policy (`repeat`),
`wl_pointer` event decoding with axis-frame accumulation (`pointer`), the
core data-device decoding with the offer record and its budgets (`data`),
and the clipped, allocation-free XRGB painter over the pinned face
(`raster`): rectangle and glyph primitives, the integer scale, the warm
palette, scrollbar geometry, the text-run painter and the `Raster` that
writes a `Composition`'s draws into a caller-owned buffer, with the face's
provenance and licence texts embedded in `notices`; and the Wayland client
connection (`wayland`): display endpoint resolution from explicit
environment values, the bounded connect, request framing with at most one
descriptor per send, the receive path that owns every delivered right until
an event's consumer takes it, the startup and write deadlines, the unlinked
private pool file and the pointer image, over the private raw module `sys`
that `UNSAFE.md` §19 records; and the client over that connection
(`client`): the object table with its fixed and dynamic ids, the registry
with its budgets, the three globals every consumer binds, one toplevel
surface with its SHM buffers and frame callback, the seat with the keyboard
and pointer its capabilities give (the keymap right consumed and compiled,
focus, held keys, the modifier snapshot and repeat timing applied, pointer
events decoded and the pointer image shown on enter), the clipboard's data
device with its offers, barriers and sources (the send right consumed and
handed on), and the turn loop that drives a consumer's `App` under the
startup deadline; and the chrome bands over the raster (`chrome`: the menu
bar with its drop-down panel, the wrapped text block, the tab strip and the
status row, which td-editor's scene composes and paints, and the list, text
entry, button strip and slider later consumers asked for). It re-mounts the
compositor's `font`, `font_data`, `wire` and `filter` sources exactly as
td-editor did, its `reportable` report-text predicate and its
`proc_status` reader, so there is still one Unifont face, one wire codec,
one rule for which characters a report prints and one reading of a
process's effective uid in the tree, and it owns the 8x16 cell constants every
consumer lays text out on.
td-editor depends on it by path and uses those modules through the crate's
public surface.

Newly built: `td-setup`, the second consumer, is a new crate whose
`welcome` page renders from the toolkit and whose Wayland turn loop
presents it as a live `App`, proven under the native compositor harness
(increment 6). Moved: td-portal's file chooser, the third consumer, renders
from the toolkit's raster and chrome bands (increment 7(c)) and its private
transport is now a live `App` over the shared client (increment 7(d)), the
old hand-rolled Wayland transport deleted. Its dialog is the first `App` to
own a Wayland object of its own, the privileged `td_portal_manager_v1` it
binds through its `Tag`. td-editor's window is the first `App` and
td-setup's the second; each of those owns no Wayland objects of its own.

Moved (increment 8(a)): the transport of td-editor's driven UI, the layer
an agent or a test operates a program through. `control` carries the
length-prefixed frame, the tab-separated ASCII envelope, the scalar and
byte codecs and the two response lines; `control_socket` publishes the
private Unix listener under td-editor's path and ownership contract;
`control_worker` is that transport's bounded thread, now generic over the
consumer's request type behind `Parse`; and `replay::run` is the
consecutive-frame runner behind a headless `--replay`. td-editor's control
socket, worker and replay run on them, wire-compatible with its
CONTROL.md; what a request means, and whether it may read or act, stays
the consumer's. Newly built (increment 8(b)): `driven`, the semantic
half, a `Controller` a consumer implements once over its own action
table, the generic verbs routed over the envelope, the text read back
from the draw stream and the painted frame, digested and paged; and the
raster's one XRGB-to-PPM writer, which td-setup's and td-editor's
previews now use. td-photo consumes the seam in its next increment.

Built and deleted (increments 11 and 13): the cell screen, a styled grid
a program written for a terminal painted in rows and columns, and the
window that presented it and polled the program's handler. td-news and
td-mail drew in it until increment 12, when each moved onto the widget
window below; a grid kept the terminal's shape, and that was the wrong
abstraction for a reader whose views are lists and texts. Nothing draws
in it now; the widget window kept its loop, without the grid, and its
section below describes that loop whole.

Newly built (increment 12): the widget window under "Widget window"
below. `window` is the same loop for a program that lays toolkit widgets
and an embedded document pane out over its surface instead of a grid:
its handler paints into a raster over the surface and reads the
keyboard's chords, the left button's press, drag and release in surface
pixels with Shift, wheel travel, the surface on configure, focus and the
close request. td-news and td-mail have moved onto it, with their lists
and td-editor's pane (APPLICATIONS.md §W.8, "Reworked again"), and the
cell screen and its window are deleted (increment 13).

Moved and newly built (increment 16): the clipboard's transfer owners
under `clipboard`, td-editor's `Outgoing` moved here with the destination
owner that makes the send's right nonblocking through the raw module's
two pinned `fcntl` status commands, and a bounded `Incoming` that reads
the selection's text whole; and the widget window's clipboard, a
`Clipboard` handed to the handler with every input, through which it
offers text as the selection at the press it is answering and asks the
selection for its text, which the window delivers as `Input::Paste`. The
editor's window keeps its own clipboard half over the moved writer.

Newly built (increments 17 and 18): the outline reader `sfnt` and its
coverage rasterizer `coverage`, and the atlas executor over them: the
coverage page `atlas`, the outline face `face` at one pixel size with its
`Cell`, and `Raster::with_face`, which executes the unchanged draw
stream's glyphs through that face (see "Outline faces and the glyph
atlas" below).

Pinned (increment 19): the JetBrains Mono Nerd Font as pinned upstream
data, at `/etc/fonts/jetbrains-mono-nerd` in the image and in the jail of
a program on `static-runtime`.

Newly built (increment 20): `Face::fit`, which fits an outline face to the
bitmap grid's cell; `typeface`, the style bytes and a face fitted at the
current scale; `pinned_face`, which reads the pinned face; and the widget
window's typeface, through which td-news and td-mail draw their text
(see "The grid fit" below).

Newly built (increment 21): the programs with their own windows take the
face for their live windows too, through `Raster::with_typeface`:
td-editor's file window and window preview, td-setup, the portal's file
chooser (`DialogConfig::typeface`, read once when the service starts),
td-photo and the task manager. Their still-image `--preview` output,
`--render-check` and in-process tests stay on the bitmap face, and each
`--font-license` output names where the outline face's notices ship.

Newly built (increment 22): `face_file`, the outline face's paths and
bounded read; the italic and bold italic styles (`Face::with_slant`,
`Face::style` over bold and italic); and td-term's cell painter
(`vt_render::render_with`), which draws through a face fitted to its grid
(see "td-term" below).

Moved (increment 23): the terminal. td-term, once an argv[0]
personality of the compositor multicall, is its own crate over the
toolkit, and what another program could reuse to embed a terminal moved
here out of the compositor: the VT parser and model (`vt`) with its
native corpus under `spec/vt` and the libvterm importer
`td-ui-import-libvterm`, the renderer (`vt_render`) with its PPM goldens
under `spec/vt_render`, the terminfo compiler (`vt_terminfo`), and the
PTY with its reader, writer and waiter threads (`pty`), whose four
`ioctl(2)` requests joined the raw module under `UNSAFE.md` §19. Newly
built beside them: `vt_keys`, the chord encoder that replaces the
compositor's evdev key table, so the terminal reads the chords the
toolkit's keymap makes of any compositor's map rather than verifying td's
byte for byte; and the client's `activated`, `presented` and
`focus_serial` accessors the terminal's readiness and proof read.
`td-term/DESIGN.md` is the normative contract for all of it; this
document records only the toolkit's side.

Newly built (increment 24): a program run outside the image finds the
face on its host. `face_file::find` searches the user's and the host's
font directories after the image's, and `./install-fonts` installs the
pinned face, verified, where that search looks (see "Delivery and trust
position" below). A program that draws with Unifont for want of the face
says to run it.

Moved (increment 27): the editor core. td-editor's document model and
the controller, key profiles, layout, scene, clipboard capture, filling,
text bounds and dialog permits over it are `editor` and the `editor_*`
modules, so a program embeds the document pane through the toolkit alone
(see "Editor core" below).

Moved (increment 30): td-review, the integrator's branch review and
landing tool, from a raw terminal of its own (`stty`, the alternate
screen, its own escape decoder and a `less` pager) onto the widget
window. Its panes are rows of styled text over a scrolled region, so it
paints them in the raster's cells through `text_run` with the warm
palette and a `Scrollbar`, and reads the window's chords. It is the first
consumer to run its state machine off the window thread: every key can
start a git process that takes as long as the network does, so a worker
owns the program and the window paints the frame it sent last. Inputs
cross to the worker stamped with the newest frame painted whole a dwell
before the window read them, and the program's typeahead drop discards
those stamped before the frame that raised a confirmation (td-review's
`window.rs`). The toolkit
is unchanged. td-review is a recipe consumer packed into the image.
Its review pane then moved into the editor core's read-only document
pane (increment 31), so a diff is selected and copied as an article is
in td-news; the pane's inks keep its colours, and the reading keys,
selection and copy stay on the window thread, outside the stamps.

Newly built (increment 32): the message list `messages`, a chat
transcript with selectable text and whole-message copy, under "Shared
message list" below. td-agent (`td-agent/DESIGN.md` §4) is its planned
first consumer; nothing draws it yet.

Newly built (increment 33): the colour themes under "Themes" below.
`theme` holds six fixed palettes over the shared palette's roles, and
`Raster::with_theme` paints a draw stream's palette colours in one of
them. The widget window paints in its program's theme, moves to the
next on `F12` and keeps the choice in `theme_file`'s one-line file
under the program's configuration directory, so td-news, td-mail,
td-review, td-agent, td-dua and td-pass each keep their own. td-review's
three status inks moved here as `SUCCESS`, `WARNING` and `ACCENT`, so a
theme recolours its diff lines too.

Newly built (increment 34): `theme_file::Kept`, a window's theme with
the file it is kept in, which the widget window now holds and the
programs with windows of their own hold alike: td-editor, td-setup,
td-photo and the task manager paint their live windows in their theme
and move it on `F12`, each keeping its own file; the live installer
wizard runs without a home and keeps none. td-term and the portal's
chooser stay in `SAND` (see "Themes").

Newly built (increment 35): the key list `keys`, a program's keys and
the window's own shown over the frame on `F1`, under "Key list" below.
The widget window shows it for every program it serves, asking the
handler for its sections when it opens; a program that lists none shows
the window's own keys alone.

Newly built (increment 36): `keys::Overlay`, the key list as a window
keeps it, which the widget window now holds, so a program with a window
of its own keeps the same list by routing its keys, wheel and resizes
to it and painting it last.

Newly built (increment 37): one spelling and style for the key list's
rows. `keys::check` holds a program's rows to the keymap's own spelling
of its chords and to the list's style, `lines` shows every description
as a sentence, a left press closes the open list (`Overlay::press`, and
the widget window itself), and `keys::BUTTON` and `keys::ITEM` label a
program's pointer entry to it (see "Key list").

## Purpose and trust position

td-ui is target-zone source: it ships only inside the programs that embed
it, as a Cargo path dependency resolved offline from the checkout. It is not
a runtime library, a plugin host or a general Wayland toolkit, and it does
not claim third-party toolkit compatibility. Its themes are palettes
compiled in, chosen by name; a user cannot define one. It carries
no foreign payload and no external crate; its lock lists exactly its own
package, and its confinement tests pin that its manifest declares no
dependency at all.

A consumer names it as `td-ui = { path = "../td-ui" }`. That is the one
sibling-dependency spelling `builder/src/affected.rs` admits, and the
consumer's lock then lists exactly its own package, td-ui, and any other
td crate it names the same way (td-news, td-mail and td-agent also name
td's shared library crates, td-json, td-toml and td-fetch-client among
them). A program
that depends on td-ui is built by a cargo recipe that stages sibling source
trees (`local_source_trees`, the td-net shape); a flat-staged direct-rustc
recipe cannot link a second crate. td-portal, td-taskmgr, td-editor,
td-news, td-mail, td-review, td-setup and td-term are built that way:
each stages `td-ui`, and `td-compositor` because td-ui mounts the font
and wire modules from it, beside its own tree. td-portal and td-setup stage
further siblings of their own. In the other direction the compositor's
flat recipe stages td-ui's five outline-face modules beside its own, since
its chrome mounts them, and `tests/fonts/mod.rs` as `tests/fonts.rs` for its
session-tests build, so an edit to that test encoder changes the
compositor recipe too. A toolkit edit changes each consumer's
locally derived source identity and selects each consumer's
realized-output check.

## Public surface

The crate's `pub` items are the whole contract. Consumers use them through
`td_ui::` paths and nothing else; a consumer's confinement tests pin which
of its own files may name each module.

- `CELL_WIDTH`, `CELL_HEIGHT`: the 8x16 bitmap cell. `font::pinned` is held
  to them by a test.
- `font`: the compositor's PSF2 reader and pinned Unifont face, unchanged.
  Provenance and licences stay in `td-compositor/assets`.
- `wire`: the compositor's Wayland framing codec, unchanged.
- `reportable`: the compositor's report-text predicate, mounted from
  `td-compositor/src/reportable.rs` unchanged, which says whether a
  character may appear in a line a reader parses as a record; td-term
  blanks what it refuses in a program name and a last-screen report.
- `proc_status`: `effective_uid`, the second field of a
  `/proc/<pid>/status` `Uid:` line, mounted from
  `td-compositor/src/proc_status.rs` unchanged; td-term's account lookup
  and the compositor's terminal-authority probe read it through one parser.
- `filter`: `MAX_QUERY_BYTES`, `insert` and `matches`, the compositor's
  bounded ASCII query rule shared by the launcher and td-portal's chooser,
  mounted from `td-compositor/src/filter.rs` unchanged; the finder takes
  its `insert` and bound and repeats its match rule with the name folded
  at the comparison.
- `keyboard`: `Keymap::parse` over an XKB text-v1 map, `Modifiers`,
  `Stroke` (its chord, whether it repeats, and `text`, the printable
  character resolved before a Control or Alt chord lowercased it),
  `InputError`, `Selected`, and translation from evdev keycodes
  plus a compositor modifier snapshot to logical chords; `Held`, the
  control, alt and shift roles a snapshot holds (`Keymap::held`; a state
  the map refuses holds none), with its chord `prefix` (`C-M-S-` in that
  order, `-` for none) and the `parse` of one. No display,
  descriptor, environment, action execution or clock is accessed. The
  bounded lexical envelope and the compatibility target are the ones
  td-editor/DESIGN.md records under "Implemented keyboard compiler"; that
  text remains the behavioural specification and moves here with the next
  documentation increment.
- `xkb`: `TypeCatalog`, `VirtualBinding`, `Selection`, `ResolvedType`,
  `Diagnostic`, the type-table half of the compiler that the keyboard
  compiler builds on.
- `repeat`: `Input`, the held-key set and repeat policy over an explicit
  millisecond clock: focus with held keys, modifier snapshots, timing,
  key press and release, arming, repeat and next-wake computation. The
  client owns one for its keyboard and applies the lifecycle half (map,
  focus, snapshot, timing, presses) itself, lending a consumer a read of
  the state and the repeat half: `arm`, `repeat`, `wait_ms` and
  `cancel_repeat`, and `held`, the roles the synchronized snapshot holds
  while focused, which a pointer press reads (none unfocused, before the
  snapshot or without a map).
- `pointer`: `Event`, `decode` for `wl_pointer` v5 through v7 events and
  `Wheel`, which accumulates axis, axis-discrete and axis-value120 input
  into whole cell rows and columns per frame.
- `links`: `at`, the byte range of the link over a byte of a text,
  `around`, the same without the bound, for a caller walking the text
  once (td-mail's wrap, so it never splits a link a press could find),
  `whole`, whether a text is one link whole at any length, `stop`, the
  bytes that end one, and `MAX_BYTES`, the bound either side of a byte;
  under "Following links" below.
- `data`: `DeviceEvent`, `SourceEvent`, `device`, `source` and `offer`, the
  exact core data-device v3 schemas, and `primary_device`,
  `primary_source` and `primary_offer`, primary-selection v1's; `Board`,
  which of the seat's two selections a device serves, with its manager
  global and version, its request and send opcodes, and its schemas by
  board; `Offer`, the record the client keeps
  per server offer, with `mime`, its preferred supported text type, and
  `announce` under the budgets; `UTF8` and `PLAIN`, the two text MIMEs a
  consumer offers and accepts; `OFFER_LIMIT`, `ANNOUNCEMENTS` and
  `MIME_BYTES`.
- `raster`: `Rect`, `Scale` (1 through 4), `Weight`, `GlyphStyle`, `Primitive`
  (`Fill`, `Glyph`, and `Mark`, a scalar of the hint face in one ink),
  `Draw`, `Surface`, the `Composition` trait, `Scrollbar`, `text_run`,
  `hint_run`, `Raster`, `Error`, the axis and frame-byte ceilings, the
  palette constants with the status inks `SUCCESS`, `WARNING` and
  `ACCENT`, and `rgb` and `ppm`, a painted frame as tight RGB rows
  and as a binary PPM. A
  composition reports the surface it was laid out for and streams the draws
  inside a damage rectangle; `Raster::new` validates surface, font, stride and
  buffer before any write, and `Raster::paint` refuses a composition laid out
  for another surface. `Raster::with_face` lends the raster an outline
  `face::Face`, through which it executes every `Glyph` from then on;
  `Raster::with_typeface` lends it a `typeface::Typeface`'s face at the
  raster's scale, or leaves the bitmap face when it has none.
  `Raster::with_theme` paints every draw's palette colours in a
  `theme::Theme`'s; a raster given none paints in `SAND`, the palette as
  it is.
  The behavioural contract (clipping, the medium fringe,
  scrollbar proportions and drag rounding) is the one td-editor/DESIGN.md
  records under "Implemented reference-renderer contract"; that text moves here
  with the documentation increment.
- `sfnt`: `Font` (`parse` over a caller's bytes, `units_per_em`,
  `ascender`, `descender`, `line_gap`, `glyph_count`, `glyph` for a
  scalar, `advance` and `outline`), `Outline` (`new`, `clear`,
  `is_empty`, `points`, `contours`, `push_contour`, and `overlap` with
  `set_overlap` for the font's overlap flags), `Point` in font
  units with its on-curve bit, `Error` (`Truncated`, `Missing`,
  `Unsupported`, `Malformed`, `Limit`, each naming its item) and the
  budgets `MAX_FONT_BYTES`, `MAX_TABLES`, `MAX_POINTS`, `MAX_CONTOURS`,
  `MAX_COMPONENTS` and `MAX_DEPTH`; the bounded TrueType reader under
  "Outline faces and the glyph atlas".
- `coverage`: `Rasterizer` (`new`, `rasterize` an outline at a scale in
  pixels per font unit), `Mask` (its `width`, `height`, `left`, `top`
  and row-major `alpha`, with `clear` and a bounded `get`) and
  `MAX_MASK_AXIS` and `MAX_OVERSAMPLED_AXIS`; refusals are
  `sfnt::Error`.
- `atlas`: `Atlas` (`new`, `page`, `epoch`, `len`, `is_empty`, `get`,
  `record`, `place`, `reset` and `take_dirty`), `Style` (`Regular`,
  `Bold`, `Italic`, `BoldItalic`), `Slot` (`Placed` with its `Entry`,
  `Blank`, `Missing`),
  `Entry` (its rectangle on the page and its bearing), `PAGE_WIDTH`,
  `PAGE_HEIGHT` and `MAX_KEYS`.
- `face`: `Face` (`new` over the regular style's bytes, an optional bold
  style's and a pixel size; `sized`, the same over shared style bytes at
  a fractional size; `fit` over shared style bytes and a grid cell's
  width and height; `with_sizing`, `fit` or `sized` as a `Sizing` (`Cell`
  or `PixelsPerEm`) asks; `resized`, the face's styles again at another
  sizing with an empty atlas; `with_slant` over an italic and a bold
  italic style's shared bytes, covered at the face's size, which empties
  the atlas; `id`, unique within the process to each face made and to
  each `with_slant`, which a clone keeps; `cell`, `size` (unrounded),
  `pixels_per_em`, `atlas`, `take_dirty`, `style` (the style bold and
  italic ask for, as the face has it: bold italic falls to italic, then
  bold, and any style to regular) and `glyph`), `Cell` (`width`, `height`,
  `baseline`, `pen`), `MIN_PIXELS_PER_EM`, `MAX_PIXELS_PER_EM` and
  `MAX_CELL_AXIS`; refusals are `sfnt::Error`.
- `typeface`: `Typeface` (`new` over the regular style's bytes and an
  optional bold style's, refused as a face fitted to the grid's cell at
  scale one is; `face` at a `Scale`, refitted when the scale changes).
- `face_file`: `DIR`, `INSTALLED` (the same directory under the XDG
  data home) and the four styles' file names (`REGULAR`, `BOLD`,
  `ITALIC`, `BOLD_ITALIC`); `SETTING` (`TD_UI_FACE`), the variable whose
  value a program passes, and `wanted`, whether that value asks for the
  face (all but `bitmap` do); `Place`, `places` and `host_places`, where
  the face is looked for, and `find`, the bounded search of them
  (`SEARCH_DEPTH`, `SEARCH_ENTRIES`); `INSTALL_HINT`, what a program
  without the face says to do; and `read`, the bounded read of one file.
  td-term reads the four styles through it.
- `theme`: `Theme` (`name`, `colors` in `KEYS` order; `map`, a colour
  as the theme draws it; `primitive`, a draw's colours mapped; `next`),
  `ROLES` and `KEYS`, the shared palette's colour for each role; the six
  themes `SAND`, `HARBOR`, `MOSS`, `ROSE`, `DUSK` and `EMBER`, and
  `THEMES`, their order; `named`; `CHORD`, the widget window's key;
  `FILE`, `MAX_FILE_BYTES`, `parse` and `text`, the file's name and
  contents; `MAX_APP_ID` and `path`, where a program's file is given the
  configuration and home directories. Under "Themes" below.
- `xdg`: `Base`, the per-user base directories; `dir`, the one rule
  (the variable when its value is absolute, else the fallback under an
  absolute `HOME`, else none: never a relative or shared directory),
  pure; and `from_env`, that rule over the process's environment.
  `theme::path` and td-mail's draft directory are built on `dir`;
  td-mail finds its configuration, cache, state and data, and td-news
  its configuration and cache, through `from_env`. `face_file::places`
  keeps its own copy of the rule: td-compositor and td-recipe-eval
  compile that file by `#[path]` without this module.
- `theme_file`: `host_path`, `theme::path` from the process's
  environment; `read`, the bounded read of the theme a file names, none
  when there is no file; `write`, its whole replacement; and `Kept`, a
  window's theme and its file (`default`, `SAND` keeping nothing; `load`
  from a path and `host` from a program id, each with the notice a bad
  file gives; `theme`; `advance`, which moves the theme and writes it,
  returning why it was not kept).
- `keys`: `Row` and `row`, a key and what it does; `Section`, a titled
  block of rows (`new` from `(keys, what)` pairs); `window`, the
  window's own section; `check`, `check_style` and `spelled`, the rows'
  spelling and style, with `SPACE` and `WORDS`; `Line` and `lines`, the
  list as text, and `sentence`, a description as it shows; `Help`, the
  list's state (`open`, `close`, `is_open`, `first`, `key` with its
  `Step`, `clamp`, `wheel`); `Panel`, its place over a surface (`new`,
  `page`, `columns`, `emit`); `Overlay`, the list as a window keeps it
  (`open`, `lay_out`, `key`, `press`, `wheel`, `emit`, `is_open`,
  `help`, `lines`); `CHORD`, `BUTTON`, `ITEM`, `MAX_KEYS_COLUMN`,
  `MIN_WRAP`, `MAX_COLUMNS`, `MARGIN`, `TITLE` and `TITLE_HINT`. Pure.
  Under "Key list" below.
- `pinned_face`: `load` from `host_places`, `load_in` given places and
  `load_from` a directory, the regular style through `face_file::read`;
  `SETTING`, re-exported; and `load_or_note` and `load_in_or_note` given
  places, which take that value, draw with Unifont without reading
  anything when it is `bitmap`, and otherwise say on standard error why
  a program draws with Unifont instead and `INSTALL_HINT`; and, for a
  terminal, `styles_from` a directory and `styles_in` given places, the
  four styles as one `Face` sized as a `Sizing` asks, read from the one
  directory that holds the regular style and refused whole if a style
  is missing or refused, with `styles_or_note` and `styles_in_or_note`
  taking the setting and saying so as the regular loaders do.
- `hint`: the hint face, a hand-authored 4x5 glyph (`WIDTH`, `HEIGHT`,
  `ADVANCE` 5) per printable ASCII scalar, `glyph` and the pixel `width`
  of a text; a scalar it lacks is a box. It is the small lighter text a
  button shows under its caption (`Button::emit_hinted`), never body text.
- `chrome`: `Bar` with its `Panel`, `Block`, `Strip`, `Button` and
  `Buttons`, `Slider` with `KNOB_WIDTH`, `Status`, `List` and
  `TextEntry`, the `Row` a panel paints, the `Item` a list paints, the
  `Field` a text entry paints and `step`, the bands, the paged list, the
  slider and the text entry a td-owned window shares, over `raster` and
  independent of any scene. The bar, a panel's rows, the tab strip, the
  button strip (a slider beside it) and the status row are each `ROW` (24)
  reference-renderer pixels tall, of 8x16 cells, scaled by the surface;
  the text block wraps in 16-pixel cell rows. `Bar` fills the first row
  and lays its labels from cell one, three cells apart, and answers a
  header hit. Its `Panel`,
  under a header and clamped to the right edge, is `PANEL_WIDTH` (320)
  pixels wide with one row per entry, at most `PANEL_ROWS` (13), and
  exists only with a status row's height below it; it paints each `Row`
  with the selected one highlighted, a disabled one dim, a checked one
  prefixed and the shortcut at the right edge, and `step` walks the
  enabled rows with wrap. `List` shares that row painter over a
  scrolling window the caller owns: `ROW`-tall rows filling a
  rectangle, the same highlight, dim and prefix, a marked row starred
  and an optional right-aligned `Item` column, empty rows below left
  chrome, and a scrollbar in a `SCROLL_GUTTER` (16)-pixel gutter at its
  right, all inside a `BORDER` bezel a scaled pixel wide round the
  rectangle, as a `Button`'s, so a list's bounds show against a field or
  pane beside it. The bezel takes no row geometry: `rows`, `row`, `body`
  and `hit` are the rectangle's as without it, and the rows and the thumb
  paint clipped inside it, so a row's outermost pixels are the bezel's
  and a thumb at the top of the track loses a scaled pixel, and at its
  foot whatever of the bezel the remainder below the rows leaves over
  it; `reveal` keeps the selection shown, `reveal_within` keeps a
  margin of rows shown on each side of it too where the list has them,
  the window stopping at the list's ends and the margin clamped to the
  whole number below half the rows, `(rows - 1) / 2`, so a large one
  centres the selection (an editor's `scrolloff`), and `hit` maps a
  point to a row. `Block` wraps a caption at its columns, the
  width less a cell each side, over up to `BLOCK_ROWS` (15) rows, a
  newline starting the next, at most `BLOCK_SCALARS` (`BLOCK_ROWS` * 73)
  scalars visited. `Strip` lays `TAB_WIDTH` (160)-pixel tabs from the left
  keeping the active one in view and clipping a tab that a narrower
  surface cannot hold, painting the active one paper and the rest chrome
  under a top and right border, a dirty tab starred and a `CLOSE_WIDTH`
  (24)-pixel close mark at its right; with no tabs it still fills its row.
  Document tabs retain that default. `with_close_buttons(false)` gives
  resource tabs the close-mark space for their labels and removes the
  close hit region. `hit` returns typed `Select`/`Close` intents only on
  the visible surface, including when a tab is partly clipped.
  `selection` handles `Previous`, `Next`, `First` and `Last`, wrapping
  previous/next and returning no selection for an empty strip. The caller
  stores the returned active index and chooses keyboard bindings; the
  strip's overflow layout keeps that active tab visible.
  `Status` shows one line in whole cells, the width less a cell each side
  and at most `STATUS_COLUMNS` (512), a control scalar blank and the last
  cell an ellipsis when the line is longer, under a top border; `frame`
  paints the row without a line for a consumer laying out its own status.
  `TextEntry` is one `ROW`-tall paper field inside a `BORDER` bezel a
  scaled pixel wide, as a `Button`'s, so a field's bounds show against
  the list or pane under it; the bezel lies in the text's cell inset, so
  the columns, `hit` and `reveal` are as without it. The caller drives it
  with a `Field`: the text from the first shown column, an optional
  selection filled `SELECTED` focused or `INACTIVE_SELECTION` not with
  the ink flipped over it, a one-pixel `INK` caret the caller blinks, a dim
  placeholder when empty, and a masked mode drawing a fixed mask glyph
  per character; `reveal` keeps the caret shown and `hit` maps a point to
  a caret column. Each streams its fills and glyphs inside a damage
  rectangle and reads nothing but its inputs; a draw-stream oracle pins
  each, and the status band, the list and the text entry are rasterized
  whole to pixels. td-editor's `Geometry` and `Scene` compose the bands
  and its `--preview` stays byte-identical.
- `entry_model`: `EntryModel`, the editing state a `TextEntry` paints:
  text bounded at a byte `limit`, at most `MAX_LIMIT` (1 MiB) and
  reserved by `new`, so no edit reallocates, with the caret, the
  selection anchor and the first shown column in character columns.
  `act` takes an `Action` (`Move` by a `Motion` with `extend`,
  `SelectAll`, `Backspace`, `Delete`, `DeleteWordLeft`,
  `DeleteWordRight`, `Insert`) and answers `Changed`, `Moved` when the
  caret or the shown selection moved, or `Ignored`; `Action::from_chord`
  is the default binding set (arrows, `C-` words, `S-` extends, Home,
  End, Backspace and Delete with `C-` for words, `C-a`, one printable
  character), and clipboard chords stay with the consumer. `paste` and
  `set_text` take text whole or refuse it, a control character or a
  line or paragraph separator as `Control` and a result past the limit
  as `Limit`, so the entry holds one line without a tab; `set_text`
  leaves the caret at the end with no selection, also for the same
  text. `copy` and `cut` hand the selection out as the clipboard's
  `Arc<str>`; `set_masked` makes them refuse with `Masked` and the word
  motions and deletes go to the ends. `place` maps a pointer point
  through `TextEntry::hit`; `drag` extends to any point, one column
  past a field edge, so a `reveal` after each drag event scrolls;
  `reveal` scrolls, an edit keeps the first shown column inside the
  text, and `field` gives the painter its `Field`. The bytes an edit
  leaves past the new end are zeroed in place, `clear` zeroes the whole
  text and drop the buffer, best effort as the editor core's is; a
  key's chord, a paste's source and the copies `copy` and `cut` return
  are outside the model. `Debug` shows the length, never the text. The
  masked mode is still no trust boundary (see "Invariants").
- `list_model`: `ListModel`, a `List`'s count, selection and first shown
  item, the geometry passed in on each call. The selection is a
  position: `set_items` takes a new count from a filter or a reload
  together with the position the selected item now holds, or none, so
  the consumer re-selects by its own identity. `step` takes a `Step`
  (`Up`, `Down`, `PageUp`, `PageDown` by the shown rows, `Home`, `End`;
  `Step::from_chord` the defaults) without wrapping; it, `select` and
  `press` through `List::hit` show the selection even when it is
  unchanged, and `scroll` moves the window alone. Each answers a
  `Change` naming whether the selection, the window or both changed.
  `relayout` follows a resize, and `with_margin` keeps rows shown
  around the selection through `reveal_within`. `window` names the item
  range a consumer hands `emit`, which paints through `List::emit` with
  no row highlighted when nothing is selected. Activation, a double
  press and filtering stay with the consumer.
- `messages`: `Message` (built from `new` with `text`, `section`,
  `excerpt`, `status`, `verdict`, `source` and `collapsed`), `Tone`,
  `Point`, `Shown`, `Key` with `from_chord`, `Event`, `Outcome`, `Error`,
  the budgets and `COPY_LABEL` and `MORE`, and `Controller`, the message
  list under "Shared message list" below.
- `notices`: `FONT_PROVENANCE`, `FONT_COPYING` and `FONT_LICENSE`, the
  texts beside the face in `td-compositor/assets`, embedded at compile
  time for a program's `--font-license` output, and `OUTLINE_FACE`, one
  literal naming the outline face and where its notices ship.
- `open`: `link`, which starts the browser on one whole link,
  `link_on`, the same on a display the caller names, `url`, the same on
  a URL a program lists from markup, `url_on`, that on a display the
  caller names (td-term's OSC 8 links), `is_url`, whether `url` takes a
  URL, and `file`, the same on a local file the program wrote; under
  "Following links" below.
- `wayland`: `Endpoint` and `endpoint` (from the `WAYLAND_SOCKET`,
  `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` values a consumer passes),
  `connect`, `Connection` (`new`, `send` with at most one borrowed file,
  `words`, `take`, `read_more`, `budget`, `pop_descriptor` with the
  pending `descriptors` count, the `wait` and `startup_deadline`
  accessors, and `waker`), `Waker` (`wake`; `Clone`, `Send`, `Sync`), the
  budgets `READ_BYTES`,
  `PENDING_BYTES`, `DESCRIPTORS`, `WRITE_DEADLINE`, `CONNECT_DEADLINE` and
  `IDLE_WAIT`, `backing_file`, `CURSOR_WIDTH`, `CURSOR_HEIGHT` and
  `cursor_pixels`, and `peer`, the test support: `drain` reads the far
  end of a socket pair a consumer's tests own (giving a blocking end the
  idle wait as its read timeout when it has none), and `push_descriptor`
  queues a right under the reader's bound without a socket; both are
  public because a consumer's tests are another crate, and `drain` is
  held to the connection's byte and right budgets. The FIFO itself is
  never lent out: a consumer pops rights one at a time in arrival order
  and cannot reorder or extend them. Errors are strings under the
  module's own `Result` alias, as the wire codec's are; a consumer that
  glob-imports the module shadows `std::result::Result`. Its behavioural
  contract is the transport invariant below. `sys`, the raw module
  beneath it, is private to the crate.
- `client`: `Client<T>` over one `Connection`, with the consumer's own
  object kinds as `T: Tag` (`retired` says which of them wait for
  `delete_id`) inside `Kind<T>`, beside the client's own (the fixed slots,
  pools, buffers, frames, the pointer image's, the seat, keyboard and
  pointer, and each board's manager, device, sources and sync barriers,
  with their retired states, each naming its `Board`); the fixed ids
  `DISPLAY` through `TOPLEVEL` and
  the budgets `OBJECTS`, `GLOBALS`, `NAME_BYTES`, `BUFFERS`,
  `MESSAGES_PER_TURN` and `INITIAL_DEADLINE`; `new`, the table (`allocate`
  and `set_tag` for the consumer's own objects, `kind` for any slot), the
  registry (`find_global`, `global_name`, `globals` for a consumer that must
  assert the compositor advertised an exact global set, `bind`, `required`,
  `is_required`, `forget_global`), the toplevel (`set_title`, `set_app_id`,
  `commit`,
  `acknowledge`, `close`), presentation (`can_present` and `present`, which
  refuses an extent the raster could not paint, then paints through the
  caller's closure into the reused raster and submits under the three-buffer
  rule; `present_changed`, the same for a closure told whether the raster
  holds the last frame at this extent that answers the pixel rows it
  changed (`Changed`), at most `DAMAGE_BANDS` bands; `buffers`, `pixels`,
  `frame_callback`, and `scrub_frames`), the
  devices (`seat`, `keyboard`, `pointer`, `entered` for the pointer's enter
  serial, `input` for the keyboard's state, and the repeat half of that
  state a consumer drives: `cancel_repeat`, `arm`, `repeat` and
  `wait_ms`), the clipboard (`clipboard` for a live data device,
  `selection` and `selection_mime` for the seat's selection and its
  preferred text type, `receive` to ask it for its text over a consumer's
  endpoint, `offer_selection` to offer text at a serial,
  `withdraw_selection` to destroy the live source without one, and
  `source` for the live source whose text the consumer keeps;
  `want_primary` before the initial roundtrip asks for the primary
  selection too, read through `primary`, `primary_mime`,
  `receive_primary`, `offer_primary` and `primary_source`),
  the pointer image's `cursor`, the state accessors `bound`, `configured`
  and `closed`, `activated` (whether the last applied toplevel configure
  carried the xdg `activated` state), `presented` (the buffer the last
  `present` attached, until another replaces it) and `focus_serial` (the
  keyboard enter's serial while the surface has focus, so one focus can be
  told from the next), `words`, `send` and `pop_descriptor` with the pending
  `descriptors` count, `needs_descriptor` for the keymap and send rights the
  client consumes, `connection` for the schedule inputs, and `handle`, which
  takes the consumer's clock, consumes what is the client's in an event and
  returns `Handled`: `Done`, `Bound`, `Configure`, `CloseRequested`,
  `FrameDone`, `GlobalRemoved`, `Capabilities`, `SeatRemoved`, `Keyboard`
  with a `KeyboardEvent` (`Keymap`, `Focus`, `Ready`, `Key` with its serial,
  key and `Stroke`, `Refused`, `Held` with the roles a modifier snapshot
  holds), `Pointer` with the decoded `pointer::Event`,
  `Clipboard` with a `ClipboardEvent` (`Selection`, `Send` carrying the
  right to write the offered text to, `Cancelled`, `Released`), `Primary`
  with the same for the primary selection, or
  `Unhandled` for the consumer's own objects. `App` (`client`,
  `needs_descriptor` for the consumer's own rights, `descriptor_wait`,
  `tick`, `event`, `end_turn`, `draw`) is what `run` drives: the registry
  request and initial sync under the startup deadline, then, until the
  client is closed, at most `MESSAGES_PER_TURN` events per turn with one
  event parked while its right has not arrived (the client's keymaps and
  sends and the consumer's rights alike, cancelling repeat), the consumer's
  end of turn and draw, and the transport's wait. `unconfigure` and
  `input_mut` are test support, public because a consumer's tests are
  another crate and hidden from the crate's documentation.
- `control`: `MAX_FRAME`; `Error` (`Protocol`, `Limit`) with its `code`,
  and `Result`; `ErrorCode`, the one method a consumer's error type gives
  so its codes can travel, and `valid_code`, the grammar of a code;
  `Refusal<E>` (`id`, `error`) with `response`, the error line, and `ok`,
  the success line; `Parse`, the request type a worker parses off the
  wire; `Decoder` (`push`, `payload`, `finish`) and `frame`; `Envelope`
  (`id`, `name`, `args`) and `envelope`, generic over the consumer's error
  so its parser can `?` through; and the codecs `decimal`, `size`,
  `boolean`, `hex` and `unhex`.
- `control_socket`: `Socket` with `bind`, `accept` and `close`, the
  private listener publication under "Private socket publication" below.
- `control_worker`: `CONNECTIONS`; `Worker<R>` (`start` for an `R: Parse`,
  `try_request`, `close`) and `Job<R>` (`request`, `is_live`,
  `respond_with`, `respond`), the bounded transport under "Bounded
  worker" below.
- `replay`: `run`, the consecutive-frame runner over a consumer's handler.
- `driven`: `KEY_BYTES`, `ARGUMENTS`, `PAGE_BYTES` and `WHEEL_LIMIT`;
  `PointerPhase`, `Input` (a key, the held roles, a pointer phase, wheel
  travel, a resize, focus, a tick) and `Outcome` with its `word`; `Binding`, one
  row of a consumer's action table, with `check`, `bound` and `help`; the
  `Controller` trait (`bindings`, `action`, `input`, `state`,
  `compose`, `request`); `VERBS` and `request`, the generic router;
  `Payload`, the worker's request type for a driven consumer; `text`,
  the read-back of a composition's draw stream; `paint` and the `Frame`
  it returns with its `ppm`; and `fnv1a64`, the frame digest.
- `clipboard`: `MAX_BYTES`, the text ceiling either way; `Outgoing`
  (`begin` over the send's right, `step`, `expired`, `cancel`), the
  bounded nonblocking writer of an offered text, which owns the right's
  status flags for the transfer and restores them; and `Incoming`
  (`begin`, `step`, `expired`, `finish`), the bounded reader of the
  selection's text over a private socket pair, handing it on whole at
  EOF as UTF-8. Both take the caller's clock in milliseconds, keep a
  five-second deadline from `begin` and make at most four 16 KiB
  attempts a step; a failure is terminal. They are transport owners
  over the raw module's pinned `fcntl` status commands (`UNSAFE.md`
  §19), not clipboard ownership, which is the client's.
- `window`: `DEFAULT_WIDTH` and `DEFAULT_HEIGHT`; `Flow`;
  `PointerPhase`, the driven seam's, re-exported; `Input<'a>`, the
  vocabulary a widget program reads, `Paste` among it; `Refusal`, why a
  copy or paste was not made (`NoDevice`, `NoFocus`, `NoSerial`,
  `Sending`, `Pasting`, `NoSelection`, `TooLong`, `NoEndpoint`), with
  its `Display`; the `Clipboard` trait
  (`available`, `has_text`, `pasting`, `copy`, `paste`), what a handler
  reaches with each input, `WindowClipboard<'w>`, the window's, and
  `NoClipboard`, which refuses everything, for a test or a headless run;
  the `Handler` trait (`app_id`, `title`, `input` with a clipboard,
  `poll`, `wait_ms`, `needs_redraw`, `paint`, `notice`,
  `take_withdrawal`, `take_scrub`, `keys`, `take_show_keys`); `Object`,
  the empty tag; `Window<'h, H>` (`new`, `with_typeface`,
  `with_theme_file`, `theme`, `help`, `handler`, `handler_mut`,
  `surface`), the `App` over a handler it
  borrows; and `run` with an optional `Typeface`, which reads the
  program's theme file, under "Widget window" below.
- The editor core, under "Editor core" below: `editor_text`, the text
  bounds and lossless codec; `editor_model`, the `Editor` with its
  documents, transactions and `RevisionPoint`s; `editor_fill`, the fill
  planner; `editor_keys`, the key profiles; `editor_layout`, the
  visual-row map and viewport; `editor_clipboard`, the bounded capture
  and paste; `editor_dialog`, the permits (`Discard`, `Reload`) and the
  conflict and close flows; `editor`, the `Controller` (`default` for a
  window, `pane` for an embedded pane) with its `Event` and `Outcome`;
  `editor_render`, the `Geometry` and the `Scene` that implements
  `Composition`; `editor_search`, find's `History`, `Found` and
  `Intent`; and `editor_error`, their `Error` and `Result`.
  `Controller::generation_for_test` is test support, public because a
  consumer's tests are another crate.
- `vt`: `Terminal`, the terminal model with its byte-stream parser (`new`
  at a grid, `feed`, `resize`, the `cell`, `row_text`, `cursor` and `mode`
  reads, `take_replies` one reply at a time and `replies`, `ring` and
  `take_bell` for the coalesced bell, the history reads and `scrollback`,
  `wrapped`, `primary_wrapped` and `history_wrapped`, which rows an
  autowrap ended, `search`, the nearest match of a query older or newer
  than a `Place` as a `Found`, `title`, the window title OSC 0 or 2
  set, `link`, the URI of an OSC 8 link a cell
  names (`Attributes::link`), `prompt`, the nearest line older or newer
  than one whose row holds a prompt's start (OSC 133;A,
  `Attributes::prompt`), `still_matches`, whether a found match's
  cells and wraps still spell its query, and `mouse`, the pointer
  reporting the child asked for), `Toward`, `Place`, `Found`,
  `MAX_QUERY`, `MouseMode` with `MouseTracking`, `Cell`, `Attributes`
  with its `Underline` style and underline color, `Color`,
  `MAX_DIMENSION`, and `selftest`. Pure; its specification,
  `vt_spec.rs`, runs the native corpus under `spec/vt`.
- `vt_render`: `Palette` (`pinned`, foot's with its own default ink),
  `Snapshot` (`new` with focus and bell, `with_cursor`, `scrolled_back`,
  `with_selection`, `with_link`, a `Hover` ruled in black or white
  against each cell's ground (`linked`): a `LinkSpan` of a row, or an
  OSC 8 link's id wherever its cells are,
  `with_status`, a line over the row at an `Edge`, its
  reads, `wrapped`, whether a row of the view goes on at the next, and
  `span` and `select`, a pointer gesture's unit at a cell and its
  selection from an anchor to an extent, carried across wraps), `Edge`
  (`Top` or `Bottom`),
  `Unit` (a cell, a word or a row) with `WORD_DELIMITERS`, foot's,
  `Cursor`, `Selection`, `render` of a snapshot into a tight XRGB8888
  surface over the bitmap `font::Font`, `render_with`, the same with an
  optional outline `Face` on that face's cell, `render_changed` with a
  `Drawn`, the same over the frame it last drew, painting only the view
  rows whose cells, hovered links or cursor differ and answering their
  pixel rows, or the whole frame when the surface held none or its
  surface, cell, face (by `id`), palette or grid moved or the bell is
  in this frame or the last; one `Drawn` serves one bitmap font,
  `cell_size` (the cell a
  grid is laid on: the outline face's, else the bitmap font's), `Zoom`
  (an outline face with the sizing it started at, stepped `ZoomTo::In`,
  `Out` or back to `Start` by `ZOOM_STEP`, foot's half a point), `ppm`
  and `from_ppm`,
  `BYTES_PER_PIXEL`, and `selftest`. Pure; its specification,
  `vt_render_spec.rs`, holds the goldens under `spec/vt_render`.
- `vt_keys`: `action(chord, modes, viewing)`, which routes one chord as the
  keymap spells it to `Action::Bytes` with a bounded `Sequence`,
  `Action::Scroll` with a `Scroll`, or `Action::Silent`; `sequence`, the
  bytes alone; `Modes`; `report`, the pointer's report of a `Pointer`
  (a press, release or motion of a `Button`) at a cell with its
  `PointerModifiers` under a `vt::MouseMode`, a bounded `Report` of at
  most `MAX_REPORT` bytes in X10's or SGR's encoding; `Viewport` with
  `Scrollback`, the scrollback view anchored to a line; `InputQueue`, the
  whole-or-nothing queue both encoders feed; `MAX_SEQUENCE`,
  `MAX_INPUT_BYTES`, and `selftest`. Pure and keymap-independent: it
  knows chords, never keycodes, and cells, never pixels.
- `vt_terminfo`: `parse` of the capability source into an `Entry` of
  `Capability` values, `compile` to the legacy binary format, `decode` back
  to `Decoded`, `entry` (the compiled `td-term` entry), `INSTALL_PATH`
  (`share/terminfo/t/td-term`), and `selftest`. Pure.
- `pty`: `Pty` (`open` unlocks `/dev/ptmx` opened without acquiring it,
  `master`, `into_master`, `peer` for the slave by descriptor, `window`,
  and `resize`, which publishes a grid and reads it back before trusting
  it), `WindowSize` and `window_size`, `grid_size` and `grid_for_tile`,
  `ChildCommand` and `spawn` (a caller-composed command on the slave, the
  environment cleared and set to the caller's list of OS-string pairs, a
  program named without a slash found on that list's `PATH` by std's
  rule, leading a new session
  whose controlling terminal is the slave exactly when `leads_session` is
  set), the threads
  `spawn_reader`, `spawn_writer` and `spawn_waiter` (named `pty-output`,
  `pty-input` and `pty-child`, each reporting its own ending on the
  caller's channel as `Output` or `Waited` and running the caller's
  `notify` after each send), `Input`, the bounded queue the loop fills and
  the writer drains (`new`, `push`, `close`), `write_input`, the
  `DEV_PTMX`, `READ_CHUNK`, `MAX_OUTPUT_BYTES` and `MAX_OUTPUT_CHUNKS`
  constants, and `selftest`. `Input::take_for_test` is test support,
  public because a consumer's tests are another crate and hidden from the
  crate's documentation. Which account, environment and command a
  terminal runs is its caller's policy, never this module's.

## Driving

A td program is operated by a person at the keyboard and by an agent
acting for that person, and APPLICATIONS.md §S asks that agents be a
first-class consumer rather than a reader of an accessibility tree bolted
on afterwards. The toolkit therefore carries the driven UI's mechanism,
so that every consumer speaks one protocol on one transport and a
consumer adds only its own vocabulary. The shape is td-editor's, the
tree's precedent: a display-independent dispatcher, a headless replay of
it, and a control socket on the live window, all speaking one envelope.
td-editor's CONTROL.md remains the reference for its own verbs and their
admission rules; this section is normative for what moved here.

The split is fixed. The toolkit owns the wire (frame, envelope, codecs,
the two response lines and the transport's two error codes), the private
socket publication, the bounded worker and the replay runner. The
consumer owns its request type, parsed behind `Parse` on the worker's
thread; the meaning of every verb; the body of its `state`; and the
decision, per request, whether it reads or acts. The toolkit never
decides that last question: as td-editor separates `response(&Controller)`
from `execute(&mut Controller)`, a consumer answers a reading verb from a
borrow and dispatches an acting verb through the same admission its
keyboard goes through. Reading is the lower authority (§S); a consumer
that wants to grant it separately does so at its own boundary, not in the
toolkit.

### Framing

One frame is a four-byte unsigned big-endian length followed by that many
payload bytes. Length excludes the header and must be 1..=1,048,576 bytes
(`MAX_FRAME`), for requests and responses. There is no newline terminator.
`frame` checks the bound before allocating its output. `Decoder`
accumulates one frame, accepts arbitrary header/body fragmentation, and
exposes its payload only after all declared bytes arrive. A zero length
is `protocol`; a length over the ceiling is `limit`. Incomplete `finish`
is `protocol`.

Any decoder refusal permanently poisons it and drops partial payload
storage. More input cannot revive it. Bytes beyond the declared body, if
supplied to that decoder, are refused rather than treated as a second
request. Completion does not prove peer EOF or that no later bytes will
arrive: the worker dispatches at most once per connection and closes it
after the response. `finish` transfers the completed buffer; it performs
no read or EOF check. Empty input chunks make no progress. The decoder
allocates at most one MiB of payload storage, only after validating the
length; the full declared storage is allocated on header completion,
before body bytes arrive, so the worker budgets each admitted connection
against that one-MiB allocation, not against bytes received so far.

### Envelope and responses

Payloads are ASCII tab-separated records: `1 ID NAME ARGS...` with literal
Tab separators. Literal control bytes other than Tab, DEL and non-ASCII
bytes are refused. `decimal` fields contain one or more digits only:
leading zeros are accepted; signs, spaces, exponents and overflow are
refused. IDs fit `u64`; `size` additionally fits the host's `usize`;
`boolean` is `0` or `1`. Byte strings are lowercase `hex`, two digits per
byte, the empty string spelled `-`. `envelope` validates the version, the
ID and the presence of a name, and hands the consumer the name with the
remaining fields as a bounded iterator, never a vector proportional to
the Tab bytes in an untrusted payload; unknown names, missing or extra
fields and argument grammar are the consumer's `protocol` refusals.

A success is `1 ID ok BODY` (`ok`); an error is `1 ID error CODE HEX`
(`Refusal::response`), the diagnostic being the code's own ASCII bytes
in hex. A code is one to 32 bytes of lowercase ASCII letters, digits and
hyphens (`valid_code`), since it stands unquoted in the tab-separated
line; the toolkit renders whatever `ErrorCode` returns, so a consumer
pins its codes against that grammar in its own tests. The toolkit
renders these two lines; a consumer may answer with a further status
word in the same position for its own protocol, as td-editor answers a
queued job with `1 ID pending JOB`, which its own reference specifies.
A recoverable request ID is echoed even if the name is missing or
later arguments are invalid; failures before a valid ID is parsed use
zero, so frame-size, ASCII and version checks precede ID parsing. Zero is
also a valid caller ID and carries no authorization meaning. The
transport's own codes are `protocol` and `limit`; a consumer's `Error`
maps them into its own type (`From<control::Error>`) and gives its codes
through `ErrorCode`, so one `Refusal<E>` renders every refusal, the
worker's included.

### Private socket publication

`control_socket::Socket::bind` opens a Linux Unix-domain listener only when
explicitly called. It does not accept a CLI option, create a worker, decode
a request, read consumer state or send a response. `accept` and each
accepted stream are nonblocking; connection limits, deadlines,
cancellation and turn-loop integration are the worker's and the
consumer's. No raw syscall surface or dependency is used: the filesystem
operations use safe `std` and procfs, which must be mounted at `/proc`.
Like the transport beneath `wayland`, the crate's raw guard requires
Linux x86-64 at compile time; these open flags name that ABI, not all
Unix platforms or Linux architectures.

The socket pathname must be absolute, non-NUL and at most 107 bytes, with
a nonempty basename of at most 80 bytes. Trailing slash, `.`/`..`
basenames and parent `..` traversal are refused. Interior `.` and
repeated separators have ordinary `Path` normalization; there is no shell
expansion or canonicalization through symlinks. Non-UTF-8 names remain
literal OS bytes. The basename cap keeps the internal descriptor-relative
bind name within Linux's pathname socket limit independently of the
current descriptor number.

The final parent must already exist, belong to the calling UID and have
mode exactly 0700, including no special bits. The binder neither creates
nor chmods directories. Every ancestor, including root, must be owned by
root or the caller and must not be group/other-writable unless sticky.
This admits ordinary `/tmp` while protecting caller/root-owned path
components from rename by another UID. An otherwise private parent
inside a non-sticky shared writable ancestor is refused.

Ownership is interpreted in the consumer's current user namespace.
Unmapped ancestor owners are not trusted: the overflow UID (commonly
65534) is not an alias for root. A rootless container must provide a
path whose complete ancestry satisfies these checks; the optional
endpoint otherwise refuses. Before walking the path, the binder reads
`/proc/sys/kernel/overflowuid` with an 11-byte limit plus one byte for
overflow detection, requiring a decimal `u32` with at most one final LF;
missing, malformed or oversized data refuses publication. If the
configured overflow value equals the caller UID or zero, it refuses:
otherwise an unmapped owner could be numerically indistinguishable from a
trusted owner. This includes callers legitimately mapped to the overflow
number; the binder cannot distinguish the two cases from stat ownership.
No fixed overflow number is assumed and no sysctl is changed. Concurrent
changes to that global sysctl require the already-excluded privileged
host authority. Linux's [user namespace
contract](https://man7.org/linux/man-pages/man7/user_namespaces.7.html)
specifies this substitution for stat and process-status ownership. The
predicate is never relaxed on environment variables or test execution.
The crate declares `trusted-test-root = true` in its gate metadata: the
gate's derived test commands then set the variable the capped runner
consumes to run each test binary under a private caller-owned root with
its own `/tmp`, so the fixtures' ancestry is trusted inside the gate's
namespace, where the host's root otherwise appears as the unmapped
overflow UID and publication refuses (DEVELOPMENT.md, "Trusted filesystem
roots for permission tests"). That is a test environment, not a
production admission; the socket module reads no such variable and its
confinement test pins that.

The UID comes from a bounded `/proc/self/status` read (64 KiB plus one
byte for overflow detection): exactly one complete `Uid:` record with four
equal representable `u32` values, real, effective, saved and filesystem.
Missing/duplicate/malformed/mixed records refuse. UID zero is allowed
when all slots agree; this is not a sandbox for root. Environment
variables are never evidence of identity.

The binder walks directory components from an opened root using `O_PATH
| O_DIRECTORY | O_NOFOLLOW`, one component at a time through
`/proc/self/fd/N`. These kernel descriptor links are intentional; no
user-supplied symlink component is followed. It retains the final
directory descriptor and binds through its pinned descriptor path rather
than reopening the user's absolute path. Every existing final name is
refused without connecting to it: regular files, directories, symlinks,
live listeners and stale sockets all remain untouched. A concurrent
binder is still subject to the kernel's exclusive pathname creation.
Socket address diagnostics report the internal `/proc/self/fd/N/name`
bind address, not the requested pathname. A peer must connect using the
requested path it was given, not reuse `peer_addr` as a path in its own
descriptor table. Callers retain the requested path for diagnostics; the
guard does not store another copy.

Binding briefly creates a listening socket with umask-derived permissions
before mode 0600 is set. The already-private parent contains that
interval to the same caller/root trust boundary; changing process-global
umask would race unrelated file operations and is not used.

The binder opens the new filesystem socket inode with `O_PATH |
O_NOFOLLOW`, requires a socket owned by the caller, and pins that
descriptor too. It sets mode 0600 through its kernel descriptor path and
reads back mode/owner, then rewalks the visible parent, rechecking trust,
private mode and identity, and verifies the named socket still matches
the pinned inode. This catches parent/name changes during publication.
The filesystem socket inode is distinct from the listener's socket
descriptor inode; cleanup pins the former.

Explicit `close` makes one checked cleanup attempt and reports errors.
Drop attempts the same cleanup best-effort. Before unlinking the original
basename inside the pinned parent, cleanup requires socket type and
matching device/inode. A missing name is already clean only after
rechecking that the procfs parent descriptor path remains accessible;
this is an accessibility check, not detection of a renamed or removed
parent. A real procfs link still addresses that pinned inode after either
operation. This also handles a name removed between the identity check
and unlink. A replacement file/socket is left alone. Renaming the parent
does not redirect cleanup into a replacement directory. Keeping the
socket inode open prevents inode-number reuse from fooling the
comparison. If binding succeeds but inode ownership cannot be
established, the binder does not unlink an unverified name; a stale
endpoint may remain for caller inspection. Later setup failures attempt
normal checked cleanup.

Identity checks and pathname operations are separate kernel operations,
not an atomic compare-and-unlink primitive. These rules protect against
other UIDs under the admitted permissions; they are not isolation from
root or another process running as the same UID. Such a process can race
bind-to-pin or check-to-unlink, rename names, or change permissions, and
already has the endpoint's authority. The consumer must keep this path
private. Sharing it across a jail boundary remains a separate explicit
grant.

### Bounded worker

`control_worker::Worker::start` takes an already admitted `Socket`. One
thread, named `td-ui-control`, owns the listener and every accepted
connection; no socket I/O occurs in `try_request` or `Job::respond`. The
thread holds no consumer state and no lock on it: the one thing it knows of
the consumer is the request type `R: Parse`, whose `parse` it runs on each
complete payload. The consumer's turn loop owns dispatch and command-line
opt-in.

The worker admits at most eight connections (`CONNECTIONS`). When all
slots are occupied, further clients stay in the kernel listener backlog;
no descriptors, threads or request allocations are created for them. No
backlog connection deadline or backlog size is promised. The five-second
whole-request deadline starts when the worker accepts, never renews on
progress, and includes header, body, queue/response time and output. At
or after expiry the connection closes silently. Every loop checks each
live connection, allowing at most one 16-KiB read or write for it, then
parks for at most ten milliseconds with live connections, or 100
milliseconds when idle. Replies, abandoned jobs and shutdown unpark the
worker early. A ready reply attempts its first write in the same step; a
refusal after a read waits until the next step to preserve the I/O
bound. This deliberately paces output near 1.6 MiB/s per connection: a
maximal one-MiB reply takes 64 steps, about 640 milliseconds, and
td-editor's largest hex-encoded text page, 512 KiB, 32 steps, about 320,
excluding scheduling delays.
Idle polling trades up to 100 milliseconds of acceptance latency for
fewer wakeups; blocking accept would require a separate reliable shutdown
wakeup. Scheduling delay and kernel execution are not hard real-time
guarantees; the worker never deliberately blocks on socket I/O.

The endpoint is not an availability boundary against the same UID or
root. Such a process can continually occupy all eight slots, keeping
legitimate clients in the backlog. Deadlines bound each admitted
connection, not a peer's share of future admissions. A full consumer
queue closes without an error frame.

One complete request dispatches at most once, without requiring write
EOF. The shared decoder rejects zero/oversized frames and extra bytes
delivered in the same read. After completion no further request bytes
are read: later bytes never execute a second command. A premature EOF or
decoder refusal queues a framed error with ID zero; a parse refusal uses
its recovered ID and the consumer's code. Transport failures, deadline
expiry or a full consumer queue close the connection without a
guaranteed error frame. Successful output closes after its one complete
frame, without waiting for the peer to close.

Eight typed jobs fit in the consumer's queue; submission is nonblocking
and a full/disconnected queue closes that connection. Each job has a
one-element response channel and a liveness token combining the
acceptance deadline and connection lifetime. `try_request` never waits
and makes at most eight receives per call, returning the first live job.
After eight expired entries it returns no job; remaining arrivals or
worker disconnection are observed on a later poll. A disconnected worker
reports an error once that bounded expired prefix has been drained.
Dropping a job without a response disconnects its response channel
before waking the worker to close the client.

The consumer answers through `Job::respond_with`, which checks liveness
before passing the borrowed request to the consumer's handler; the
handler applies the consumer's own admission and returns one payload.
The underlying `Job::respond` checks liveness and frame limits before
copying/queuing it; success means queued, not delivered or rendered. It
does not validate the handler's response fields. A disconnected or
expired job returns false; an invalid payload size returns the shared
frame error. Closing a connection cancels its queued/held job; a deadline
never reserves a consumer snapshot. This transport API does not replace
the consumer's live admission of a request against its state.

Request payload allocation is at most one MiB per reading connection.
What a parsed `R` retains is the consumer's contract (td-editor's holds a
fixed-size descriptor plus at most 256 KiB of decoded text); the worker
drops the wire payload when moving to the wait and may briefly hold
both. Each response channel/connection owns at most one framed reply of
one MiB plus four bytes. A reply may briefly coexist with its original
request allocation during handoff; there are at most eight such
connections and eight queued jobs. One reusable 16-KiB scratch buffer
serves the thread. These bounds exclude consumer-owned response inputs
and retained jobs, allocator overhead and kernel socket buffers; there is
no unbounded worker byte queue. A job's debug output is its request's;
a consumer whose requests carry text redacts it there.

Explicit `close` and Drop set the stop flag, unpark and join the worker.
It invalidates all live jobs, closes clients, and uses the socket's
checked cleanup. Explicit close reports worker/cleanup failure; Drop is
best-effort. No detached thread survives a completed join. Interrupted or
aborted accepts and temporary descriptor/memory/buffer exhaustion retry
after the normal park; other listener errors stop the worker and socket
Drop attempts cleanup. A deadline representation overflow also stops it
rather than accepting without a deadline. Shutdown is not a wall-clock
promise about filesystem operations or host scheduling.

### Replay runner

`replay::run` reads consecutive frames from a reader until EOF and writes
one framed reply per frame, produced by the consumer's handler, to a
writer, flushing after each. EOF between frames ends the session
normally; a partial header, a zero or over-ceiling length, a short body
or a reply the frame encoder refuses is an error that ends it. The runner
owns no state and no descriptor: a consumer's `--replay` hands it its
stdin and stdout and a closure over its own headless session, so the
same parser and dispatcher answer both the socket and the replay.

### The semantic seam

`driven` is the half a consumer implements once so an agent or a test can
operate it without a protocol of its own; td-photo consumes it first. A consumer
gives a `Controller`: `bindings`, its closed action table; `action`, applying a
named action with the fields the request carried; `input`, delivering one
semantic `Input` (a key chord, the roles held with no key under them, a pointer
phase at a pixel position, wheel rows and columns, a resize, focus, a tick)
through the same key and pointer paths its window uses; `state`, the
tab-separated body of its facts; `compose`, one
reading of what the window shows now, building a scene that borrows its model
per request if that is how it draws; and `request`, its own verbs beyond the
generic set, `protocol` by default. Its `Error` maps the transport's two in and
gives its codes out through `ErrorCode`, as for the transport. td-editor keeps
its own `Event`, whose tab and revision fences are its admission contract, and
does not adopt the seam; td-setup and td-portal adopt it when their pages need
driving.

The action table is `Binding` rows: the name `action` takes, the chord
the consumer's keyboard binds by default, the argument shape and a help
line. `check` holds a table to its grammar (names in the code grammar and
unique, chords unique and within the key bound, shapes and help printable
ASCII, help present), and a consumer pins its table with it; `bound`
answers which action a chord names; `help` prints the table aligned,
which a consumer's `--help actions` shows so an agent reads the table
instead of guessing.

`request` routes one payload over `control`'s envelope, at most eight
fields after the verb, and answers as `control` frames it (field spaces
denote literal Tabs):

- `1 ID state`: the consumer's body.
- `1 ID actions`: `N`, then per action its name and, each in hex with
  `-` for none, its chord, argument shape and help.
- `1 ID action NAME ARGS...`: NAME must be in the table, else
  `protocol`; the consumer's `action` judges the fields; the reply is the
  outcome word, `changed`, `ignored` or `quit`.
- `1 ID key HEX_CHORD`: the chord, 1 to 32 bytes of UTF-8 without control
  characters, through `input`; the outcome word.
- `1 ID held PREFIX`: the roles held as `Held::prefix` writes them
  (`C-M-S-` in that order, `-` for none), through `input`; the outcome
  word. It is what the keyboard reports on a modifier change with no key
  under it, so a consumer's hints can be driven.
- `1 ID pointer PHASE X Y`: `press`, `move` or `release` at pixel X, Y,
  each a `u32`; the outcome word.
- `1 ID wheel ROWS COLUMNS`: signed whole cells, magnitude at most
  16,777,216; the outcome word.
- `1 ID resize W H SCALE`: refused as the raster's `Scale` and `Surface`
  would refuse it, before the consumer sees it; the outcome word.
- `1 ID focus 0|1` and `1 ID tick MS`: the outcome word.
- `1 ID text`: `ROWS COLUMNS HEX_TEXT`: the cell grid's rows and columns
  and the read-back below, of at most ROWS lines.
- `1 ID frame`: `W H SCALE DIGEST`, the FNV-1a 64 of the frame's RGB rows
  in hex.
- `1 ID frame-page OFFSET LIMIT`: `W H OFFSET HEX_BYTES` of the RGB rows,
  LIMIT 1 to 256 KiB (`PAGE_BYTES`), an offset past the end `protocol`.
- A generic verb (`VERBS`) with fields it does not take: `protocol`,
  at the router. Anything else: the consumer's `request`.

The reading verbs borrow the controller; `action`, `key`, `held`, `pointer`,
`wheel`, `resize`, `focus` and `tick` go through the consumer's own
admission, which is where reading and acting are told apart (§S). The
digest is an equality witness for tests and agents, not a hash for
anything adversarial; a frame of any size crosses the wire in pages under
the frame ceiling, and `paint` repaints per request, so the pages of one
frame are consistent only while the consumer's state is.

A socket consumer runs `Worker<Payload>`: `Payload`'s `Parse` validates the
envelope and the field count on the worker's thread, so a malformed frame is
refused there as the transport promises, and the turn answers each job with
`request` over the payload's bytes, where the verb and its fields are judged
against the consumer. A replay hands `request` each frame's payload. `quit` is
reported, not acted on: the runner keeps answering and a window closes on its
own terms, so a consumer tracks its own quit.

`text` reads the composition's draw stream onto the surface's cell grid at its
scale: each glyph lands in the cell its origin names (an origin off the grid, a
glyph straddling the top or left edge included, names none), a later draw over
an earlier one, a fill clearing every cell it wholly covers, so an opaque panel
hides what it paints over, a glyph whose clip excludes it wholly absent, a
mark (a hint's pixels, not text the window shows) passed over; rows
are trimmed on the right and trailing blank rows dropped, so the text has at
most ROWS lines. That is the accessibility question answered at the seam the
toolkit already has: every widget emits its text as Unicode scalars, so nothing
is retyped for the agent. `paint` paints the composition into a fresh buffer
through the pinned face, parsed once per process, and returns the `Frame`, its
surface and tight RGB rows, whose `ppm` is the binary image; `raster::rgb` and
`raster::ppm` are the one XRGB-to-PPM writer, which td-setup's `preview_ppm` and
td-editor's `--preview` use (td-photo's PPM writer is for its RGB thumbnails,
not frames, and stays its own).

## Invariants

- Pure modules read no environment, clock, descriptor or filesystem.
  Adapters pass explicit ticks in milliseconds and explicit byte inputs.
  Outside the pure set are `notices` (three `include_str!` constants,
  one literal, and nothing else), the transport pair `wayland` and
  `sys`, which own the stream, its deadlines and the pool files in the
  directory a consumer names, `client`, whose `run` reads the monotonic
  clock for the consumer's ticks and whose buffers are those pool files,
  and the three driving adapters: `control_socket`, which owns the
  listener it binds and reads procfs for the caller's identity;
  `control_worker`, which owns its thread and reads the monotonic clock
  for its deadlines; and `replay`, which reads and writes only the
  streams it is handed; the widget window `window`, whose `Window::new`
  reads the embedded face, whose loop is `client::run`, and whose `run`
  reads and, on its chord, writes the program's theme file through
  `theme_file`; `open`, which reads `BROWSER`, starts the browser as a
  child process with its streams closed and reaps it on a thread of its
  own; `face_file`, which reads `HOME`, `XDG_DATA_HOME` and
  `XDG_DATA_DIRS` for the places it searches, lists directories within
  its depth and entry bounds to find the outline face, and reads one of
  its files, a regular file within the reader's bound, checked before it
  is opened and again on the open file, which it opens without waiting,
  writing nothing; `pinned_face`, which reads only through it;
  `theme_file`, which reads `XDG_CONFIG_HOME` and `HOME` for the theme
  file's path, reads that one file under the same checks to
  `MAX_FILE_BYTES`, and replaces it by making its directory (0700),
  try-locking it and renaming a private (0600) sibling over the file;
  and `pty`, which opens `/dev/ptmx`, spawns the caller's command and
  owns the threads around it. `xdg`'s `from_env` reads one base
  directory's variable and `HOME`. Apart from `open`'s `BROWSER`,
  `face_file`'s three directory values, `theme_file`'s two and `xdg`'s
  two they read no environment variable, taking the display values,
  the socket path, the face setting and a child's whole environment as
  explicit arguments. The terminal's pure modules are `vt`, `vt_render`,
  `vt_terminfo` and `vt_keys`: bytes, sizes, chords and snapshots in;
  cells, replies, pixels and byte sequences out.
- `control` and `driven` are pure: the frame, envelope and codecs touch
  no descriptor, and the seam reads only the composition it is handed
  and the embedded face. The decoder allocates at most one frame, after
  validating its length, and `frame` checks the ceiling before
  allocating its output; the envelope hands fields on as a bounded
  iterator; the codecs and `ok` allocate in proportion to the caller's
  own input, which the caller budgets (hex-encoding a page allocates
  twice its bytes before `frame` judges the reply). The worker knows the
  consumer only as `R: Parse`; it holds no consumer state, and the one
  parse site is `R::parse`. Whether a parsed request reads or acts is
  the consumer's decision at dispatch, never the transport's.
- The transport: `endpoint` prefers `WAYLAND_SOCKET`, which `connect`
  duplicates close-on-exec rather than adopting, so the borrowed original
  stays open until its owner closes it and the consumer must give the
  transport exclusive use of the stream, whose timeouts are shared;
  otherwise an absolute `WAYLAND_DISPLAY` needs no `XDG_RUNTIME_DIR`, a
  relative one (default `wayland-0`) is joined to an absolute
  `XDG_RUNTIME_DIR`, and an invalid explicit value fails without trying
  another display. `backing_file` creates a pool file with `create_new` and
  mode 0600 in the directory the consumer names, under a checked
  process-local serial with 64 collision attempts, unlinks it at once, and
  hands back the owned `File` through which alone it is sized and written;
  an unlink failure reports the residual name. Every received right is owned
  at once in the eight-right FIFO, independent of message boundaries, and
  overflow, parse failure, protocol failure and disconnect close every
  retained right. `connect` gives a path connection attempt one bounded
  worker under `CONNECT_DEADLINE` and drops any late result; each outgoing
  message has one absolute `WRITE_DEADLINE`, including retries after
  temporary backpressure, capped by the remaining startup deadline while the
  consumer holds one, and reads use the same remaining startup budget; a 16
  KiB read feeds a 128 KiB pending-byte budget and an eight-right FIFO; the
  idle reader waits `IDLE_WAIT` through a socket timeout, or elapsed-time
  backoff for an inherited nonblocking socket whose shared status flags it
  never changes, and a consumer's `set_wait` shortens that wait. Once a
  consumer has asked for the connection's one `Waker`, the wait is a
  `poll` over exactly two descriptors, the stream and the waker's own end
  of a private nonblocking datagram pair, readable only, for the same
  budget rounded up to a whole millisecond and capped at 65535 (a longer
  wait ends early and the loop waits again; a zero wait is a probe); a
  wake ends it after draining at most 64 queued wakes, so wakes sent
  together are one, a wake sent before the wait is not lost and one left
  queued is another turn, and the stream is read only when poll reports
  it, for what is left of the budget. A program whose work arrives on
  another thread (td-term's PTY output) wakes the loop when it sends
  instead of being served on a short wait. A waker whose connection has
  gone reports an error, without a `SIGPIPE`, rather than blocking.
- The client's table is dense: the fixed ids 1 through 9 are published in
  order before any dynamic id from 10, a slot is reused only after
  `delete_id` (for a consumer's object, only when its tag says it is
  retired), and exhaustion is a diagnostic. A consumer allocates and re-tags
  only its own objects: the client's slots and free ones refuse `set_tag`,
  so no consumer can free, fix or double-allocate an id. At most 128 live
  registry entries of at most 256 bytes each; an invalid event, an
  unexpected `delete_id`, a protocol error, or the removal of a required
  global the consumer does not forget ends the connection with a diagnostic.
  At most 256 events are dispatched before the turn ends and draws again.
  The first buffer must be submitted within 20 seconds of the registry
  request; after it a hidden surface may wait indefinitely for its frame
  callback. `present` refuses a zero axis, an axis past the raster's 8192
  ceiling or a frame over 32 MiB before any request. At most three backing
  files stay live; a submitted buffer is immutable until
  `wl_buffer.release`, a frame callback does not release it, a free buffer
  of the frame's pixel extent is preferred (the extent alone sizes the pool,
  so a consumer's other layout changes reuse it), a free one of another
  extent is destroyed and replaced, and while all three are busy nothing is
  painted. A frame is painted into the client's one raster and written
  into the buffer's file: whole when the file holds no frame (new, or
  zeroed by a scrub), otherwise only the pixel rows changed since the
  frame it holds, by the client's count of frames and the frame each
  row last changed in. The commit damages the rows the paint answered,
  one row when it answered none, or the whole buffer. A paint that fails
  leaves the raster holding no frame, as a scrub does, so the next paint
  is told so. The pointer image is one immutable 1536-byte ARGB8888 pool
  built on the first `show_cursor` after ARGB is advertised, its role set
  before its first attach; later calls only re-send `set_cursor` with the new
  serial, relying on core wl_surface content staying attached across pointer
  leave and unmapping (enter makes the pointer-image association undefined,
  not the cursor surface's committed contents). `run` starts every turn from
  the idle wait, so what the consumer's `end_turn` sets decides each turn's
  wait; an event whose right has not arrived is parked in wire order,
  capping the wait by its remaining write deadline, and the consumer's
  `descriptor_wait` is told each turn it waits.
- The seat is bound on the initial roundtrip, after the toplevel: the lowest
  global offering `wl_seat` v5 or newer, at the lesser of its version and 7,
  and marked required; without one the client is still bound and the
  consumer decides. The keyboard and pointer are created only after their
  capability bits, the pointer first, and released when a bit clears; a seat
  name over `NAME_BYTES` and an unknown seat event end the connection. A
  released device's events are schema-checked and drained until `delete_id`,
  a keymap's right dropped unread, and its replacement is a fresh id.
  Removal of the bound seat's global releases the keyboard, the pointer and
  the seat, forgets the global and is reported as `SeatRemoved`, not as a
  fatal required removal. Keymaps are the client's one right consumer
  (`UNSAFE.md` §19): format 1 only, a regular file covering the advertised
  1..=1 MiB extent read positionally at zero, so the compositor's shared
  offset never moves, UTF-8 with one trailing NUL, compiled whole by
  `keyboard` before any press translates (the compiler's own `parse` takes
  the NUL as optional); a refused map leaves none, and every map cancels
  repeat and awaits a modifier snapshot. Keyboard enter and leave must name
  the surface and are the repeat policy's focus gain, with the held keys,
  and loss; the modifiers event is its snapshot, and the first after a map
  and focus is `Ready`, from which presses translate, and each later one
  whose roles (`Keymap::held`) differ from the last reported is `Held`,
  once per change (the `Ready` snapshot's roles are the baseline, not
  reported: a consumer starts from none, and a release from them is a
  change; leaving, and a new map, clear the roles without a report,
  `Focus(false)` and `Keymap` being the consumer's cues); the timing
  event is
  its rate and delay, at the consumer's clock. The pointer's events are
  decoded by `pointer`, which refuses unknown opcodes, truncated or trailing
  payloads, invalid axis and source numbers and invalid button states; its
  enter must name the surface, is remembered as `entered` until leave, and
  shows the pointer image at its serial, or on ARGB's arrival while inside;
  the image is built once, and later enters only re-send `set_cursor`.
- The clipboard: one seat-bound data device over core v3, bound after the
  seat at the v3 cap and not required, only when the initial registry offers
  both, its manager and device taking the two ids after the seat's; a
  manager advertised later is not bound. Offers are the server's ids at or
  above 0xff000000, at most `OFFER_LIMIT` retained, live and retired
  together, and a live id never reoffered, either an error; each offer's
  first `ANNOUNCEMENTS` MIMEs are inspected and later ones drained and
  ignored, of which the two text spellings, in any ASCII case and up to
  `MIME_BYTES`, are kept as announced, the explicit UTF-8 one preferred and
  no other encoding or parameter form considered. The selection names a live
  offer or nothing and retires every other; keyboard leave, keyboard loss
  and the device's release retire every offer and clear the selection; a
  drag's offer is retired without accepting, finishing or selecting it, a
  drag naming the selection's offer is an error, and a retired device's
  offers are retired at once. A retired offer is destroyed once and its id
  dropped behind one outstanding `wl_display.sync`, a batch of retirements
  sharing one, by id and generation, so an id reused before the callback
  survives it; retirements while one is outstanding coalesce behind the
  next. `offer_selection` creates a source advertising `UTF8` then `PLAIN`
  and sets the selection at the consumer's serial, destroying the previous
  source; `withdraw_selection` destroys the live source, if any, and sends
  nothing else; a send on the live source for either MIME, in any ASCII
  case, pops its right and hands it on as `Send`, any other send (a
  retired source's, another MIME's) pops and drops exactly its right, and
  the compositor's cancel destroys the live source and is `Cancelled`.
  Removal of the manager's global releases the source and device and
  is `Released`; seat removal retires the offers with the keyboard,
  then releases the pointer, the source and the device, then the seat;
  the manager has no destructor and stays inert, and a retired device's
  events are schema-checked and drained until `delete_id`. Every event
  schema is checked whole before a right is waited for or consumed.
- The primary selection: only for a consumer that called `want_primary`
  before the initial roundtrip, one seat-bound
  `zwp_primary_selection_device_manager_v1` device at v1, bound after the
  clipboard's under the same rules and taking the next two ids, its
  outcomes `Primary` rather than `Clipboard`. It is the clipboard's
  machinery over a second board: the same offer budgets, selection,
  retirement barriers (one outstanding per board), source with both text
  MIMEs, send, cancel and release, and the same focus rule, keyboard leave
  and loss retiring both boards' offers. Its protocol has no drag, no
  actions and its own opcodes, which `Board` holds: `set_selection` 0,
  device destroy 1, offer `receive` 0 and destroy 1, and the source's
  `send` event 0. Its manager is left inert like the clipboard's, though
  it has a destructor. The boards share the server's id space, so an offer
  id one board holds live is an error on the other, and one it holds
  retired, its barrier still out, is dropped there and owned by the newer
  board.
- The repeat policy (`repeat`): held keys up to the held-set budget;
  focus gain installs its held keys without typing or arming repeat, and
  presses wait for the modifier snapshot that follows; duplicate presses
  and unmatched releases are ignored; focus loss clears the held keys,
  the modifiers and the repeat; a modifier change and any new press
  cancel the old repeat, a release only its matching one; a map change
  cancels repeat and requires a new snapshot; a negative rate or delay is
  malformed and a zero rate disables repeat; rates above 1000 Hz clamp,
  intervals round upward to whole milliseconds, and a new positive rate
  and delay retime the current repeat from the clock they arrive with; at
  most one repetition fires per `repeat` call, missed repetitions are
  dropped, never burst after a stall, and timer arithmetic is checked,
  with exhaustion disarming the repeat.
- The raster writes only inside the validated surface and each draw's
  clip; row padding and bytes beyond the frame are untouched, and every
  refusal precedes every write. Medium-weight fringe colours derive from
  the explicit background a caller paints, never from buffer bytes.
- The pixel backend is a seam. Every widget and scene emits a semantic
  draw stream, `Fill` rectangles and `Glyph` scalars in a `GlyphStyle`,
  into a sink; none reads or writes buffer pixels. A `Glyph` names a
  Unicode scalar and a style, never a bitmap, so the glyph source is the
  font mount's alone. `Raster` executes that stream into a software XRGB
  buffer today; because the stream, not the buffer, is the toolkit's
  contract, a backend that keeps the cell model is a swap below this seam,
  executing the same draws at the same scalar positions: a GPU or
  glyph-atlas executor, or antialiasing that shades a glyph's coverage
  within its cell. Only proportional or variable-advance layout, which
  moves the scalar positions the widgets emit, is a remodel above the
  seam. td-editor/DESIGN.md owns the reference backend's operation set and
  lists the version-1 exclusions, antialiasing among them. Draw-stream
  oracles are the portable contract every backend keeps; the exact-pixel
  oracles and the `--preview` checksum pin the current software backend.
  `Raster::with_face` executes `Glyph` through an outline face's atlas,
  opt-in per raster, and changes no draw. It paints each glyph in the
  face's cell, so it keeps the cell model only for a composition laid
  out on that cell; the widgets lay out on the bitmap grid, so a
  consumer opts in with a face fitted to that grid (`Face::fit`) until
  runtime cells (increment 25).
- The atlas is one 1024 by 1024 page (1 MiB) of at most 8192 keys, placed
  and missing together, with a pixel of gutter right of and below each
  entry, zeroed when it is placed and inside the dirty band; a full page
  or spent budget resets it whole under a new epoch, a held key keeps its
  slot, and a mask that with its gutter would not fit the page, or whose
  alpha is not its width by height, is recorded missing. An entry is valid
  in the epoch that placed it. A face is 6 through 256 pixels per em with
  a cell of at most 512 on an axis, each style covered at its own units
  per em. The executor writes only inside the glyph's cell and the draw's
  clip, only pixels with coverage, each the style's background moved
  toward its ink by the coverage, and never reads the buffer; a scalar the
  face lacks or refuses draws the bitmap face's glyph centred in the cell,
  and other primitives are the bitmap raster's unchanged.
- The outline reader and its coverage are bounded before they allocate
  or recurse: a 32 MiB font, 64 tables each inside the file, a glyph's
  range inside `glyf`, 8192 points and 1024 contours per outline with
  composites included, a composite depth of 8 and 256 components per
  glyph at every depth together, and a mask of at most 512 pixels on an
  axis. A refused glyph leaves an empty outline, a refused mask the
  caller's mask as it was. Hinting bytecode is skipped, never run.
- Every budget carries over from td-editor unchanged: 1 MiB keymaps, the
  parser's token, depth, keycode, type, virtual-modifier, level,
  interpretation and modifier-map ceilings, and a 768-key held set.
- Production code has no `unwrap`, `expect`, panics or panicking indexing;
  invalid input returns a diagnostic or error naming the item.
- `unsafe` is confined to `sys`, the transport's raw module, under
  `UNSAFE.md` §19: three function-scoped allowances, one syscall
  instruction carrying `recvmsg`, `sendmsg`, `fcntl` pinned to
  `F_DUPFD_CLOEXEC`, `F_GETFL` and `F_SETFL`, `poll` over the connection's
  stream and its waker, `ioctl` pinned to the five PTY requests
  `TIOCSPTLCK`, `TIOCGPTPEER`, `TIOCSWINSZ`, `TIOCGWINSZ` and `TIOCSCTTY`,
  one wrapper each with the request never a parameter, and `setsid`, one
  descriptor adoption site, one pre-exec hook that makes a PTY child lead
  a session on its slave, and a crate root that denies it. Only `wayland`,
  `clipboard` and `pty` name the module, the transport through its four
  wrappers, the clipboard's destination owner through the two status
  ones, and the PTY through the four ioctl ones and the session hook, each
  at one site. Reusing it does not transfer
  authorization to a new consumer, which gets its own roster entry;
  td-term reaches the PTY only through `pty::Pty` and forbids `unsafe`.
- The terminal modules keep the bounds `td-term/DESIGN.md` §2 states
  (CSI parameters, grid, history, reply, keyboard-queue and PTY-output
  ceilings), admit a key sequence or a reply whole or not at all, and
  never join a PTY reader or writer on a teardown path (§4 there). That
  document is the terminal's behavioural specification; this one does not
  restate it.
- The shared font, wire, filter and report-text sources are mounted here by
  exact repository path and nowhere else among td-ui's consumers. A future
  move of their canonical home updates staging, check mappings and every
  consumer atomically, as td-editor/DESIGN.md already requires.
- The text entry's masked mode is a plain rendering option, not a trust
  boundary. It draws a fixed mask glyph for each of the field's
  characters instead of the characters, so a consumer can collect a PIN
  or passphrase; the widget asserts nothing about whether the field is
  trusted, and td-ui does not gate its use. Anti-spoofing is the
  consumer's: a field collecting a secret must be presented only within
  the compositor's secure-attention and trusted-input path, which
  `td-install/ENCRYPTION.md` and Principle 7 require. td-ui provides the
  primitive; the consumer verifies it is used appropriately. A masked
  `EntryModel` adds only that it never hands its text to the clipboard
  and that its word motions do not show where words break; that is no
  trust boundary either.

## Test contract

`tests/keyboard.rs` and `tests/xkb.rs` are td-editor's suites moved intact,
with the `tests/fixtures` directory: the libxkbcommon-generated `us.xkb`
map, the independent type and key oracles, and the retained
`XKB-COPYING`. `tests/fixtures/README.md` records their provenance and
reproduction. td-editor's in-file window tests read the same fixture by
relative path rather than carrying a copy.

`tests/raster.rs` holds the pixel oracles moved from td-editor's render
suite (fills and glyphs at every scale and weight against per-pixel
references), the validation order, a literal surface's ceilings, a
composition for another surface refused unpainted, and scrollbar
proportions along both axes. td-editor's render suite keeps the
scene-level oracles and the `--preview` checksum, which is byte-identical
across the move.

`tests/sfnt.rs` encodes its own fonts, with every compact coordinate form,
flag runs, both loca formats, both character-map formats and composites,
and holds the reader to them: metrics, format 4 through deltas and the
glyph array, format 12 across planes and preferred over format 4,
advances past the long metrics, every coordinate decoded exactly, empty
glyphs, composite offsets, the three scale forms, scaled offsets, nesting
and point matching, and every directory, fixed-field, loca and glyph
refusal by the item it names, a glyph header under ten bytes among them;
the point budget admitted exactly, the component budget admitted at 256
and refused at 257 in one glyph and crossed by a fan-out, both offset
flags together taking the unscaled one, a broken format 12 passed over
for the format 4 beside it and refused when alone, and both overlap flags
read and cleared with the outline.
`tests/coverage.rs` holds the rasterizer's oracles over hand-built
outlines: an aligned square exactly covered and placed, half-pixel edges
at half coverage, direction independence, a hole and a clamped overlap, a
triangle pixel by pixel against a 64x64 point-sampled reference with its
area, a curve and an all-off-curve contour against their closed-form
areas, start-point independence, empty and degenerate outlines, every
refusal leaving the mask untouched, reuse across sizes, flagged overlaps
resolving to their union on the finer grid (and past its axis covered
unrefined), and a mask reading zero outside itself. `tests/fonts` is
the encoder those fonts come from, shared with `tests/face.rs`.
`tests/atlas.rs` holds the page over hand-built masks: placement apart
with its gutter, the coverage copied intact, shelf sharing, the dirty
band, blank and missing slots taking no space, the reset of a full page
keeping only the mask that did not fit, the key budget, each entry's
gutter zeroed after a reset over opaque bytes, a held key keeping its
slot and a short alpha recorded missing.
`tests/face.rs` holds the face and the executor: the cell from encoded
metrics at two sizes, the size, advance and byte refusals, a glyph covered
once with misses remembered, and pixel oracles over a buffer of garbage
(so nothing is read back): ink and the half-covered blend from the
explicit background, the cell and draw clips, blank glyphs writing
nothing, the missing glyph equal to a bitmap raster's centred one at
scales one and two, every weight drawing the regular style even when the
face has a bold one, the bold style at the bold file's own units per em
and falling back to its regular outline, a Bold lookup on a face without
one the regular slot, fills and marks unchanged, and a padded stride at
scale three whose padding is never written, with origins at the i64
extremes writing nothing. For the fit it holds the cell, size, baseline
and pen for the four grid cells and two others, the fit's refusals, an
exact square from the pen and baseline, a full-width glyph leaving no seam
between two cells, an integral advance centred to the pixel, a tall line
box leaning up centred with its baseline below the cell, and a scalar the
face lacks equal to the bitmap raster's own draw at scales one to four.
For the slanted styles it holds the style each of bold and italic
resolves to with and without each style, the italic style at its own
units per em, bold italic on a face without it the italic slot, a scalar
italic lacks covered from the regular outline, a fitted face covering
its italic at the fitted size, a refused italic, and replacing the
slanted styles emptying the atlas so an old slot does not answer.
`tests/typeface.rs` holds the face fitted at each scale, kept while the
scale holds and replaced when it changes, the typeface's refusals, a
raster given a typeface drawing exactly as one given the face fitted at
its scale, at two scales in turn, the `notices` literal naming the
directory the loader reads, and the loader over a directory the test
writes: the regular style read, and a missing file, a file past the
reader's bound (refused before it is read), a refused font and a
directory each an error naming the path.

`tests/chrome.rs` holds the band oracles, draw-stream checks that read each
band's fills and glyphs: the menu bar's fill, its labels from cell
one and a header's cell extent; the panel's selection highlight, disabled
dim, check prefix and right-edge shortcut, and `step`'s wrap over the enabled
rows; the tab strip painting the active tab paper under a top and right
border with a dirty star, and an empty strip still filling its row with no
tab; the text block wrapping at its columns across a newline; and the status
row truncating a long line to an ellipsis in the last cell; and one
whole-surface pixel oracle rasterizing the status band to confirm its
border and fill land and the rows above stay untouched. The list adds
its own: the selection highlight, disabled dim, star mark and
right-aligned column, the four bezel bands round a chrome face behind
the rows with the selection clipped inside them, a remainder row above
the lower bezel, a selection off the window drawing no highlight,
`reveal`'s least-move window and `reveal_within`'s margin kept on each
side, stopped at the ends and clamped to the whole number below half
the rows, the scrollbar thumb tracking it and a disabled bar's border
thumb, `hit` mapping a point to a row, `new` refusing a rect the
surface or the gutter cannot hold, all at more than one scale, and a
whole-surface pixel oracle for its selection, its scrollbar and the
pixels around it left untouched. The text entry adds its own: the four
bezel bands round the paper face, the one-pixel caret after the text,
the mask glyph shown in place of each character, a selection filled and
its ink flipped focused and unfocused, a dim placeholder only when
empty, a scrolled window from a non-zero first with the caret and
selection shifted, `reveal`'s
least-move window keeping the caret at the inset without a needless
scroll, `hit`'s point-to-column clamped to the shown columns, `new`'s
refusals, a stale first that cannot panic, the geometry, caret and `hit`
at a second scale and an offset, and pixel oracles that show the
selection ground focused and unfocused with the ink over it, a clean
caret column, the mask glyph rasterized to exactly the bullet, and the
pixels around the field left untouched. A field over a list whose items
fill every row is painted at scales 1-4: each closed by its bezel on all
four sides with the face a scaled pixel in, every bezel fill inside its
rectangle and off the face and every other draw inside the face, and a
repaint of a face alone streaming no `BORDER`. td-editor keeps its
scene-level render, ui and menu oracles.

`tests/messages.rs` holds the message list through a clipboard that
records each copy or refuses as told: bodies wrapped at a word, or
within one too long for a row, across a newline, the row heights stacked
at scales one through four; rows tiling the text, a space past the last
column kept unshown with no empty row after it at the text's end or
before a newline, and empty text and a trailing newline each an empty
row; a tab drawn as a space and an escape as the replacement character;
a reply, an excerpt and the first of two sections streamed in
three-scalar pieces laid out, at every step and at both ends of the
view, as the whole text is; a titled section and a message collapsed
and opened by
the title row, the header's mark and `Toggle`, hidden text left out of a
select-all, an untitled section refused; scrolling by row, page, wheel,
`Home` and `End` at every scale, a message arriving while scrolled back
leaving the view and one arriving, or text streaming in, at the end
followed; a resize at another scale and width, a rectangle too small
for any row and back, and a trim of older messages keeping the first
shown row; a view one header tall showing the header and not its gap; a
fold pressed at the end keeping its title where it was; the scrollbar's
thumb at the foot at the end, at the top at the start and the whole
track disabled when everything fits; `NextMessage` and
`PreviousMessage` focusing and revealing a header, and `Toggle` folding
the focused message and revealing its header; a drag past the view
scrolling a row a motion and asking a repaint when only the view moved;
a drag across a header into the next message copying its text with a
blank line between and no chrome, and past the view's foot running to
the end; every press and drag between two places of a section with
empty, leading and trailing lines and a wrapped row copying exactly the
source between them, a select-all the whole text, and across messages a
blank line between and nothing of a section only touched at its end; an
empty section held whole keeping its place, as the whole-message copy
does; Shift moving the head from the kept anchor; a double click
selecting a word, its drag keeping it, the third press a first, a slow
or distant second press a single click, a word wrapped across rows
selected whole, one past a row's text nothing, and one running into an
excerpt's hidden rows ending at its shown text's end; an excerpt cut at
`EXCERPT_ROWS` with its more row, a
select-all copy holding exactly the shown text and none of the labels,
titles, status, verdict, button caption or more row, a press on a
header starting no drag, and a drag over the more row ending at the
excerpt's shown text; the header's button copying a message's sections
whole at the press, its release nothing, `CopyMessage` copying a tool
block's whole source rather than its excerpt, and a collapsed message
copying whole; nothing to copy, a clipboard refusal handed back, a
selection and a message past the clipboard's ceiling refused as too
long, and a repeat never copying; an append keeping the selection and a
replace clearing it, and a trim moving the selection and focus with
their messages or dropping them with theirs; the label, text, section,
source and whole-list bounds and their release on a trim; the row
budget refusing a push and an append with the list left as it was; a
trim never refused, laying out what fits of a list too long for its
width; a rectangle too small laying nothing out until
a resize gives it room and one outside the surface refused; a draw
stream inside the rectangle naming the label, status, verdict mark, body
and button caption with a body glyph at its cell, damage off the list
drawing nothing; and pixel oracles at scales one through four for the
header rule and ground, the focused message's highlight, the button's
bezel, the selection's ground focused and unfocused, everything outside
the list untouched, and a partial repaint equal to the whole inside its
damage and untouched outside it. `tests/confinement.rs` holds the module
among the pure set.

`src/sys.rs` and `src/wayland.rs` carry the kernel tests moved from
td-editor's adapter: close-on-exec duplication of an inherited stream,
owned and closed received rights, the ancillary walk past unknown records
and invalid entries, kernel truncation, byte-only EOF, the eight-right
FIFO budget with disconnect closing every owner, the idle wait on an
inherited nonblocking socket without touching its shared flags, poll
readiness naming each stream with a timeout naming neither and a hangup
counted as readiness, a wake ending a long wait from another thread and
before it with queued wakes drained as one, compositor bytes still read
beside a waker, a waker outliving its connection refused, write
backpressure under the startup deadline, environment precedence, and the
peer reader draining requests with their rights from a pool file that is
private, unlinked and exactly sized.

`tests/client.rs` drives the client with the smallest consumer, a probe that
fills one colour behind a marker pixel: the fixed ids and budgets by value;
the fixed ids in order and a pool crossing the socket private, unlinked and
pixel-exact; the frame callback not releasing a buffer and three busy
buffers bounding a resize storm; a release before done still waiting and a
matching buffer reused; invalid events, a protocol error, ids waiting for
`delete_id` and an oversize surface refused before any request; missing,
low-version, removed and excessive globals named; registry lookups binding
the lowest global and removal reporting whether it was required; a
consumer's objects handed back untouched and retired through their tag;
pings serviced while a frame waits and close stopping presentation; a later
free matching buffer preferred; presentation waiting for configure, XRGB and
the callback; a hidden surface without a callback deadline; the pointer
image's one pool, serial reuse and late arrival on ARGB, with decoded
motion, buttons, axes and frames handed on and another surface refused; the
seat bound from the lowest v5 global capped at v7 after the toplevel, its
devices created in order once and a name over budget refused; a keymap
crossing the socket and presses translating only after focus and the
snapshot, with arming, repeat, the timing retime and leave clearing
everything; a refused map disabling input, an unsupported format dropping
its right unread and every keyboard schema refusal; capability loss
releasing a device, retired devices drained until `delete_id` and a lost
keyboard's in-flight keymap dropped; seat removal releasing both devices and
the seat and forgetting the global; the data device bound after the seat at
v3 or not at all; offers budgeted, selected and retired behind one barrier
by generation; focus loss, drags and a retired device retiring offers and
the selection; a source offering both text MIMEs, its sends handed on or
dropped with their rights, a malformed send leaving the FIFO alone, and its
retirement on cancel, replacement or withdrawal, the last once and without a
serial; manager and seat removal releasing the source and device in order;
the primary selection bound only when asked for, after the clipboard, and
its offers, selection, receive, source, send, cancel and release on its own
opcodes, focus loss retiring it and its removal leaving the clipboard alone,
an offer id reused across the boards owned by the newer, and seat removal
releasing it after the clipboard; the complete loop on a thread accepting
split events and closing cleanly; and the loop parking an event until
its right arrives, in wire order, with the idle wait restored. Sixteen
of these moved from td-editor's window tests (the presentation nine,
the pointer image, the seat binding, the keymap reader's shared-offset
and bad-source oracle, the last a unit test beside `read_keymap`, and the
clipboard lifecycle four: offer retirement and reuse, coalesced barriers,
drag offers with the malformed-send FIFO oracle, and offer budgets with a
retired device); the editor keeps its transfer, request and control coverage
and its reactions to the client's device and clipboard outcomes.

The terminal's suites are its specification files and in-file tests,
listed by obligation in `td-term/DESIGN.md` §6. `vt_spec.rs`, compiled
only as `vt.rs`'s test module, runs the native corpus under `spec/vt`
against its generated `expectations.txt` overlay, checks the libvterm
migration against its report and source manifest, and holds the model to
chunking invariance, malformed-stream recovery, bounded replies and
history, and the terminfo entry's attribution; its `key` cases run through
td's own keymap, compiled from the compositor's source, and `vt_keys`.
`vt_render_spec.rs`, compiled only as `vt_render.rs`'s test module, holds
the renderer to the exact PPM goldens under `spec/vt_render` and the
structural rendition, palette, cursor, selection, bell and viewport
oracles, writing an actual frame and a diff beneath the build's temporary
output on a mismatch, and holds `render_with` to its outline oracles over
fonts the tests encode (`tests/fonts`, mounted by path). `vt_keys.rs` pins
the chord table, the viewport and the input queue; `vt_terminfo.rs` its
capability order, binary layout, decoder refusals, key bytes and
per-capability effects; and `pty.rs` carries kernel tests over a real PTY:
unlock, peer, published and read-back sizes, a child's view of the slave
and grid, and the reader, writer and waiter lifecycles.
`td-ui-import-libvterm` is a developer tool, run only by hand against a
supplied archive tree.

`tests/confinement.rs` pins the source inventory, the exact six shared
source mounts, the absence of ambient I/O in pure modules, that `notices` is
three embedded texts and the outline face's literal and nothing else, the
absence of `include!`, `cfg_attr` and any dependency declaration, that the
shared sources bind no input interface, that `face_file` opens one file
after checking it and bounding the read, lists directories in one place,
builds its paths from its own names alone, reads exactly its three
directory values from the environment and writes nothing, and that
`pinned_face` reads only through it; that `theme` is pure and
`keys` is pure and `theme_file` reads exactly its two configuration
values, opens its file
nonblocking under its bound, makes and locks its directory and writes
only a private `create_new` sibling it renames over the file; and the
raw layer: the complete
fingerprint of `sys.rs`, its syscall and flag values, its three
function-only allowances, the single instruction and adoption sites, that
the crate root denies `unsafe` and declares the module private, that
`wayland`, `clipboard` and `pty` are its only callers, the transport through
exactly five wrapper calls (the poll over two readable-only entries among
them), the clipboard's destination owner through exactly five status calls,
and the PTY through exactly its four ioctl wrappers and its session hook,
each at one site, with `SYS_IOCTL` named only at its definition and its five
wrappers, `SYS_SETSID` and the pre-exec hook each at one site, and each
request constant and the peer flags only at their definition and their one
use, the master opened with `O_NOCTTY`, every published size read back, and
the child's environment cleared, that no production module calls the test
support (the client's `unconfigure` and `input_mut`,
`pty::Input::take_for_test`), and that the client is the toolkit's one
consumer of a received right, through the pinned keymap reader with its
format, size and regular-file checks and the send that hands its right on.
For the terminal it pins `pty`, `vt`, `vt_keys`, `vt_render` and
`vt_terminfo` as public modules, `vt_keys` among the pure set, and `vt`,
`vt_render` and `vt_terminfo` as pure production text -- no environment,
filesystem, network, process, time, I/O, embedded file, path mount or nested
module before the test tail -- whose tails are exactly their specification
mounts (`vt_terminfo`'s two test modules), with the specifications test code
throughout and `vt_spec.rs` mounting only the engine's SHA-256.

`control`'s in-file tests are td-editor's framing tests moved: every frame
split and single-byte delivery, truncation, zero/oversized/trailing frames,
the poisoned decoder, the envelope grammar with ID recovery and the lift
into a consumer's error type, the scalar and hex codecs, both response
lines, and arbitrary bytes as headers and as framed payloads.
`control_socket`'s kernel tests moved intact: they connect through the
requested pathname, exchange bounded bytes, check nonblocking and
close-on-exec flags and mode/owner, preserve every existing endpoint class,
reject symlinked/unowned/nonprivate/untrusted ancestors, exercise
replacement-name and renamed-parent cleanup, and prove through a
deterministic publication hook that a replaced visible parent refuses and
only the pinned candidate is cleaned. They create 0700 fixtures under
`/tmp`; the crate's `trusted-test-root` declaration gives them a private
caller-owned root inside the gate's namespace, where the unmapped host root
would otherwise refuse publication. These are transport-ownership tests, not
a consumer's endpoint or an adversarial same-UID race proof.
`control_worker`'s tests moved with a toy `Parse` type in place of the
editor's requests: deterministic connection tests inject time and cover
bytewise input, complete and truncated frames, refusal IDs, late extra
requests, full queues, abandoned jobs, invalid response sizes, and expiry
while reading, awaiting the consumer or blocked on output, with a separate
pre-output expiry test independent of socket-buffer tuning, plus large
draining replies, retry classification, idle pacing, liveness before
dispatch and explicit thread-error propagation; real Unix-socket tests cover
a framed round trip, admission backpressure with continued client progress,
shutdown cancellation and owned endpoint removal. `replay`'s tests drive the
runner bytewise through consecutive frames, EOF between frames, partial
headers, bad lengths, short bodies and a refused reply.

`tests/driven.rs` drives the seam end to end through a toy consumer, a counter
with a four-row action table, a two-line composition and one verb of its own:
the table's grammar (each refusal of `check` by message, `bound`, the aligned
`help`), every generic verb and its refusals in the envelope (the consumer's own
codes among them, and a code of its own marking whatever reached it, so the
router's refusals are told from the consumer's), the text read back with an
overpainted cell, a wholly clipped glyph, a fill over a row and over one cell, a
surface smaller than the text, one narrower than a cell and one taller than the
text, the frame digest stable across identical states and moved by a change, its
pages reassembling the RGB rows exactly and the largest page framing under the
ceiling, the whole seam behind the replay runner, and `Payload` behind a live
worker, a malformed frame refused on the worker's thread and the rest answered
on the turn.

`tests/window.rs` drives the widget window against a scripted peer: the
binding delivering the default surface, configure laying the surface
out, keeping it through a zero axis, a refused extent and a repeated
extent, and the close request delivered again while the handler
continues and closing the window when it quits; the frame presented from
the handler's paint into a raster over the surface, clean until a
redraw, the buffer reused once released, every buffer busy asking for no
paint and leaving it dirty, and the title sent when it changed, ahead of
the frame when one follows and alone when none can; presses arriving as
the keymap's chords, plain and with modifiers, marked when the repeat
clock made them and not after the window closed, a bare modifier
nothing, focus following the keyboard and a handler quitting on a chord;
the left button's press at the pointer in surface pixels rounded down,
motion while held a drag signed past the edge, a second press while held
nothing, the release where the pointer is, motion without the button, a
stray release and the right button nothing, leaving or losing the
pointer while held a cancel with no release after it, a handler quitting
on that cancel hearing nothing of the resize that made it nor of
anything after, and Shift under a focused synchronized keyboard
extending the press; wheel frames in cells; the poll and wait each turn,
the keyboard capability's loss as focus loss once; and the whole loop
over a socket.

`tests/window.rs` also presents the corner composition, whose glyph
the window executes through a typeface's fitted face when it is given
one, exactly as a raster with that face paints it.

`tests/confinement.rs` holds `window.rs` among the adapters, adds
`control.rs` and `driven.rs` to the pure set and the three adapters to
the inventory, and carries td-editor's pins over the moved modules: the
socket's open flags, path and identity constants, procfs reads, mode and
identity checks, and the absence of `connect`, environment reads,
canonicalization, a fixed overflow number and the trusted-root variable;
the worker's thread name, its slot, buffer and deadline constants, its
bounded channels and nonblocking operations, `R::parse` as its sole
parse site, and the absence of blocking reads, writes and receives; and
the runner's ceiling check, its two exact reads and one framed reply.
The socket's `/proc/sys/kernel/overflowuid` literal is excluded from the
raw-module identifier count by name, as td-editor excluded it. The
consumer-level conformance, td-editor's request grammar, refusal parity
between socket and replay, and its window's two-jobs-per-turn polling,
stays in td-editor's suites and confinement tests.

The builder discovers the crate by existing. Its gate runs `cargo test` and
all-target Clippy; a change under `td-ui/` selects each consumer's tests,
td-editor's and td-term's among them, through the reader graph, because
each consumer's manifest names the crate.

## Widget window

A program that lays the toolkit's widgets out over its surface, td-news
and td-mail with their lists and the editor core's document pane, has no grid
to draw in: it paints compositions into a raster over the surface and
reads its input in surface pixels. `window` is the window over the
client for such a program. Its `Handler` names the toplevel and its
title, reads each `Input`, is polled every turn with the loop's clock,
says how long the loop may wait, says when the surface must be painted
again, and paints it whole through `paint`, which receives a `Raster`
over the frame's buffer and the `Surface` it is laid out for; a paint is
not a frame and must be repeatable. The window owns the client, the
pinned face, the pointer's position and whether the left button is held;
the handler owns everything else, and stays the caller's whichever way
the loop ends.

The vocabulary is `Input`: `Key` with the chord as the keymap spells it
(`a`, `C-x`, `S-Right`), so a handler translates it itself or hands it
to td-editor's controller unchanged, and `repeat` for a delivery the
repeat clock made; `Pointer` with the driven seam's `PointerPhase` (`Press`,
`Move`, `Release`), the position in the surface's physical pixels, signed so
travel past an edge is representable, `extend`, Shift held at the
press, and `follow`, Control held at the press, as the keyboard's
synchronized modifier state reports them while the window has focus;
`CancelPointer` when the pointer leaves or the
device goes while the button is held, so the handler ends a drag
without a release; `Hover` with the pointer's position while Control is
held, by that same state, and the pointer has entered the surface (a
held button's grab keeps it entered through travel past an edge), and
`None` once either ends, delivered on a change only, button held or
not, so a handler marks the link a Control-press there would follow;
`Wheel` in cell rows and columns as `pointer::Wheel` accumulates a
frame; `Resize` with the `Surface` the handler lays out for; `Focus`
and `Close`. Motion is delivered as `Pointer` only while the button is
held and as `Hover` only while Control is, so a handler without drags
sees no motion stream until Control is held; a second press while held,
a release without a press and every other button are nothing.

The loop, turn by turn: on `Bound` the window sets the title and app id,
commits, and hands the handler the default `Resize`; on `Configure` it
lays the surface out for the extent (a zero axis keeps the current one;
an extent the raster refuses keeps the last surface, is reported through
`notice` and delivers nothing; the extent the surface has delivers
nothing), cancels a held button, hands the handler the new `Resize` and
acknowledges. The compositor's close request is `Close` to the handler,
which closes the window by answering `Flow::Quit` or keeps it by
answering `Flow::Continue`, to ask about what a close would lose, and
hears the request again when it is repeated. Focus is delivered as a
change only, so a keyboard that was never there is not a loss; the
seat's removal and the keyboard capability's loss are one focus loss,
and the seat's removal, or the pointer capability's loss, ends a held
button without a release and drops the wheel's accumulation. A press
whose key repeats arms the client's repeat clock, and a turn that
drained the queue with no event parked delivers the repeat that is due
as a `Key` marked `repeat`, at most one a turn. Every turn's end first
polls the handler at the loop's clock, milliseconds since the loop
began, and asks for a withdrawal; then it steps the clipboard transfers,
which may deliver a paste, and delivers a due repeat. A paste or repeat
delivered there reaches the handler's next poll a turn later, up to its
`wait_ms` later. The connection then waits for the lesser of the
client's wait (a hundred milliseconds, or the armed repeat's remainder)
and the handler's `wait_ms`, floored at one millisecond; a closed window
neither repeats nor polls. A handler that answers `Flow::Quit` hears
nothing more: the window is closed, and the inputs that would have
followed, a resize after the cancel it quit on or the focus loss after
a seat's removal, are not delivered. The surface is presented when the
handler says it needs a redraw or the window's own state changed, once a
frame can be presented, into a raster over the pool file at the surface's
own stride; the handler paints only into a buffer, so a frame refused by
the client (every buffer busy) asks for no paint and leaves the window
dirty until one is back. A paint that fails ends the loop with its
error, as a program that cannot paint its surface has nothing to show;
the refusals the loop absorbs are the extent's and the frame's. The title
is read before every attempted present, which is when the window is dirty
and a frame can be presented, and sent then when it changed, ahead of the
frame's commit when a frame follows and alone when none can; a retitle
made while painting rides the next attempt.

The clipboard is the window's, reached by the handler through the
`Clipboard` handed it with every input. `copy` offers a text as the
seat's selection at the serial of the key or button press being
delivered, the window focused, the data device live, no earlier copy
still being sent and the text within `clipboard::MAX_BYTES`; it refuses
otherwise with a `Refusal` that says which, and a copy asked outside a
press (from a poll, a repeat, a paste or the button's release) has no
serial and is refused, as td-editor refuses it. The text is kept behind
the live source: each `wl_data_source.send` the client hands on begins
an `Outgoing` over its right, a send arriving while one is pending
drops exactly its right, the source's cancellation drops the text (a
send already begun keeps its own), and the device's release with its
seat or its manager cancels the send and drops the text. A handler takes
its copy back through `Handler::take_withdrawal`, an edge asked after
every input and after every poll. Each turn polls the handler, and asks,
before it steps a transfer, so a lock decided by a timer withdraws a
copy before that turn writes any of it; a quit in the same poll
withdraws before the window closes, and a closed window is not asked.
On a request the window cancels a send in flight, drops the text and
destroys the live source, which clears the selection while it still
names that source. Destroying needs no serial, so a poll can withdraw.
Bytes a send already wrote cannot be recalled; a paste in flight is
untouched, and the compositor's later sends to the retired source drop
their rights. `paste` asks the selection for its text over a fresh
private endpoint when the device is live, the window focused, the
selection offers text and no paste is pending; the text arrives whole as
`Input::Paste`, delivered from the end of an idle turn, one whose queue
was drained, so a focus loss, a selection change or a close request
still queued behind it is seen first. A paste in flight is dropped, with
a notice, when the selection changes, when focus is lost, when the device
goes, when a copy replaces the selection, or when the compositor asks
the window to close (a handler that keeps the window asks again). Both
transfers are stepped at the end of every idle turn under their own
bounds, and at any turn once expired, the loop's wait capped at ten
milliseconds while one is pending; a transfer that fails or expires is
a notice, never a partial text. A request the connection refuses (the
source's requests, the receive) is refused to the handler as no device
and ends the loop with the connection's error once the input is handled,
as the editor's window ends on it; a paste's private endpoint the process
cannot make is a refusal with a notice, and the window goes on, as the
editor's does. The tests pin the copy's requests and its text crossing
the send's right, the paste's request and its text arriving as an input,
each refusal but the endpoint's (which needs the process's descriptors
exhausted), the idle-turn admission, the cancellations, the seat's
removal, and the refused request ending the loop. Withdrawal tests cancel
a send short of its text from a poll and destroy the source, leaving
the window no copy of the text; destroy it at once from the input that
copied; write none of a short secret whose send is pending when an idle
turn's poll locks; withdraw before closing when that poll also quits;
send nothing with nothing offered; and do not ask a closed window.

A handler clears the frames the window keeps through
`Handler::take_scrub`, an edge asked with `take_withdrawal` after every
input and every poll, as a lock does. The client zeroes the raster's
pixels with their spare capacity and every buffer the compositor has
released at once, and a buffer still attached when the compositor
releases it; nothing is presented for it, and the next frame is painted
as usual over a zeroed buffer. The request does not repaint: an
attached frame is released, and so zeroed, once the handler's next
frame replaces it, so a handler clearing what it shows also asks a
redraw. A buffer still awaiting its release when the window closes is
zeroed then, as best it can be, since a closing window reports no
error. A refused withdrawal ends the loop only after the scrub. The
backing files are the consumer's
directory's: a consumer showing secrets gives a memory-backed one. What
the compositor copied of a frame, a buffer retired by a resize before
the request, and the pixel vector's earlier allocations, freed when a
larger frame grew it, are beyond the window. The scrub tests check a
released buffer zeroed at the request, an attached one only at its
release, the pixels zeroed, nothing done unasked, a request made by an
input as well as by a poll, a pending buffer zeroed when the window
closes, and a closed window not asked.

The window paints in a theme (see "Themes" below). `with_theme_file`
reads the theme a file names and keeps the window's choice there; `run`
gives it the program's file under the configuration home
(`theme_file::host_path` for the handler's `app_id`), and a window made
with `new` alone paints in `SAND` and keeps nothing. A file that cannot
be read, or names no theme, is `SAND` with a notice. `theme::CHORD`,
`F12` with no modifier, is the window's own: the press moves it to the
next theme, marks it dirty so the next frame is painted in that theme,
and writes the file, a failed write being a notice; the handler hears
neither the press nor a repeat of it, as no repeat is armed, and `S-F12`
or any other modified press is the handler's. No consumer binds the
chord and no editor key profile does; td-agent's `C-S-t`, the key first
proposed, is why it is a function key. The tests pin the chord kept from
the handler, the frame repainted in the next theme's paper with the
handler's own colour passed through, the file written and read back by a
later window, a held key not repeating, `S-F12` delivered, and a file
naming no theme starting the default with a notice.

`keys::CHORD`, `F1` with no modifier, is the window's too: it opens the
key list over the frame (see "Key list" below), asking the handler's
`keys` for its sections then and ending them with `keys::window`. While
the list is open the window keeps every press but `theme::CHORD` from
the handler, the list taking its reading and closing keys and a held key
repeating there while it moves the list; a drag under way when it opens
is cancelled, so its motion and release are the list's and reach nobody;
a left-button press closes the list and reaches nobody else, and with
nothing held the release after it reaches nobody either, and a key held
through it stops repeating; the wheel scrolls it. A handler's own help
key, or a press on its `keys::BUTTON`, opens the same list through
`take_show_keys`, an edge asked after every input and every poll as
`take_withdrawal` is; a closed window opens nothing. A handler sets that
edge from the live pointer or the physical keyboard only, never from its
control socket or a replay. The lines are laid out at the panel's width
when the list opens and again when the surface changes, and the first
shown line is held to the last page before each paint. The list is
painted after the handler's paint, into the same raster, so it is in the
window's theme; a surface with no room for it shows the handler's frame
alone while the list keeps the keys until it is closed, and closing it
drops its lines and paints the frame again. The test pins `F1` kept and
the list open, its title bar painted over the handler's frame and the
frame in the margin, a reading key and its repeat scrolling it, a
handler key reaching nobody, the wheel scrolling it, `F12` still moving
the theme, a left press closing it with neither it nor its release
reaching the handler, the frame painted again and a reading key held
through it repeating nowhere after, `Escape` closing it, the handler's
key opening it from its top through `take_show_keys`, `F1` closing it
unheard and the frame painted again, and a drag cancelled when the list
opens with its motion and release reaching nobody.

## Key list

`keys` is what a program's keys look like to the person reading them:
sections of rows, each row the keys spelled as td-ui's keymap spells its
chords (`j/Down`, `C-x C-s`, `1..9`) and what they do, a row with no
keys a sentence of prose, or with no description either a spacer. A
program owns its rows: one whose bindings live in a table derives its
rows from that table, and one whose bindings are code lists them beside
it. Nothing checks a row against the program's bindings; `check` holds
its spelling and style.

The spelling is the keymap's, so a key reads as the chord a program
matches:

- a chord is the prefixes `C-`, `M-` and `S-`, in that order, then a
  key: a name the keymap emits (`Return`, `Escape`, `Tab`,
  `Backspace`, `Delete`, `Insert`, `Home`, `End`, `PageUp`,
  `PageDown`, `Up`, `Down`, `Left`, `Right`, `F1` to `F12`, read from
  the keymap's own table, `xkb_symbols::COMMANDS`), `SPACE` (`Space`,
  which the keymap spells `" "` unmodified and `Space` under `C-` or
  `M-`), or one printable ASCII character as itself (`j`, `?`, `G`). As
  the keymap spells them, `S-` with a character needs `C-` or `M-`
  beside it, since Shift alone types the shifted character, and a
  letter under `C-` or `M-` is lower-case;
- alternatives join with an unspaced `/`, each written out whole
  (`Tab/S-Tab`, `C-c/C-x/C-v`); the `/` key unmodified stands alone as
  a whole cell, so no `/` is read two ways;
- a sequence joins chords with one space (`C-x C-s`, `s y`), and a
  range is `A..B` of two chords (`1..9`, `C-1..C-9`);
- what is not one key is one of `WORDS`, as a whole alternative:
  `a character`, `characters`, `any other key`, `click`, `C-click`,
  `S-click`, `double-click`, `drag`, `wheel`, `arrows`, `S-arrows`,
  `C-arrows`. The list grows only for a row that needs a word.

A description is written as its source keeps it, for its other readers
(CLI help, a control socket's `actions`, an export); `lines` shows it as
a `sentence`, its first character upper-cased when it is a lower-case
ASCII letter and a `.` appended unless it ends in `.`, `?`, `!` or `:`.
A description must not start with a lower-case key name or variable
(`p and P push`, `n of its m`), since the list capitalises it; a first
word of one letter other than `a` is refused. A section's title starts
with an upper-case letter.

`check` returns one problem per bad cell, naming its section and row: a
keys cell `spelled` refuses, a row with keys and a blank description, a
description whose first word is one letter other than `a`, or a title
not starting with an upper-case letter; a row with empty keys is prose,
or with an empty description a spacer, and is held only to its first
word. `check_style` is the same without the spelling, alternatives still
joined by an unspaced `/`, for a program whose rows show its own menus'
spelling: td-editor's key profiles, which keep `Ctrl+N` or `C-x C-s` as
their menus show them. Each program's tests are to run one of them over
every list it shows, as their landings follow increment 37.

`lines` makes the text: each section's title, its rows indented with the
keys padded to the widest of all the rows, at most `MAX_KEYS_COLUMN`
cells, so one long key runs on rather than pushing every description
right, and a blank line between sections. A description wider than the
columns left beside its keys wraps at its spaces onto lines under it,
aligned with it, a word wider than that at its width; at least
`MIN_WRAP` columns are left, below which the list clips it. Every list
ends with `window`, the window's own `F1` and `F12`.

`Help` is the list's state, whether it is open and its first shown line.
`Escape`, `q`, `?` (the help key of the programs that have one) and
`CHORD` close it; `j`, `Down`, `k`, `Up`, `PageDown`, space (the
keymap's `" "`), `PageUp`, `Home`, `g`, `End` and `G` scroll it, held so
the last line stays on the last row, as `clamp` holds it when the page
or the lines change; any other key is kept from the program while the
list is open, so a key read while looking up keys does nothing behind
it. `Panel` places it `MARGIN` cells in from the surface's edges and at
most `MAX_COLUMNS` cells wide, centred, with a one-pixel border at the
scale around a `ROW`-tall title bar in the selection's colours, `TITLE`
at its left and `TITLE_HINT` at its right where it fits beside the
title, and chrome's `List` under it: no row is selected, and a section
title's row is left blank there and its title painted in `ACCENT` where
the list puts a label. `columns` is the cells a line has there. `None`
when the surface cannot hold the title bar and one row.

`Overlay` is the list as a window keeps it: `open` with the program's
sections, which it ends with `window`'s, laid out for the surface;
`lay_out` again when the surface changes, which holds the first line
to the new page; `key`, `press` (a left-button press, which closes it
as `Escape` does) and `wheel` while it is open, which keep nothing and
move nothing while it is closed; `close`, for a window that ends it
itself; and `emit` last over the frame, which holds the first line to
the page first and paints nothing when the surface has no room for a
panel. The window decides when the list opens and what it keeps from
the program, and every window routes alike, as the widget window does
(see "Widget window"):

- the physical keyboard's unmodified `CHORD` opens it, and the
  keyboard's chords go to `key` while it is open, `theme::CHORD`
  excepted; nothing from a control socket or a replay reaches it;
- the program hears no keyboard chord while it is open; focus,
  resizes, the close request and, in the widget window, Control's hover
  still reach it, as does anything its own control socket drives;
- a held key repeats only while `key` answers `Moved`, and a repeat
  that does not move the list is cancelled; a reading key at an end
  moves nothing and answers `Kept`, so a key held there stops; a key
  that opened it arms no repeat;
- opening it ends a drag under way; a left-button press while it is
  open goes to `press`, which closes it, and reaches nobody else, and
  the release after it reaches nobody either; a click that closes it
  cancels a held key's repeat;
- a program may open it from the pointer too: a bar's or strip's
  button labelled `BUTTON` (`Help`), an item `ITEM` (`Keys`) in a
  menu named `BUTTON`, whose shortcut shows `CHORD`, or a press on a
  status or footer row while it shows a hint naming `CHORD` whole; the
  list's sections gain no row for that entry, `window` already listing
  `CHORD`. Only the live pointer or a physical-keyboard menu choice
  opens it so; a control socket, a replay, a preview or a render check
  never does, and in the widget window that is the rule for
  `take_show_keys`;
- wheel rows scroll it while it is open, columns are dropped;
- a resize calls `lay_out`;
- every step but `Kept`, and every wheel scroll, is a frame to paint,
  and `emit` is the last thing painted, in the window's theme; a
  program's own previews and render checks never show it.

The tests pin the lines (titles, padding, the ceiling, the window's
section last, wrapping and its floor, descriptions as sentences,
trimmed, with their sources kept), `check` passing each form of the
spelling, the words and td-ui's own rows (`window`,
`confirmations::KEYS`) and naming each bad cell of each kind, a
one-letter first word named in keyed and prose rows by both checks,
`check_style` skipping the spelling, every chord the supplied td and US
keymaps emit passing `spelled` with every key name reached and names
beyond the keymap's refused, every reading and closing key with the
clamp at both ends, `clamp`, the wheel, a kept key, the hint naming
`CHORD`, the overlay's opening, laying out again, scrolling, clamping as
it paints, closing on `press` and emptying on close, the labels, the
panel's place, title bar, hint left out on a slim panel and accented
titles, the lines fitting its columns, nothing painted outside its
frame, and no panel on a surface too small.

## Themes

A theme gives each role the shared palette names its own colour: paper,
ink, chrome, border, the selection and the unfocused one, line numbers,
the misspelling ink, the panel's selected row and disabled ink, and the
three status inks. `KEYS` lists the palette's colour for each role and
`Theme::colors` the theme's, in that order. A raster given a theme maps
every draw before executing it: a colour equal to a key, its top byte
ignored, becomes the theme's for that role with its top byte kept, and
any other colour passes through, so an article's or a chart's colours,
td-dua's treemap and a handler's own fills are left as given. The map is
by value: a handler's own colour that happened to equal a key would be
recoloured, so one that must stay as given avoids the thirteen keys;
none in the tree does today. `SAND`'s colours are the keys, and a theme
whose colours are passes every draw through unmapped. A glyph's
ink and background are mapped before the raster derives the medium
fringe or blends an outline's coverage between them, so the fringe is
the theme's. No widget derives a colour of its own from the palette;
one that did would escape the map, and the map's tests would not see
it.

The themes are `SAND`, the palette as it is and the default; the light
`HARBOR` (blue-grey), `MOSS` (green) and `ROSE` (blush); and the dark
`DUSK` (blue-slate) and `EMBER` (warm brown). A dark theme's selection
and status inks are light, since selected text and a banded status row
are drawn in paper over them. Each theme is held, by test, to WCAG
contrast floors at or under the default's own for the pairs the widgets
draw: ink on paper (7:1), on chrome (6:1), on the panel's selected row
and on an unfocused selection (5:1); paper on the selection and each
status ink on paper (4.5:1); line numbers on paper (3:1) and on chrome,
and disabled ink on chrome (2.8:1); borders on paper (1.6:1) and chrome
(1.4:1); disabled ink on paper (3.4:1), the panel's disabled button,
and on the selected row (2.3:1); and the message list's tone inks, the
selection and the misspelling ink on chrome (4:1, 3.9:1) and on the
selected row (3.1:1, 3:1), and line numbers on the selected row
(2.3:1).

A program's theme is kept in `FILE`, `theme`, in its own directory under
the configuration home: `$XDG_CONFIG_HOME/<app_id>/theme`, else
`$HOME/.config/<app_id>/theme`, a relative or empty value ignored
(`xdg::dir`), which for td-news and td-mail is beside their
`config.toml`, found by the same rule. Under the application jail that
home is the application's own private configuration directory
(APPLICATIONS.md §B.4), so a jailed program's choice stays in its
state. An `app_id` that
is not one plain name of ASCII letters, digits, `-`, `_` and `.` up to
`MAX_APP_ID`, not beginning with `.`, has no file. The file holds the
theme's name and a newline; a reader takes at most `MAX_FILE_BYTES` of a
regular file, checked before the nonblocking open and again once open,
and ignores surrounding ASCII whitespace, so a hand-edited name is read.
A write makes the directory (0700, as the base directory specification
asks of a directory it makes) when missing, locks it, and renames a
flushed private sibling (0600, the file's name and `.new`, a crash's
stale one removed first) over the file, so two instances of one program
never share the sibling and either choice is whole; the lock is tried,
not waited for, so a write while another holds it is a notice and the
next press tries again. A link at the file's path is replaced, not
followed. The write is synchronous on the window's thread, one small
file per press. A user may write the file by hand; there is no other
configuration.

A program with a window of its own keeps its theme in a `theme_file::Kept`
as the widget window does: it loads its file with `Kept::host` for its app
id when its live window opens, paints that window's rasters `with_theme`,
and takes `theme::CHORD` from the keyboard's own key event, before its
dispatch and never from its control socket or replay, moving the theme
with `Kept::advance`, reporting a failed write as it reports any notice,
and drawing the whole frame again. td-editor, td-setup, td-photo and the
task manager do; the live installer wizard runs without a home, so its
`Kept::host` finds no file and F12 keeps none. Their previews, render
checks and in-process tests paint in `SAND`. Two programs do not. td-term draws its own fixed terminal
palette, not the shared one, and the programs it runs own F1 and F12, so
it has no theme. The portal's file chooser runs as the portal's service
account, whose home is the portal's runtime directory, and reads raw key
codes so no keymap can remap its keys; it keeps `SAND`, which the image's
chooser proof pins.

## Shared action button

`chrome::Button` paints a bordered paper action with selected and disabled
styling, using the shared text and palette: a `BORDER` bezel one scaled
pixel wide around a `PAPER` face, `SELECTED` with `PAPER` ink when
selected, `DISABLED` ink when disabled, the text a cell in and centred in
the height (four pixels down in a `ROW`-tall button). It accepts a
nonempty rectangle fully inside the surface; its hit test uses that exact
rectangle. Rendering clips both text and borders to damage and allocates
nothing. The consumer owns focus, enabled-state hit policy and matching
press/release activation, as with the other chrome geometry primitives.
Scale 1-4 pixel tests cover bounds, focus/disabled colors, the centring
and partial repaint equivalence.

`emit_hinted` is `emit` with a hint under the caption when one is given
(a chord, while a consumer shows its shortcuts; an empty one is none):
the caption lifted four pixels, a band of the face's colour five pixels
tall from seven above the foot (over the second row of the caption's
descenders, which a strip's button, `ROW` less its margins, has no room
for beside a hint), and the hint on it in the hint face (`hint`),
`LINE_NUMBER` ink on an enabled unselected button and the caption's own
ink otherwise, centred under the caption, from a pixel in from the bezel
when the hint is the wider, a mark the bezel would cut left off. On a
strip's button the caption's capitals then end a pixel above the hint,
which ends a pixel above the bezel; a `ROW`-tall button has three rows
between. With no hint it is `emit` exactly. Its test pins the lift, the
band, the marks' places and inks, the wider hint's marks left off, the
pixels and the plain and empty equivalence at scales 1-4.

`chrome::Buttons` is a strip of them on a band at a `y` the consumer
chooses, across the surface (`new`) or on a given left and width
(`in_band`): the buttons from cell one, each its label's cells and a
cell each side (so the first label starts at cell two), one cell between,
inset `BUTTON_MARGIN` (2) pixels above and below each `ROW` so two strips
stacked keep their bezels apart; the band fills `CHROME` behind them. A
button its row cannot hold whole starts the next row, so a band too
narrow for its buttons on one row wraps rather than cuts and is as many
`ROW`s tall as that takes (`rows`, one at least; `rect` is the band);
`end` is the x after the last button on its row and that row's top, for
what a consumer lays after them. A button wider than a whole row is
neither painted nor a target and takes no room. The rows are the labels'
on the band's width whether or not the surface holds them: a button on a
row past the surface's foot, or whose geometry leaves the integer range,
has its place (and `end`) but is neither painted nor a target, so a
consumer laying past the last button checks it was held; the layout is
one pass over the labels (`buttons`). `emit` takes each
button's `(selected, enabled)` in label order, a state it runs out of
painting an enabled unselected button; `emit_hinted` takes the hints
too, in label order, `None` or one it runs out of being no hint
(`Button::emit_hinted`); `hit` answers the button whose own
pixels hold the point, the gap and the margin none. A consumer that wants
one selected at a time (a mode or a filter strip) selects one; the strip
itself imposes nothing. Its test pins the geometry, the hit rule, the
draw stream and the pixels at scales 1-4, a descender's lowest row inside
the bezel, partial repaint equivalence, damage off the band painting
nothing, a last button wrapped to a second row and hit there, a band on
its own left and width wrapping to three rows, a button too wide for a
row left out, a band at the surface's foot and past the integer range,
and an empty strip.

## Shared slider

`chrome::Slider` is a horizontal slider in a rectangle the consumer
chooses: a `BORDER` track one scaled pixel thick through the middle and
a knob `KNOB_WIDTH` (12) font pixels wide, the rectangle's height less
`BUTTON_MARGIN` above and below, so one on a `ROW`-tall band lines up
with a strip's buttons. The knob is a bezelled block in a button's
shape: a `BORDER` bezel a scaled pixel wide round a `PAPER` face, or
round the band's own `CHROME` when disabled, so a slider nothing moves
reads flat (a disabled `Button` keeps its paper; the knob is not one).
The rectangle fills `CHROME` behind both. `new` refuses a rectangle
outside the surface, narrower than two knobs or shorter than its
margins and a knob of a bezel, a scaled pixel of face and a bezel, so
the bezel always shows.

The consumer owns the value and its meaning; the widget knows `steps`,
the count of intervals, and places the knob's left edge at one of
`steps + 1` positions spaced evenly along the travel from the
rectangle's left edge to a knob short of its right, each at its nearest
pixel, `value` clamped to `steps` and zero steps one position.
`value_at(x, steps)` is the inverse a press or drag uses: the knob's
centre put under the pointer, clamped to the travel, rounded to the
nearest position, so a press past either end is that end and no pointer
coordinate can overflow it. Both directions rounding, a knob's centre
maps back to its own value whenever the travel (`travel`, the pixels the
knob's left edge can take) has at least `steps` of them; a consumer
wanting every step reachable sizes its steps or its rectangle by that.
`hit` is the whole rectangle, band included, so a press on the track
beside the knob jumps there; a drag is the consumer forwarding motion
through `value_at` and committing the value when it likes (td-photo
commits an exposure on release, painting the knob at the pointer's value
meanwhile). Draw order is chrome, the track across the travel (its ends
under the first and last knob), bezel, face, so the knob covers the
track, and every fill clips to damage. Its test pins the geometry and the
pointer mapping at scales 1-4, the draw stream, the disabled face, partial
repaint clipping, damage off the slider painting nothing, the pixels at
scale two and the refusals.

## Shared menu controller

`menus::Model` is an immutable, caller-revisioned tree of `Node` values.
Each node carries a bounded `chrome::Row` and either a typed action ID
or a submenu. Parent indices precede children; only submenus have
children, branches cannot be empty, and bar roots are labelled submenus.
Context roots are the first panel's rows. `Model::storage_bytes` exposes
owned capacities for consumer budgets; borrowed labels remain their owner's
responsibility. Construction refuses invalid
trees, more than 256 entries, more than eight open panels,
empty/control-bearing labels, labels over 256 bytes or shortcuts over 64
bytes. Allocation is fallible; event handling and painting allocate no
collections.

`menus::Controller` owns the open path, enabled selection, scrolling,
placement, keyboard and pointer navigation, and dismissal. Consumers
feed explicit events and the current model revision and receive typed
outcomes; the toolkit never executes an action. Right/Activate opens a
submenu, Left/Escape closes one level, Up/Down wraps among enabled rows,
and Dismiss closes the path. Root Left/Right switches enabled bar
headers; a lone enabled header keeps its selection. Disabled header
presses and zero wheel deltas do nothing. Pointer movement into a child
retains its ancestors. An action closes the menu before emitting its ID;
release and key repeat cannot activate it again. Outside presses dismiss
and are consumed. Focus loss or resize closes all panels. A mismatched
or absent revision invalidates the menu without an action; replacing
data requires a new controller. Opening a closed menu is an explicit
consumer action, except for a press on an enabled bar header.

An open or switched panel that cannot fit returns `NoRoom`; the consumer
can show an enlargement notice. `Other` consumes unhandled input without
an action, subject to the same revision check.

`Fit::Adaptive` places children rightward, then leftward, within the
surface without overlapping any visible ancestor. When neither side
fits, it replaces the parent with a child and an actionable Back row.
Panels show at most 13 rows, scrolling within the available height, and
keyboard selection reveals its row. Scrolling clears a selection that
becomes hidden, so Activate cannot choose an unseen row. Width shrinks
to the surface; fewer than three text cells or no available content row
refuses with `NoRoom`. `chrome::Panel::within` validates the complete
rectangle and whole rows, sharing panel rendering and hit geometry.
`Fit::Complete` preserves document-menu admission: a root has the
existing 320-scaled-pixel width and every row above one status row; a
too-small surface refuses without clipped actions. A fully disabled
complete root retains its first-row highlight for editor compatibility,
but cannot activate. Submenus still adapt, including wheel scrolling.
`panel` and `row_rect` expose only visible levels. Bars remain part of
the consumer's chrome; the controller emits semantic panel draws.

The editor uses this controller for its complete menus. Its immutable
data captures application availability, checks, shortcuts and
tab/revision/key profile. Its adapter owns document cancellation policy
and executes typed items through existing commands; it has no private
menu navigation state. td-mail's mailbox list opens a context menu of
its folder actions under its action bar's Folder label through the same
controller, in adaptive fit, the session routing keys and the pointer
to it while it is open, painting it after its frame, and closing it
when the view it was opened for goes under it.
`tests/menus.rs` covers tree and text bounds, disabled navigation,
nested pointer paths, left/replacement placement, Back, scroll/reveal,
revision invalidation, focus/resize, repeated input and complete-mode
compatibility. Draw-stream and pixel oracles preserve
the editor's existing panel output at scales one through four; its scene
and native input regressions remain.

## Shared confirmation dialog

`confirmations::Model` captures owned immutable request text, a typed
confirmation action and a caller revision. Construction validates before
copying and allocates fallibly: at most 256 detail entries, 4096 bytes per
entry and one MiB of detail text, plus nonempty title/action labels of at
most 256 bytes each. Control characters are refused. A source change or
drop after capture cannot change the request presented for confirmation.
`storage_bytes` on the model and controller exposes actual retained text,
container and wrapped-row capacities for consumer accounting, excluding
allocator bookkeeping. A consumer reserves its bound before construction
and reconciles actual capacity before admitting the widget.
The consumer owns any authority or descriptors behind the action ID.
`with_alternate` adds a second action with its own label, validated as the
confirmation label is, for a three-way choice such as Save, Discard or
Cancel; either action closes as `Confirmed` with its own ID.

`confirmations::Controller` composes a title panel, a scrolling detail list
and fixed action rows inside a fully visible rectangle: Cancel, the
alternate when the model has one, then Confirm. Details
wrap at scalar boundaries without loss; precomputed offsets into the
captured strings avoid borrowed self-references and allocation while
handling ordinary input or painting. Layout reserves at most 65,536
wrapped rows. Insufficient width, height, label space or wrapping capacity
refuses with `NoRoom`, without omitting an action or part of the request.
A valid layout shows the complete title and action labels, at least one
detail row, and every action. Title and actions stay visible while details
scroll, separated from the fixed controls by the detail list's own bezel
(`chrome::List`). Resize
retains the selected detail and scroll anchor by entry and byte offset,
then reflows fallibly; a refusal closes with `Unavailable` and never
leaves an old invisible confirmation target active.

Focus starts on Cancel. Tab/BackTab cycle only through the detail list,
Cancel, the alternate if any, and Confirm; without an alternate,
`Focus::Alternate` is never focused and has no row. Up/Down,
PageUp/PageDown and Home/End navigate details; Activate acts only on the
focused action. Escape cancels. `Key::from_chord` is the default set:
Tab and Shift+Tab, Up and Down, Page Up/Down, Home and End, Return or
Space for Activate and Escape; Left and Right, like any chord it names
nothing for, reach the dialog as `Event::Other`. Primary pointer press
arms an action and release on that same action chooses it; moving away
or any intervening keyboard input, including repeats, cancels the arm.
Pointer actions preserve the keyboard focus choice; abandoning a pointer
gesture cannot move the default keyboard action to Confirm. Blank detail
rows do not select content. Outside input is consumed without closing or
reaching underlying controls. Other unhandled input is consumed. Key
repeat never activates. Focus loss cancels; resize cancels a pending
gesture and returns focus to Cancel. A missing or changed revision
closes stale without confirmation.

Confirmation, cancellation, stale data and an unavailable resized layout
each produce one `Closed` outcome. Later events are ignored and a closed
dialog emits no draws. The controller captures an optional opaque prior
focus ID, exposed by `prior_focus()`; the consumer supplies whether that
exact control still exists. Event handling returns an outcome directly;
resize failures are represented by `Closed` with `Unavailable`.
A close returns that ID only when valid, for the adapter to restore focus.
The adapter routes input through the modal controller while it is open
and executes only the typed outcome; td-ui grants no process authority.
The adapter must not position Confirm, nor an alternate such as Discard,
beneath the pointer that opened the dialog: a second click in a
double-click is otherwise a fresh gesture.

`confirmations::KEYS` is the dialog's keys as a key list shows them, so
each program with a dialog lists the same rows for it.
`tests/confirmations.rs` pins the default chords, the listed keys being
exactly the ones `Key::from_chord` takes, default cancellation,
focus confinement, press/release pairing, duplicate/repeated input,
outside input, stale data, focus loss, resize refusal and focus
restoration. It covers capture independence, lossless Unicode wrapping
and scrolling, the full one-MiB request bound, malformed input and
unusable geometry. A three-way dialog's rows stand in order inside it,
each choosing its own action by press and release and the alternate by
Tab and Activate, each painting its own label with only the focused row
highlighted at scales one through four; its label is bounded and
validated, counted in storage, and refused with `NoRoom` when too wide
or when the height holds only two actions, and a resize to such a height
closes it `Unavailable`. Draw-stream and pixel checks at scales one
through four keep the controls within the dialog, preserve pixels
outside it and respect partial damage.

## Shared directory finder

`finder::Listing` is one folder as the consumer read it: its path as
shown, its `Entry` values in the consumer's order, and whether the read
stopped short. An entry is a name, a right-aligned meta, a `Kind` of
folder or file, whether it can be descended into or chosen, and whether
it carries the mark, the list's star prefix: set on the entry with
`with_marked` or on the shown listing by entry index with `set_marked`,
refused `NoEntry` for one not there; what a mark means, a multiple
selection in td-portal, is the consumer's, and the marks go with the
listing they were on. The consumer reads the filesystem under its own
bounds and trust, as td-portal's chooser, td-editor's directory tabs
and td-review's repository chooser do, and the widget reads nothing: at
most 4096 entries, 1024-byte names, 16-byte metas, a 4096-byte path and
one MiB of name and meta text between the entries, all control-free,
refused before capture. `storage_bytes` on the listing and the
controller exposes the retained capacities for consumer accounting.

`finder::Controller` lays a path row, a filter `TextEntry`, the entries as
a `List` and a status row inside one rectangle: four `ROW`s and twenty
cells at least, the list taking the whole rows between, else `NoRoom`. It
owns the filter query, the shown indices, the selection and the scroll
window; construction, like `set_listing`, selects the entry it is given
by name when listed, else the first. The filter is the launcher's rule,
`filter` mounted from the compositor: `insert` takes ASCII folded to lower
case under `MAX_QUERY_BYTES`, and every whitespace-separated term is found
in the name, `matches`' rule with the name folded at the comparison so no
folded copy of each name is held; a change resets the selection to the
first shown.
Up/Down, PageUp/PageDown and Home/End move and reveal the selection and
honour repeats; a move that goes nowhere is consumed. Activate descends
into an enabled folder (`Descend` with the entry's index) and, when files
are what is chosen, chooses an enabled file; Accept chooses the listed
folder itself (`Here`) when folders are chosen, so a selection resting on
a subfolder cannot be taken for the folder in view, and the selected
enabled file otherwise; Parent, and Backspace on an empty filter, ascend
(`Ascend`); Escape cancels. A repeated Activate, Accept, Parent, Escape or
empty-filter Backspace is consumed. A press on a shown row selects it and
the wheel over the list moves the window, keeping the selection in it;
other pointer input on the finder is consumed, and pointer input off its
rectangle is ignored, the consumer's to act on (its bar, its own
controls). Descend and Ascend leave the widget as it
is: the consumer lists the folder and installs it with `set_listing`,
which clears the filter and the note and selects the entry it names (the
folder an ascent came from), or says why it could not with `set_note`,
shown in the status row until the next listing. Resize relays out and
keeps the selection shown; a layout that cannot hold the finder closes it
`Unavailable`. A choice, a cancel and an unavailable layout each produce
one `Closed` outcome; later events are ignored and a closed finder emits
no draws. The consumer owns physical key bindings, the double click it
turns into Activate, the prompt that tells the user what Return, Backspace
and Accept do (the finder paints no affordance for them), and whatever the
chosen path is used for; the widget grants no process authority.

`emit` paints chrome under the rectangle, the path's tail when it is
longer than the row, the field with its `Filter` placeholder, the list
with a folder's meta `folder` when the consumer gave none (a meta is left
out of a row that cannot keep `LABEL_COLUMNS` of the label beside it, so
a meta at its bound never hides the name), `No match` or `Empty folder`
in an empty list, and the status row under a rule: the entry count
(`0 entries` for an empty one), the match count over the total while
filtering, `cut short` for a truncated read whether or not anything was
listed, or the note. It allocates nothing, which `tests/confinement.rs`
holds at the source: no formatting, collecting or boxing in the module.

`tests/finder.rs` pins the listing bounds (the text bound measured as
text, not capacity), the filter rule, navigation and reveal, an empty
listing taking every key and press, the outcomes of choosing in either
mode and of a disabled entry, listing replacement selecting by name, the
note's life, the press and wheel paths on and off the finder, a meta left
out of a row too narrow for it, a long query keeping its caret in a
narrow field, the geometry refusals and resize, and draw-stream and pixel
oracles at scales one through four keeping every draw inside the
rectangle, naming each band's text with counts of several digits and
leaving the surface around it untouched.

## Shared time-series charts

`charts::Chart` is a borrowed, validated view over caller-owned `Time`
and `Series` slices. `State` owns only an optional semantic selection and
an armed pointer gesture, so a consumer needs no self-reference or copied
history. Construction, ordinary input and painting allocate no storage
and perform no I/O. The caller owns collection, units, aggregation,
series choice, downsampling and revision policy.

A view admits up to 1024 strictly increasing u64 timestamps and 16
nonempty, uniquely identified series. Each series has one optional u64
value per timestamp. Zero samples are allowed. Labels are nonempty,
control-free and at most 128 bytes. The caller supplies a nonzero maximum,
its display label and units, plus a nonzero display divisor and zero
through six decimal places. Displayed values round half up in a fixed
stack buffer, so percentages and byte units retain integer observations
without unreadable raw counters. Values above the maximum and overflowing or
above-maximum stacked sums are refused. Invalid data never becomes a
partial plot. The view must fit completely inside a valid surface and
show its labels, legend and a plot at least two label rows high; otherwise
`NoRoom`
allows the consumer to show its fallback.

Lines interpolate only between adjacent present observations. A stacked
column requires every component; a missing value makes a gap in that
column rather than an invented zero. Cumulative boundaries interpolate
with exact integer arithmetic and consistent rounding, keeping layer
order and the maximum intact even at u64 limits. Painting and hit testing
share these clipped columns. Fixed input bounds and `MAX_AXIS` bound
emission to at most 155648 draw commands (`DRAW_LIMIT`); input validation
and the maximum-size draw oracle pin the work bound.

Explicit observation markers preserve isolated readings whose timestamps
fall between pixel columns. Markers paint after the interpolated plot;
later timestamps and then later series paint last where their markers
overlap. Hits use the reverse order and return the visible marker's exact
timestamp and opaque series ID. Elsewhere a hit returns the nearest
supplied timestamp (earlier on a tie), with a series only inside a stacked
band or within three scaled pixels of a line. A zero-height band has no
hit. The widget never interprets an ID as a PID or invents an identity
for a gap. The consumer may use a typed Other series to open a ranked list.

Axes, units, all legend labels, selected time and the selected series'
value are textual. A single observation has one centered timestamp label.
Missing selections or values show Unavailable. With no observations,
legends remain informational and emit no selection because there is no
timestamp to select. A selected time has a contrasting outlined plot
marker; the selected legend and graph focus use contrasting text and
backgrounds. A time absent from the supplied observations has no marker.
Legend clicks select that series at the current valid time, or the newest
time when none is retained.
Previous/Next/First/Last time keys and Previous/Next/Clear series keys
provide pointer equivalents. The adapter routes keys only while the
graph has focus and supplies focus separately for painting.

A primary press captures the semantic target and caller revision;
matching release selects once. A different revision, layout or plot mode
disarms even if a resize event was omitted; stale motion/release returns
Stale. A fresh press or key still processes its own intent. Moving away,
focus loss, resize, other input and keys (including repeats) cancel the
arm. Previous/Next time keys honor repeats; other repeats are consumed.
A consumer
may set a selection explicitly; unavailable IDs/times are not silently
retargeted until a new navigation intent. An explicit Resize event also
disarms when the geometry is unchanged. Navigation from an absent time
starts at the newest observation before applying the requested step.
Idle motion and cancellation events return Ignored when no arm exists.
`cancel_gesture()` retires input without a view, including before a
relayout that may refuse with NoRoom; it preserves the selected time/ID.

`tests/charts.rs` covers line/stack paint-hit agreement, gaps, isolated and
coincident readings, timestamp and counter extremes, keyboard and pointer
selection, refresh and interrupted gestures, input maxima/refusals and
bounded drawing. Scale 1-4 pixel oracles preserve pixels outside the
chart and compare partial repaint with full repaint.

## Shared split pane

`split::Controller` partitions a fully visible rectangle horizontally or
vertically into two children and an eight-pixel logical divider. Config
supplies nonzero logical child minima, each bounded by `MAX_AXIS`;
Surface scale converts them once into device pixels. `Share` stores an
exact nonzero-denominator fraction for the preferred first-child extent.
Layout clamps to both minima without changing that preference, so a
temporary small window cannot permanently move the divider. A stationary
click/release leaves that exact preference intact, including while
clamped. A drag released back at its initial position restores the
captured preference too.

A valid `Layout` contains three disjoint rectangles covering the entire
input rectangle. If the extent cannot hold both minima and the divider,
`layout()` returns None, emits no draws and supplies no hit targets. An
empty rectangle contained in the surface uses this same fallback. The
consumer owns the fallback and retains its content state. Invalid
surface or out-of-surface resize returns an error after retiring old
geometry and pointer capture. No child receives overlapping or invisible
hit regions. Focus gained during fallback is retained when useful
geometry returns.

A primary press on the divider takes local pointer capture, preserving
the offset within the handle. Motion and release adjust the split even
outside the window, clamping safely. A focused divider accepts decrease,
increase, first-minimum and last-maximum keys; the consumer maps
physical keys for the chosen axis. Keys, resize and focus loss end a
drag. A release without capture does nothing. A new press outside the
divider retires old capture and returns Ignored so a child can handle
that press. Repeated focus assignment is consumed without claiming a
visual change. The preferred share is exposed for the application; the
widget neither persists it nor owns child content.

Painting emits only the divider and its grip, with a distinct focused
background, through the clipped semantic draw stream. Geometry, input
and painting allocate no storage and use no I/O. Twelve tests cover both
axes and scales 1-4, exact partition/minima, extreme pointer
coordinates, keyboard movement, capture cancellation, temporary
clamp/fallback restoration, stationary/returning clicks, invalid inputs
and focused/partial repaint pixel oracles on both axes that leave child
pixels untouched.

## Completed double clicks

`pointer::DoubleClick<I>` pairs completed semantic clicks on the same opaque
identity within 500 milliseconds and four logical pixels per axis. The
caller supplies monotonic nanoseconds and logical coordinates; the helper
reads no clock and allocates nothing. Backward time, excessive delay,
distance or a different identity starts a new candidate. A completed pair
is consumed, so a third click cannot reuse it. The caller cancels on other
input or geometry/focus changes and remains responsible for matching each
individual press/release against its captured target. Tests cover identity,
time, coordinate extremes, explicit cancellation and consumed pairs.

## Following links

A Control-press on a link opens it in the browser in td-editor's
documents, in td-mail's messages and in td-news's articles,
which each shows in the editor's pane (td-news's log cuts its lines at
the pane's width, so it follows none), and in td-term's grid. The rule
and the launch are the toolkit's so the programs agree.
`links::at` finds the link over the byte under the pointer: an
`http://` or `https://` scheme with at least one byte after it, running
to the next ASCII whitespace or `<>])"'` backtick, trailing `.,;:!?`
left out, which is td-mail's link-list rule. It looks at most
`MAX_BYTES` either side of the byte, and a run longer than that on
either side is no link, so a press costs the same in any line. td-term
reads its grid's row through it (td-term/DESIGN.md §3).
td-editor's controller maps a surface pixel to the glyph under it
(`Controller::link_at`, read-only) and the program follows what it
answers: a press over a link opens it and is not the document's press,
so the caret, the selection and a drag are untouched; a press anywhere
else is a plain press. A followed press also ends a click sequence, so
the next press is a first click.

While Control is held with the pointer over the window, the link a
Control-press there would follow is underlined, so a person sees what
will open before pressing. The widget window reports the pointer as
`Input::Hover` and td-editor's native window reads its own; each program
hands the controller the point (`Controller::hover_link`) only where a
press would reach the pane and follow (no menu, finder or prompt taking
the press, the mouse on in td-mail's and td-news's configuration, and in
td-news an article), and none otherwise. The controller
keeps the point, not the link, and its scene underlines the link
`link_at` finds there in the text it draws, a one-pixel-per-scale rule
along the cells' last row in the glyphs' ink, so text that changes or
scrolls under a still pointer is underlined as it now is. The point
moves no caret, selection or controller generation; `hover_link`
answers whether the link under the new point differs from the one under
the old, both in the text as it is now, and only then does the program
redraw (a change of the text is its own redraw). It keeps the link it
found with the generation, tab and revision it found it at, so each
point the pointer moves to walks the layout once, as a drag's motion
does, and a scene of the same state walks it for none. The underline
also shows while the button is held, where no press can begin until
the release; hiding it there would flicker it off and on under every
Control-click. td-term does not underline yet.

`open::link` refuses anything that is not one link whole
(`links::whole`, the same rule without the press's bound, so a link
found at any byte opens), so no option, whitespace or other scheme
reaches the browser's arguments. `open::url` takes a URL a program lists
from markup or a feed rather than finds in shown text (td-news's links
and an article's own link), whose ends the markup gives, so a `)`, a
quote or a trailing `.` the text rule leaves out is kept: it must be an
`http://` or `https://` scheme, in any case, with at least one byte
after it, that not a further `/`, and hold no whitespace, control
character or backslash, so it too is one word and no option, and the
host a browser opens is the one its text names: a browser reads `\` as
`/` and skips slashes after the scheme, so
`https://evil.example\x.example/` and `https:///evil.example/` would
open `evil.example`. td-term's
OSC 8 links pass the same test (`open::is_url`) before they are ruled,
shown or followed. `open::file` is the one other target: a local file
the program wrote (td-news's digest), which must be an absolute UTF-8
path and is given as a `file://` URL with every byte but an unreserved
one or `/` percent-encoded. The portal's `OpenURI` refuses `file`
(APPLICATIONS.md §W.6), so a program in a jail cannot open one this way.
The browser is the program's configured command, else `BROWSER`, else
`xdg-open`, split on whitespace and run directly, never through a shell:
a word holding `{url}` has it replaced by the link (quotes put around
the bare placeholder are taken off), and a command without one gets the
link as its last word. The child's standard streams are `/dev/null`, its
environment is the program's without `WAYLAND_SOCKET` (so the browser
cannot speak on the program's display connection) and, through
`open::link_on`, with `WAYLAND_DISPLAY` set to a path the caller names
(td-term, whose own environment need not name the socket it dials, names
that socket, made absolute so the browser's libwayland does not resolve
it under `XDG_RUNTIME_DIR`), and it is reaped on its own thread, so the
window's loop never waits on it; the program reports the program
started, or why none was, in its status. A `WAYLAND_SOCKET` descriptor
the program inherited without close-on-exec is still inherited by the
browser, as by any child: the client duplicates it and leaves the
original alone. td's launchers pass `WAYLAND_DISPLAY`. The pins in
`tests/confinement.rs` hold `BROWSER` as the opener's one environment
read outside tests (the face and theme files' reads are pinned with
them), the `WAYLAND_SOCKET` removal and the named `WAYLAND_DISPLAY`.

## Shared tree table

`tree_table::Model` captures a validated visible preorder over opaque
`Copy + Ord` row IDs. The consumer chooses the full hierarchy, filtering,
expansion and sibling ordering; each visible nonroot names its nearest
preceding ancestor, whose children must be expanded. Root rows have no
parent and depth zero. Duplicate IDs, depth jumps, revived old ancestors
and children beneath collapsed or leaf rows are refused. Synthetic roots
have ordinary opaque IDs; the widget gives them no process authority.

The model owns only row metadata, a sorted ID/index lookup and column
headings. It reserves fallibly, sorts without allocation and exports
capacity-based `storage_bytes()` including titles and inline model state,
not allocator bookkeeping. Limits are 32769 visible rows, accommodating
32768 processes plus a synthetic root, depth 256, 16 columns, 128-byte
nonempty control-free headings and 16 MiB of model
storage. Columns provide logical minimum/preferred widths, at most 8192;
minima hold their complete headings, insets, sort mark and border. The first minimum is at
least 32 pixels; consumers can widen it to reveal deeper indentation.

Cell values remain in the consumer. `Cell::new` validates control-free
text through 4096 bytes, with an explicit empty-cell fallback. `emit`
requests borrowed values only for rows and columns intersecting both the
visible viewport and damage, then streams clipped draws without allocation
or I/O. Returned text must outlive the complete emit call; a callback cannot
return a borrow into its mutable formatting scratch. Consumers prepare
numeric strings in a bounded visible cache before painting.
Numeric columns align right; the first column reserves 16 logical pixels
per hierarchy level and a disclosure slot, clipped to that column. A row
with children shows its disclosure as a box nine logical pixels square in
the row's ink, holding a minus, and a plus while collapsed; it is drawn
as fills rather than a glyph, none overlapping another, so it is the same
crisp mark at every scale in either face.

`Controller` owns the model and stable selection. A model replacement
preserves selection and the first visible anchor by ID when present
(subject to viewport clamping), clamps the old position when its anchor disappears, and clears selection
when its ID disappears. Replacement preserves widths when headings and numeric roles are unchanged,
clamping to new minima; a changed column schema uses its new preferences.
`width` and `set_width` let the consumer save and restore widths. Mutation,
resize, focus changes, scroll and keys retire pending pointer capture.
`captured()` lets a consumer defer refresh while a pointer gesture is active.
The consumer must replace the model when row membership, ordering or
expansion changes; editing values alone does not reinterpret row IDs.

Shared geometry moves headings and cells together during horizontal
scrolling. A vertical scrollbar has a reserved gutter; a horizontal one
appears only when columns exceed the viewport. Captured scrollbar motion
retains its grab offset and exact starting position, clamps beyond the
window and ends on release or cancellation. Wheel-style scroll events
carry rows and 16-logical-pixel horizontal steps. Invalid geometry returns
an error after retiring old draws and hits. Extents too small for a header
and one row yield no layout, leaving the consumer its fallback.

Pointer presses capture row IDs and a role: selection, disclosure or
column heading. Matching release emits that semantic intent once. Moving
away, refresh, focus loss and resize prevent a later release from naming
a new row at the old position. Moving away retains canceled capture until
release, consuming that release without an intent even outside the table.
Disclosure intents include the requested
expanded state; sort intents name a column, without applying policy.
Pointer presses give rows keyboard focus; clicking a sort heading keeps
that row focus. Reapplying the same focus preserves capture, while changing
focus retires it. Selected rows and keyboard-focused headings have
contrasting feedback.
The consumer provides an optional ascending/descending sort indicator.

The adapter routes focus to rows or a validated heading. Up/Down,
PageUp/PageDown and First/Last select and reveal rows. Up/Down without a
selection start at the first visible row; unchanged navigation is
consumed without repeating a selection intent. Left collapses an
expanded branch, otherwise selects its parent; Right expands a collapsed
branch, otherwise selects its first visible child. Activate emits a row
activation or heading sort intent. Header Left/Right and First/Last move
and reveal the focused heading. ScrollLeft/ScrollRight move the
horizontal viewport. Navigation honors repeats; repeated Activate is
consumed. The consumer owns physical key bindings and focus traversal;
`Key::from_chord` is the default set: arrows, Page Up/Down, Home and End
for First and Last, Shift+Left/Right to scroll sideways, and Return or
Space to activate.

Tests cover the default chords, bounded hierarchy and text validation,
stable ID/anchor replacement, stale and interrupted gestures, disclosure
and navigation, scrollbar capture, horizontal header/cell/hit alignment,
visible-only formatting, fallback and scale 1-4 clipping/partial-repaint
pixel oracles, and the disclosure box's exact plus and minus, its colour
on a focused selection, its clipping to partial damage and its absence
on a leaf, at scales 1-4.

## Shared message list

`messages::Controller` is a chat transcript over one rectangle, built
for td-agent's conversations (`td-agent/DESIGN.md` §4) and general to
any program that shows one. It owns its messages and their layout. A
`Message` is a header label (its role or source: user, orchestrator,
assistant, a tool's name), an optional status shown after the label and
an optional verdict shown before the copy button, each in a `Tone`'s ink
(a verdict with its mark, a tick, a cross or a dot), and up to
`MAX_SECTIONS` sections of caller-supplied text in order. A section is
untitled body text, or titled (reasoning, a tool's arguments), which
collapses to its title row; an excerpt section shows at most
`EXCERPT_ROWS` rows and `MORE` below them when it has more. A message
may carry a `source`, the text its copy action copies in place of its
sections: a tool block is a message whose sections are its arguments
and the excerpt of its result, and whose source is the whole result the
caller retains. Labels, statuses, verdicts and titles are nonempty,
control-free and at most `MAX_LABEL_BYTES`; a section's text and a
source are at most `MAX_TEXT_BYTES` (the clipboard's ceiling) of any
text, a tab drawn as a space and another control scalar as the
replacement character; at most `MAX_MESSAGES` messages and
`MAX_TOTAL_BYTES` of text between them, and at most `MAX_LINES` laid-out
rows. `push` appends, `append` adds text to a section as a reply streams
in, `replace`, `set_status`, `set_verdict` and `set_source` change one,
`set_collapsed` and `set_section_collapsed` fold one; each allocates
fallibly and leaves the list as it was when refused. `remove_first`
trims the oldest and is never refused: a trim cannot grow a layout, and
a list left without one lays out again if what remains fits. A list
given messages or text, or unfolded, while it had no layout (below),
since nothing counts rows then, may lay out past
`MAX_LINES` at its next width, which `resize` refuses with `Limit`
until a trim makes room.

The layout is rows in the cell model: a `ROW`-tall header, title, text
and more rows a cell high, and a `GAP` below each message, all scaled.
The rectangle less a `chrome::List`-sized scrollbar gutter holds the
rows, text a cell in from each side. Text wraps at the columns that
leave: a newline ends a row, a row breaks after the last space or tab
that keeps its word whole, else within the word, and a space or tab
reaching past the last column stays on its row unshown, so the rows tile
the text between newlines; a newline right after it, or the text's end,
adds no empty row. Appended text is wrapped from its section's last
shown row on, since a row starts with nothing carried from the one
before, so a reply streamed in small pieces is laid out as it arrives
rather than rewrapped whole, and lays out as the whole text would. A
header shows its open or shut mark, the
label, the status two cells after it, the verdict a cell before a
`chrome::Button` reading `COPY_LABEL` at the right, inset by
`BUTTON_MARGIN`; the label and status stop a cell short of the verdict.
Headers are `CHROME` under a `BORDER` rule, the focused message's
`SELECTED_ROW`; text is `INK` on `PAPER`, selected text the text entry's
`SELECTED` with paper ink while the list has focus and
`INACTIVE_SELECTION` without; titles and the more row are
`LINE_NUMBER`. A rectangle narrower than `MIN_COLUMNS` and the gutter,
or shorter than a `ROW`, lays nothing out: `has_layout` is false, the
list draws nothing and ignores input, the consumer shows its fallback,
and a resize that gives it room lays it out again at the place it
showed; one outside the surface is refused.

The view scrolls by row: `Up` and `Down` a row, `PageUp` and `PageDown`
to the first row the view did not hold whole and back, `Home` and `End`
the ends, the wheel a row per row of travel, and a drag past the view's
top or foot a row per motion. The last page is the earliest row from
which the rest fit, but never past the last message's last header,
title, text or more row, so a view too short for a header and its gap
shows the header. At the end the list follows: a message arriving or text
streaming in keeps the end shown; scrolled back, the first shown row
keeps its place in the text through every change, resize and period
without a layout (the row holding its start, or the title of a section
folded over it). A fold the user asks for (a press or `Toggle`) keeps
the first shown row rather than the end, so the row pressed stays where
it was; the list follows again only if that leaves the view at the end.
`PreviousMessage` and `NextMessage` focus a message, the first shown
when none is, and scroll its header into view, as `Toggle` does.

A selection is two `Point`s, a byte of a section of a message, so it
spans messages in reading order. A press on text starts it, Shift
extending the selection's head from its kept anchor, and the drag moves
the head; a second press within `MULTI_CLICK_MS` of a click's release
and four scaled pixels of it selects the word of letters and digits
under it, across rows it wrapped over but not into text the section
hides, and nothing past a row's text; its drag does not extend it.
The pointer over chrome is the nearest point: a header its message's
start, a title its section's, the more row its excerpt's shown end, the
gap past the message's sections, above the view the first shown row's
start and below the rows the last's end. Copying visits shown text
only. A section's shown text is the source from its first shown row's
start to its last's end, newlines and empty rows included; the
selection's part of it is one piece, a section the selection only
touches at an end gives none, an empty section strictly inside it an
empty piece, and a blank line goes between two pieces. So a selection
within a section copies exactly the source between its ends, newlines
at either end included, a select-all copies what the whole-message copy
would of a message with no `source`, nothing hidden and neither its
first nor its last section empty, and no header, title, status,
verdict, button or more row is ever copied; a collapsed section's and
an excerpt's hidden text are not selected. A press on a header focuses its
message and starts no selection; on its first three cells it folds the
message, and on the copy button it copies the message whole. A press on
a title row folds the section.

Copies go through the window's `Clipboard` (see "Widget window") while
the event is delivered, the button's at its press and the keys' at
theirs, since a release, a repeat or a poll has no serial: `Copy`
(`C-c`) copies the selection, `CopyMessage` (`C-S-c`) the focused
message, and the button its own. A message's copy is its `source` when
it has one, else every section's text whole, collapsed and excerpted
ones included, a blank line between two. `Outcome` says `Copied`, the
clipboard's `Refused`, or `NothingToCopy`; a copy past the clipboard's
ceiling is refused as `TooLong` before the text is gathered. A repeat
never copies or folds; navigation honours repeats. `select_all`,
`clear_selection`, `selected_text` and `message_text` give a consumer
the same without a key.

The caller owns the clock, passed with each press and release in
milliseconds, the key bindings (`Key::from_chord` is the default set:
`Up`, `Down`, `PageUp` and `PageDown`; `Home` and `End`, and `C-Home`
and `C-End`; `M-Up` and `M-Down`; `Return` to fold; `C-c`, `C-S-c` and
`C-a`), routing the wheel and keys to the list when it has focus, focus
itself (`Event::Focus`), and what a message means. Painting and input
allocate nothing, but for the text a copy hands the clipboard and the
rows a fold lays out, which it reserves fallibly as its setter does;
nothing reads a clock, a file or the environment. `shown` and
`copy_button` give a consumer the shown rows and a header's button for
its driven read-back and its tests.

## Task-manager widgets

[td-taskmgr](../td-taskmgr/DESIGN.md) is a planned consumer. Menus and
confirmations, nonclosable resource tabs, charts, the split pane and tree
table are implemented as shared widgets. Process collection, history and
signal execution stay in the consumer.

All these widgets preserve the pure-input and semantic draw-stream seams
above. They introduce no filesystem access, process authority, external
crate or direct pixel writes. State-machine and draw-stream tests cover
their interaction contracts, with software pixel oracles at multiple
scales, clipped/narrow surfaces, focus loss and stale consumer data. Extend
shared primitives atomically where necessary; no temporary application
copy of a widget is an acceptable completion of this work.

## Editor core

The document pane td-mail, td-news and td-review embed, and the td-pass
notebook window is to embed, is td-editor's own editing core, moved here
whole so no consumer depends on the editor crate: the text bounds and lossless
codec, the model and its transactions, the fill planner, the key
profiles, the layout, the clipboard capture, the dialog permits, the
controller and the scene, as `editor_text`, `editor_model`,
`editor_fill`, `editor_keys`, `editor_layout`, `editor_clipboard`,
`editor_dialog`, `editor` and `editor_render`, with their refusals in
`editor_error`. Their behavior did not change in the move, and
`td-editor/DESIGN.md` remains the normative contract for it: the
document model and file safety, the input-controller contract, filling,
the reference renderer and "Embedding the document view". This section
holds the toolkit's side. Find's history, `editor_search`, followed (see
the vault-document bullet below).

- All eleven modules are pure in the confinement test's sense: no
  environment, filesystem, network, process, clock or I/O access, and at
  most one test module, at the file's tail. Files, the clock, spelling,
  the control socket, replay and the reference preview stay td-editor's.
  The scene takes a spelling checker's status and marks as borrowed
  values, not td-editor's spelling state, and a host's inks the same
  way (`Scene::inks`, td-editor/DESIGN.md "Implemented
  reference-renderer contract").
- Permits bind an editor and a revision across the crate boundary. A
  `RevisionPoint` carries a private owner token only its `Editor` mints,
  and its tab and revision are read through accessors, so a consumer
  cannot retarget one; a compile-fail example pins that. `Discard` and
  `Reload` are made only through the dialog flows here, `Close` and
  `Conflict`, and their `apply` stays crate-private, so a consumer can
  hold, check and hand back a permit but never forge or apply one. The
  flows record a decision; the user's consent behind it is the
  embedder's to ask for, as it was td-editor's window's before the move.
  The one discard without a permit is `Event::Clear`, a lock that forgets
  every document, dirty or not, at once; it is for a host that locks,
  td-pass, and td-editor's confinement test pins that the editor never
  sends it. What td-editor reached as crate-private before the move is
  public now because it is another crate, except the model's `open` and
  `missing_file`, which consumers reach through the controller's events
  and which stay crate-private.
- The crate root re-exports nothing of the core; consumers name the
  `td_ui::editor*` paths. td-editor alone re-exports `Error` as its own.
- `Controller::generation_for_test`, `cfg(test)` before the move, is
  test support, public because a consumer's tests are another crate: it
  sets the staleness counter so they can reach its exhaustion. The
  confinement tests here and in td-editor hold both crates' production
  sources to never calling it.
- A pane can hold a document whose text is its own, as a td-pass vault
  entry's is (`td-pass/DESIGN.md`, "Notebook"). `Event::Fillable`
  cleared refuses Fill Paragraph and turning Auto Fill on, turns Auto
  Fill off, and ignores the fill chord as a read-only document ignores
  an edit, so visual wrapping is the only wrapping and no newline is
  inserted. `Snapshot::capture_selection` takes the selection alone,
  never the caret's line, for a host whose copy and cut take exactly
  what is selected. `editor_search`'s `History` gives the pane the
  editor's find: literal, from the selection, stopping at the end
  before a repeated find wraps; td-editor keeps only its minibuffer
  prompt. Line endings are the codec's: a document keeps the one
  ending it was loaded with, LF or CRLF, and mixed endings are
  refused; inserted and pasted text is held with LF, its CRLF pairs
  folded, and saved with the document's ending; any other control
  character but tab, or invalid UTF-8, refuses a paste whole. A
  selection capture carries the document's own ending, so a copy from a
  CRLF entry is its stored bytes; the line capture the editor's window
  uses keeps LF. `Event::Clear`, for a lock, forgets every document with
  its undo and redo history, its views and the input state, leaves the
  gutter and scrollbars as a fresh controller's, and gives the editor a
  new identity, so every `RevisionPoint` and dialog flow held before is
  stale; tab IDs and the generation keep counting. Whenever the model
  frees a document's text or a history transaction's text (a close, a
  discard, a reload's replacement, history eviction or redo truncation,
  a clear, the editor's drop) it zeroes the buffer across its whole
  capacity first, and a find `History` does the same with its query.
  Buffers a growing string left behind earlier are out of reach, and so
  are copies outside the model: a host's `Snapshot` and `Paste`, and
  the transient strings an insertion passes through. This is best
  effort, not erasure. Spelling was never the core's.
- The core's tests are `tests/editor_core.rs`, `editor_layout.rs`,
  `editor_render.rs`, `editor.rs` and `editor_clipboard.rs`, moved with
  it, the controller's half of td-editor's caret-tick fence in
  `tests/confinement.rs`, and `tests/editor_policy.rs` for the
  vault-document policy. `editor_search` carries its history tests.
  td-editor keeps the replay wire's, the real binary's and its window's.

## Outline faces and the glyph atlas

Before this section every td-ui consumer and td-term drew text from one
bitmap face: Unifont's 8x16 cells, pixel-doubled at scales above one. This
section adds a second kind of face, a TrueType outline covered with
antialiasing at the surface's own pixel size. The first such face is
JetBrains Mono Nerd Font. The bitmap face stays as the fallback and as the
face of the compositor's attention request raster (its attention notice
keeps the compositor's own compiled-in glyphs); the rest of the compositor's
chrome draws the outline face (td-compositor/DESIGN.md, "Chrome text"). The
work is ordered so that a GPU backend, when td has one, reuses the same
model as the CPU raster (see "The GPU path" below).

### The face

The first outline face is the Mono variant, `JetBrainsMonoNerdFontMono`,
from the Nerd Fonts v3.5.1 release. The Mono variant holds every icon to
the single advance, so the icons fit td's cell grid. Its measurements:
1000 units per em, a 600-unit advance, a 1020 ascender, a -300 descender,
no line gap and 12608 glyphs, all quadratic `glyf` outlines. Its Unicode
map is cmap format 12, because the Material Design icons sit in plane 15
(from U+F0001). So a scalar-to-glyph lookup must cover every plane, not
only the BMP.

At 13 pixels per em the cell is 8x17, close to today's 8x16. At 16 it is
10x21. Cell metrics come from the face, not from constants. Bold, italic
and bold italic are that release's own files. The terminal's fake bold
smear and italic shear remain only for the bitmap face.

Its ligatures are never applied. td-ui maps one scalar to one glyph through
the character map and reads no `GSUB`, so the calt ligatures do not
change what a cell shows. Hinting programs (`fpgm`, `prep`, per-glyph
instructions) are skipped and never executed, and coverage is unhinted
linear area. Kerning (`GPOS`, `kern`) is not read, since a monospace face
has none that matters on a grid.

### Delivery and trust position

The Unifont face is committed as generated Rust source (see
`td-compositor/assets/PROVENANCE`). That route does not scale to this
face. Each style is 2.5 MiB, about 5 MiB of hex per style as source. Target
recipes stage sources as strings, so `include_bytes!` of a binary is not an
option either. The face is therefore data. A recipe fetches the release
archive as a fixed-output source by its upstream URL and SHA-256 (the
recipe `jetbrains-mono-nerd-font`):

```text
https://github.com/ryanoasis/nerd-fonts/releases/download/v3.5.1/JetBrainsMono.tar.xz
sha256 04d5e8f903693f9dd13e16f867e994834e681eb3c72c0d337a770dcda09010cf
JetBrainsMonoNerdFontMono-Regular.ttf
sha256 f2a5ea6cfab397445ffab00c0370927b66d61e560a05db5db271b42006381c1a
OFL.txt
sha256 30f0c136e3c88e422d0791acd97238870f9054a9729bc34cf2ff0d4ed8cac4ad
```

The recipe copies the four Mono styles, `OFL.txt`, the release's
`README.md` and the notices below into `share/fonts/jetbrains-mono-nerd`
of its output, and builds nothing. The image carries that output read-only
and names it `/etc/fonts/jetbrains-mono-nerd`, an immutable `/etc` link
into the store. `static-runtime` carries a copy at
`files/etc/fonts/jetbrains-mono-nerd`, and td-jail binds the runtime's
`files/etc/fonts` at `/etc/fonts`, so a jailed program reads the face at
the path an unjailed one does. A consumer reads a style's bytes once at
startup from that path and hands them to `sfnt::Font::parse`. The pure
modules never open a file. A missing or refused face falls back to Unifont
and says so once, naming `./install-fonts`; a program never fails to
start because the face is missing.

A program run on another host, from a checkout, has no such path. So
`face_file::find` looks in order in `/etc/fonts/jetbrains-mono-nerd`,
then in `fonts/jetbrains-mono-nerd` under the XDG data home
(`$XDG_DATA_HOME`, else `~/.local/share`), and then under the font
roots: the data home's `fonts`, `~/.fonts`, and `fonts` under each XDG
data directory (`$XDG_DATA_DIRS`, else `/usr/local/share` and
`/usr/share`). It takes the first directory holding the regular style as
a regular file. Each root is walked a level at a time to depth 4, each
level in name order, so the answer does not depend on the order a
directory lists its entries in. Hidden names are skipped, so a staged
install is never taken. A symbolic link to a directory is followed,
since a Guix or Nix profile's font directories are links, and the depth
bounds a cycle. The whole search reads at most 16384 directory entries,
and the one entry read past that stops this walk and every later one.
The search lists directories and looks for the one name; it opens only
the files the consumer then reads, under the reader's bounds. Those are
host paths, so `read` checks a file is regular and within bound both
before it opens it and on the opened descriptor, and opens it without
waiting: a pipe swapped in between the two can neither hold a program's
start nor be read. On the image and in a jail the first place answers.

`./install-fonts` (`td-builder install-fonts`) puts the pinned face in
the data-home directory. It builds `td-recipe-eval` with the host's
cargo and has it print the font recipe's plan: the directory, the
archive's pin, the members and each notice's pin and path. A pin found
in the shared sources cache (`~/.td/sources`) with its digest is used as
is. A cold one is fetched by the checkout's td-net, verified and moved
into the cache. Every pin is verified again before it is read. The
archive is unpacked by the reader the recipe's `unpack` step uses, and
the members, regular files only, and the notices are staged beside the
directory and put in its place. The installed directory is the recipe's
output directory, notices included. Scratch, staged and replaced
directories are hidden and named for the installing process, and those
of an install that was killed are swept by the next.

A face found under a host font root is whatever the host packaged under
the regular style's name, not the pinned bytes. It is parsed under the
same bounds as any other input, so it can only change what a glyph looks
like; nothing executes it.

A compiled TTF is not source: JetBrains builds it with fontmake from
`.glyphs` sources, and Nerd Fonts patches it with FontForge. Neither
toolchain is in td's graph, so td cannot rebuild the face. It is not an
executable either: td never runs its bytecode, and no program but td-ui's
reader parses it. It is read at runtime, never embedded, so it is not a
compilation input. AGENTS.md names it pinned upstream data: unmarked,
because nothing executes, links or loads it as code, and reached only as
bytes a td-built program parses under its own bounds.

Licences: JetBrains Mono and the patched faces are under the SIL Open Font
License 1.1 (`OFL.txt`). The release's `README.md` names each icon set
Nerd Fonts merges in, with its upstream, version and licence. Both ship
beside the faces. The release archive carries no icon-set notices, so the
recipe also pins, each by its URL at the `v3.5.1` tag and SHA-256, the
notices the Nerd Fonts repository carries there, and ships them under
`licenses/`: the repository's own `LICENSE` and those of Codicons and
Font Awesome (CC BY 4.0 for the icons), MaterialDesign (the Pictogrammers
licence, naming Apache 2.0), Octicons, Powerline Extra and Powerline
Symbols (MIT), and Pomicons and Weather Icons (OFL). The repository
carries no notice text for Devicons, Font Awesome Extension, Seti, the
IEC power symbols or Hack's extra glyphs (each MIT upstream), for Font
Logos, which the README lists as unlicensed, or for the Apache 2.0 text
MaterialDesign's names by URL; for those the README's attribution and
that URL are what ship, and pinning the texts from their own upstreams
is a separate reviewed change. The notices travel as files beside the
faces, in the image and in `static-runtime` alike; a program with a
`--font-license` output names that directory beside the embedded
Unifont texts once it reads the face (increment 21).

### Reader and coverage

`sfnt` is a bounded reader, and it is pure. It accepts a TrueType
table directory and refuses collections and CFF outlines by name. It
checks each table's range and refuses duplicate tables. From the tables it
reads:

- `head`: the magic, 16 to 16384 units per em, and the loca format;
- `maxp`: the glyph count;
- `hhea`: the ascender, descender, line gap and long-metric count;
- `hmtx`: advances; glyphs past the long metrics share the last one;
- `loca`: each glyph's range, with the start no later than the end and
  the range inside `glyf`;
- `cmap`: format 12 is preferred over format 4, and Windows full repertoire
  over Unicode platform over Windows BMP. A malformed subtable is passed
  over; the font is refused, with its error, only when no Unicode
  subtable is usable.

A non-empty glyph has at least its ten-byte header. Simple glyphs decode
every flag and coordinate form. Composite glyphs
decode offsets, the scale forms (uniform, x and y, and two-by-two), scaled
and unscaled offsets, and point matching. Composites are held to a depth of
8 and 256 components per glyph at every depth together, so a fan-out
cannot multiply through the depth bound. The whole outline is held to 8192
points and 1024 contours. Every refusal names its item, and a refused glyph
leaves an empty outline behind. The outline carries the font's overlap
flags (`OVERLAP_SIMPLE` on a simple glyph's first flag, `OVERLAP_COMPOUND`
on a component).

`coverage` is also pure, and it turns an outline into an alpha mask. For
each pixel it accumulates the exact signed area of the outline's edges per
row, and flattens curves to within 1/32 of a pixel (a quadratic's chords
after n steps stray a quarter of its second difference over n squared,
and 128 steps hold the bound for any curve inside a mask). The fill is
nonzero: inside any winding a pixel is covered whole. An edge pixel two
overlapping contours share sums their areas and reads darker than their
union, as FreeType's smooth rasterizer does, and as there an outline the
font flags as overlapping is covered on a grid four times finer each
way, where each grid pixel clamps before the mask pixel averages them;
masks past 128 pixels on an axis stay unrefined. The Mono Regular and
Bold faces flag no glyph. The resulting mask is at most 512 pixels on an
axis and carries its bearing from the pen and the baseline. A refusal
leaves the caller's mask untouched. Both the rasterizer and the outline
are reused, so a steady state allocates nothing.

### The atlas executor

The draw stream does not change. The seam under "Invariants" holds: a
`Glyph` names a Unicode scalar and a style, never a bitmap. So widgets,
scenes, `driven`'s text read-back and every draw-stream oracle are
untouched by the face. The change is below the seam: `Raster` executes a
`Glyph` through an outline face when it is given one.

Per-frame text cost must not include rasterizing. `atlas` is one 8-bit
coverage page of fixed extent, shelf-packed. Its entries are keyed by face
style and glyph at the page's pixel size, and each entry records its
rectangle and bearing.

The executor looks each scalar up in the face's character map. It covers
the glyph into the page on first use and blends it from the page after
that. A scalar the bold style lacks is covered from the regular one, and
one the face lacks draws from its stand-in (see "Stand-in glyphs") or
the Unifont face, as it does today. When the page is full it resets
whole under a new epoch and fills again from the draws that follow, so
its memory is bounded by its extent. The page reports the band of rows
written since it was last taken, which is exactly the sub-image a GPU
backend uploads.

The blend is `background + (ink - background) * alpha / 255` per channel,
using the `GlyphStyle`'s explicit background. The executor never reads the
buffer back. This keeps the bitmap face's rule that fringe colours derive
from the explicit background a caller paints, and it means a repaint is
independent of old pixels. It is also exactly a GPU fragment shader with
blending off, so a CPU frame and a GPU frame of one draw stream are
pixel-identical by construction. Zero coverage writes nothing, as an unset
bitmap pixel does. Every weight draws the regular style: `Weight::Medium`
is the bitmap face's body text, its fringe thickening a thin face, not a
bold. The bold, italic and bold italic styles, each covered at its own
file's units per em, are reached through `Face::glyph`; the stream has no
bold weight, and td-term, which draws them, reads the atlas directly
(see "td-term"). An entry is valid
in the epoch that placed it, and any miss may reset the page: a GPU
backend holding a frame's entries compares epochs. Each entry's gutter is
zeroed as it is placed, so a sampler filtering across its edge never
reads a retired glyph.

The exact-pixel oracles and `--preview` checksums pin the bitmap backend,
so the executor is opt-in per raster. It paints each glyph in the face's
cell from the draw's origin, so a composition must lay glyphs out on that
cell: a consumer opts in with a face fitted to its grid (see "The grid
fit") or with runtime cells, and its oracles move when it does. At the largest sizes a page holds few glyphs, and a
scene needing more than a page resets it every frame; that is bounded
work, not a fault, and a larger page is the remedy if a consumer meets
it.

### The grid fit

Every widget consumer lays text out on the bitmap face's 8x16 cell,
scaled (td-term, given a font size, lays its grid on the face's own cell:
see "td-term"). Runtime cells (below) would replace that grid; until they
do, `Face::fit`
fits an outline face to it, so a consumer takes the face with no change
to its layout, hit testing or draw stream. The fitted size is the
largest, fractional, whose advance fits the cell's width and whose em
fits its height: the Mono face is 13.3 pixels per em in the 8x16 cell
and 26.7 in the 16x32 one, its advance filling the width exactly, so box
drawing and block elements meet across columns. The advance is centred
across the cell and the line box (ascender to descender, 1.32 em for
this face) down it, the baseline below the cell if a tall line box
leaning up puts it there. What the line box holds past the cell is
clipped with it: box drawing, which spans the whole line box, still meets across
rows, and only the extremes of accents and descenders are lost. A scalar
the face lacks draws exactly as the bitmap raster draws it, since the
cells coincide.

`typeface::Typeface` holds the style bytes once, shared, and fits a face
at the scale asked for, refitting when it changes, so one atlas is held.
`pinned_face::load` reads the regular style from the directory
`face_file::find` finds (see "Delivery and trust position"), the only
style the draw stream selects; the other three styles ship for td-term.
A program calls `load_or_note` at startup with the value of
`TD_UI_FACE`, which it reads as it reads the Wayland endpoint's
variables: `bitmap` keeps it on Unifont without reading anything;
otherwise, on any failure it says once on standard error that the
program draws with Unifont and to run `./install-fonts`, and the program
starts. In-process tests and still-image previews never load it. The
face reaches a window only when handed to it (`window::run`'s typeface,
`Window::with_typeface`), so every in-process oracle and `--preview`
checksum stays on the bitmap face, and each native-compositor harness
starts its live window with `TD_UI_FACE=bitmap`, so its captured frames
match those oracles on a machine that has the face as on one that does
not.

### Runtime cells

`CELL_WIDTH` and `CELL_HEIGHT` are constants because the bitmap face fixes
them. Runtime cells (increment 25) make them a `Cell` value derived from
the face at the surface's pixel size, so a face can be drawn at a size
the grid does not fix:

- the cell width is the rounded advance of `0` (else `M`);
- the baseline is the rounded ascender;
- the cell height is that plus the rounded descender and line gap.

Consumers thread that value through layout, hit testing and painting in
place of the constants. The glyph is rasterized at the device pixel size
(the base size times the integer scale), not drawn at 1x and doubled.

A scalar the face lacks falls back to its stand-in or the Unifont glyph,
centred in the cell. Wide cells (CJK) remain a separate decision;
td-term/DESIGN.md §2 already excludes them from the terminal profile.

### Stand-in glyphs

Some single-cell scalars that TUIs print as status marks and spinner
frames are missing from the pinned Unifont. Unifont draws them 16 pixels
wide, and the single-width import drops those glyphs. JetBrains Mono
Nerd Font lacks them too. Claude Code's `⏵⏵` mode line, its `⏺` and
`⏸`, and its `✢ ✳ ✶ ✻ ✽` spinner are examples.

`font::stand_in` maps each such scalar to a glyph of like shape, for
example `⏵` to `▸`, `⏺` to `●`, `⏸` to `‖` and `✔` to `✓`. The spinner
frames map to `+ * ⋆ * ⊛`, so neighbouring frames stay distinct.

`Face::glyph` tries the scalar in its style and then in regular before
it tries the stand-in, and caches the result under the scalar.
`Font::index` tries the stand-in before U+FFFD. So every td-ui draw
through either face takes the stand-in, not only td-term's. The
compositor's attention prompt draws only what `Font::covers` reports,
and that is unchanged, so no stand-in reaches it. A scalar with no
stand-in draws the replacement box as before.

The table holds only scalars Unifont lacks, so a stand-in never
replaces a real Unifont glyph. Every stand-in is a glyph Unifont
carries and has no stand-in of its own. Tests pin both properties
against the committed face. The pinned JetBrains Mono styles were
probed to carry every stand-in. No test pins that, since no test reads
that face, but an outline face lacking a stand-in still draws Unifont's.
The table reads no other face and is not a fallback chain. Adding an
entry is a reviewed change to that table.

### td-term

td-term is its own crate over the toolkit (`td-term/DESIGN.md`). Its
renderer is td-ui's `vt_render`, which paints cells straight into the
XRGB buffer over the bitmap `Font` rather than as a `Composition`, and
draws each glyph the pinned outline face has through that face, on its
cell (`vt_render::cell_size`): the bitmap font's when the face is fitted
to it, the face's own when td-term is given a font size:

1. At startup td-term loads the four styles through
   `pinned_face::styles_or_note` unless `TD_UI_FACE` is `bitmap`, which
   reads them through `face_file` from the directory its search finds
   and fits them to the 8x16 cell with `Face::fit` and `with_slant`, or,
   given a font size, covers them at it with `Face::sized` on the cell
   the face's metrics give, as runtime cells (above) derive it; on any
   failure it says so once, naming `./install-fonts`, and td-term draws
   with Unifont. Its font chords step the face through `vt_render::Zoom`,
   which resizes it from the bytes it holds.
2. `vt_render::render_with` draws a cell through the face when the face
   has its scalar: the cell's ground, then the glyph's coverage from the
   atlas page blended from the ground toward the ink and clipped to the
   cell, then the rules. The terminal's attributes map as follows:
   - bold, italic and bold italic select those styles through
     `Face::style`, falling back to what the face has;
   - faint and inverse stay colour operations, applied before the blend;
   - underline and strike keep the cell's rows, at the same distances from
     its bottom and middle whichever face draws it.

A scalar the face lacks is the bitmap painter's own glyph, its bold smear
and italic shear included, centred in the face's cell as runtime cells
centre it, and the cursor and the bell are unchanged; a fitted face's
cell is the bitmap one, so there it is exactly the bitmap cell.
The painter reads the atlas page directly rather than through a td-ui
`Composition`: the page and its entries are what a GPU backend uploads
and samples, so the GPU path takes the terminal's glyphs as it takes the
raster's.

The terminal reports no pixel size through `TIOCSWINSZ`: its pixel
fields are zero (`pty::grid_size`), whatever the cell.
The compositor's chrome fits the outline face to the same cell, through
`sfnt`, `coverage`, `atlas`, `face` and `face_file` mounted by path
(td-compositor/DESIGN.md, "Chrome text"); those five modules name only
each other, `font` (the compositor's own, which td-ui mounts) and `std`.
td-recipe-eval mounts them as well, to draw the status bar text its
`qemu-boot-live` screen oracle expects. td-term/DESIGN.md §3's rule that
host tests and the target consume the same face bytes holds for Unifont.
`vt_render_spec.rs`'s PPM oracles stay on `render` and the bitmap face;
the outline painter's oracles use fonts the tests encode (`tests/fonts`,
mounted by path), and the image check is what realizes the pinned face.

### The GPU path

"GPU-accelerated text" means this: glyphs are rasterized to coverage once
and cached in a texture atlas, and each frame draws one textured quad per
glyph, blended in a fragment shader. The atlas executor above is that
model. The draw stream a `Composition` emits is the quad list (one quad per
`Glyph`, at its cell), the page's dirty band is the texture upload, and the
blend is the shader. The
CPU raster is the reference backend and the pixel oracle a GPU backend is
held to.

td has no GPU stack to run that model on. The compositor composites in
software and scans out DRM dumb buffers. Clients present only through
`wl_shm`, and APPLICATIONS.md records `zwp_linux_dmabuf_v1` as absent
("no GPU, nothing to export"). A client's GPU-rendered frame reaches the
screen without a copy only as a dmabuf, which the compositor imports and
then either composites on the GPU or scans out directly. Until then, a GPU
frame is read back into `wl_shm`, which costs more than the mask blend it
replaces.

GPU text therefore needs, in this order:

1. **A driver stack in the target graph.** Mesa built from source is a
   large reviewed non-Rust package: C, C++, meson, Python and, for most
   hardware drivers, LLVM. That needs principle-2 sign-off. A td-owned
   driver is not realistic.
2. **A render-node grant.** Applications reach the GPU only through td-jail
   and the confinement path. GPU drivers are among the kernel's largest
   attack surfaces, so the grant is a threat-model decision
   (APPLICATIONS.md), not plumbing.
3. **The compositor's GPU path.** That means dmabuf import with explicit
   synchronization, and GPU composition or direct scanout in its DRM
   backend. The compositor is where td-term's and every client's pixels
   meet, so it is the first GPU consumer.
4. **td-ui's GPU backend.** It sits below the same seam: the atlas page is
   uploaded by its dirty band, and each `Glyph` becomes a quad.

Until then, the CPU path is already GPU-shaped in cost as well as form:

- a glyph is rasterized once per face and size;
- a frame's text is a bounded blend per cell;
- damage confines frames to what changed: td-term repaints, writes and
  damages only the rows that did (`vt_render::render_changed`,
  `Client::present_changed`).

Measured on the Mono Regular face, every one of its 12608 glyphs parses
and covers in about 30 ms at 16 pixels per em. A frame needs only the
glyphs it shows, once.

## Independently landable increments

The task-manager widget sequence is tracked in
[td-taskmgr's delivery plan](../td-taskmgr/DESIGN.md#validation-and-delivery):
menus, confirmations and resource tabs, followed by charts, a split pane
and a tree table, each with shared-widget oracles and existing consumer
regressions. Those increments extend the original sequence below.

1. Rule and crate: the lock guard admits sibling roster dependencies; the
   crate exists with the input layer, the shared codecs and the cell
   constants. Landed.
2. Raster: `Rect`, `Scale`, `Weight`, `GlyphStyle`, `Primitive`, `Draw`,
   `Surface`, `Composition`, `Raster`, the scrollbar geometry, the text-run
   painter, the palette and the font-licence strings. td-editor's
   `Geometry` and `Scene` compose them and `--preview` stays
   byte-identical. Landed.
3. Transport, in two landings. (a) `sys` (sendmsg, recvmsg, the pinned
   fcntl request) with `UNSAFE.md` §19, and `wayland` with the endpoint,
   connect, `Connection`, pool files, the pointer image and `peer::drain`;
   td-editor's window drives the connection from its own loop and its
   transport tests moved. Landed. (b) `client::Client` with the object
   table, registry, SHM buffers, frame callback, pointer image and the
   turn loop `run` behind the `App` trait, a consumer's own objects held
   in the table under its `Tag`; td-editor is the first `App`, its nine
   presentation tests moved, and the probe consumer in `tests/client.rs`
   drives the client. Landed.
4. Devices, in two landings. (a) The seat, keyboard and pointer inside the
   client: bound and created there, the keymap right consumed there, the
   keyboard's state owned there and its events delivered typed with their
   serials; td-editor acts on the outcomes and keeps its gestures. Landed.
   (b) The clipboard's data-device lifecycle: manager, device, offers with
   their budgets and retirement barriers, and sources, delivered typed;
   td-editor keeps its transfers. Landed.
5. Widgets. (a) The chrome bands td-editor already draws, as `chrome`:
   the menu bar and its panel, the wrapped text block, the tab strip
   and the status row, each with a draw-stream oracle and the status
   band rasterized whole to pixels; td-editor's `Geometry` and `Scene`
   delegate and `--preview` stays byte-identical. Landed. (b) The new
   widgets a scene did not draw, in two landings. (i) `List`, the
   panel's row painter over a scrolling, selectable window with a
   scrollbar and an optional right-aligned column, for the file chooser
   and choice lists, with a draw-stream and a pixel oracle. Landed.
   (ii) `TextEntry`, the single-line field with a caret, selection,
   placeholder, right-scrolling window and a masked mode a consumer
   applies under its own trust rules, for the second and third
   consumers, with a draw-stream and a pixel oracle. Landed.
6. `td-setup`: the installer front end's first page as the second
   consumer, in two landings. (a) The crate, and its `welcome` page built
   from the raster and the chrome bands: a chrome ground, a heading over a
   hairline rule, the wrapped disclosure prose in a `Block` and a `Status`
   footer, disclosing that storage is unencrypted and the account signs in
   automatically with no password or PIN field, per
   td-install/INSTALLER.md; the prose is word-wrapped in the crate, with
   draw-stream and pixel oracles and no live compositor. Landed. (b) The
   Wayland turn loop making it a live `App`. A minimal copy of td-editor's
   native compositor harness launches the client against the real headless
   compositor, reads the tile it is placed in and asserts the captured
   pixels equal the crate's own `preview` of that surface. Landed.
7. td-portal: the file chooser on td-ui, its private handshake and second
   rasterizer deleted, and its recipe converted to stage sibling trees.
   (a) The recipe becomes a generic `Recipe::rust` cargo build that stages
   td-portal beside the sibling trees its `#[path]` modules name — td-secret,
   td-busd, td-compositor, and (through td-secret's sha256) engine — the same
   shape td-net uses, so (c) can add the toolkit by naming one more tree. The
   static-shape and selftest proofs the hand-rolled recipe ran inline move to
   a td-portal-test companion, the split td-ui-test makes for the compositor.
   Landed. (b) The second private Wayland client — the boot channel probe —
   and its `TD-PORTAL-CHANNEL-READY` marker, boot-evidence service unit and
   boot assertion retired; the private registry it pinned moves to the
   surviving dialog. Landed. (c) The file chooser's render on td-ui's raster
   and chrome bands — a `Composition` with a chrome ground, the title over a
   hairline rule, the guest path and selection-status lines, the filter as a
   `TextEntry` and the entries as a `List`, on the shared light palette — its
   own second rasterizer and font mount deleted and its render oracles regolded
   from a byte fingerprint to named palette colours. Landed. (d) Its transport
   a td-ui `App`, the private dialog client deleted, under the native
   compositor harness.
8. Driving, in two landings. (a) The transport lifted from td-editor:
   `control` (frame, envelope, codecs, response lines, `Parse`),
   `control_socket` moved intact, `control_worker` generic over the
   consumer's request type, and `replay::run`; td-editor cut over
   atomically and wire-compatibly, its framing, socket and worker tests
   and their confinement pins moved, its own `Refusal` and worker types
   specialisations of the toolkit's. Landed. (b) The semantic seam under
   "Driving": `driven` with the input event, outcome, `Controller`
   trait, action table and generic verbs, `text` read back from a
   `Composition`'s draw stream and one XRGB-to-PPM writer, proven with a
   toy controller in the crate's tests. Landed; td-photo consumes it
   next.
9. Directory finder, in two landings. (a) `finder`, the shared directory
   finder over a consumer-supplied listing (see "Shared directory
   finder"), with its oracles; td-photo's roll chooser is its first
   consumer. Landed. (b) td-portal's file chooser over `finder`: its
   filesystem model stays (descriptors, no-follow, the bounds and the
   guest-path mapping) and hands the widget a `Listing` per folder, its
   own filter, selection, scroll window and `ChooserView` deleted, the
   multi-select mark added to the widget's `Entry` for the multiple-file
   mode, and its render oracles regolded over the widget's bands. Landed.
10. Button strip: `chrome::Buttons`, a row of bezelled `Button`s on one
    band (see "Shared action button"), the button's text centred in its
    height; td-photo's mode and filter strips are its first consumer.
    Landed; the strip wrapping to more rows on a narrow band, with a
    left and width of its own, landed after (td-photo's develop and
    history bands are consumers too).
11. The cell screen: `screen`, a styled grid as a `Composition` with a
    key vocabulary and chord translation, and `screen_app`, the window
    that presented it and polled a `Handler`, proven with a recording
    handler against a scripted peer. Landed; td-news and td-mail drew
    in it (APPLICATIONS.md §W.8, "Reworked"), each landed in its own
    increment with its recipe, package and unit; both moved off it in
    increment 12 and it is deleted in increment 13.
12. The widget window under "Widget window": `window`, the same loop
    without the grid, whose handler paints into a raster over the
    surface and reads chords, button phases and wheel travel in surface
    pixels, proven with a recording handler against the scripted peer.
    td-news has moved onto it with the toolkit's `List` and td-editor's
    document pane (APPLICATIONS.md §W.8, "Reworked again"), and td-mail
    likewise, reading a message in the pane; td-mail's composing
    increment follows.
13. The cell screen deleted: `screen`, `screen_app` and their two test
    files are gone, with their public surface, their "Cell screen"
    section and their oracles here; the widget window's section stands
    on its own. Landed.
14. Slider: `chrome::Slider`, a knob on a track over `steps + 1`
    positions with the pointer-to-position inverse (see "Shared
    slider"); td-photo's exposure control is its first consumer. Landed.
15. Shortcut hints: the hint face (`hint`), the `Mark` primitive and
    `hint_run`, `Button::emit_hinted` and `Buttons::emit_hinted`, the
    held roles (`keyboard::Held`, `Keymap::held`) reported by the client
    as `KeyboardEvent::Held` and driven as `Input::Held` through the
    `held` verb; td-photo shows its chords while Alt is held. The widget
    window and the other consumers ignore the event (the task manager's
    remote answers `held` ignored). Landed.
16. The clipboard: `clipboard` with td-editor's `Outgoing` and its
    destination owner moved here, under the raw module's two pinned
    `fcntl` status commands, and a bounded `Incoming`; the widget
    window's `Clipboard` with `copy`, `paste` and `Input::Paste`, its
    serial, focus and idle-turn rules the editor's, proven against the
    scripted peer; td-editor's window on the moved writer,
    its own raw module down to two syscalls. Landed. td-mail and
    td-news copy and paste through it in their own increment
    (APPLICATIONS.md §W.8, "Reworked again" and "Composing in place").
    The primary selection, the client's second board under the same
    rules, landed after for td-term, which sets it from a selection and
    pastes it on a middle click.
17. Outline reader and coverage: `sfnt` and `coverage` under "Outline
    faces and the glyph atlas", pure, proven against fonts the tests
    encode, with pixel and closed-form area oracles. Landed.
18. The atlas executor: `atlas` with its page, shelf packing, epochs and
    dirty band; the face's `Cell` at a pixel size; and `Raster` executing
    `Glyph` through an outline face, opt-in, with the Unifont fallback
    and the explicit-background blend, under pixel oracles over encoded
    fonts. The draw stream is unchanged. Landed.
19. The face pin: a data recipe fetching the Nerd Fonts v3.5.1
    `JetBrainsMono.tar.xz` by URL and SHA-256, extracting the four Mono
    styles and the notices into an output the image carries read-only at
    `/etc/fonts/jetbrains-mono-nerd`, with a copy in `static-runtime`
    for jailed programs, and the AGENTS.md amendment naming pinned
    upstream data. Landed.
20. The grid fit: `Face::fit`, `typeface` and `pinned_face` under "The
    grid fit", and the widget window's typeface, which td-news and
    td-mail load at startup, falling back to Unifont with a note. Every
    oracle stays on the bitmap face. Landed.
21. The other consumers on the grid fit: td-editor's window, td-setup,
    the file chooser, td-photo and the task manager load the pinned face
    for their live windows, and their `--font-license` output names
    where its notices are. Landed.
22. td-term on the outline face: its cell painter draws through a face
    fitted to its grid in four styles, with the attribute mapping above.
    Its PPM oracles stay on the bitmap face. Landed.
23. td-term on td-ui: td-term its own crate and static binary over the
    toolkit, out of the compositor multicall: the VT model and its
    corpus, the renderer and its goldens, the terminfo compiler, and the
    PTY with its four ioctl requests (`UNSAFE.md` §19) moved here;
    `vt_keys`, the chord encoder, and the client's `activated`,
    `presented` and `focus_serial` accessors newly built; `reportable`
    and `proc_status` mounted; td-term's window, session policy and
    readiness socket in its own crate, which forbids `unsafe`; the
    `td-term`, `td-term-terminfo` and `td-term-test` recipes. Landed.
24. The host face: `face_file`'s bounded search of the user's and the
    host's font directories after the image's, used by every consumer
    and td-term; `td-builder install-fonts` and its `./install-fonts`
    entry, installing the font recipe's files from its verified pins
    under the XDG data home; and the Unifont note naming it. Landed.
25. Runtime cells: `Cell` replaces the constants in layout, hit testing
    and painting, so a face is drawn at a size the grid does not fix.
    The editor core goes first, since the other consumers lay out over
    its pane; td-term already derives its cell this way when given a
    font size.
26. The GPU path, gated on the sign-offs "The GPU path" lists: the
    compositor's GPU composition first, then client dmabufs, then td-ui's
    GPU backend held to the CPU raster's oracles.
27. The editor core: td-editor's text bounds, model, fill planner, key
    profiles, layout, clipboard capture, dialog permits, controller and
    scene moved here as `editor` and the `editor_*` modules, with their
    refusals as `editor_error` and their tests; td-mail and td-news
    embed the pane through the toolkit alone and drop td-editor, which
    keeps its window, file session, control socket, replay, spelling,
    prompts and preview. Landed.
28. The vault-document policy: `Event::Fillable` and `Event::Clear`,
    `Snapshot::capture_selection`, and find's history moved from
    td-editor as `editor_search`, with their tests in
    `tests/editor_policy.rs`; the codec's line-ending policy written
    down. td-pass's notebook pane is their first user. Landed.
29. The entry and list controllers: `entry_model` and `list_model`, the
    editing and selection state over the `TextEntry` and `List`
    painters, for td-pass's search field and title list first. The
    hand-rolled entries and lists in td-taskmgr, td-setup, td-mail,
    td-news, `finder` and `confirmations` may adopt them, each in its
    own landing. Landed.
30. td-review on the widget window: the integrator's tool paints its
    panes as styled rows through `text_run` and reads the window's
    chords, its state machine on a worker thread with frame-stamped
    inputs, its terminal layer and pager deleted. Landed.
31. td-review's review in the document pane: the preview a frame hands
    the window whole, shown read-only in `editor::Controller::pane`
    with `Scene::inks` colouring its diff lines, scrolled by the reading
    keys and the wheel, selected by drag, word and line, and copied on
    `C-c` through the window's clipboard. Landed.
32. The message list: `messages` under "Shared message list", a chat
    transcript with collapsing, scrolling, selection across messages
    and whole-message and tool-result copy through the window's
    clipboard, with its oracles; td-agent's transcript is its first
    consumer, in that program's own increment.
33. Themes: `theme`'s six palettes over the shared palette's roles,
    `Raster::with_theme`, the widget window's `F12` and the per-program
    file `theme_file` keeps, with td-review's status inks moved into the
    palette. Landed.
34. Themes in the programs with their own windows: `theme_file::Kept`,
    held by the widget window and by td-editor, td-setup, td-photo and
    the task manager, each painting its live window in its theme and
    moving it on `F12`; td-term and the portal's chooser stay in `SAND`.
    Landed.
35. The key list: `keys`, shown by the widget window on `F1` over the
    frame with the handler's sections and the window's own, scrolled and
    closed by its own keys, and opened by a handler's help key through
    `take_show_keys`. Landed.
36. The key list in the programs with their own windows:
    `keys::Overlay`, the list's state, sections and laid-out lines,
    held by the widget window, and the rules a window routes to it by.
37. One spelling and style for the key list, and the list from the
    pointer: `keys::check` and `check_style` over the keymap's own key
    names and the fixed words, descriptions shown as sentences, a left
    press closing the open list (`Overlay::press` and the widget
    window), and the `BUTTON` and `ITEM` labels. Each program's rows,
    check tests and pointer entry follow in its own landing.
