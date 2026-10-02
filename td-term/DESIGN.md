# td-term

td-term is td's terminal: one Wayland window over one PTY, a standalone
program built on td's shared UI toolkit (`td-ui/DESIGN.md`). The terminal's
reusable pieces -- the VT parser and model, its renderer, the keyboard chord
encoder, the terminfo compiler, and the PTY with its threads -- live in td-ui
so that another td-owned program can embed a terminal. This crate holds what
makes them one terminal program: the window and its readiness, the child's
session policy, the pointer and clipboard gestures, and the readiness socket.
This document is the normative contract for the terminal as a whole, td-ui's
terminal modules included, and the starting point for successive agents; the
root `AGENTS.md` and `DEVELOPMENT.md` still govern changes and submission.
The raw surface beneath the PTY is `UNSAFE.md` §19's; td-term itself forbids
`unsafe`.

## 1. td-term boundary and philosophy

Its product reference is foot: one process per terminal, native Wayland,
immediate startup, a quiet interface, and no server process or application
framework. It is not a foot reimplementation and does not inherit foot's
implementation or compatibility claims.

td-term is its own crate, `td-term/`, and its own static binary. Its manifest
declares one dependency, `td-ui = { path = "../td-ui" }`, and its lock lists
exactly td-term and td-ui. It is a td-ui client like td-editor or td-portal's
chooser: the Wayland wire codec, the connection and its SCM_RIGHTS transport,
the object table, the seat, keymap compilation, key repeat, pointer decoding,
the data device, and the clipboard's transfer owners are the toolkit's,
shared with every td-owned graphical program rather than copied. td-term was
once an argv[0] personality of the compositor multicall; it left so that the
compositor carries no terminal and so that the terminal can run under a
Wayland compositor other than td's (§7). The two now share only the sources
td-ui mounts from the compositor tree -- the Unifont face, the wire codec and
the report-text predicate among them -- and the compositor treats td-term as
it treats
any client: its launcher runs `/bin/td-term run` for a terminal window, and
its terminal authority learns that a terminal it launched is up by running
`/bin/td-term probe SOCKET` as a subprocess (§4).

All terminal code is dependency-free Rust built by td's source-built stage2
toolchain. It has no external toolkit, GPU API, dynamic font system, terminal
daemon, configuration language, plugin interface, or external crate. Its
renderer is software XRGB8888 into the `wl_shm` buffers td-ui's client owns.

The implementation has four separable layers:

- a byte-stream parser that emits bounded terminal actions;
- a terminal model that owns grids, modes, cursor, history, and replies;
- a bitmap renderer that converts an explicit model snapshot to pixels; and
- PTY, Wayland, keyboard, and clock adapters around those pure layers.

The parser and model are `td_ui::vt`, the renderer `td_ui::vt_render`, the
keyboard encoder with its scrollback viewport and bounded input queue
`td_ui::vt_keys`, and the terminfo entry `td_ui::vt_terminfo`. The adapters
are `td_ui::pty` (the device, the child, and the reader, writer and waiter
threads), `td_ui::client` (the Wayland client and its turn loop), and
td-term's own `app.rs` (the window), `session.rs` (account, environment and
child command), and `ready.rs` (the readiness socket and its probe).

The parser, terminal model, renderer, keyboard encoder, and terminfo compiler
read no descriptors, sockets, clocks, environment, or global process state;
td-ui's confinement tests pin that of each. Tests can therefore drive every
state transition with explicit bytes, sizes, keys, and time values. Adapter
failures close the affected terminal without corrupting model state.

One process per terminal is load-bearing rather than a packaging choice:
closing a terminal IS exiting its process, which is what lets the PTY's
reader and writer threads have no retirement path but process exit (§4).

## 2. First terminal profile

The first profile is a bounded, keyboard-first ECMA-48/DEC terminal sufficient
for td's shell and userland. It implements:

- streaming UTF-8 decoding with replacement of malformed input;
- a primary grid, an alternate grid, a cursor, tab stops, scrolling margins,
  origin mode, autowrap, and bounded primary-screen history;
- C0 bell as a coalesced visual notification, backspace, tab, line feed,
  vertical tab, form feed, carriage return, shift-in, shift-out, escape,
  cancel, and substitute controls;
- index, next-line, reverse-index, tab-set, save/restore, and reset escape
  operations, plus G0/G1 ASCII and DEC special-graphics designation;
- cursor movement and position, erase in display and line, insert/delete/erase
  characters, insert/delete lines, scroll, margins, tab clearing, and repeat;
- SGR reset, bold, faint, italic, underline in five styles (single, double,
  curly, dotted and dashed, `4:n`), inverse, strike, default colors, the
  16-color palette, indexed 256 colors, 24-bit colors, and the underline
  color (`58`, reset by `59`), each color in semicolon or colon form;
- normal and application cursor keys, primary device attributes, cursor
  position reports, and the replies required by the claimed profile;
- DEC cursor preservation for mode 1048 and alternate-screen mode 1049;
- bracketed-paste mode 2004, initially disabled and cleared by terminal
  reset;
- pointer reporting: tracking modes 9, 1000, 1002 and 1003 and SGR's
  encoding, mode 1006, each initially off and cleared by terminal reset,
  reported as §3 says;
- OSC 8 hyperlinks, which name the cells written in them, followed as
  §3 says; and
- OSC 133;A shell-prompt marks, which §3's prompt chords jump between.

UTF-8 scalars are initially single-cell glyphs. Wide cells, combining
sequences, grapheme clustering, bidi, shaping, and emoji presentation require
a separately pinned Unicode-data design. A scalar neither face draws renders
its stand-in (td-ui/DESIGN.md, "Stand-in glyphs") or, with none, a visible
replacement cell. This limitation is part of the claimed profile rather than
an accidental difference hidden by the test overlay.

Ordinary C0 controls and DEL execute or are ignored without cancelling a
partially received UTF-8 scalar; ESC, CAN, SUB, and malformed non-continuation
bytes retain their parser recovery behavior. SGR parameters take colon
subparameters as well as semicolons: `4:n` an underline style, `38`, `48`
and `58` a color as `5:n` or `2:r:g:b`, or `2:id:r:g:b` with a color space
id that is ignored, as foot ignores it and any fields after blue, an empty
component being 0. A parameter with subparameters it does not take, or a
form it does not know, changes nothing and takes nothing from the
parameters after it; so does a semicolon color whose operands carry
subparameters. Where foot guesses, td-term keeps to that rule: an unknown
or empty `4:n` leaves the underline as it was (foot turns it off), as
does more than one style, an out-of-range `5:n` changes nothing (foot
clamps it to 255), and subparameters void any other parameter (foot
applies it). A colon in any other control sequence drops the sequence
whole.

The initial cursor is steady rather than clock-blinking. Shift+PageUp and
Shift+PageDown navigate scrollback. Ordinary text input returns to the live
bottom. An unmodified End key is consumed for the same purpose while viewing
scrollback and is forwarded in the selected cursor-key mode at the live
bottom. Hyperlinks, images, sixel, ligatures, and shell integration are
deferred, as are the pointer's other encodings (UTF-8's 1005 and urxvt's
1015), focus reports (1004) and the alternate screen's wheel as arrow
keys (1007). Pointer selection and reporting, the wheel, scrollback
search, the core data-device clipboard and the primary selection are
specified in §3. A protocol is not parsed merely because another
terminal implements it.

Unsupported CSI operations are ignored as complete sequences. DCS, SOS,
APC, and PM strings enter allocation-free streaming ignore states and cannot
execute commands or open paths. An OSC's payload is kept, to 4 KiB, and an
OSC 8 or OSC 133;A is acted on when BEL or ESC, the start of ST, ends it;
one that outgrows the bound is dropped whole, and every other OSC is
ignored. CAN and SUB cancel a string. ESC either begins ST or recovers
through the normal escape state. Unsupported input must not leak
printable fragments or desynchronize subsequent supported input.

`OSC 8 ; params ; URI` makes the cells written after it the link's, until
an OSC 8 with an empty URI ends it, as does one whose URI is longer than
2 KiB or holds anything but printable ASCII; an erase drops a cell's link
with it, and neither SGR nor DECSC and DECRC touch it: a link is not
saved with the cursor. Two links with the same URI and the same nonempty
`id=` parameter are one link, however far apart their cells were written;
an `id=` longer than 256 bytes is no id. The model remembers the newest
1024 links in a table whose ids are never given twice, so a cell whose
link was forgotten, or reset away, is no link rather than another one;
once every 32-bit id is given there are no more. The id makes each cell
4 bytes larger, so history's and the grid's byte budgets hold that many
fewer cells.

Resource ceilings are part of the model contract:

- at most 32 CSI parameters;
- at most 1,048,576 history cells, 16,384 history lines, and 16 MiB of history
  storage;
- at most 1 MiB of queued PTY output, 64 KiB of queued keyboard input, and
  64 KiB of queued terminal replies;
- an OSC payload of at most 4 KiB, and at most 1024 OSC 8 links, each
  with a URI of at most 2 KiB and an `id=` of at most 256 bytes; and
- a grid of at most `vt::MAX_DIMENSION` (16,384) rows or columns and 1,048,576
  cells in 16 MiB, on a surface within td-ui's raster ceilings (8192 pixels an
  axis, 32 MiB a frame), each checked before anything is allocated for it.

Exceeding a syntactic ceiling transitions to a sink state that consumes through
the sequence's final byte before returning to ground. A full PTY-output channel
blocks its reader thread, applying kernel PTY backpressure without dropping
bytes. A keyboard sequence is enqueued atomically; if the complete sequence
cannot fit, td-term drops that whole input event and marks the visual bell
rather than truncating stream bytes or closing the session. History evicts
only complete oldest lines. A reply is also admitted atomically; if it cannot
fit, td-term drops that whole reply and marks the visual bell rather than
deadlocking the child's input and output paths. No queue or storage grows
without limit.

The child environment is cleared and reconstructed. A bounded parse of
`/proc/self/status` selects the effective uid (`td_ui::proc_status`, the
parser the compositor's terminal-authority probe also uses), and the matching
unique `/etc/passwd` entry supplies `HOME`, `USER`, and `LOGNAME`; a missing,
duplicate, malformed, or mismatched account closes the terminal before child
creation. Malformed is whole-file, not per-entry: any line without seven
fields or with a non-numeric uid closes it, wherever it sits. td owns this
file, so a line it cannot account for is a system-integrity problem rather
than an entry to skip past on the way to the one being looked up. That file is
the whole account namespace: td resolves no Name Service Switch, so an account
that exists only in LDAP, NIS, or a directory service does not exist for
td-term. This is the same single-local-seat assumption `td-seatd` is built on,
and lifting it is a separately reviewed design change, not a parser change.
The remaining values are `TERM=td-term`, `COLORTERM=truecolor`, `PATH=/bin`,
`SHELL=/bin/sh`, and `TERMINFO=/etc/terminfo`. `XDG_RUNTIME_DIR=/run/user/UID`
comes from the verified numeric uid and `WAYLAND_DISPLAY` carries the display
td-term actually connected to: the socket path it dialled, from `--socket` or
resolved from its own environment, made absolute. An inherited
`WAYLAND_SOCKET` is refused: that descriptor crossed exec without
close-on-exec, so the child would inherit the terminal's own connection. The
stock image uses `/run/td-compositor/1000/wayland-0`; host development carries
its supplied endpoint. Consumers must honor absolute-display semantics
(`Path::join` does); concatenating the runtime and display strings is invalid.
`TD_CONTROL_SOCKET` is passed on, last, only when td-term's own environment
carries it, so a shell in the terminal can reach the compositor's control
channel without naming the socket by hand. Nothing else is inherited:
`pty::spawn` clears the environment, so the child gets exactly this list
(the desktop profile's child inherits instead, §7).
Descriptors are another matter: td-term opens its own close-on-exec, but one
its parent passed without it -- a `WAYLAND_SOCKET` that `--socket` overrode,
say -- reaches the child as through any exec, and td-term, which forbids
`unsafe`, cannot close what it never opened. td's launchers pass `--socket`
and no such descriptor.

The td-owned terminfo entry is compiled by `td_ui::vt_terminfo` from a
human-readable capability source; `tic`, ncurses, and a host terminfo
database are not build inputs. `td-term terminfo PATH` writes it, and refuses
a path that does not end in `share/terminfo/t/td-term`, the only place
ncurses looks for it. The `td-term-terminfo` data recipe runs that verb from
the built td-term into its own `share/terminfo`, because a Cargo build
installs binaries and nothing else, and the system closure exposes that
immutable directory through `/etc/terminfo`; no new top-level image root is
needed. Every boolean, number, output sequence, and input key in that entry
names a blocking native case. A structural test decodes the compiled entry
and compares it field-for-field with the source capabilities.

The encoder emits the legacy binary format rather than the 32-bit-number one,
which exists to carry `pairs#65536`; this entry's largest number fits the
signed 16-bit field, so the older format every reader understands is enough.
The three capability arrays are declared only one past the highest index
claimed, and a reader treats the rest as absent -- which is also why the
pinned `Caps` ordering stops after `setab` instead of continuing through
printer and bit-image capabilities this profile will never claim.

That ordering is the whole trust surface. Position in the name tables IS the
wire index, so a capability written at the wrong one is a well-formed entry
that means something else, and no round-trip through the module's own decoder
can see it -- the encoder and decoder would share the mistake. It is therefore
pinned as ordered lists rather than per-capability integers, so what a
reviewer checks is one list against `Caps`, with the counts and the order's
non-alphabetical joints asserted separately: `kf10` between `kf1` and `kf2`,
`lf10` between `lf1` and `lf2`, and `kf11` after `rfi` rather than after
`kf10`.

Attribution is checked, not merely declared. Key capabilities are compared
byte-for-byte against the sequence `vt_keys` encodes for that key's chord,
because a key is emitted exactly as the entry spells it; the same comparison
runs against the corpus case's own input expectation. Output capabilities are
compared by escape-sequence shape -- introducer, private flag and final byte,
and, for the finals where a parameter SELECTS the operation rather than
counting or positioning, the parameters too -- because a capability spells
the default-parameter form (`\E[A`) of a sequence a case naturally writes
with parameters (`\E[3A`), and demanding the literal bytes would only push
the corpus into writing degenerate sequences to satisfy a test.

Attribution alone is not enough, and the gap is worth stating precisely. It
asks whether the named case exercises an operation; it cannot ask whether that
is the RIGHT operation for that capability. Where a family shares one case the
distinction is the whole point: the cursor case writes all four of
`CSI A/B/C/D`, so exchanging `cuu` and `cud` satisfies every attribution and
ships an entry that moves the cursor the wrong way -- as do exchanging
`il1`/`dl1`, `ich`/`dch`, `indn`/`rin`, or any two of the nine renditions that
share a case. Each such capability is therefore pinned twice more: its
declared spelling must be the same operation as a concrete form written
beside it, and feeding that concrete form to the model must produce the
effect its name promises. A capability that shares a case with another and
has no such check is refused, so the coverage cannot quietly lapse as the
entry grows. The colour capabilities are pinned by expansion instead -- every
branch of `setaf`/`setab` is instantiated and driven through the model --
because a redirected branch still emits a well-formed SGR, just for the wrong
channel.

The entry is reachable at runtime. The child is given
`TERMINFO=/etc/terminfo`, the image's immutable `/etc/terminfo` resolves to
the `td-term-terminfo` output's store `share/terminfo`, and td-jail's
terminal grant binds the one entry for the launcher's `TERM` read-only into a
jail's own `/etc/terminfo`, where it requires the file at mode `0444` -- the
mode a bind reads from the store file itself. The build step's own mode does
not reach the image: every tree the builder stages is copied writable by its
owner, and a NAR restore writes `0644`, so the image's mode-fixing step sets
`0444` on the packed entry and the root-tree check refuses an image whose
entry is anything else, naming the launches it would refuse. The terminal
applications' first boot is what found the requirement: both windows closed
on td-jail's refusal of a `0644` entry, and nothing short of a booted jail
runs that readback.

What the entry omits is as deliberate as what it claims. `cols`/`lines` are
absent because td-term sets and verifies the PTY winsize before the child
starts, so the pre-winsize fallback they exist to serve is unreachable by
construction. `smir`/`rmir` are absent because this profile implements no ANSI
insert mode, `Smulx` and `Setulc`, which advertise the underline styles and
color, because they are extended capabilities and td's compiler writes no
extended section (a child that sends the forms anyway has them drawn), and
`blink`/`invis` because it has no SGR for either -- an entry that claimed them
would be describing a terminal td-term is not. `bel` is absent for a different
reason: BEL sets the model's coalesced visual-bell bit, which no corpus
observation can see; only a frame shows it (§3), and a capability whose
case would be a fiction is worse than a missing one.

An outer `TERM=foot`, `TERM=linux`, or other value describes the parent
terminal and is never an oracle or a capability claim for td-term. An
optional developer check may ask a pinned host `infocmp` to decode the
generated entry, but neither that tool nor its result participates in the
required gate. The check remains green when the optional host tool is absent.

## 3. Font, keyboard, and rendering

The first implementation pins one licensed PSF2 bitmap font with a Unicode
table: GNU Unifont 16.0.04, single-width, 8x16, 20673 glyphs. It is the one
bitmap face every td-owned program draws with: it is committed as generated
Rust source in `td-compositor/src/font_data.rs`, read by
`td-compositor/src/font.rs`, and mounted by td-ui as `td_ui::font`; its
license texts and upstream provenance are under `td-compositor/assets`, the
archive hash in `PROVENANCE`.

That face is derived rather than downloaded, which the provenance record and
`td-compositor/tools/import-unifont.rs` exist to make reproducible: upstream
publishes no full-coverage PSF2, only an APL-specific PSF1. The importer pins
the upstream `.hex` by hash and takes only its single-width records, which is
not a narrowing of Unifont so much as the only thing PSF2 can express -- one
fixed cell for every glyph -- and matches §2 making double-width cells a
deliberate first-profile exclusion. It also excludes the two jiskan16 files
COPYING carves out of the dual license, by construction rather than by
choice, since both are 16x16. Host tests and the target recipe consume those
same bytes; no host font lookup or fetched-only test input participates. The
PSF2 reader checks headers, dimensions, glyph counts, table bounds, scalar
validity, and all pixel arithmetic before use.

td-term also draws in td-ui's pinned outline face (td-ui/DESIGN.md,
"td-term"). At startup it loads JetBrains Mono Nerd Font's four styles
through td-ui's `pinned_face::styles_or_note` from the first directory
td-ui's bounded search finds: `/etc/fonts/jetbrains-mono-nerd` on td's
image, and on another host the user's or the host's font directories,
where `./install-fonts` puts the pinned face (td-ui/DESIGN.md, "Delivery
and trust position"). By default it fits them to Unifont's 8x16 cell, so
the grid and every rule stay Unifont's. The desktop profile's
`--font-size POINTS` (§7) instead covers them at that size, in foot's
points: 96 dots per inch at scale one, so `--font-size 9` is foot's
`size=9`, 12 pixels per em. The face's own metrics then give the cell,
as td-ui's `Face::sized` derives it -- the rounded advance wide, the
rounded ascender, descender and line gap tall -- and the grid, the
cursor, the rules, hit testing and the fallback grid are laid on that
cell (`vt_render::cell_size`); JetBrains Mono at 9 points is a 7x16
cell. A scalar the outline face lacks, a missing or refused face, and
`TD_UI_FACE=bitmap` all draw from Unifont as below, Unifont's glyph
centred in a cell of another size and clipped about its middle in a
smaller one, an odd difference trimming the right column or bottom row
once more; a missing or refused face is one `td-term: outline face
unavailable` line on stderr, which names `./install-fonts`, and with
`--font-size` a line says the size went unused, since the grid is then
Unifont's (the only line under `TD_UI_FACE=bitmap`, which says nothing
else). The Unifont bytes above take no part in that search. No test reads the
pinned outline face: the outline painter's oracles use fonts the tests
encode, and every other oracle renders from Unifont.

foot's font chords zoom the outline face: Control with `+` or `=` covers
it half a point larger, Control with `-` half a point smaller, and
Control with `0` returns to the size td-term started at -- fitted to
Unifont's cell again, when it started there. Each is the chord exactly,
so an added Alt is another chord, and none is a repeat candidate. They
are td-term's whether or not there is a face to zoom, and none was ever
the child's: the encoder's table (below) sends nothing for Control on
any of those characters, so taking them costs the child no byte, and
like the copy chord none clears the selection. Without an outline face a
font chord does nothing. A step goes on past a size whose cell would
move against it in either axis, up to four half points: a face fitted
to Unifont's cell clips its taller line box, so the face's own cell half
a point smaller can be taller than the fitted one, and zooming out would
show fewer rows. A size past the face's bounds (6 to 256 pixels per em)
leaves the face as it is. A zoom resizes the face from the bytes it
already holds, reading nothing, and lays its new cell's grid on the same
surface: the next frame adopts it as it adopts a resize (§4), setting
and verifying the PTY's winsize before the model reflows and the pixels
move, or touching neither when the grid is the one they have. The
window keeps its size, as a tiled one must.

The renderer gives every claimed rendition a deterministic presentation from
the bitmap face; through the outline face, bold, italic and bold italic
select those styles, and faint, inverse, underline and strike are as
here. Bold adds a clipped one-pixel rightward copy of set glyph bits,
faint blends foreground halfway toward background with integer channel
arithmetic, and italic applies a bounded row-dependent one-pixel shear.
Strike and a single underline draw fixed clipped cell rows, and inverse
exchanges foreground and background. A double underline adds a second
rule two rows above the first; a curly one is a triangle wave from the
cell's last row up two units and back, a unit being a sixteenth of the
cell's height and at least a pixel; a dotted one alternates unit-long
dots and gaps; a dashed one leaves the cell's last quarter bare. The
wave and the dots run on the surface's x, so they meet across cells. An
underline color draws the underline alone, as it is; without one the
underline takes the drawn foreground, the strike always does. Blocking
PPM cases prove that each claimed attribute differs from an otherwise
identical normal cell.

The fixed palette is what foot, which td-term replaces, draws: foot's
own sixteen base entries since 1.15, the starlight table (`242424`, `f62b5a`,
`47b413`, `e3c401`, `24acd4`, `f2affd`, `13c299`, `e6e6e6`, then `616161`,
`ff4d51`, `35d450`, `e9e836`, `5dc5f8`, `feabf2`, `24dfc4`, `ffffff`),
and the remaining 240 xterm's, as foot's are, computed from the
arithmetic that defines them -- the six-level cube on 0, 95, 135, 175,
215, 255, then the grey ramp from 8 in steps of 10 -- so those entries
cannot drift from their own definition. Default ink is a seventeenth and
eighteenth colour, `dcdccc` on `222222`, the foreground and background
the user's foot.ini sets; like foot's, neither is a palette entry, so
`SGR 39` and `SGR 49` restore them and no index names them.
Faint follows inverse rather than preceding it: after the exchange the
drawn foreground is the one to dim, and blending before it would brighten
an inverse-and-faint cell instead.

The cursor is a presentation of that same exchange. Focused, it is its cell
drawn with inverse toggled, so a cursor over an already-inverse cell reads
as the surrounding text. Unfocused, it is a hollow one-pixel box in the
cell's foreground, leaving the glyph legible underneath: present, but not
claiming the keyboard. That box is the same colour as the glyph it rings, so
over a cell whose border pixels are all set -- `U+2588`, and some
box-drawing -- an unfocused cursor is invisible. That follows from drawing it
in the cell's own foreground and is accepted for the first profile, where an
unfocused terminal has nothing to locate; a focused cursor is never affected,
since exchanging the ink is visible against any glyph. A pending wrap does
not move either, because the model already reports the column the cursor
still occupies. Focus here is the toplevel's `activated` state, which td-ui's
client reports from the last applied configure: a configure that only takes
activation away still redraws the frame, at the same size, with the hollow
cursor.

The renderer consumes a complete terminal snapshot, a fixed palette, focus
state, and cursor state. It performs no allocation in the cell loop
beyond the outline face covering a glyph into its atlas on first use. A full
redraw is acceptable for the initial profile. td-term draws only when td-ui's
client can present, so rendering is coalesced to the latest state behind the
frame in flight, and the buffer rules are the client's: a submitted buffer is
reused or mutated only after its `wl_buffer.release`, at most three stay live,
and a resize paints into a replacement while the old buffer waits for its
release. Every glyph and decoration is clipped to the surface before pixels
are visited, and a surface smaller than the grid renders its visible corner.

C0 BEL, an atomically dropped keyboard event, or an atomically dropped reply
sets one coalesced visual-bell bit in the model. The renderer presents that
bit as the inverted one-pixel ring inside the surface, and a blocking PPM
case pins that presentation. Setting the bit marks the picture stale; the
next frame td-term submits takes the bit and inverts the ring, and every
frame for 100 ms after it keeps the ring, a bell taken meanwhile putting
that end forward. A frame that cannot be submitted, every buffer held,
puts the bit back for the next. When the flash is over, each turn's wait
from the ring frame's on having been cut to end there, the next frame is
drawn without it. Bells between two frames are one bell, and a frame
waits for the one in flight as any frame does, so a ringing child queues
no frames of its own.

Keyboard input goes through td-ui's keymap rather than through a td-term
key table. td-ui's client binds the lowest `wl_seat` of version 5 or newer,
capped at 7, and td-term refuses a compositor that offers none, because
`repeat_info` is a version-4 event and the timings below are read out of it.
The compositor's keymap arrives as a descriptor the client reads positionally
at offset zero, so reading one SCM_RIGHTS duplicate cannot advance the shared
open-file-description offset seen by a restarted or second client, and
td-ui's bounded XKB text-v1 compiler compiles it whole before any press
translates (`td-ui/DESIGN.md`, "Invariants"; `UNSAFE.md` §19). A keymap the
compiler refuses before the child starts closes the terminal: a terminal that
started its shell with no way to type into it would not be a terminal. After
that, a refused map is logged and leaves the keyboard without one until a
later map compiles. td-term is keymap-independent: td's own map, or any other
the compiler accepts, has already chosen the character or named key a press
means and which modifier roles are left over, and hands td-term a chord --
`C-`, `M-` and `S-` in that order, each at most once, then the character or
the key's name (`a`, `C-c`, `M-S-a`, `S-Tab`, `F5`). A modifier the keymap
gives no role, Super among them, makes the keymap refuse the press, so an
untranslated chord such as `Super+q` reaches the child as nothing rather than
as `q`.

`td_ui::vt_keys` encodes a chord. `action(chord, modes, viewing)` returns
exactly one of three things -- bytes for the child, a move of the scrollback
viewport, or nothing -- because a key that scrolls must never also send
bytes. The table is exhaustive:

- a printable ASCII character is itself; Shift beside a letter without
  Control means the uppercase letter, the keymap having already applied
  Shift and Caps Lock everywhere else;
- Control gives the C0 byte of the character the keymap chose: `@` and Space
  give NUL, letters give 0x01 through 0x1a, `[ \ ] ^ _` give 0x1b through
  0x1f, and `?` gives DEL; Control on any other character is silent;
- Alt (`M-`) prefixes the whole resulting sequence with ESC; an Alt chord
  spells its letter lowercase, so the case sent is the one the keymap
  resolved (`Stroke::text`, Caps Lock included), not the chord's;
- Escape is ESC, Backspace is DEL (`0x7f`), matching the slave's
  Linux-default canonical `VERASE`, and Return is CR; Shift passes through
  all three, since they have one spelling at both levels and real terminals
  send CR for `Shift+Enter` and DEL for `Shift+Backspace`;
- Tab is HT and `S-Tab` is `CSI Z`, Tab being the one fixed key with a defined
  second spelling;
- the arrows, Home and End are `CSI A/B/C/D/H/F` normally and
  `SS3 A/B/C/D/H/F` under DECCKM;
- Insert, Delete, PageUp and PageDown are `CSI 2 ~`, `CSI 3 ~`, `CSI 5 ~` and
  `CSI 6 ~`;
- F1 through F4 are `SS3 P/Q/R/S`, and F5 through F12 are `CSI 15 ~`, `17`,
  `18`, `19`, `20`, `21`, `23` and `24 ~`;
- `S-PageUp` and `S-PageDown` move the viewport back and forward and reach
  the child as nothing; End while the viewport shows scrollback returns it to
  the live bottom (Control on `+`, `=`, `-` and `0`, also silent here, are
  the font chords above, which td-term answers before the table); and
- Control on any named key, Shift on a named key other than Escape,
  Backspace, Return and Tab (and the two paging chords), and every name the
  table does not list are silent.

Those rules exclude things deliberately. Control reaches printable keys only,
and only where a C0 spelling is defined; the character it maps is the one
Shift already selected, so `Ctrl+Shift+6` needs no second rule to reach `RS`.
Alt prefixes ESC uniformly rather than folding a modifier into a CSI
parameter, because this profile does not claim the modified-key encodings
such a parameter implies, and a sequence it does not claim would be
indistinguishable to the child from one it does. Shift on an arrow or a
function key is therefore silent, and `S-PageUp` and `S-PageDown` belong to
the scrollback viewport rather than to the child. Keys the keymap names but
the table does not -- Print, Pause, Menu, and the media keys -- are silent for
the same reason. The acceptance is two-sided and needs no byte-exact keymap:
td's own keymap compiles, and each chord encodes per the table, which the
encoder's unit tests pin and the corpus's `key` cases exercise through td's
keymap end to end (§5).

The terminal mode that picks between two spellings is read from the model
for each key, because a child's reply to one key can change how the next is
spelled. Key repeat is td-ui's: the client applies the compositor's
`repeat_info` -- a rate in keys per second and a delay in milliseconds, a rate
of zero being the protocol's "do not repeat" -- through its explicit-clock
repeat policy. td-term arms a key the keymap marks
repeatable after the key did something; a release, any modifier change, focus
loss, a keymap change, and a republished rate of zero retire it, a new
nonzero rate retimes it rather than dropping it, and repetitions missed while
the loop was busy are dropped rather than delivered as a burst. A repetition
is routed when it is emitted, not when the key went down: the child can
change cursor-key mode while an arrow is held, and a stored sequence would
keep sending the spelling that was correct at the press. A held chord that
scrolls repeats as a scroll, since walking back through history is what
holding it is for, and a repetition that routes to nothing ends the repeat.
Routing per repetition is also what makes a held End coherent: the first
repetition closes the viewport and the ones after it are the child's,
because by then the view is at the live bottom. The turn loop's wait is
capped by the armed key's next repetition, so a held key is not polled for.

Keyboard bytes reach the PTY writer through a bounded queue that admits a
sequence whole or drops it whole: half a `CSI` arriving at the child would be
worse than the key never having been pressed, so an overflowing queue rings
the visual bell instead of truncating. The writer consumes only what the
kernel accepted, so a partial write leaves the remainder queued; keystrokes
have nowhere to come back from. Because the master is blocking, a child that
stops reading blocks that writer once the line discipline fills -- which is
why the writer is its own thread and the main loop only enqueues. Terminal
replies take the same queue, one reply at a time.

`S-PageUp` and `S-PageDown` move the viewport a screen less one row at a time,
so the line last read is still on screen to read on from, and a one-row grid
still scrolls by one rather than not at all. Both stop at the ends: there is
nothing above the oldest retained line and nothing below the live screen, so
a chord at either end is inert rather than an error.

The viewport stores the line it is looking at in a monotonic numbering of
lines ever pushed to primary history, not a distance from the live bottom,
because the bottom moves. A stored distance would let a child writing
underneath an open viewport drag the view along with it, one line per line
of output. Clearing that history -- a reset, or `CSI 3 J` -- retires the
numbering along with the lines, so an anchor is tagged with which numbering
it belongs to. Zeroing the count alone would not close the view: the old
anchor's line number comes back around as new lines arrive, and the view
would reopen on lines that have nothing to do with it.

The distance the anchor implies is clamped on every read rather than stored,
because eviction drops the oldest lines and a resize can shorten the history
an anchor lives in. An anchor whose line has been evicted rides the top of
what history still holds, rather than being thrown back to the live bottom:
that is where the reader was heading, and on a full buffer the alternative
moves the view on every further line of output. Riding the top is therefore
the end of what scrolling back can reach, and a retired numbering is the
only thing that returns a view to the live bottom without a key. A silent
key does not re-anchor at where a clamp put the view -- the anchor still
names the line asked for. End's two meanings follow that position rather
than whether the viewport was ever opened, since what it asks is whether
anything but the live screen is showing.

The renderer's half of the viewport: a snapshot carries how many lines back
it is scrolled, rows above the split come from the primary history and the
rest from the live screen, and a request deeper than the stored history
clamps rather than blanks. History is primary-screen only, so the viewport
reads it even while the alternate screen is active -- which is what lets it
show the shell a full-screen program is covering. A line is stored at the
width it scrolled off with, so a widening resize leaves the tail of an old
line blank rather than fabricating cells for it. While the view is open the
cursor is drawn where the shift puts it, and stops being drawn once that
pushes it past the bottom: the renderer shifts the live screen and the cursor
by the same offset, so neither needs a special case.

A WHEEL moves the same viewport, and reaches it by a second route rather than
through `Action`: that enum is what one KEY PRESS does, and a notch arriving
as one would be a third thing a key could mean. td-ui's pointer `Wheel`
accumulates a frame's axis events into whole rows: three a detent when the
compositor sends discrete steps, because a wheel is turned in flicks -- a page
a notch overshoots, and a line a notch makes crossing a screenful a dozen
turns -- and, for a smooth source that sends no steps, one row for each cell
height of travel with the remainder carried, so a trackpad scrolls in
proportion rather than a notch per event. A `wl_pointer.frame` is what
APPLIES the accumulated rows, not each axis event: the frame is the
transaction, so a tilting wheel moves the view once and repaints once for one
flick. The horizontal axis is accumulated and discarded, since a terminal has
no sideways scrollback and counting the two together would send a sideways
flick up the history.

Pointer selection is an inclusive row-major range over the visible snapshot.
A left-button press anchors it at the pointer, motion while the button is
held extends it, and release retains it; `wl_pointer.frame` applies the
accumulated transaction so one physical report causes at most one repaint. A
plain press selects nothing until its drag leaves the pressed cell, so a
click without motion clears the selection, as foot's does. Left presses at
one cell within 500 ms of each other, by td-term's clock as each is
dispatched and with no other button's press between, are one gesture: the
second selects the word under the pointer, the third its row, and a fourth
starts over. A drag after either extends a word or a row at a time, the
pressed one kept whole whichever way it goes. A word is a run of cells of one
class -- blanks, foot's default `word-delimiters` (`` ,│`|:"'()[]{}<> ``), or
cells that are neither -- so a run of delimiters is one word, as in foot. A
row is the line: every row the terminal wrapped it across, so far as the
view shows them, as foot's is. A word goes on across a wrap too, while
the next row starts with a cell of its class. The model records which
rows an autowrap ended, on the screen and in history, and the view reads
the mark from where it reads the row (`Snapshot::wrapped`). A mark stands
only while its row still reaches the edge as written into the row that
follows: erasing to the last column, inserting or deleting characters,
replacing the row it went on at (rows inserted, deleted or scrolled
beneath it, or the row scrolled away from a region's last row) take it
away, and so does a change of width, which without reflow pads or clips
the row. The newest history line's mark goes when the screen's first
row is cleared whole or replaced without being pushed to history, and a
history line marks a wrap only at the width it was stored at. All of
this
is td-ui's (`vt_render`'s `Snapshot::span`, `Snapshot::select` and
`Snapshot::wrapped`). A followed link (below) counts toward no gesture.
Leaving the surface abandons a drag still held and forgets the press count; a
drag released past the edge comes as release, leave and frame, and that frame
still finishes it. Reverse drags normalize only when text is copied. The
renderer inverts every selected cell. Resizing, new terminal output, viewport
movement, or a key press other than the copy and paste chords clears the
range and schedules a repaint; a drag still held re-selects from its press
point at its next motion, so the range it shows is over what is on screen
then. A bare modifier is not a press the keymap reports, so the Control and
Shift needed for the copy chord cannot erase it first. A selected row the
terminal wrapped runs on into the next with nothing between them and its
cells kept to the edge, so a wrapped line copies as the child wrote it;
any other row loses trailing ASCII spaces and is followed by one newline,
and the text ends with no trailing space.
The text is bounded at 64 KiB; a longer one rings rather than being cut.
A range that trims to no bytes is a no-op: it neither replaces the seat
clipboard nor emits a zero-byte success marker.

A left-button press with Control and no other modifier (Caps and Num Lock
ignored), read from the keyboard's synchronized modifiers while td-term has
focus (td-ui's held roles), reads the link under it as soon as it is
dispatched, from the viewport's row by td-ui's rule (`links`), and only
while the screen shows the model: a model changed since the last committed
frame, a resize's reflow among the changes, or a committed frame for a
changed model whose callback has not yet said it reached the screen, is not
what the person saw, and the press is then a plain one. A screen under
continuous output never shows its model unchanged, so there a Control-press
is always plain. Output or a wheel later in the same frame cannot change the
link read. A press past the drawn grid reads none, though a selection's
clamp would reach the edge cell. The frame that closes the press decides:
with a link it opens through td-ui's opener (`open`) and the press selects
nothing, its drag and release ignored, so the selection a copy would take
stays unless output clears it, as output clears any selection; without one
it is a plain press. The link is read from the cells, not from what they
look like, so text whose foreground is the colour of its background is part
of it: what opens is what the row holds, which may be more than the person
can read. A link that cannot be opened is a `td-term: open link:` line on
stderr and rings the visual bell (§3). A cell holds one scalar, so the row's
text is its cells in order; a link the terminal wrapped onto the next row is
found only up to the row's end.

A cell written in an OSC 8 link the model remembers, whose URI is an
`http://` or `https://` URL (`open::is_url`), is that link, whatever the
row's text around it spells: a Control-press there follows the URI,
through td-ui's URL opener (`open::url_on`), and a link the text holds is
not read. An OSC 8 link with any other scheme, with a backslash, which a
browser reads as `/`, or with a `/` after the scheme's, which it skips,
so that the host it opens is not the one the status line shows, is no
link, and the cell's row is read as text as ever, so a child cannot make
a press open a file or a program through it. Because the URI need not be
what the cells say, hovering such a link shows it on a status line,
`link: ` and the URI, over the view's last row, or its first while the
link has a cell on the last and the pointer is not on the first. A URI
too long for the row loses its path to an ellipsis, and then its
authority's head, so that the end of the host, its registrable name,
stays shown. A Control-press on a row a status line covers, in the frame
on the screen or the one in flight to replace it, follows nothing: what
is under it is hidden.

While the pointer is over the surface and Control alone is held, the
link a press there would follow is ruled (`Snapshot::with_link`): a
one-pixel line on the underline's row across each of its cells, black on
a light ground and white on a dark one, whatever the cells' own colors,
so the person sees how far the link runs before following it, the cells
they cannot read included. Nothing is ruled where a press would not
follow a link: while the child takes presses as reports, or while a
search, which the press would end, is open. The link's cells are part of
the frame td-term wants, beside its size and activation, not a change to
the model, and a frame drawn for the model the screen already shows
(nothing marked stale, no resize) is taken as showing it before its
callback comes: holding Control, letting it go, or moving onto or off a
link draws a frame, and a Control-press meanwhile still reads the link.
A resize reflows the cells, and its frame rules the link where the
reflow put it.

The browser is td-term's one child outside the session child's rules
(§4): it is `BROWSER`, else `xdg-open`, found on td-term's own `PATH`, run
directly with the link as one argument and never through a shell, in
td-term's working directory, with td-term's own environment less
`WAYLAND_SOCKET` and with `WAYLAND_DISPLAY` set to the socket td-term
dials, made absolute (a relative `WAYLAND_DISPLAY` would be resolved under
`XDG_RUNTIME_DIR`), so it opens on the display the terminal is on; the
session child is told the same absolute path. Its streams are
`/dev/null`, every descriptor td-term holds is close-on-exec, and a thread
reaps it; each followed link holds one such thread until its browser
exits, at the rate a person clicks. td-term's environment in the image
carries no `BROWSER` and the image has no `xdg-open`, so until
APPLICATIONS.md §W.6's `OpenURI` opener lands a followed link rings the
bell there; in a session whose environment names a browser it opens.

The child can ask for the pointer (§2). Setting tracking mode 9, 1000,
1002 or 1003 replaces whichever was set, and resetting the one that is set
turns reporting off; resetting another does nothing, as in foot (xterm
turns reporting off on any such reset). While a mode is set and the view
is the live screen -- a view scrolled back into history shows cells the
child cannot address -- a press of the left, middle or right button is
the child's, not a selection, a paste or a followed link, unless Shift is
held or td-term's own drag is under way: Shift keeps a gesture td-term's,
as foot's selection override does, so text can still be selected over a
program that takes the pointer. A press the child took is its gesture to
the end, so neither side sees a button stuck down: the release is the
child's whatever is held or shown by then, or absorbed if the child has
since stopped asking, and leaving the surface releases every button the
child was told is down, at the last cell it was told of. Motion is
reported once each cell the pointer enters, at the live screen: under
mode 1002 or 1003 while a reported button is held, Shift or not, and
under 1003 with none held, but not under Shift; never during td-term's
own drag. A dropped motion report does not ring. The wheel is the
child's too, unless Shift is held, the view is scrolled back, or mode 9
is set, which reports presses alone and no wheel, as in xterm: a wheel
press for every three rows, a smooth wheel's remainder carried to the
next frame (and dropped when td-term keeps the wheel or the pointer
leaves), at most ten a frame, rather than moving the view. Reports are
`vt_keys::report`'s: the button 0, 1 or 2 for left, middle and right, 64
and 65 for the wheel up and down, and 3 for motion with no button and
for an X10 release, motion adding 32 and Shift, Alt and Control 4, 8 and
16 (none under mode 9); X10's encoding is `CSI M` and the button, column
and row each plus 32 in one byte, the cell one-based, which cannot name a
cell past the 223rd and then sends nothing, as foot does; SGR's is
`CSI <` and the three in decimal ending in `M`, or `m` for a release,
which keeps its button. Every report takes the keyboard's queue, admitted
whole or (but for motion) rung for. The entry's `kmous` is X10's prefix,
`\E[M`, which is what tells ncurses the terminal reports the pointer as
xterm does; ncurses then sets mode 1000 itself.

`C-S-c` -- the chord exactly, so an added Alt makes it another chord -- is a
td-term command, not PTY input and not a repeat candidate. With a selection
it needs a live data device and keyboard focus, or it rings. It offers the
selection through
td-ui's client, which creates a core `wl_data_source` advertising
`text/plain;charset=utf-8` and `text/plain` and sets the selection at the
key event's compositor serial, destroying the previous source. td-term keeps
the text behind the live source; the source's `send` hands td-term one owned
descriptor, which td-ui's `clipboard::Outgoing` writes nonblocking against a
five-second deadline, restoring the descriptor's status afterwards. One send
is written at a time: a send arriving while one is in flight, or for a source
no longer live, drops exactly its descriptor, which the receiver reads as EOF
without blocking the Wayland reader or the terminal loop. The compositor's
cancel drops the text; the data-device manager's removal cancels any transfer
and drops it too. The client's offer and source budgets, retirement barriers,
and drag handling are td-ui's (`td-ui/DESIGN.md`, "Invariants").

`C-S-v` requests the selection's text only while td-term has keyboard focus,
a live data device, a selection offering a text MIME, and no paste already in
flight; otherwise it rings. It is neither PTY key input nor a repeat
candidate. td-ui's `clipboard::Incoming` makes a private socket pair, the
client passes one end to the offer's `receive`, and td-term drops its copy of
that end at once. The reader takes at most 64 KiB against a five-second
deadline from the request, in bounded nonblocking steps the turn loop runs at
least every 50 ms while it is pending. A focus leave, keyboard loss, a
replacement selection, or the data device's release cancels the transfer, and
a completed one is admitted only on an idle turn -- so a focus leave or a
selection change already queued is seen first -- and only while focused.
These checks use focus and selection events observed by the client; a
transfer accepted by the compositor is not retroactively revoked before this
client learns of a change.

Only complete valid UTF-8 is admitted. Control characters other than tab, CR,
and LF are refused, including ESC that could terminate a bracketed paste.
No newline or Enter is appended. Mode 2004 wraps nonempty text in the
standard `CSI 200~` and `CSI 201~` delimiters; otherwise bytes pass
unchanged. The whole encoded paste must fit the 64 KiB PTY input queue or none
is admitted and the terminal rings its visual bell. Empty text is a no-op.
Successful nonempty paste clears the visual selection and returns to the live
viewport. Transfer, encoding, and queue failures leave the terminal usable.
No terminal escape sequence reads or writes the host clipboard.

The primary selection is td-term's where the compositor offers
`zwp_primary_selection_device_manager_v1`, as sway does and td's compositor
does not; td-ui's client binds it beside the clipboard. When a release
finishes a selection that holds text, that text becomes the primary
selection at the release's serial, through a source advertising the same two
MIMEs, and its sends are written as the clipboard's are, one at a time, and
silently once written: other programs read the primary selection at every
middle click. A click that selected nothing, a selection over the 64 KiB
bound, and a drag still held offer nothing, and output that clears the shown
selection leaves the offer standing. A middle press pastes the primary
selection under the paste's rules above -- focus, a live device, a text
offer and no paste in flight, the same bounds, admission and bracketing --
and without them does nothing, with no bell, as a middle click on nothing
does elsewhere. A change of the primary selection cancels its paste and not
the clipboard's, and the reverse; focus loss cancels either. `C-S-c` and
`C-S-v` stay the clipboard's.

`C-S-r`, foot's scrollback-search chord, opens a search, which takes
every key until it ends and sends the child none; outside a search `C-r`
is the child's as ever. The search line covers the view's last row in
inverse video (`Snapshot::with_status`), or its first while the match is
on the last, and shows `search: ` and the query, or `search (no match):
` when a nonempty query has found nothing, and hides the cursor. Text
without Control or Alt extends the query, to at most `vt::MAX_QUERY`
(256) scalars, past which a key rings; `Backspace` shortens it. A
changed query looks for the nearest match at or before the one shown, or
the last one shown when a dead end left none, so a match that still fits
stays where it is, and from the newest text when none was shown. `C-r`
or `C-S-r` steps to the next older match and `C-s` or `C-S-s` to the
next newer, ringing and keeping the one shown when there is none
further. A key that edits or steps repeats while held, its repeats taken
by the search as its press was, until a ring stops them. `Return`,
keypad Enter's name too, ends the search with its match brought on the
view, selected and made the primary selection, or with no match the
selection as it was; `Escape`, `C-g` or `C-c` ends it with the view and
the selection put back as they were. A pointer press ends it with the
selection put back and the view where it is, so the press acts on what
it was made over. The match shows as the selection does, inverted, cut
to the part on the view; the frame shows it rather than the selection
the search began with. A match on the alternate screen shows only at the
live view, since a view scrolled back shows the primary's history. When
the whole match is not on the view, the view scrolls its first row to
about mid-view, or as low as still shows its last. The text searched
(`Terminal::search`) is the active screen with, while that is the
primary, its history: one line per line the child wrote, its wrapped
rows joined by their marks, so a match can span a wrap, each row read
only as wide as the screen. The alternate screen is searched alone, at
the live view, as foot's has no scrollback. A query with no uppercase
letter matches either case, each scalar folded on its own as foot's
`towlower` does. Matches are placed in a line numbering output does not
shift (history line `n` is `pushed - lines + n`, screen row `r` is
`pushed + r`), so a match stays put as lines scroll. After output or a
resize, a match whose cells no longer spell the query, or whose wrap has
gone (`Terminal::still_matches`), is dropped; a clear that renumbers
history or a switch of screen drops it and the last place too, since
both name a line of text that went.

A shell that marks where its prompt starts with OSC 133;A, with or
without parameters (foot's shell integration), marks the next cell
written (`Attributes::prompt`); no other OSC 133 mark is kept. Writing
over the cell keeps the mark, since a line editor redraws its prompt
without a new one, as foot's row mark survives; an erase drops it with
its cell, and a reset or a screen switch drops one pending. foot's
prompt chords scroll the view between them: `C-S-z` puts the nearest
marked row above the view's first on top, and `C-S-x` the nearest below
it, or returns the view to the live screen when that row is on it
(`Terminal::prompt`, in the search's numbering). Neither reaches the
child, `C-z` and `C-x` remain the child's, and the selection goes as
with any key. A chord rings when there is no prompt that way, on the
alternate screen, whose programs are not the shell, or when the view is
already where it would go; one that moves the view repeats while held,
each repeat a jump, never sent to the child, until one rings.

The system image's input proof drives td-term's clipboard end to end, and
td-term's half of it exists only when the exact `td.firefox-input=1` kernel
token is on `/proc/cmdline`, read once at startup and bounded at 4 KiB. Under
that token td-term arms no key repeat and prints, each once per condition:
`TD-TERM-CLIPBOARD-FOCUS-READY serial=N` when the keyboard has entered with
serial N and its modifier state is synchronized;
`TD-TERM-CLIPBOARD-TARGET-READY rows=R columns=C row=Y column=X bytes=7`
when the exact word `Welcome` is visible on the live screen in a presented,
current frame; `TD-TERM-CLIPBOARD-SELECTION-READY bytes=7` when the selection
is exactly that word and its highlighted frame is presented; and, after a
copy, a `wl_display.sync` whose callback prints
`TD-TERM-CLIPBOARD-READY bytes=N` only if the new source is still current
after the compositor has processed `set_selection`. That marker proves source
admission, not that another client pasted the bytes. The writer separately
prints `TD-TERM-CLIPBOARD-SENT bytes=7` only after it has written the exact
`Welcome` payload to the requested endpoint; its endpoint close is the
transfer delimiter. Normal boots retain ordinary repeat behavior and execute
none of the proof scan.

The image's fixed QEMU flow focuses td-term, waits for its focus marker,
clears the screen, physically types `Welcome`, and waits for the target
marker naming the live viewport's settled grid and cell coordinates. It
selects those cells, waits for their highlighted frame to become visible,
then injects the copy chord and waits for the exact seven-byte source marker.
Firefox browser chrome must first report that its URL bar is focused after
the physical focus and `Control+L` sequence. One continuous bounded chrome
session stays live after that acknowledgement, admits the physical paste
chord, and permits one through four events to tolerate bounded TCG key-repeat
timing. If the first command boundary exposes only empty paste data while the
selected URL remains unchanged, one exact guest marker admits one retry
command. Across the at most eight events, every nonempty value must be the
exact bytes. Because Firefox can expose empty event data while its default
action asynchronously consumes the Wayland transfer, the URL bar must contain
one or more exact payload copies, no more than all paste events and no fewer
than events exposing exact data. A harmless Shift tap follows each paste
command, and Firefox must observe that ordered keyup before classifying the
command; there is no timing sleep or success before an unobserved chord. The
image proof requires both td-term records, so neither source admission alone
nor a synthetic chrome assignment can pass it.

The same token-gated QMP session continues from the clipboard acknowledgement
to the authenticated page's fixed download link. Firefox content validates and
focuses that link before td-jail emits the arm marker; only then does the host
send one Enter chord through the ordinary keyboard path. The page records one
through four trusted key events under TCG, suppresses every default action
after the first, and requires exactly one trusted link activation. A later
physical Shift keyup terminates that command, so success cannot precede a
delayed repeat. This compositor input evidence is necessary but not
sufficient: the root-owned image unit publishes completion only after a
separate unprivileged td-jail probe, outside the application namespace,
validates the exact regular file and bytes at the source of Firefox's
writable Downloads grant. Ordinary boots retain neither the fixture link nor
any of these listeners or markers.

The Firefox package's reviewed `GTK_USE_PORTAL=1` environment forces the same
portal backend in ordinary and autotest launches. The session first exposes a
full-viewport focus control. One physical pointer click must focus the
document and reach its one-shot refocus listeners. A fresh bounded, read-only
Marionette session validates that persistent record and current Firefox
focus, then closes before the host sends physical `Control+O`. Firefox's
native Open File command issues the broker-authenticated FileChooser request
without a DOM picker call or synthetic file assignment. The portal validates
Firefox's bounded native filter list and admits its selected `All Files` glob
and matching current filter, renders that bounded label, and returns the
selected filter. Non-current filters are validated compatibility metadata
rather than selectable UI; selecting richer filter semantics remains a typed
refusal. The portal reports its first frame only after the private manager
acknowledgement, keyboard enter, shm release and frame callback. At that
boundary the host captures the virtio display through QMP, requires the
centred portal client geometry and chooser background, panel and
selected-row palette, and reconstructs the client XRGB bytes from that
rectangle to match the portal's announced checksum. Only that pixel proof
admits physical Enter. Firefox content must then load the exact granted
`file:` URL as `text/plain` with the download fixture's exact bytes. The
root-owned input unit writes its atomic completion record only after the
portal service's next captured exact success line in its volatile bounded log
and a fresh result-only Marionette session. The focus-evidence poll closes
before the native command and supplies no input or DOM mutation. A hidden
dialog, synthetic DOM assignment, stale portal line or response that never
reached Firefox cannot pass. This machinery remains gated by the exact
input-test boot token.

## 4. PTY and process lifecycle

After mounting devtmpfs and before graphical services, the system creates
`/dev/pts`, mounts devpts there with
`newinstance,ptmxmode=0666,mode=0620,gid=5`, removes devtmpfs's existing
`/dev/ptmx` node, and creates the relative `ptmx -> pts/ptmx` symlink. The
image pins `CONFIG_UNIX98_PTYS=y` and its existing `tty` group owns gid 5.
td-term opens `/dev/ptmx` with safe `std` file operations and `O_NOCTTY`,
unlocks it, and obtains the slave as an owned descriptor with `TIOCGPTPEER`
and `O_RDWR | O_NOCTTY | O_CLOEXEC`. No `/dev/pts/N` path is resolved. The
image proof pins the startup mount command and re-checks `mode`, `gid` and
`ptmxmode` out of `/proc/mounts` on the booted machine, in the kernel's own
`%03o` spelling rather than the mount's -- the `mode=0620` asked for comes
back as `mode=620`, so a check written to match what was passed would red
every correct boot, and rootcheck is a gate; it does not require
`/proc/mounts` to echo the modern kernel's accepted no-op `newinstance`
token. The effective SLAVE gid and mode are proven by opening one, which
lands with the client that opens the first pty.

That sequence is one `td-init` applet rather than four sysinit lines. Three
of the four would otherwise be uutils `mkdir`, `rm` and `ln` reached at
absolute paths, with nothing tying them to the boot that needs them. It
composes the mount as the argv the `mount` applet parses rather than calling
`mount(2)` itself, so flag composition stays in the one module td-init's
confinement tests allow it in -- and this mount needs no `MS_*` bit at all,
since every option it passes is filesystem data. It adds no syscall, so it
is not an amendment to `UNSAFE.md`.

It reads its own mount back out of `/proc/mounts` before relinking
`/dev/ptmx`, which is why the sysinit line comes after `/proc` rather than
beside the devtmpfs mount: an option devpts does not know makes the mount
fail outright, so what a readback catches is a known option that took a
DIFFERENT value than the one asked for, and nothing distinguishes that until
a pty is opened. Each option is matched as a whole comma-separated token, so
`mode=620` cannot be satisfied by `ptmxmode=620`, and the expected spellings
are derived from the ones passed rather than restated beside them. The
instance `ptmx` is checked too -- character device, mode 0666 -- since it is
mode 0000 on a mount that dropped `ptmxmode`. Relinking requires a value only
that verification returns, so the order is the compiler's to enforce, and it
is a rename rather than an unlink and a create, so a failure cannot leave the
machine with no `/dev/ptmx` at all. A second run is refused rather than
served: devpts stacks, and an instance mounted over a live one hides every
pty the first is serving while every check still reads healthy.

The symlink is the setup the kernel's own devpts documentation describes.
It is not that a `/dev/ptmx` device node would allocate from the initial
instance -- modern kernels resolve a `pts` directory beside the node and use
that mount -- but that the link makes this instance the answer explicitly
rather than resting on a sibling-directory lookup nothing checks. `mode=0620`
is likewise the tty convention rather than a relaxation: owner read/write and
tty group WRITE, which is how anything reaches a terminal it does not own,
where the devpts default would be 0600 owned by group root.

Stable Rust does not expose the required PTY operations. td-ui's raw module
carries `ioctl(2)` pinned to exactly five requests, each at its own wrapper
with the request never a parameter, and `UNSAFE.md` §19 is their normative
record:

- `TIOCSPTLCK=0x40045431`, to unlock the slave;
- `TIOCGPTPEER=0x5441`, to obtain the slave as a new owned descriptor;
- `TIOCSWINSZ=0x5414`, to publish rows and columns;
- `TIOCGWINSZ=0x5413`, to verify every published size before it becomes
  visible to the child; and
- `TIOCSCTTY=0x540e`, with `setsid(2)`, in the pre-exec hook that makes the
  default shell lead a session on the slave (below).

td-ui's confinement tests pin the request values and the wrappers, with
`td_ui::pty` the only caller of the four device wrappers and of the session
hook's installer, that only for a child that leads a session, each at one
site. This setter applies only
to the terminal's newly created PTY; it does not weaken the separate
repository prohibition on resizing an operator's terminal. td-term forbids
`unsafe` and names no raw layer: its confinement tests refuse `unsafe`,
`from_raw_fd`, `as_raw_fd`, `libc`, and inline assembly in its sources, and
refuse process creation in its session policy, which composes a command for
`pty::spawn` rather than spawning one.

The wrappers use a four-byte native-endian `int` for `TIOCSPTLCK` and an
eight-byte `[u16; 4]` of native-endian rows, columns, and the two pixel fields
for both winsize requests. The array rather than a `#[repr(C)]` struct
because the language guarantees that layout, which turns the field ORDER into
an ordinary tested function: a swapped rows/columns pair is a well-formed
resize to a different size, and an attribute nobody can observe would not
catch it. The pixel fields are published as zero; a terminal here publishes a
character grid. The kernel never receives a pointer to a temporary or shorter
object. `TIOCGPTPEER` receives the open flags as an immediate value rather
than a pointer, and the flags are pinned in td-ui's `sys.rs` rather than
chosen by a caller, so `O_NOCTTY` cannot be forgotten by the one call site
that must not acquire the terminal. Its nonnegative return is adopted once,
at the raw module's one adoption site, as an owned `File`; the slave is
never looked up by name, which is the property the peer request was chosen
for.

No termios construction, signal syscall, process creation, or descriptor
duplication enters that unsafe surface. The slave's kernel defaults provide
canonical input and echo. Safe `Command` and `Stdio` operations wire three
slave clones to the child.

When the compositor declines to choose a size, the terminal falls back to a
grid rather than to a rectangle: 80 columns by 24 rows, multiplied out by the
cell the grid is laid on, since that is what a terminfo entry and anything
drawing a box assume when they cannot ask. A cell so large that the grid
would pass the raster's ceilings (8192 pixels an axis, 32 MiB a frame)
falls back to as many columns, then rows, as fit within them, at least
one of each. Each axis declines independently: a zero
axis in a configure keeps the size the surface already has on that axis.

td-term's default child is `/bin/sh` leading a new session whose
controlling terminal is the slave. Safe `Command` cannot call `setsid(2)`,
so td-ui's `spawn` does it for a `ChildCommand` with `leads_session`: a
pre-exec hook in the forked child, after `std` has put the slave on
descriptors zero to two, issues `setsid(2)` and then `TIOCSCTTY` with
argument zero on descriptor zero -- two raw syscalls on td-ui's raw surface
(UNSAFE.md §19), nothing that allocates or locks -- and an error from either
fails the spawn rather than starting a shell with no job control. That
failure is td-term's, in `start` before readiness is published, and its
message names the session; the wrapper it replaced failed inside a child
already started, which then exited. The claim never steals: a terminal
another session holds is refused. This replaced
td-term's use of td-init's `cttyhack --stdin`, which did the same from a
second exec, because a terminal that runs on a host without td-init has no
`/bin/cttyhack` to name. A `--command PROGRAM [ARG...]` on its own command
line ends td-term's flags and is exec'd exactly as given and, under td's
profile (the desktop's every child leads, §7), leads NO session: the slave
is its stdio, and it starts in td-term's session with no controlling
terminal. The session exists for a shell, which expects a
controlling terminal it does not create; a program that wants one names
`/bin/cttyhack --stdin` itself, as td-authd's launch does, and a td-jail
terminal application must not, because the jail's terminal grant
(`devices=tty`, its own increment in APPLICATIONS.md §C) acquires the
terminal inside stage 1's detached session, and the kernel refuses
`TIOCSCTTY` for a terminal another session already holds. The consequence
for a child that
never acquires the slave is stated here because a unit author would
otherwise discover it: the slave then belongs to no session and has no
foreground process group, so the kernel generates NO terminal signals for
it -- no `SIGWINCH` when td-term resizes it, no `SIGINT` for `^C`, no
`SIGHUP` when the terminal closes. Such a child must read its window size
itself and notices the hangup only as `EIO` on the slave. A jailed terminal
application is unaffected, which is the case `--command` exists for; an
unjailed program that wants those signals names the wrapper. An explicit
program is an absolute path, refused at argument parsing before td-term
dials the compositor; the constant shell path is checked when the child
command is assembled. td-term has no PATH to search for it (the browser
a followed link starts is the exception, §3), and its argv after
`--command` is bytes rather than text, since a filename argument is whatever
the filesystem holds.

The child starts in the verified account home by default: setting `HOME` does
not move a process, so without an explicit working directory the shell would
start wherever td-svc left the graphical service and disagree with its own
environment. `--working-directory PATH` accepts one absolute path before
`--command`; failure to enter it fails the spawn rather than silently landing
in `/`. The paired authority never accepts that path from its caller: its
typed task-terminal request derives `/home/NAME/src/td-vm/work` from the
validated primary account. `pty::spawn` consumes the slave and its two
clones, so once the child exists td-term holds no slave descriptor, only the
master and the reader's and writer's duplicates of it. Closing the master
produces the kernel's normal PTY hangup; child exit ends the terminal.

The terminal ends when the child's output has run out AND the child has been
waited for, since the two race and ending on either alone drops the parting
output or names no status. td-term then prints
`td-term: the terminal's child exited with status N` (or that a signal
killed it) and exits. A `--command` child that ends badly, by a non-zero
status or a signal, first leaves its last screen in the log: the rows of the
active screen at exit -- whichever screen that is, and however far back the
reader had scrolled, because the program's last words are there and not in
the history -- with trailing blanks and the blank rows below the last written
one dropped, one `td-term: last screen (<program>): ` line each, the program
being the command's final path component bounded at 32 characters, in one
buffer written once and beginning on a fresh line, since a writer sharing the
console may have left one unfinished. The prefix means no row can begin a
console line, which is what the line-anchored boot markers require, short of
a write the kernel cuts and td-term resumes, the residue every console writer
shares; it does not defend the markers the boot oracle latches as
substrings, so the oracle blanks those records, from the prefix wherever it
stands in a line to the line's end, a co-writer's residue on that line going
with it, which can lose a latch but not forge one, before its latches read
the console, because a marker on a jailed application's screen is not
evidence. A character the shared report-text predicate (`td_ui::reportable`)
would not print in a title is a space here for the same reason. A program
that leaves the alternate screen before dying shows the primary screen,
which is what the window showed. The window closes with the session, and
what td-jail or the program wrote there was otherwise on no log; the first
boot of the terminal applications ended in two windows that had shown a
refusal nobody could read. The default shell reports nothing, because a shell
exits with its last command's status and a logout after a failed command is
not news; a clean exit reports nothing; and the output is bounded by the
grid. The screen may hold what the user typed, and the console it goes to is
trusted by image configuration today (td-login/THREAT-MODEL.md); under
principle 7's target trust model a screen copied to a shared console is a
disclosure, so this is scoped to the current model rather than a permanent
grant.

The PTY reader thread (`pty-output`) owns a master descriptor and parks in
`read` whenever the child is idle, and safe `std` cannot interrupt that:
there is no poll, no read timeout, and closing a descriptor another thread is
reading is not something this crate may express. Its only retirement is the
child's exit closing the last slave. That is sound because td-term is one
process per terminal: closing the terminal IS exiting, process exit closes
the descriptor, and the kernel then sends `SIGHUP` to the session holding the
slave as its controlling terminal -- the default shell's, or the one a
td-jail terminal application acquires. A bare `--command` child holds no
such session and sees the hangup only as `EIO` on the slave, so its
retirement is its own exit, which is the same path one step later. The
consequence is a contract rather than a mechanism -- a teardown path must not
join that thread -- and interrupting the reader for any other reason requires
a separately reviewed wakeup surface. Because nothing joins it, the reader
reports its own ending, clean hangup or fault, on the loop's channel rather
than only through its join handle; the child waiter (`pty-child`) does the
same with the exit status, and a failure to start the waiter kills and reaps
the child rather than leaving a live process holding the slave.

The writer (`pty-input`) differs, but less than it first appears: it parks in
a condition-variable wait rather than in a syscall, so closing the keyboard
queue retires it and its handle IS joinable -- for a writer that is waiting
for bytes. Closing sets the predicate the writer checks BETWEEN writes; it
does not interrupt one, and nothing safe cancels a blocking write. A child
that never reads does not by itself park the writer: in the kernel's default
canonical mode the line discipline accepts and discards rather than blocking,
and the tests cover that case against a live child that reads nothing. A
child in RAW mode that stops reading is the case that parks it, and that is
every shell and editor. The child's exit does not free such a writer either
-- the last slave closing hangs up the reader, which is the reader's whole
retirement, while the writer stays parked in `write` on the same terminal at
the same instant. So the teardown rule is the reader's rule: td-term ends a
terminal by exiting the process, not by joining either thread, and joining
the writer is for a writer known to be idle. Because the writer can therefore
die unobserved, its failure is recorded where the main loop meets it: a push
after the writer is gone is an error rather than the bell §2 rings for a full
queue, since a terminal beeping at every keystroke would be reporting the
wrong thing forever. One bounded queue serves both ends -- the main loop
admits a sequence whole or drops it whole and rings the bell, and the writer
drains it -- because a second buffer downstream would be a second place for
half a sequence to sit. Bytes are copied out under the lock and written
without it, so a child that has stopped reading parks the writer in `write`
without ever delaying an enqueue. Only the writer consumes, so a partial
write's remainder stays at the front in order however much arrived meanwhile.

td-ui's turn loop is the main loop. The Wayland connection, the PTY reader, the
writer, the child waiter, and the readiness listener surround it; the reader
and waiter send on one channel bounded at 128 read chunks of 8 KiB, the 1 MiB
PTY-output ceiling, and wake the loop through the connection's waker after each
send, so output reaches the screen without the loop polling for it. A full
channel blocks the reader and lets the kernel PTY buffer backpressure the
child. One turn takes at most 16 chunks from the channel and, if more wait,
makes its next wait 1 ms, so a child writing as fast as the reader reads cannot
hold the turn from the keyboard that could interrupt it or from the frame that
shows it. The clipboard's transfers have no threads: td-ui's owners are
nonblocking and stepped by the loop, so a receiver that stops reading cannot
backpressure input, rendering, or the protocol reader, and its deadline ends
it. The main loop alone mutates the terminal model and writes Wayland requests.
No correctness condition relies on poll, elapsed sleeps, or scheduler order;
the startup deadline bounds failure detection rather than ordering state
transitions.

The PTY is opened before the first frame, so a machine whose devpts is
missing fails without drawing a window. Once td-ui's client has bound its
globals, td-term sets the title `td terminal` and app id `td-term` and
commits the empty toplevel, the required initial commit before any buffer.
Every
configure is bounded before anything is allocated for it -- a surface with no
area, an axis past 8192 pixels, or a frame over 32 MiB closes the terminal --
then acknowledged. A configure whose size needs no new frame is applied with
a bare `wl_surface.commit`, since an `ack_configure` takes effect on the
surface commit that follows it; a chosen tile equal to the fallback is
exactly that case. Before painting at a new size td-term derives the exact
cell grid, sets and verifies the PTY winsize, and then reflows the model, so
the child learns the grid before the pixels move. A new size whose grid is
the one the PTY and model already have touches neither: a reflow resets
the scrolling margins a child set, which nothing about the grid asked
for. A surface smaller than one font cell uses a logical 1-by-1 grid
whose pixels remain clipped to the actual surface. Later configures
preserve horizontal overlap without reflow.
On primary-screen vertical shrink, blank tail rows disappear first; otherwise
top rows move to primary history so the lowest content and cursor survive.
The alternate screen discards removed rows, and resizing the hidden grid
never adds history. Growth appends blank rows to both grids.

Readiness is a frame the compositor chose, presented, at a terminal that can
be typed at. td-term starts its child only when all of these hold:

- some configure has chosen a size: zero is a declined axis, and a configure
  choosing one axis has chosen;
- a frame has been drawn at the size the surface now holds and for its
  current activation, and the PTY and model were last set for that size
  too -- an adopted size whose frame never presented does not count;
- that frame's callback has fired AND its buffer has been released, which
  mean different things; and
- the seat still offers a keyboard, and its keymap compiled.

Reaching that frame takes TWO frames on td's compositor, and that is the
protocol rather than a retry: it cannot tile a surface it has not mapped, so
its first configure is zero in both axes, presenting at the client's own
fallback is what maps the surface, and the tile arrives in the configure that
follows. The keyboard half is a precondition rather than a parallel errand: a
terminal that started its shell before knowing what a key MEANS would take
its first keystrokes against no map at all. Keys struck between the surface
mapping and the child existing are type-ahead the writer drains once it runs.
The seat's LATEST capability is what counts, since a seat may withdraw what it
announced; td's own server announces keyboard and pointer once at bind and
withdraws neither, so this bounds another compositor rather than describing a
state this one reaches. It is a startup gate only -- a keyboard withdrawn after
readiness is not a reason to end the session.

Once ready, td-term resolves the account, composes the child command, spawns
the child, and starts the waiter, reader and writer, in that order; only then
does it publish the readiness socket and print
`TD-TERM-READY rows=R columns=C` on stdout in one locked write, so a probe
told the terminal is up is never told so about a terminal whose shell never
started. Readiness not reached within 20 seconds of the loop's start closes
the terminal, below td-svc's 30-second ready timeout, so the supervisor
hears the terminal's own reason. One encoder produces both the diagnostic
and the socket's answer, since the integration test compares them and two
spellings could drift while each stayed plausible. A readiness line is parsed
fail-closed and order-pinned -- no sign, no leading zero, nothing after the
grid -- and its grid is held to the same definition the winsize ioctl is: a
line describing a grid no terminal could have been set to is not readiness.
The terminal refuses to publish a grid its own probe would reject.

The readiness socket is the `--ready-socket` path, bound mode 0600. A socket
a dead terminal left behind is replaced, while a live one or a path that is
not a socket is refused. A listener thread (`td-term-ready`) answers every
caller with the line under a five-second write bound, so a caller that never
reads cannot stop it answering others, and gives up only after 64
consecutive failed accepts. The path is unlinked when the terminal's loop
ends; the listener, parked in `accept`, retires with the process.
`td-term probe SOCKET` connects, reads at most one line's 39 bytes against an
absolute four-second deadline -- inside td-svc's five seconds per probe, so the
probe reports its own timeout -- and prints the terminal's line unaltered.
The probe requires a ready state and nonzero, internally consistent rows and
columns; its output and the matching `TD-TERM-READY` diagnostic are compared
in tests. The stock image's `[terminal]` unit runs `td-term run` as the
graphical user through `/bin/td-login exec-primary`, with
`TD_CONTROL_SOCKET` in its environment, `--socket` naming the compositor's
display and `--ready-socket /run/user/UID/td-term-ready`; its `ready=`
command runs `/bin/td-term probe` on the same path the same way. The
compositor's terminal authority runs the same probe for the terminals it
launches. The compositor and serial recovery greeter remain independently
restartable.

## 5. Native terminal corpus

td-term behavior is specified in one td-native text corpus in td-ui. Imported
and td-authored cases use the same format and live together by subject:

```
td-ui/spec/vt/
  README
  parser.term
  cursor.term
  editing.term
  wrapping.term
  modes.term
  color.term
  replies.term
  input.term
  resize.term
  unicode.term
  libvterm-0.3.3.term
  libvterm-0.3.3.report
  expectations.txt
  LICENSE.libvterm
```

The corpus runner is `td-ui/src/vt_spec.rs`, compiled only as `vt.rs`'s test
module, which embeds every file above; an inventory test holds the embedded
list to the directory.

The visual oracle is two-tiered, and only the lower tier is built. Goldens
that pin the renderer itself live in `td-ui/spec/vt_render/`, driven by native
Rust fixtures in `td-ui/src/vt_render_spec.rs` that name a snapshot, a
surface size, focus, and a cursor directly; they are what the renderer's own
landing proves. The upper tier -- a corpus case rendering its own final grid
through an `expect ppm` statement -- does not exist yet. A corpus case cannot
render until the parser below it also carries a surface size and a focus
state, which is a corpus format change rather than a renderer one.

The model starts with a small td-authored seed corpus. The bulk migration
converts a source archive and SHA-256 pin of the MIT-licensed libvterm 0.3.3
suite. A sibling license file retains the complete upstream copyright and
permission notice. The archive and original harness do not enter td's build
or repository. The dependency-free Rust importer, `td-ui-import-libvterm`
(`td-ui/tools/import-libvterm.rs`), accepts an explicitly supplied source
tree, verifies its source-file manifest (`td-ui/tools/libvterm-0.3.3.sources`),
rejects every unknown source command or assertion, and emits deterministic
native cases. Its migration report counts source files, cases, assertions,
converted assertions, and every intentional exclusion. The landing records
those counts and reasons.

Each derived case retains its source release, path, and original case
identity. The conversion targets externally observable cells, cursor, modes,
history, properties, and replies rather than libvterm callback names. After
the migration the native cases are normative and maintained with td-authored
cases; provenance remains even when a derived case is clarified. There is no
separate upstream test directory or legacy-format reader in the blocking
corpus or target artifact; the developer-only importer is the reproducer.
Pinned cases are classified against the first-profile feature matrix:
upstream-positive tests for deferred protocols are exclusions, not product
xfails, excluded sections roll back to their last reset, and retained cases
never replay deferred control sequences. Primary DA is normalized from
libvterm's identity to td's. The pinned report still excludes libvterm's
two colon-separated color cases, as classified when they were imported,
though td-term now takes the form; td-authored color cases pin it.

The std-only importer remains a non-shipped developer provenance tool, not a
runtime or build reader. Its unit tests and committed complete source
manifest exercise the upstream parser without the archive; when an explicitly
supplied tree is available, its `check` mode verifies all source hashes and
reproduces the committed corpus and report. The corpus runner checks the
committed migration against its own report and the engine's SHA-256. No
upstream-format case runs in the gate.

The native language has stable case identifiers and a deliberately small
vocabulary:

- `case`, `source`, `tags`, `size`, and `end`;
- `write`, `resize`, `key`, and `pointer` operations; and
- `expect` statements for rows, imported text and glyph observations, cells,
  cursor, modes, cumulative terminal replies, cumulative keyboard input,
  history, the scrollback viewport, and an optional rendered PPM -- the last
  of these deferred, as above. Cursor expectations accept only the optional
  `pending-wrap` flag.

Every case has a source. td-authored cases use `source td`; derived cases name
the pinned release, path, and original case. `size` is rows followed by
columns, byte strings use Rust-like ASCII escapes, and cursor coordinates are
zero-based. A representative case is:

```
case wrapping/right-margin
source "libvterm-0.3.3:t/20state_wrapping.test:right margin"
tags core wrapping
size 2 5
write b"ABCDE"
expect cursor 0 4 pending-wrap
write b"F"
expect row 0 "ABCDE"
expect row 1 "F    "
expect cursor 1 1
end
```

Byte literals use one specified escape syntax and reject ambiguous or invalid
escapes. Row expectations are shorthand for default single-width cells; cell
expectations state scalars, colors, and attributes explicitly. Imported
character-only observations use `text` and `glyph`, which deliberately ignore
rendition absent from the source oracle. Replies are ordered byte strings.
The parser rejects unknown fields, duplicate stable identifiers, empty cases,
assertions before initialization, and expectations that escape the declared
grid. Reply expectations name the complete byte stream emitted since case
initialization. Input expectations separately name the keyboard encoder's
complete generated byte stream, making `key` operations observable before the
PTY writer merges the two bounded sources.

A `key` operation names modifiers (`ctrl`, `alt`, `shift`, `caps`, `super`)
and one key by its evdev code's name. The runner compiles td's own keymap --
the one td's compositor publishes, read from its source -- with td-ui's
keyboard compiler, translates the press to a chord through it, and hands the
chord to `vt_keys::action` with the modes and viewport the case has reached.
A `key` case therefore observes the whole path a press takes on td, keymap
included, and a press the keymap refuses (Super held) or a key the encoder
does not translate contributes no bytes rather than being a corpus error.

A `pointer` operation names what the pointer did (`press`, `release` or
`motion`), modifiers and a button joined as a key's are (`ctrl+left`,
`wheel-up`, `none` for motion with nothing held), and the zero-based row
and column, and runs `vt_keys::report` at the pointer reporting the case
has reached. Its report joins the same input stream a `key`'s bytes do,
so an `expect input` observes both, and a pointer event the mode does not
report contributes nothing.

Feature tags distinguish deliberate profile exclusions such as
double-width cells from missing behavior inside the first profile. An
exclusion's reason is the one the pinned migration report recorded, and a
later landing does not rewrite it: the imported libvterm pointer cases
still read "mouse input is outside the first profile", as its selection
cases still read that selection is, because the importer does not convert
libvterm's pointer calls to the `pointer` operation. td-authored `mouse`
cases carry the pointer claim instead. A generated
`expectations.txt` records in-profile known failures by case and expectation,
so another observation cannot regress behind an existing failure. Every
in-profile case still runs. An unlisted failure, unexpected pass, stale entry,
unmatched case, unknown tag, or malformed corpus reds the gate.

Every byte-stream case runs as one write, one byte per write, at every
two-piece split, and under deterministic pseudorandom chunkings. All forms
must produce identical cells, cursor, modes, history, and replies.
Deterministic arbitrary-byte cases additionally enforce total parsing,
resource ceilings, valid cursor/grid relationships, and absence of panics.

The committed native expectations are the blocking semantic oracle. No host
terminal or external emulator runs in the gate. `$TERM` is only a capability
label. Foot remains a product reference and an optional black-box comparison,
not the normative state model.

## 6. Visual and end-to-end proof

The pure renderer's blocking visual oracle is exact P6 PPM output. Selected
cases render with the pinned font, palette, surface size, focus, and cursor.
Those five are parameters rather than defaults, so no case can be green
against a face or a palette it did not name. A mismatch reports the first
differing coordinate and writes an actual image plus a high-contrast PPM
diff beneath the build's temporary output; no PNG encoder or image library
is required. The cases are Rust fixtures today and native corpus cases once
the corpus format carries a surface, per §5.

Exactness is the contract in both directions: a golden whose bytes differ
from what the encoder emits fails even when it decodes to identical pixels,
because the only thing that could produce one is a hand-edit, and a
hand-edited golden is no longer an oracle. Goldens are generated by the
renderer, so what makes them evidence is not their provenance but the
structural assertions beside them -- that each rendition differs from an
otherwise identical normal cell, that bold only adds pixels and each added
one is a step right of a set one, that italic's every top-half pixel is its
normal neighbour shifted one column, that underline and strike are exactly
one full row each, and that each underline style lights exactly its
pattern, in its own color when it has one. Those are what a wrong renderer
fails; the goldens are what a CHANGED one fails. The committed set is
exactly the set the cases render.

A compositor-level gallery -- td-term against a real td-compositor with a
file-backed framebuffer, comparing the compositor's exact final XRGB8888
frame, including tile geometry, borders, clipping, buffer replacement, and
frame-callback lifecycle -- is the target pixel-parity gate for the shipped
stack. It is not built; until it is, the window's lifecycle is proven by
td-term's scripted-peer tests and the booted image, below.

Foot comparison is a separate, non-blocking developer operation. It uses a
pinned foot binary, font, configuration, fixture, geometry, and isolated
headless Wayland environment to produce side-by-side captures. Different font
and rasterization stacks make exact cross-terminal pixels a false contract;
the gallery adjudicates taste and exposes behavioral disagreements for a
native semantic case to settle. The required check remains green when these
optional host-side comparison tools are absent.

The proof is split the way the code is. td-ui's suites prove the reusable
terminal, and td-term's prove the program:

- the native corpus is structurally valid, attributed, consistent with the
  committed migration counts and digests, and guarded by a generated
  no-regression expectations overlay (`vt_spec.rs`);
- parser and model results are invariant under every required input chunking
  and remain bounded for malformed streams; replies are bounded, drainable
  and admitted whole, the bell coalesces, and history evicts whole lines
  within its byte ceiling (`vt_spec.rs`);
- exact model-renderer PPM goldens pass, beside the structural rendition,
  underline-style and underline-color, palette, cursor, selection,
  bell-ring, and viewport assertions, through the bitmap face and the
  outline face (`vt_render_spec.rs`);
- the encoder spells every chord in §3's table, the pointer's reports are
  what each mode asks for in each encoding, bounded to what X10's byte
  carries, the viewport moves, clamps, and survives eviction and clears as
  specified, and the input queue admits or drops a sequence whole
  (`vt_keys.rs`);
- the compiled `td-term` terminfo entry decodes to exactly the capabilities
  exercised by the native corpus, its key capabilities are the encoder's
  bytes, and each capability that shares a case has an effect check
  (`vt_terminfo.rs`);
- a real PTY unlocks, hands out its peer, reads back the grid's exact
  winsize, and gives a spawned child the slave and that grid; the reader
  delivers output until hangup, the writer never holds its lock across a
  write, survives a child that never reads, reports its own death to the next
  push, and retires on close; and the waiter reports the child's exit and
  reaps a child no thread will wait for (`pty.rs`);
- a search steps through history and the screen in one numbering, across
  wraps, in either case for a lowercase query, the alternate screen
  alone, as wide as the screen, within its query bound, and a match
  still holds only while its cells and wraps do; and a status line
  covers the last or the first row, inverted, and hides the cursor
  (`vt_render_spec.rs`);
- a face covered at a size of its own lays the grid on its cell, with the
  bitmap fallback centred or clipped about its middle in it and the rules
  and the cursor taking it, and a zoom steps half a point, passes a size whose
  cell moves against it, keeps every style, stops at the face's bounds
  and returns to a fitted start fitted (`vt_render_spec.rs`);
- td-term's window, against a scripted compositor peer, reaches readiness
  only with a chosen size, a released and presented frame and a compiled
  keymap; applies a single chosen axis and keeps its size on a bare
  configure; refuses an oversized configure before allocating; redraws on
  lost activation; commits an acknowledged configure nothing redrew; carries
  keys through td's keymap and the encoder to the child; moves and returns
  the viewport by key and by wheel; selects by drag and offers the selection;
  selects nothing on a click, a word on a double press and a row on a
  triple, dragging by them, a row being its whole wrapped line; rules
  exactly the link under the pointer while Control alone is held over
  the surface, and none under reporting, in a search or once focus or
  the pointer has gone, an http OSC 8 link by its id with its URI on a
  status line, cut to keep the authority's end, off the row pointed at
  and over which a press follows nothing, followed as that URI, and no
  other OSC 8 link, drawing a
  frame for it with the model unchanged
  in which a Control-press still follows the link, and after a resize
  ruling it where the reflow put it; copies a wrapped line without the
  wrap's newline; makes a release's selection
  the primary selection and writes its sends; pastes the primary
  selection on a middle press and does nothing without one; opens a
  search on `C-S-r` that takes every key, finds and steps between
  matches older and newer, refines from the last one shown after a dead
  end, scrolls the whole of the one shown on, moves its line off a match
  on the last row, rings at the last match and past the query bound,
  repeats a held key into the query and never to the child, stopping at
  a dead end, drops a match its text no longer holds or a resize moved,
  shows an alternate-screen match only at the live view, ends on a press
  where the view is, puts the view and selection back on `Escape`, `C-g`
  or `C-c`, and on `Return` or keypad Enter selects its match and makes
  it the primary selection, or keeps the selection with no match;
  jumps the view between OSC 133;A prompts on `C-S-z` and `C-S-x`,
  ringing with none further or nowhere to move;
  rings the visual bell in the next frame and in every frame until its
  flash, which a later bell puts forward, is over;
  waits for the proof's sync on the live source; rings for a paste with
  nothing offered and receives a selected offer over a fresh endpoint;
  reports presses, releases, the wheel in carried, bounded notches and
  motion once a cell to a child that asks, keeping presses, buttonless
  motion and the wheel td-term's under Shift, a scrolled-back view or its
  own drag, and the wheel under mode 9; sends a reported press's release
  to the child wherever the view is, absorbs it once the child stops
  asking, releases reported buttons at the last reported cell on leave,
  and drops a motion report the full queue refuses without ringing;
  lays and hit-tests its grid on its cell, adopting a new one as a resize
  and keeping the scrolling margins when the grid is unchanged; bounds its
  fallback grid to the raster's ceilings; does nothing for a font chord
  with no outline face, keeping the selection; and feeds output
  to the model with its replies to the child (`app.rs`); its flags take a
  font size in points within the face's bounds (`main.rs`);
- the account, environment, and child command are the specified ones,
  constructed rather than inherited, the default child is the shell leading
  a session and an explicit command is literal argv leading none
  (`session.rs`), and td-ui's spawn makes a child lead a session on the
  slave exactly when asked;
- a readiness line is refused for every way it can be wrong, a probe reads
  back exactly what was published, a live terminal is never displaced but a
  dead one is, and a silent, dripping, or over-long answer fails the probe
  within its deadline (`ready.rs`);
- td-term's confinement tests pin its closed source inventory, that it
  forbids `unsafe` and reaches no raw layer, that its manifest declares td-ui
  alone and joins the gate, and, by value, the words td's units and boot
  oracle read: the title, the app id, the proof token, every marker, the
  last-screen prefix and the exit report;
- the shipped artifact is static, and its target selftest runs without host
  paths or libraries: `td-term selftest` exercises the model, encoder,
  renderer, terminfo compiler, PTY grid arithmetic, session policy,
  readiness codec and fallback grid, and prints `TD-TERM-SELFTEST-OK` only
  once all ran; the `td-term-test` recipe requires the binary, asserts it
  static, runs the selftest, and requires the `td-term-terminfo` entry;
- the image creates the devpts mountpoint after devtmpfs, replaces its
  `/dev/ptmx` node with the specified symlink, mounts devpts with the
  specified options, links `/bin/td-term` into the staged `td-term` output,
  packs the terminfo entry at mode 0444 behind `/etc/terminfo`, starts
  td-term as uid 1000, passes the readiness-socket probe, and observes the
  matching `TD-TERM-READY` diagnostic; and
- graphical failure leaves the serial recovery path and existing compositor
  readiness proof intact.

## 7. Running outside td

td-term builds on any host with the pinned Rust toolchain, with no
dependency but its sibling td-ui, and runs under any Wayland compositor that
offers `wl_seat` version 5 or later, `wl_shm` and `xdg_shell` -- sway, for
one, in place of foot:

```text
cargo build --release --manifest-path td-term/Cargo.toml
install -m 0755 td-term/target/release/td-term ~/.local/bin/td-term
```

```text
# ~/.config/sway/config
set $term td-term
bindsym $mod+Return exec $term
```

Invoked bare, or with a flag first, td-term is the desktop profile:

```text
td-term [--socket PATH] [--working-directory PATH] [--font-size POINTS]
        [-e|--command PROGRAM [ARG...]]
```

`run` stays td's session program, unchanged; the two differ only where a
desktop's terminal must:

- a flag may also be spelled `--flag=VALUE`, and `--` ends the flags as `-e`
  does;
- the display is `--socket` or the environment's `WAYLAND_DISPLAY`, a
  relative one a name under `XDG_RUNTIME_DIR` either way, and an inherited
  `WAYLAND_SOCKET` is refused as in td's (§2); a relative
  `--working-directory` is taken from td-term's own directory;
- the child starts once the first frame is committed, at the 80x24 fallback
  when the compositor chooses no size, as it does not for a floating window.
  It waits for no frame callback, buffer release or keymap: a compositor may
  keep the one buffer it was given, or never call back for a window on a
  workspace nobody is looking at, and keys before the keymap are only
  dropped. There is no handshake bound, which is td's supervisor's (§4), no
  readiness socket and no `TD-TERM-READY` line;
- the child is `$SHELL`, else `/bin/sh`, or the program after `-e`,
  `--command` or `--`, found on `PATH` when named without a slash, and it
  starts in `--working-directory`, else td-term's own directory. Every child
  leads its session on the terminal (§4), since a desktop's programs expect
  the terminal's signals and no jail claims the terminal;
- the child's environment is td-term's own less `WAYLAND_SOCKET`, `LINES`
  and `COLUMNS`, with `TERM=td-term`, `COLORTERM=truecolor` and
  `WAYLAND_DISPLAY` the dialled path made absolute. No account is read: the
  person's `HOME`, `PATH` and the rest are what the desktop gave td-term, and
  an outer terminal's own markers (`TMUX`, `TERM_PROGRAM` and the like) pass
  through as they do under foot;
- td-term writes its compiled terminfo entry under
  `$XDG_RUNTIME_DIR/td-term/terminfo` and sets `TERMINFO` to that directory.
  ncurses searches `TERMINFO` before `~/.terminfo` and `TERMINFO_DIRS` and
  then goes on to them, so a stale `td-term` entry elsewhere cannot shadow
  this one and every other entry is still found; an inherited
  `TERMINFO_DIRS` is kept. The runtime directory must already exist, be the
  person's and be closed to group and others; each directory below it is
  made, or found, the same and not a link, so no other account chooses where
  the entry goes or what ncurses reads. The entry is written to a new file
  beside it and renamed over, so terminals starting together each leave a
  whole one. With no `XDG_RUNTIME_DIR`, or one that fails those checks, a
  `td-term: terminfo:` line says why, the inherited `TERMINFO` stands, and
  the child finds `td-term` only where the host installed it (`td-term
  terminfo PATH`, §4);
- selecting text sets the primary selection and a middle click pastes it,
  as under foot, wherever the compositor offers the primary-selection
  protocol (§3);
- the outline face is read from `/etc/fonts/jetbrains-mono-nerd` as in
  td's image, or else from the user's or the host's font directories,
  where `./install-fonts` puts it (§3); a host with none draws in Unifont
  after one `td-term: outline face unavailable` line naming
  `./install-fonts`, which `TD_UI_FACE=bitmap` avoids by reading
  nothing;
- `--font-size POINTS` covers the outline face at that size, in foot's
  points, from 4.5 to 192, on the cell its metrics give, and foot's font
  chords zoom it (§3): `td-term --font-size 9` draws as foot's
  `font=monospace:size=9` does at scale one;
- td-term ends with its child: its status is td-term's (its code, or 128 and
  the signal that ended it) with no last-screen report, and the window does
  not wait for the output to drain, so a background job still holding the
  terminal keeps no window open. A close request from the compositor ends
  td-term with status zero, whatever the child does in the same turn, and
  the child hears the hangup as the master closes. td-term's own failure is
  a `td-term:` line on stderr and status 1, which a child's status 1 also
  is.

Another host does not know `td-term`: a program reached over ssh looks up
`TERM` on the remote side, as it does for foot's own entry. The entry can be
installed there (`td-term terminfo` writes it to a path ncurses reads), or
the remote command run with a `TERM` its host has.

Keymaps are the compositor's: td-ui compiles the XKB text it is sent and
refuses what its compiler does not accept (`td-ui/DESIGN.md`), and a refusal
before the child starts ends td-term with the reason.
