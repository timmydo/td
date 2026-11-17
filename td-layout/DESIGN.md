# td-layout

## 1. Boundary

td-layout lays out td-html's immutable document with td-css computed values.
It is a library with path dependencies on td-alloc, td-html, td-css and
td-dom, with no external dependencies. It does not fetch, decode images, paint, run scripts, handle input,
or depend on td-ui. td-web consumes it (td-web/DESIGN.md §10). td-html's
text and rich renderers, which td-mail and td-news keep, remain
independent of it and byte-identical.

The first formatting model is horizontal, left-to-right normal flow. Its
references are CSS 2.2 [box generation and normal flow][flow], [used sizes
and line height][sizes], and [CSS Text 3][text], with CSS 2.2's tables
(§5, "Tables"). This is an explicitly
bounded subset, not a claim to implement browser layout generally.

[flow]: https://www.w3.org/TR/CSS22/visuren.html
[sizes]: https://www.w3.org/TR/CSS22/visudet.html
[text]: https://www.w3.org/TR/2026/CRD-css-text-3-20260814/

## 2. Page ownership and shared styles

The caller supplies the Document and compiled StyleSet. StyledDocument
can borrow the document or retain a BudgetShared<Document> through shared().
The shared form has no self-referential borrow and can move to a worker.
It computes parents before children through td-dom's HTML projection and
owns a style pool and flat box/source records. Detached
template contents and foreign subtrees have the same exclusion as td-dom.
The caller supplies HTML_UA_CSS explicitly. Inline style attributes join
the normal cascade unless the caller leaves them out
(`Limits::style_attributes`, §6, "Caller-set limits"). The root's
computed font size supplies rem units.

Styles are interned by all computed values, including inherited custom
properties. Equality, not a hash alone, decides sharing. Identical siblings
and unrelated equal elements have the same StyleId. Different custom
properties must not alias merely because their currently used values agree.
Custom-property maps are themselves shared and canonicalized, so different
foreground colors need not duplicate a large inherited variable map.
One newly computed candidate is transient; nodes retain small StyleIds,
not one BTreeMap per node. No global cache outlives the styled document.
Hash collisions have a named work limit. td-css owns the small pooling and
storage-accounting API because its computed custom-property representation
is private. It acquires no dependency on a DOM or layout.

### Whole-page memory

The target is approximately 300 MB for a page's DOM, retained styles and
layout, including a working allowance, whatever its input within the
input limit (2 MiB of markup by default, at most 4 MiB): MAX_PAGE_BYTES,
not the input, bounds what is charged. It is td-web's page term
(td-web/DESIGN.md §10, "Memory"), within its 640 MiB memory-high and
768 MiB memory-max jail. A consumer must release or account for a
page's old representation before admitting a replacement.

MAX_PAGE_BYTES is 224 MiB of charged retained storage. Admission includes
the document's conservative charged_bytes, style-pool allocations, box
arrays, list items' ordinals, line records and fragments, markers
included, and td-dom's navigation arrays and construction's per-node
maps while construction holds them.
The original input, up to the input limit, is charged too while retained
by the caller.

`StyledDocument::charges` names the charges by owner, so a consumer
that keeps a ledger of its own (td-web/DESIGN.md §10, "Memory") counts
each once. `tree` is the document's charge, td-html's cumulative parse
bound (discarded parse temporaries included, not the storage it holds),
held by whoever owns the document and the same in every styled document
that borrows or shares it. `styled` is what the styled document holds:
the input, which it counts as its own from the cascade on, its boxes,
list items and style pool, whose charge conservatively includes strings
a computed style shares with its sheets. `index`, td-dom's arrays at 28
bytes a node, and `scratch`, construction's per-node maps at 80, are
admitted beside the other two while construction runs
(`construction()`, under `Limits::bytes`) and returned when it ends.
A layout's admission starts from `held()`, tree and styled, and its
`Layout::page_bytes` adds `retained_output_bytes` and the new output.
Neither sees a sibling: two styled documents sharing one tree each
count it in `held()` and `page_bytes`, and a caller holding another
styled document or its layout folds it into `retained_output_bytes`.

An additional 64 MiB working allowance brings the target to 288 MiB
(302 MB). It covers one cascade candidate, wrapping scratch and allocator
overhead. Charging precedes retained allocations; capacities, map overhead
and nested token/string allocations count. Shared allocations count once.
Counts and byte limits both apply, and caller limits may only lower them,
but for the input limit, which a caller may raise to 4 MiB, and a layout
request's byte limit, which may replace the styled document's up to
MAX_PAGE_BYTES (§6, "Caller-set limits").

This charge is an admission model, not an exact allocator/RSS measurement.
The tree, DOM index, CSS engine and layout arrays own fallible td-alloc
storage. Construction and layout can return memory errors. with_budget()
and with_state_and_budget() use the caller's shared identity for navigation,
computed candidates, style pooling and boxes. The default constructors
create an independent budget of min(limits.bytes, MAX_PAGE_BYTES) plus
WORKING_BYTES and budget metadata. That default does not impose an aggregate
cap on separately owned documents or compiled sheets.

layout() and wrap_text() use the styled document's budget. Their
with_budget variants accept a selected child for output and scratch. Every
retained array keeps its charge until storage is dropped, independently of
retained_output_bytes. Overlapping results therefore count automatically
against their common ancestor, even when that admission hint is zero.
Dropping a launcher handle or cancelling a worker does not release storage
still owned by the worker or its result. Output contains source IDs, not a
document owner: its consumer must retain the corresponding StyledDocument
for source lookup. Memory limits refuse work; they never return partial
successful layout.

The conservative page_bytes/output_bytes admission model remains a
separate bound. It is not an additional td-alloc reservation: doing so would
double-charge owned storage. Original input bytes and caller font caches
must be separately owned or reserved by the consumer. Shared stylesheet
tokens remain charged to their original compilation budget until their last
style owner is dropped.
The separately owned compiled stylesheets, font caches, image pixels,
framebuffer and concurrent network bodies still belong in the consumer's
jail ledger. In particular, a stylesheet compilation peak is not hidden
inside the 300 MB claim. Validation measures both page storage and fresh
process RSS including sheets. An unbounded caller-owned font cache cannot
be made bounded by this library. Hostile pages can be refused; accepting
every input within the input limit is not promised.

The previous td-dom corpus peaked at **131.9 MiB (about 132 MiB)** RSS,
with a full ComputedStyle retained per element. After shared styles and
layout, the new corpus peak is **37.9 MiB**, including compiled sheets,
input strings and resize overlap probes. MDN, the former peak, is now
**9.4 MiB** with byte-identical HTML and CSS. This is a whole-pipeline
comparison: interning, custom-map reuse and suppression of hidden subtrees
all contribute; it does not isolate the saving from interning alone.
Section 6 records the workload, capture identities and limits of this
measurement. The 2 MiB many-block case peaks at 91.1 MiB RSS and charges
115.9 MiB for one retained page, below the 224 MiB admission ceiling.
4 MiB of markup td-html admits charges up to 222.4 MiB measured, and
MAX_PAGE_BYTES refuses others by name, not in order of density (§6,
"Caller-set limits").

## 3. Public data and source identity

StyledDocument::new(document, sheets, input_bytes, limits) constructs shared styles and
box records. NodeId always belongs to the retained or borrowed document. StyleId is
local to this styled document; accessors return Option for invalid IDs.
`block::layout` takes the styled document, mutable FontMetrics and a
Request with viewport dimensions, an optional vertical window, the
charged bytes of any previous layout output retained during the call, an
optional byte limit of its own (§6, "Caller-set limits"), an optional
lookup of replaced elements' intrinsic sizes (§5, "Intrinsic sizes") and
an optional cancel flag (below). Its Result owns
geometry and emitted lines, with resource usage, work counters and the
lines cut at MAX_LINE_SCALARS (`cuts`, §4, "Long lines") and its list
markers (`markers`, §5, "List markers").
BoxGeometry records are in postorder and name DOM elements; this is not
a paint-order API. `output_bytes` excludes the shared styled document
and its tree, so it can be passed as `retained_output_bytes` on a resize
request; `page_bytes` is the four together: tree, styled,
`retained_output_bytes` and the new output (§2). Refusal is an error,
never successful partial output. There is no implicit I/O.
`cascade_work()` reports what the cascade spent (§6, "Cascade work").
`text::fit` takes the styled text page, mutable FontMetrics, a
viewport width, a cap of lines and an optional cancel flag, and counts
the page's lines through layout's own sizing and breaking without
laying it out, returning where to cut it so its lines stay within the
cap at that width and every wider one (§6, "Text pages").

**Cancellation.** `Limits::cancel` and `Request::cancel` each take an
optional `&AtomicBool`, which another thread may set while the call runs.
Construction reads it once its up-front limits (the input and the fixed
arrays' charge) pass, before each element's cascade and after every
`CANCEL_INTERVAL` (4,096)th node's tests in box generation; layout reads
it at its first work tick, after its up-front charges (tree and styled
terms, `retained_output_bytes` and the geometry array, under its
byte limit), and then once every 4,096 units of `MAX_LAYOUT_WORK`, which
every box, line and scalar visit charges. A set flag returns
`Error::Cancelled`, distinct from every limit. As with any refusal, the call
drops all it built, so its budget's use returns to what it was before the
call. A cascade stops once the element it is computing is done, so it sees
the flag at most one element's work late. td-css's caps for one compute
call bound that work, not a time. The costliest elements measured within
them (§6, "Cascade work") take 17 to 21 ms on the measured host, varying
with its load: 2,400 custom names each substituting a 50-word value,
just under MAX_SUBSTITUTION_COMPONENTS, and 2,000 such names with 5,000
inherited ones and 15,000 universal `:not()` rules. Interning an
element's style, bounded by `MAX_POOL_WORK`, adds less: an element whose
8,000-name custom map the pool interns beside another's costs about 8 ms
in all, and the next such element is refused. Layout sees
it within 4,096 units, well under a millisecond. Each read follows the
limits of the work before it (the up-front ones, the work ceiling, and
the cascade ceiling for the element before), so a call past one of them
refuses by name even when cancelled; a limit the work would meet only
after a read gives way to a set flag. A flag set while the last element
cascades, or after layout's last read, can still return Ok. A caller
that must not use superseded work tests its own flag after the call.
`text::fit` reads it as layout does. td-dom's projection, which
construction builds before the cascade, and `inline::wrap_text`, which
has no request, read no flag. The flag is
loaded with relaxed ordering: it publishes nothing else. A styled
document keeps no flag, as `limits()` shows: each layout names its own.

Every TextFragment carries a DOM **text NodeId and UTF-8 byte range** into
that node, its StyleId, position and extent. Offsets are into decoded DOM
text, not source HTML bytes. A fragment cannot cross text nodes. Contiguous
unaltered source may coalesce, up to `MAX_FRAGMENT_SCALARS` (1,024)
scalars a fragment, past which the run starts another fragment where
the last ended: a painter then finds its clip by fragments' x and
width, skipping those wholly before it, and replays at most that many
scalars before the clip in a line, however long the line. A collapsed
space or replacement scalar retains the original byte range and an
explicit display transformation. A preserved tab is unaltered source:
it coalesces with its neighbours, so a line's fragment count does not
depend on its tabs (§4, "Tab stops"). Suppressed whitespace, and what a cut line does not lay (§4,
"Long lines"), has no fragment. Generated markers, line breaks and
replaced-element records are distinct kinds and never pretend to be DOM
text: a marker is a `MarkerFragment` in `Layout::markers`, naming its
list item and no text node or byte (§5, "List markers"). Selection/find
can traverse the DOM independently of the viewport;
link focus can walk the fragment's ancestors. History keeps a node/range
place within this document, rather than a fragile wrapped-line number.
Reparsing creates new identities; cross-navigation place restoration is a
consumer policy. Layout does not implement td-web's selection or history.

Each Line names the block container whose inline content laid it
(`Line::block`, a `BoxId`; `BoxId::index` is its place in
`StyledDocument::boxes`) and how it ended (`Line::end`, a `LineEnd`,
§4, "Line ends"). A table cell's lines name the cell, a caption's the
caption, an anonymous cell's the box holding its run (§5, "Anonymous
objects"). Every `block::layout` line names a block, td-css
blockifying the root; `inline::wrap_text`'s lines name none. Both ride
in four bytes (`LineTag`) in the tag word of the line's
replaced-fragment storage, which its alignment left unused, so a Line
stays 136 bytes and §6's charge steps are unchanged. A windowed
layout's lines carry them as the whole layout's do.

Geometry uses CSS pixels with subpixel precision and finite checked bounds.
There is no per-glyph retained page array. `inline::replay` replays a
text fragment's scalars through the layout's metrics and white-space
policy, each with its byte range, the scalar as shown, its pen from the
fragment's x and its advance, so a painter, hit-testing and selection
place a point on a scalar, a tab included, as layout laid it (§4, "Tab
stops"). Box geometry names
the originating element and its style, including backgrounds and borders
for td-web's painter (td-web/DESIGN.md §10, "Painting"); no display
list or pixel buffer is allocated here.

## 4. Fonts and inline flow

FontMetrics has two operations: advance(style, scalar), returning an
advance in 1/64 CSS px, and line(style), returning height and baseline in
the same units. Values must stay stable within a layout call. Calls may populate a
caller-bounded metrics cache. Font
selection uses the computed family, size, weight and style. A deterministic
test provider makes layout tests independent of installed fonts.

td-web's adapter around td-ui::prose::Metrics (td-web/DESIGN.md §10,
"Faces") maps a computed family, style and size to a prose family,
style and device size, calls advance_at and line_at at that size, and
divides the returned device units, line height and baseline multiplied
by 64, by the scale. td-web paints at the same sizes with the same
metrics, so what it draws agrees with what was laid out.

After whitespace processing, breaking follows prose::wrap: a space is a
break opportunity after that space; a word moves intact when it fits alone;
only a word wider than the line is split internally. A single oversized
scalar still makes progress. Trailing spaces are dropped, including an
all-space prefix before a word that moves. Advances accumulate in 1/64 px
and round half up once when testing the whole-pixel line width, as prose
does. Runs and text nodes do not introduce break opportunities. No kerning,
ligatures, bidi, hyphenation, grapheme shaping or Unicode line-break table
is claimed. NBSP is not an ordinary breakable space.

The intentional preprocessing differences are CSS white-space: normal and
nowrap collapse HTML ASCII whitespace across inline nodes and drop an
initial collapsed space; pre-line also drops collapsed spaces after a
newline, preserves newlines and collapses other spaces; pre, pre-wrap
and break-spaces preserve spaces and newlines, break-spaces laid as
pre-wrap (its spaces hang and are dropped at a line's end, as below).
nowrap and pre suppress soft wrapping. Where white space collapses, a tab
is a collapsed space, as prose lays it; where it is preserved, a tab
advances to the next stop (below). In preserving modes, CR and CRLF in
DOM text become one newline; prose instead displays a literal CR as
U+FFFD. Other control scalars display U+FFFD.
Preformatted trailing spaces and tabs are still dropped to preserve the
prose contract: this differs from CSS pre/pre-wrap. A bare empty inline
sequence does not generate a line; an explicit newline does, and
preserves the following empty line. This differs from prose's
empty-input one-line rule.
Text-transform and text-indent are retained in the style but initially
not applied. Block content widths are floored to whole pixels for the
prose-compatible break test; output positions retain subpixel precision.

**Tab stops.** Where white space is preserved, a tab advances to the
next multiple of the tab interval measured from its line's start pen,
the block container's starting content edge, before text-align shifts
the line. The interval is `TAB_SPACES` (8, CSS tab-size's initial
value) times the advance of a space in the block container's style,
its letter-spacing included, in 1/64 px (CSS Text 3 §4.2): the strut
style an inline sequence is laid with, looked up once a sequence, so a
smaller or spaced run inside the block meets the block's stops. td-css
has neither `tab-size` nor `word-spacing`, so the interval is fixed. A
tab on a stop advances a whole interval, and a tab always advances
unless the interval is zero (a space of no advance), when tabs have
none. CSS's rule that a tab nearer its stop than 0.5ch takes the next
is not applied: in a proportional face a tab can advance a sliver. A
tab is a preserved space:
pre-wrap and break-spaces may break after it, it hangs at a line's end
and is dropped there with trailing spaces, and an all-space prefix
before a word that moves includes it; pre and nowrap never break at it.
Its advance follows its pen, so it is set when the tab is queued, and
again from the new line's start for a tab in a word tail a break moves
(only a run that does not wrap holds a tab there).

A preserved tab is shown as itself, `DisplayText::Source`, and coalesces
with the source text around it in one fragment, so `name\tvalue\tthird`
in one text node is one fragment whatever its tabs (one a 1,024
scalars), and a copy of the
fragment's bytes keeps them. A fragment keeps no pen of its line, so
`inline::replay`, given the style of the block that laid the line
(`Line::block`'s, or the first box's where it names none; with another
style a mixed-size fragment can be refused), recovers its tabs from its
width: the first tab takes what the other scalars leave, which ends on a
stop, and each later tab meets the next stop from there. Each replayed
scalar, a tab included, names its own byte range, so a point over a tab
maps to the tab's byte. Replay must be given the layout's metrics. It
refuses (`Invalid`) a fragment whose advances cannot sum to its width,
or whose first tab would advance nothing or more than an interval;
within that, metrics that differ from the layout's are not detected for
a fragment with a tab, whose first tab absorbs the difference. A tabbed
fragment is checked before any scalar is reported, an untabbed one only
at its end, after its scalars were: a caller that must not act on a
refused fragment gathers them first. `inline::replay_until` is the same
replay with a callback that may break: it returns Ok at once, the
scalars it did not reach neither measured nor checked against the
width, and what it reported before is what `replay` reports. A painter
stops so at its clip's right edge, which a line of tens of thousands of
scalars otherwise replays whole for every paint.

**Long lines.** `Limits::line_scalars` (`MAX_LINE_SCALARS`, clamped to
at least 1) bounds the scalars, replaced elements and inside list
marker (§5, "List markers"), which takes one slot, one line holds
pending. A line that reaches it is not refused. If the line holds a
wrap opportunity it breaks at the last one, as a word that does not fit
moves, losing nothing, unless the item that would pass the bound is a
space just after that opportunity, which hangs. Else, where that item
can wrap itself (a non-space in pre-wrap, break-spaces, normal or
pre-line, or an image there), the line breaks before it, as a word
wider than the line is split, and nothing is lost. Otherwise (pre,
nowrap, or a preserved space) the line is **cut**: that item and every
one after it on the line are visited and charged work but not laid,
and have no fragment; only that first item is measured, and an
image's size is always found. Only a space is an opportunity within a
line, so a soft letter before a run that does not wrap (a
65,536-scalar word of it) is no break point for that run, which is
cut, as CSS too gives no opportunity there. A cut line ends at its next
preserved newline or `<br>`, at the end of its inline sequence, or
before the next non-space that could start a line there: one that
wraps itself, or one that a space dropped where the line could wrap
precedes, with no non-space dropped between. That non-space starts a
new line, so a run that does not wrap after such a space is laid, not
lost. A line is counted
cut only once a non-space or replaced element is dropped: preserved
spaces alone past the bound would have hung, so a pre-wrap line of
spaces is laid as the line it would have been. `Layout::cuts` and
`WrappedText::cuts` report the number of cut lines, whether retained in
a window or not, and the first cut's place: the text node and byte
offset of the first item that line dropped, or a replaced element and 0
when that comes first. What is laid keeps its exact byte ranges; from
that place to the line's end there is no fragment, and before it
trailing spaces can lack one too. A run that wraps is never counted
cut. Breaks and cuts at the bound fall between scalars, not graphemes:
a combining mark can start a line, or be dropped from its base. A
text page, pre-wrap, is never cut; a 4 MiB single `pre` line lays its
first 65,536 scalars, one line charging 20.0 MiB in 23 ms, and 4 MiB of
pre-wrap spaces lays one empty line in 16 ms (release, the host of §6,
2026-11-15, medians of three). The breaks the bound adds differ from
an unbounded layout's only where a line holds more items than
`line_scalars` before its last wrap opportunity or the pane's width,
which needs a run of that many spaces, or of items that do not wrap,
or advances near zero.

**Line ends.** `Line::end` says how a line ended: `Wrap` where a
break may fall, at a space (laid, trimmed from the line's end, or
dropped past a cut) or at an image's or inside marker's edge; `Split`
between two scalars of a word with nothing between them, where its
content wraps anywhere (or reaches `line_scalars`), the next line
continuing the word; `Newline` at a preserved newline, CR LF or CR as
one; `Break` at a `br`; `End` where its inline sequence ends, at its
block's end or a block box inside it. A `Wrap` does not promise a
space: the white space laid between its line and the next may be none
(an edge), one, or several (pre-wrap), and a consumer copies it as
laid, past a cut only what layout dropped as space. A last `br` or
preserved newline leaves an empty line after it, ended `End`. A
consumer reads paragraphs, copy separators and selection moves from
these and `Line::block` rather than from gaps between lines: in a
whole layout a paragraph is a run of lines ended by one that ends
`Newline`, `Break` or `End` (a window can leave two runs' lines
adjacent).

`inline::wrap_text` accepts source TextRuns, a whole-pixel width and a
StyleId for the line strut, which also sets the tab interval. It returns
bounded lines, byte/work charges and cuts.
The same internal flow can measure without retaining lines. BudgetVec growth
is admitted before allocation, including the old and replacement buffers
while they overlap. The byte model also counts conservative capacity and
allocation overhead. Pending scalar scratch is reused across block flows;
wrapping moves only the remaining word tail. Small event lists use four
inline slots before falling back to budgeted storage. One text fragment
and one replaced fragment can live directly in a line; additional
fragments use budgeted arrays. These inline values count as part of the
containing line allocation. Within one layout, the most recent ordinary
block dimensions are cached by StyleId and containing width/height;
replaced elements always read their own attributes and intrinsic size
(§5). Styling counts
the geometry-producing boxes, allowing layout to admit and allocate its
geometry array once at the exact required capacity.

Line height uses participating text scalars, replaced elements and explicit
breaks, with CSS line-height and half-leading around the baseline. Empty
inline boxes and discarded spaces do not contribute additional struts in
this first model. A preserved newline
contributes its own metrics to the line it ends, without a text fragment. Inline replaced elements
align to the baseline. Initial text-align supports left/start, right/end
and center; justify uses left. Vertical-align other than baseline is
deferred. Letter spacing contributes to scalar advances, clamped so an
advance cannot be negative. Inline non-replaced margins, borders and
padding, inline relative offsets and generated content other than list
markers (§5, "List markers") are deferred. An HTML br uses the
explicit-newline rule, including its own metrics and the following
empty line.

## 5. Block flow and replaced elements

display:none suppresses its subtree. Normal blocks stack vertically;
adjacent inline content forms anonymous line sequences between blocks,
including a block encountered inside inline descendants. Width, min/max
width, height, min/max height, box-sizing, padding, border widths and
margins use computed values. Percent horizontal dimensions and all
padding/margins use the containing content width. Percent heights need a
definite containing height; otherwise height is auto and percentage
min/max height has the corresponding unconstrained initial behavior.
Auto horizontal margins divide positive free space. Over-constrained
left-to-right boxes adjust the used right margin. Negative margins remain
signed. Adjacent block sibling margins collapse by largest positive plus
most negative; parent/child and empty-through margin collapse are deferred
and their margins add. Block relative offsets move visual geometry, not the space reserved
in flow. Overflow is reported geometrically; clipping,
scroll containers and stacking contexts are consumer/future work.

Tables are laid as grids (below, "Tables"). Flex and inline-block
formatting are not implemented: those containers use ordinary block flow
and their descendants remain visible. List items use block flow, with
the markers below. visibility:hidden
retains geometry and style so the eventual painter can suppress it.
Floats use their computed blockified display in ordinary flow with **no
exclusion region**; clear has no effect. Float placement is a later
increment, not silently approximated by text wrapping around placeholders.

HTML img is a replaced element even when no image bytes are available.
Its content size comes from its CSS width and height, then its width
and height attributes, then the intrinsic size the caller supplies, then
the 300 by 150 CSS px placeholder. An attribute counts when it parses as
a finite number from 0 to td-css MAX_ABSOLUTE_VALUE (1,000,000 px);
otherwise, a percentage included, it is absent. Box-sizing applies to
the CSS values; attributes and intrinsic sizes are content sizes.

**Intrinsic sizes.** `Request::intrinsic` names the caller's
`IntrinsicSizes` lookup, `Sync` so a request stays `Send` and `Sync`,
which a layout asks once for each image box it sizes, and again for
each time a table measures that box's widths (below, "Tables"), by the
img's NodeId and StyleId, and no other node: the style lets a caller measure
a stand-in in the image's own font. Layout holds the caller's
FontMetrics mutably while it asks, so a lookup cannot borrow them: it
answers what was measured before the layout, or measures with a handle
of its own. An `IntrinsicSize` holds an optional width and height in
1/64 CSS px, FontMetrics' unit, so neither can be NaN or infinite, each
clamped to MAX_VIEWPORT, and an optional natural ratio of two `u32`
terms, none when either is zero. The ratio is given, not derived from
the dimensions: `IntrinsicSize::picture` sets all three for a decoded
picture (`picture_px` from whole CSS px, saturating), while a caller
sizing a stand-in (td-web: an `alt` text's line, td-web/DESIGN.md §10)
gives dimensions without one, so the stand-in does not scale as a
picture would. Zero is a size: an image given 0 by 0 takes no space
beyond its margins, borders and padding. An answer of None, or no
lookup, gives no intrinsic width, height or ratio.

The ratio is the natural one, else the attributes' when both are
positive (HTML maps them to `aspect-ratio: auto w / h`), and is used
only when finite and positive. The used size follows CSS 2.2 §10.3.2
and §10.6.2, with CSS Images 3 §5.2's fallbacks to a natural dimension
and then the default:

- Both CSS dimensions set: each, clamped by its own min and max.
- One set: that one is clamped by its min and max first. With a ratio
  the other is that used value through it, else its attribute, else its
  intrinsic dimension, else 300 or 150; then it is clamped by its own
  min and max, which can distort, as CSS has it.
- Neither set: the width is the width attribute, else the height
  attribute through the ratio, else the intrinsic width, else the
  intrinsic height through the ratio, else for a ratio alone the
  containing width less the box's margins, borders and padding (§10.3.2's
  suggestion for a ratio with no size), else 300. The height is the
  height attribute, else the width attribute through the ratio, else the
  intrinsic height, else the used width through the ratio, else 150.
  With a ratio, min and max sizes then resolve by §10.4's table, which
  keeps the ratio where the constraints allow; without one, each
  dimension is clamped alone.

The table's rows are taken on the tentative size, unclamped, as §10.4
writes them: each fixes one side at its bound and derives the other
through the tentative size's own ratio, which is the ratio above unless
two attributes disagree with a picture's, and is the ratio above where a
side is zero and the size has none of its own. Where both sides are
over their maxima, or both under their minima, the side fixed is chosen
by that ratio (the width's maximum through it against the height's
maximum, the height's minimum through it against the width's minimum).
Both sides are then clamped by their min and max, a minimum winning
over a smaller maximum, so a size with a zero side still meets every
bound it can. Two attributes beside a picture of another ratio keep
their own proportions, and the picture's ratio applies where the
tentative size is on it or a dimension is auto: `<img width=300
height=300>` with a 4:1 picture under `max-width:500px` is 300 by 300
at `max-height:300px`, 299 by 299 at 299 px and 200 by 200 at 200 px,
continuous as the bound tightens, where a browser, keeping both
attributes as hints, lays 300 by 200 at the last.

Every size derived through a ratio, and every side the rules above
return, is clamped to 0 to MAX_VIEWPORT; a ratio is finite and
positive, so no product or quotient of a finite size is NaN, and a
hostile ratio, from a caller or from attributes such as `width=1000000
height=0.000001`, costs only the box it sizes. Boxes are bounded each,
not together: enough images at MAX_VIEWPORT still pass MAX_COORDINATE
and refuse the layout, so a picture arriving can turn a page that laid
out into one that is refused on its relayout.

HTML maps the attributes to presentational `width` and `height` hints,
which any author `width` or `height` overrides, `auto` included. td-css's
computed style cannot tell `auto` from no declaration, so td-layout
treats the attributes as a fallback for an auto dimension, which departs
from a browser toward the common responsive sheet,
`max-width:100%; height:auto`: a sheet that sets one dimension alone
gets the other through the ratio rather than from its attribute, and
`<img width=1200 height=800>` under `max-width:100%` at 500 px is 500 by
333, its ratio kept, where a browser whose sheet does not say
`height:auto` keeps the height attribute and lays 500 by 800. A test
holds the rules with no lookup against the attribute rule written out
on its own (CSS over the attributes, the attributes' ratio for an auto
side, 300 by 150, each side clamped alone) wherever the attributes give
no ratio or no bound applies, against the same attributes given as a
picture's size where both do, and against values worked by hand for a
row of each kind.

The lookup belongs to the request, not the styled document: no style,
box or geometry count depends on it, so a changed intrinsic size needs
another layout of the same styled document, not a cascade, and that
layout equals one of a fresh build. The lookup is the caller's code and
is not timed or charged; the image box it answers for is charged its
work as before. Inline images are atomic; wrapping modes allow a break
before or after them. No image decode or load can change geometry inside
a layout call. SVG/MathML remain excluded with the td-dom projection;
object/embed/canvas have no content-specific renderer.

### List markers

A box whose computed display is `list-item` and that is not replaced
generates a marker, unless its `list-style-type` is `none`, from the two
longhands td-css computes, `list-style-type` and `list-style-position`
(td-css/DESIGN.md §4, with the `list-style` shorthand). There is no
`::marker` rule, no `list-style-image`, no `counter-reset`,
`counter-increment` or `counter-set`, and no `marker-side`: the marker
is drawn in its item's style, from its item's ordinal and type. A
replaced element with `display: list-item`, as an `img` may have, is
neither owned nor numbered and draws no marker. HTML_UA_CSS lays out
lists as HTML §15.3.7 (Lists) does (td-css/DESIGN.md §4): `dir` and
`menu` like `ul`, a list inside another list without block margins,
`ol` decimal, `ul`, `menu` and `dir` disc, and nested lists' bullets: a
`ul`, `menu` or `dir` inside any list is circle, inside two square, an
`ol` decimal at any depth. The `type`
attribute of `ol`, `ul` and `li` is a presentational hint, which td does
not map (td-web/DESIGN.md §10, "Out"), so `<ol type=a>` counts in
decimal.

**Numbering.** The ordinal is HTML's ordinal value of an owned list
item ([HTML §4.4.8](https://html.spec.whatwg.org/multipage/grouping-content.html#the-li-element)),
the implicit `list-item` counter of
[CSS Lists 3 §4.6](https://www.w3.org/TR/2020/WD-css-lists-3-20201117/).
An item's list owner is its closest `ol`, `ul` or `menu` ancestor, at
any depth, else its parent; an item in `display: none` is not owned, an
item whose type is `none` is and takes its number. An `ol`'s starting
value is its `start` attribute, else, when `reversed`, its count of
owned `li` elements, else 1; any other owner starts at 1. In tree order
each owned item takes the owner's next number, an `li`'s `value`
attribute resetting it first, and the next number is one more, or one
less in an `ol` with `reversed` (CSS maps only `ol[reversed]` to a
reversed counter, so `reversed` on a `ul` does nothing). Integers are
parsed by HTML's rules for parsing integers (whitespace, a sign, the
digits to the first non-digit) and, like every step, clamped to `i32`:
CSS Lists 3 §4 clamps a counter at the range its UA keeps rather than
letting it overflow, so `start=99999999999` numbers every item
2147483647. Nested lists number independently, each its own owner;
an item nested in another item without a list between them belongs to
the outer list.

HTML's ordinal value is not CSS Lists 3's implicit counter in every
corner, and td follows HTML. A reversed `ol` without a start counts its
owned `li` only, where CSS counts every list item:
`<ol reversed><li>a<div style=display:list-item>b</div><li>c</ol>`
numbers 2, 1, 0, CSS 3, 2, 1. A `value` in a reversed `ol` leaves the
start at the count, where CSS §4.4.2 counts down to the first one set:
`<ol reversed><li>a<li value=7>b<li>c</ol>` numbers 3, 7, 6, CSS 8,
7, 6. And an item outside any list is its parent's, where CSS's
counter runs on through its following siblings' subtrees:
`<div><li>a</li><section><li>b</section><li>c</div>` numbers 1, 1, 2,
CSS 1, 2, 3.

The styled document numbers its items once, during box generation, so a
relayout does not: owners are a stack opened at an owner's box and
closed when its subtree is done, and a reversed `ol` without a start
fixes its items' numbers when it closes and knows its count, through a
scratch list of those items. The owner stack, at most one owner an
ancestor and so bounded by the tree's depth, and that scratch list, at
most an entry a list item and so bounded by `MAX_BOXES`, are transient
scratch charged to no limit, as layout's waiting outside markers are
(at most one an open item, bounded by `MAX_DEPTH`); all three fall
within `WORKING_BYTES`. Each list-item box keeps a 16-byte record,
its box and ordinal, charged to the styled document in the doubling
steps a layout's arrays are (§6, "Text pages"), against `Limits::bytes`
(`MAX_PAGE_BYTES` by name); `StyledDocument::ordinal(BoxId)` reads it,
`list_item_count()` and `marker_count()` count them. A page without list
items keeps none.

**Text.** `MarkerKind` is the type, `MarkerKind::text(ordinal)` the
marker string of CSS Lists 3 §3.2, the predefined counter style's
representation and its suffix, in a `SmallText` of at most
`MAX_MARKER_BYTES` (17), with no allocation:
[CSS Counter Styles 3 §6](https://www.w3.org/TR/2021/CR-css-counter-styles-3-20210727/)'s
`disc` (U+2022), `circle` (U+25E6) and `square` (U+25AA) and a space;
`decimal`, and `decimal-leading-zero` padded to two symbols with the
negative sign counting toward the pad, then `. `; `lower-roman` and
`upper-roman` from 1 to 3999, their range, and `lower-alpha` and
`upper-alpha` (or their aliases `lower-latin` and `upper-latin`) from
1, bijective base 26, each falling back to decimal outside its range,
as each style's fallback is decimal. The longest is `MMMDCCCLXXXVIII. `
(3888); decimal's is `-2147483648. `, alphabetic's seven letters
(`fxshrxw. `, 2147483647). Text is measured in its item's
style, letter-spacing included, a work unit a scalar; a bullet of the
style and type measured last, or the same number, is not measured again
but charged as if it were.

**Outside**, the initial position, the marker hangs: its end abuts its
item's border-box start edge (with the item's relative offset), and its
baseline is the baseline of the first line laid within the item at any
depth, a nested block's first line included. It takes no line width,
adds no line height and stays where text-align moves the line: the
line's fragments are exactly those of the same item with no marker.
Items whose first lines coincide share the line, outermost first,
as `<ul><li><ul><li>x` draws two bullets beside `x`. An item that lays
no line (empty, only collapsible spaces, only blocks without lines)
puts its marker at the top of its content box, with no line, and its
content box is at least the marker's line height tall, as Chromium does;
a block image in the item keeps its own height. A marker counts toward
the layout's overflow, so one hanging past the page's start widens it,
with a line or without. Its baseline is its line's, so a relatively
positioned child holding that line moves the marker by its top offset
but not its left one, whose end stays at the item's edge; Chromium
places the marker at the child's static position, unmoved by either.
CSS Lists 3 §3.5 leaves the position undefined.

**Inside**, the marker is an atomic inline leading its item's first
inline content, as CSS Lists 3 §3.5 places it before the item's
contents: it takes its width on the first line, sets that line's
height with its item's metrics, moves with text-align, and is followed
by a wrap opportunity where its item's white space wraps, as its text
ends in a space. A word that does not fit beside it moves whole to the
next line, and later lines start at the content edge, not under the
text. Collapsible spaces after it are dropped as at a line's start, a
deviation from strict CSS, which keeps one since the marker's own space
is preserved and so does not collapse what follows it. An item
whose first child is a block, or that has no inline content, lays the
marker as a line of its own before the block, as browsers do. It
shares the scratch slot of an image, so a pending item is no larger,
and takes one of the line's `line_scalars` slots (§4, "Long lines").

**Fragments.** A `MarkerFragment` names its item (the element, never a
text node), its `ordinal`, `kind`, `inside` and `style`, its line's
index in `Layout::lines` (`line`, None for an item that laid none), and
its own box: `x`, `y`, `width` and `height` from its style's line
metrics, and its absolute `baseline`, which is its line's. Its text is
`text()`. It has no byte range: `inline::replay` takes only a
`TextFragment`, no fragment holds marker text, and a marker's place in
copied or found text is the consumer's (td-web/DESIGN.md §10, "Places,
find, selection and links"). Markers live in `Layout::markers`, not in
a line, so a line stays 136 bytes and a page without list items pays
nothing; those with a line are in line order, outermost first, and one
without is added when its item ends. A layout without a window admits
`marker_count()` markers at once, as it does its geometry, at 72 bytes
each charged twice; a windowed one keeps a marker where it keeps its
line, or where the window meets a marker with no line, and grows its
array as lines do. Each marker counts toward `MAX_FRAGMENTS`.

**Measured.** Release, the host of §6, deterministic metrics, pinned to
one CPU, 2026-11-15. Pages without list items do not slow: interleaved
with a build of the crate without markers, medians of nine rounds of
nine layouts, 2 MiB prose 45.6 ms without and 45.5 with, one word 44.4
and 44.1, blocks 62.9 and 62.6; styling is §6's. With list items the
markers cost their measurement: `<ol>` and 100,000 `<li>item text`
lay out in 44.3 ms without markers and 57.2 with, and style in 380 and
395 ms. 200,000
empty items, within td-html's `MAX_NODES` (262,144) only without a text
node each, style in about 790 ms, admitting 74.4 MiB while
construction runs and holding 53.8 MiB of tree and styled after, and
lay out decimal markers in 49 ms (bullets 33 ms, none 28 ms), charging
156.4 MiB with the tree and styled document (129.0 MiB without
markers), 3% of
`MAX_LAYOUT_WORK`. Inside, each of those markers lays a line, and the
lines' 68 MiB step refuses the layout `Limit("MAX_PAGE_BYTES")`; 131,000
items of one letter, which lay a line each anyway, charge 162.6 MiB
with markers either way and 144.6 without (charges after §2's split).

### Tables

A box whose computed display is a table display is laid by CSS 2.2
[§17][tables] in its separated borders model, with HTML's
[table processing model][html-tables] for the grid. The roles, read from
`display` alone, are `table` and `inline-table`, `table-caption`,
`table-column-group`, `table-column`, the three row groups, `table-row`
and `table-cell`; `StyledDocument::table_role` reports a box's and
`has_tables` whether any box has one, so a page without one never looks.
`inline-table` is laid as a block-level table where its box is: td-layout
has no inline-block formatting (above).

[tables]: https://www.w3.org/TR/CSS22/tables.html
[html-tables]: https://html.spec.whatwg.org/multipage/tables.html#table-processing-model

**Anonymous objects** (§17.2.1). Text of HTML white space alone between
table boxes makes no box. In a table, anything but a caption, column,
column group, row group or row is wrapped with its neighbours in an
anonymous row; in a row group, anything but a row; in a row, anything
but a cell is wrapped in an anonymous cell, laid with the row's style.
A run of internal table boxes (rows, cells, groups, captions, columns)
among a block's children with no table box around it is wrapped in one
anonymous table, which takes its parent's inherited values and has no
margins, borders or padding. A column group's children other than
columns are not rendered; a column, and anything in it, has no content.
Real HTML parses rows into a `tbody`, so only CSS-made tables lean on
these rules.

**Grid.** Rows are in tree order, row groups' rows in their place: a
`tfoot` is not moved to the bottom, a `thead` not to the top, unlike a
browser. Cells take the first column free of rowspans in their row.
`colspan` is HTML's non-negative integer, 0 or junk being 1 and above
MAX_COLSPAN (1,000) being 1,000; `rowspan` likewise, 0 meaning to the end
of its row group and above MAX_ROWSPAN (65,534) being 65,534, and every
span ends with its row group. A cell that overlaps a rowspan (HTML's
table model error) does not shorten it: a column stays held to the
latest row any span in the group holds it to. `span` on a column or
column group is 1 to 1,000. A grid of more than MAX_TABLE_COLUMNS (4,096) columns, counting
spans and column elements, refuses the layout `Limit("MAX_TABLE_COLUMNS")`.
Spans are recorded per column as a row-group epoch and an end row, so a
cell's placement costs its span, charged as work, and no row-by-column
array exists. The grid's columns are then merged into **effective
columns** wherever no cell or column element starts between them, as
browsers do, so `colspan=100` over three cells is three columns and not a
hundred gaps of spacing.

**Widths** (§17.5.2). With `table-layout: fixed` and a width other than
auto, column elements' widths set their columns, then the first row's
cells' widths theirs, each span divided evenly; columns with no width
share what remains evenly, and where every column has one the rest is
spread by their widths. No cell is measured, and such a table measured
as a cell's content is its set width or its columns', the wider.
Otherwise the automatic algorithm measures each cell's min-content and
max-content width with §4's white space, wrap opportunities, tabs and
advances (a separate walker, `block/intrinsic.rs`, that emits nothing),
an inside list marker counted as the first item of its line, each
unbreakable piece rounded to whole px as a line's fit test is, plus its
padding and border rounded up. A percentage of a cell's padding is
zero, measured and laid alike. A cell's set width is its preferred
width, as browsers take it (CSS Tables 3's outer max-content): its
maximum is that width, never under its minimum, which only its content
and its `min-width` set. A percentage width is kept as its column's
share. Single-column cells set their columns' minimum and maximum, a
column with set cells preferring the widest of those and not its other
cells' content; a column element's width replaces a column's maximum
the same way. Spanning cells then spread their excess over the columns
they span, narrowest span first, in proportion to the columns' own
values (evenly where those are zero); a spanning cell's percentage is
spread over its columns by their maxima where none has one, and its
set width marks them set. Nested tables, images and blocks in a cell
contribute their own intrinsic widths, a replaced box's minimum being
its border alone when its width or max-width is a percentage. A
caption's min-content width raises the table's minimum.

The table's width is its set width (border-box in HTML's sheet), never
under the columns' minimum, else its columns' preferred width within
the room its margins leave, never under their minimum: a table wider
than its container overflows it. The preferred width counts percentage
columns as Gecko does, as the width their maximum is that share of.
Each percentage column takes its share of the table's room, then auto
columns their maximum, columns with a set width theirs. Extra room goes
to auto columns by their maxima, else to set columns by theirs, else to
percentage ones by their percentages; a shortfall comes from auto
columns, then set ones, then percentage ones, each down to its minimum.
Widths are integers of 1/64 px, the table's floored from the room it is
given, and every share given or taken sums exactly, rounding's remainder
going to the last column or to those with room left, so columns sum
exactly to the table and never past its container. A cell's width is
its columns' own sum, not a difference of two x's, which at a
fractional table x fell an ulp short and wrapped content measured to
fit. Auto margins center a table as §10.3.3 would a block of that
width.

**Rows and cells.** Cells are laid in their columns' width, less
spacing, at their row's top, as §5's blocks in their own context; a
row is as tall as its tallest single-row cell, and a row-spanning cell
taller than the rows it spans grows the last of them. Cell `height` and
row `height` raise the row; a table's set height beyond its rows is
added below the last row, not spread over the rows. Within its row's
height a cell's content is aligned by `vertical-align` (§17.5.3),
against the height its content takes and not a height set on the cell:
top, middle and bottom, and baseline, where every single-row baseline cell's
first line's baseline meets the row's lowest such, and any other value
is baseline; such a cell makes its row the taller of its set height and
its content moved to the baseline. A spanning baseline cell is laid at
the top. HTML's sheet
gives cells `vertical-align: middle` through their row group. A cell's
box keeps the row's top and full height; its lines, replaced boxes,
markers and descendants' geometry move by the alignment offset, rounded
to 1/64 px. `empty-cells` has no effect; an empty cell has its
column's width and row's height.

**Borders and spacing** (§17.6). `border-spacing`, one td-css
longhand holding both axes, lies around every column and row, inside the
table's padding.
`border-collapse: collapse` is approximated, not resolved: no spacing,
no table padding, and the table's and every cell's border width halved
in their geometry, so adjacent cells' halves sum to one width. Conflict
resolution (§17.6.2.1) is not done; a painter draws each box's own
half. Row, row group, column and column group boxes have geometry for
backgrounds and are given none of the edges; a column box spans the
table's rows, a row group its rows, a row its columns.

**Captions.** A caption before the table's first row is laid above
the table box, at the table's border-box width (HTML centres its text);
one after any row is laid below it, in tree order, whatever its
`caption-side`. The table's vertical margins lie outside its captions,
as on §17.4's table wrapper box, and a caption's own margins add.

**Output and lines.** Geometry is in postorder, as every box's: each
cell, then its row, then its group, then the table. Lines are in tree
order: row by row, and within a row, cell by cell left to right, each
cell's lines top to bottom. Line order is therefore not y order inside
a table: within a row a middle-aligned cell's first line can lie below
its neighbour's second, a rowspan's lines come with its first row's
though a bottom-aligned one lies beside later rows, and a caption after
the rows comes after them whatever their heights. A table's lines are
one contiguous run of the layout's lines (its nested tables' inside
it), so a consumer seeking a line by y may binary-search the ordered
lines around a table but must scan a table's run, by y and then x.
Every fragment keeps its DOM node and byte range, as in §3; a table adds
no fragment kind and generates no text. Anonymous rows and cells have
no box and so no geometry.

**Windows.** A cell's lines move once its row's height is known, and
only down, so while any table is laid layout drops a line that starts
at or below the window's end as it would outside a table, but keeps
those above the window, and each cell's first line whatever the
window, since its row's baseline is that line's. The outermost table
keeps those its window meets and drops the rest, renumbering its
markers' lines, as each of its rows ends with no rowspan still open,
and once more as it ends. A windowed layout equals the filtered whole
one, as everywhere. What a window holds at once is the lines above it
in the row being laid, those of rows a rowspan still holds open, and
a line for each cell: 20,000 lines in one cell windowed at the top
charge 1/20 of the whole layout's bytes, and a window at the end of
20,000 rows no more than one at their top. Each filtering moves what it
kept into vectors of their size where the old ones are four times too
large, charging the new and giving back the old capacity's charge, so a
window far down a long row returns its few lines in a few lines'
capacity and output bytes, which a next layout passed as
`retained_output_bytes` can admit. A million lines of `pre` in one
cell, windowed at 0 to 1,000 px, lay out where the whole layout is
refused `MAX_PAGE_BYTES` (measured below); a window there holding more
than 524,288 of those lines above it, past 10,485,763 px or 52% of the
page, needs the lines vector to double past that and is refused
`MAX_PAGE_BYTES` too.

**Bounds.** Every box looked at, span column visited, scalar measured,
line moved and line filtered is charged to MAX_LAYOUT_WORK, and a block
or table measured, a cell, or an inline image measured, eight units
(`MEASURE_BOX`), its style
read, edges resolved and children gathered taking some ten scalars'
time. The grid, cell, row, column, item and span-order records live in
one stack of BudgetVecs a nested table extends after its ancestors' and
truncates when done, charged to the page budget as they grow. Measuring
is linear in the content per level of table nesting: a table measures
its cells, a table nested in one is measured with its cell and again
when laid, so a scalar d tables deep is measured d times, bounded by
MAX_DEPTH and MAX_LAYOUT_WORK. The child lists a measure gathers are
charged while held and given back after, so content d tables deep
charges them at their peak, not d times. A table level spends four of
MAX_DEPTH, whatever elements the page wrote for it, since laying one
recurses through the table, its row, its cell and the cell's content
and measuring as deep: nested HTML tables, four elements a level,
reach td-html's own 256 first, but cells nested in cells with nothing
between, one element a level wrapped in an anonymous table and row,
are refused `Limit("MAX_DEPTH")` at the 64th (DOM depth 66), where
blocks reach 256.

**Measured.** Release, the host of §6, §6's deterministic metrics,
pinned to one CPU, 2026-11-15 under a load average of 3 to 5, medians of
three to nine layouts per process. Wikipedia articles fetched that day
with their stylesheets (nothing kept):

| Page | Layout ms, tables as block flow / as tables | Height px |
| --- | ---: | ---: |
| *List of countries and dependencies by population* (1,390,111 bytes, 3 tables) | 3.8 / 4.4 | 57,405 / 22,220 |
| *Germany* (1,647,153 bytes, 15 tables and infoboxes) | 5.7 / 6.2 | 51,261 / 45,129 |
| *List of chemical elements* (495,772 bytes, 9 tables) | 1.8 / 2.3 | 76,567 / 12,255 |

Pages without tables lay out with identical work: §6's 2 MiB prose in
51.4 and 51.3 ms, word in 50.7 and 49.4, blocks in 69.2 to 74.4 ms
against 71.1 to 72.2 over four alternated runs. Blocks' cascade takes
22% fewer instructions and 4% fewer cycles than before td-css's faster
cascade and these longhands, and the Wikipedia articles' about 4% fewer
instructions and the same cycles (td-css/DESIGN.md, "Computed-style
sharing").

Worst cases, in one process each, the same day:

| Shape | Result |
| --- | --- |
| 100,000 cells, 1,000 rows of 100 (604,015 bytes) | 59 ms, 132 MiB charged with the tree and styled document |
| 28 rows of 4,096 columns | 64 ms, 143 MiB |
| 2,000 rows each one `colspan=1000` cell | 2.6 ms |
| 200 `rowspan=65534` cells beside 20,000 rows | 70 ms |
| a 4 MiB cell of words, `Limits::input` at MAX_INPUT, 1 / 2 / 8 / 10 tables deep | 137 / 177 / 409 / 476 ms (54,473,298 work at 10) |
| the same 30 deep | `Limit("MAX_LAYOUT_WORK")` after 583 ms |
| 4,097 columns | `Limit("MAX_TABLE_COLUMNS")` in 0.5 ms |
| 20,000 / 50,000 rows of four `colspan=1000` cells | `Limit("MAX_LAYOUT_WORK")` in 38 ms / in about 37 ms (`Limit("MAX_PAGE_BYTES")` in 9 ms before §2's split) |
| 2 MiB of one-cell tables each holding 62 nested empty divs | 62 ms, 2,120,466 work (29 ns a unit) |
| 300 one-cell tables nested 61 deep | 249 ms, 20,369,104 work (12 ns a unit) |
| 100,000 empty divs in a cell 45 tables deep | 575 ms, 40,737,039 work, 62 MiB |
| a million lines of `pre` in one cell | whole `Limit("MAX_PAGE_BYTES")` in 106 ms; windowed at 0 to 1,000 px 76 ms, 9.6 MiB, as without the table; at 10,000,000 px 143 ms, 9.6 MiB, no output bytes past its 51 lines'; past 10,485,763 px `Limit("MAX_PAGE_BYTES")` |
| 100,000 inline images 5 px square in a cell 60 tables deep | 744 ms, 54,465,584 work (14 ns a unit; 59 before images measured at `MEASURE_BOX`) |

Measuring is weighted so its boxes cost what text does: text in cells
runs about 9 ns a unit, and empty divs before `MEASURE_BOX` ran 83 ns
a unit and tables nested 61 deep 34. Laying deep empty blocks outside
tables, at 84 ns a unit, is §6's own and unchanged. Before measuring's
child lists were charged at their peak, the 45-deep divs were refused
`Limit("MAX_PAGE_BYTES")`.

## 6. Work bounds, resize and validation

All limits are named constants and tested at and beyond their edges.
The layout limits supplement, not weaken, td-html and td-css limits.

| Limit | Ceiling | Resource |
| --- | ---: | --- |
| MAX_PAGE_INPUT | 2 MiB | original input admission, by default |
| MAX_INPUT | 4 MiB | input admission a caller may set |
| MAX_PAGE_BYTES | 224 MiB | combined charged retained page, or one request's |
| WORKING_BYTES | 64 MiB | measured transient/allocator allowance |
| MAX_BOXES | 262,144 | box/source records |
| MAX_STYLES | 16,384 | distinct complete computed styles |
| MAX_POOL_BYTES | 64 MiB | style pool within page budget |
| MAX_POOL_WORK | 67,108,864 | pool field/component and string-byte visits |
| MAX_STYLE_COMPARISONS | 1,048,576 | pool equality probes |
| MAX_DEPTH | 256 | layout ancestor stack; a table level spends four (§5, "Tables", "Bounds") |
| MAX_LAYOUT_WORK | 64,000,000 | visited records/scalars per layout; a measured block, table or cell eight (§5, "Tables": worst cases there) |
| MAX_CASCADE_WORK | 128,000,000 | weighted cascade units per styled document |
| MAX_LINE_SCALARS | 65,536 | pending single-line scratch; a line past it breaks or is cut (§4) |
| MAX_LINES | 1,048,576 | counted lines, emitted or not |
| MAX_FRAGMENTS | 1,048,576 | emitted text, replaced and marker fragments |
| MAX_FRAGMENT_SCALARS | 1,024 | scalars one text fragment coalesces (§3) |
| MAX_MARKER_BYTES | 17 | one list marker's text (§5, "List markers") |
| MAX_TABLE_COLUMNS | 4,096 | one table's grid columns (§5, "Tables") |
| MAX_COLSPAN, MAX_ROWSPAN | 1,000, 65,534 | HTML's clamps on a cell's spans |
| MAX_VIEWPORT | 1,000,000 px | viewport width and height |
| MAX_COORDINATE | 1,000,000,000 px | absolute geometry extent |

Dimensions, arithmetic and metrics are validated before use; NaN,
infinity, overflow and exhausted budgets return errors. Traversal and
normalization charge work even when output is hidden or outside the window.
No panicking indexing or production panic surface is permitted. Input
length is supplied explicitly when borrowing an already-parsed Document,
because its decoded text cannot recover the original byte count.

Resizing width reuses the DOM and shared styles when the caller's media
environment is unchanged. Changed media queries require a new StyleSet
and recascade. Exact block heights require an O(nodes + text scalars)
measurement pass on every width change, which td-web pays at every
resize (td-web/DESIGN.md §10, "Relayout"). In the release-mode 2 MiB
fixtures below, a width-only resize from 1280 to 832 px took median
**54–67 ms** with synthetic font metrics; individual runs ranged from
**53 to 69 ms** on the measured host, under a load average of 1 to 2
(105–194 ms medians under a heavier load when first measured). These
figures exclude HTML parsing, cascade and the release of the previous
output. A full recascade is much more expensive for many-element pages.
This is synchronous O(page) work, not a guarantee of an interactive
frame time; the caller's flag (§3) can stop it, and scheduling belongs
to the consumer.

Line boxes can be emitted lazily for a half-open vertical window: measure
the whole flow, retain block geometry, emit only intersecting logical line
boxes. Empty windows emit none. This is line-box intersection, not a glyph
ink query; a painter must account for overhang in its overscan.
Every line is measured for overflow even when not retained. Bounds include
signed image margins and fractional replaced geometry, rounded outward to
1/64 px. A line's content advance can be negative with negative image
margins; geometric rectangles still have non-negative extents. This saves
output memory but does not skip the measurement work. The first API repeats
that pass for another window; paragraph checkpoints and incremental layout
are deferred. It has exact total height, unlike a progressively unknown
document tail. A consumer may discard the previous Layout before resize to
avoid overlapping outputs. If it keeps both, it passes the previous
output_bytes in Request.retained_output_bytes; the same admission budget
then covers both results. Width changes that alter active media rules add
stylesheet compilation and cascade costs to the layout-only pass.

Tests cover source ranges and transformations, sibling style sharing and
custom-property differences, CSS used-size examples, inline/block splits,
hidden trees, image placeholders, every refusal edge and deterministic
repeated layout. A small independent prose-wrap oracle checks line breaks
across changing widths, node boundaries and fractional advances. Windowed
output must equal filtering a full layout. No live network test enters the
gate. Corpus HTML/CSS remains under agent scratch, never committed; only
small synthetic reproductions enter the repository.

### Measured corpus and 2 MiB cases

Measured on an AMD Ryzen 7 5700G Linux host, with release optimization,
three fresh processes per page/configuration. Timings below are medians,
RSS is the largest Linux VmHWM across those processes, and MiB means
2^20 bytes. Other host checks ran concurrently: these are observations,
not portable latency thresholds. File reading is outside timed stages;
source strings and compiled sheets remain alive for RSS measurement.

The deterministic font provider returns 0.5 em advances for scalars through
U+00FF and 1 em for other scalars, a 1.25 em line box and a 0.9375 em
baseline, rounded to 1/64 px. It does not measure TrueType parsing, glyph
rasterization or font-cache growth. Separately, 3,500 generated text/width
cases passed against actual td-ui prose::wrap through the scale-one,
16 px metrics adapter. The committed independent word-wrap oracle and
synthetic corpus shapes run without a td-ui dependency or installed fonts.

The eight URLs are the td-dom corpus in td-dom/DESIGN.md, captured again
2026-11-11 05:19 UTC. Each page includes inline and linked stylesheets in
source order, with link/style media attributes wrapped as @media. No
@import, font, image or script is fetched. The four environments match
that earlier experiment: screen 1280x720 light; screen 390x844 dark;
print 1280x720; and desktop interaction with the first link focused and
hovered through its ancestors, plus checked/selected state from attributes.
All 32 page/configuration combinations of that capture complete with zero
refusals in all three trials, including full layout, resize and window
retention. The Guardian's front page fetched on 2026-11-15 styles and
lays out within the default limits (below, "Style-pool work on The
Guardian"). This
is a workload check, not a visual browser-equivalence claim. Unsupported
CSS and the fallback formatting in §5 still apply.

The table shows desktop values except peak RSS, which spans all four
configurations and all layout passes. Setup is parse + stylesheet compile
+ cascade; layout excludes setup. Charged page storage includes DOM,
original input, shared styles, full output and conservative capacities.
It can exceed process RSS because it deliberately overcharges allocations.
As measured it holds td-dom's index and construction's scratch too,
108 bytes a node, which a layout's charge no longer does (§2): 0.07 to
1.30 MiB for these pages.

| Page | HTML bytes | Nodes / styles | Charged page MiB | Setup ms | Layout ms | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Wikipedia | 635,870 | 12,607 / 335 | 17.1 | 409.5 | 8.5 | 14.1 |
| MDN | 328,385 | 8,164 / 118 | 9.4 | 1101.9 | 6.4 | 9.4 |
| Bootstrap | 96,197 | 3,770 / 365 | 11.1 | 83.3 | 2.4 | 11.3 |
| Debian | 15,863 | 713 / 45 | 1.1 | 5.8 | 0.4 | 3.8 |
| kernel.org | 20,436 | 857 / 46 | 1.2 | 3.7 | 0.4 | 3.7 |
| Rust Book | 34,512 | 696 / 34 | 1.1 | 7.6 | 1.0 | 4.3 |
| Guardian | 1,498,839 | 5,745 / 836 | 37.8 | 471.6 | 3.2 | 26.5 |
| GitHub | 320,585 | 3,099 / 205 | 11.3 | 297.2 | 1.4 | 37.9 |

For Wikipedia, MDN, Bootstrap, Debian, kernel.org and Rust Book, both HTML
and raw CSS bundle hashes equal the earlier td-dom capture. The latter
hash concatenates fetched sheets without media wrappers. Guardian and
GitHub changed; their new identities are below. No fetched bytes enter
the repository. MDN's exact-match comparison supports the reduction from
131.9 to 9.4 MiB; the overall 37.9 MiB peak comes from GitHub's retained
stylesheet inputs, not a claim that every page uses less than 38 MiB.

| Page | HTML SHA-256 | Raw CSS bundle SHA-256 |
| --- | --- | --- |
| Guardian | `0ccd62832c437c3fe14c593d77a666ffe5246c37d83805c174e03f3609490690` | `bf9390636879592cbf11920faecf320cb622fd48dcb7cefac319c87c1c6750bc` |
| GitHub | `4a2e1770d3de492ad9e66a6fb89dd258bc1541251504475cc88c611c8190c3cc` | `8ac92b79d9f97b8b98fd27509a2e848a54f9590f57e0a7e7eb89e783fc7697d5` |

The synthetic HTML inputs are exactly 2,097,152 bytes. Prose starts with
`<p>` and repeats `words here `; word starts with `<p>` and repeats `x`;
blocks starts with `<body>` and repeats `<p>abcdefghijklmnop</p>`. Each
repetition stops before exceeding the input ceiling and remaining bytes
are filled with `x`. They use HTML_UA_CSS alone. All have three interned
styles; prose and word have six DOM nodes, blocks has 182,365. Desktop
full layout is followed by a 1280-to-832 px resize with frozen styles and
a 1000..1800 px vertical window at the resized width. Measured again on
2026-11-15 under a load average of 1 to 2, before td-css's faster
cascade, which styles blocks in about 400 ms on the same host
(td-css/DESIGN.md, "Computed-style sharing").

| Shape | Parse / cascade ms | Full / resize / window ms | Full / window lines | Full / resized / window charged MiB | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| Prose | 16.0 / 0.0 | 54.5 / 55.3 / 48.0 | 13,618 / 41 | 14.3 / 18.5 / 10.1 | 12.4 |
| Word | 16.1 / 0.0 | 54.7 / 53.7 / 47.0 | 13,274 / 41 | 14.3 / 18.5 / 10.1 | 12.3 |
| Blocks | 49.2 / 416.0 | 70.5 / 67.0 / 52.9 | 91,181 / 23 | 115.9 / 115.9 / 81.9 | 91.1 |

Charges are recomputed after §2's split; blocks' were 18.8 MiB more,
the index and scratch of its 182,365 nodes.

Resized full line counts are 21,184, 20,561 and 91,181 respectively.
Resized layout work is 6,333,820, 6,291,454 and 4,650,203 visits; windowed
work is identical, confirming that line retention is lazy but measurement
is not. Window output alone is 18,720 bytes for each text case and
36,299,872 bytes for blocks, whose whole-page box geometry remains.
These figures do not justify promising viewport-only CPU cost.

The overlap probe keeps the old full layout while constructing the new
one. It succeeds for prose and word at 22.8 MiB charged, and for blocks
at 184.5 MiB, its 68.6 MiB old output beside a 34.1 MiB tree, a 13.2
MiB styled document and the new 68.6 MiB, in a median 66.9 ms; the
table times the resize after releasing the old output. A denser page's
overlap is refused MAX_PAGE_BYTES (below, "Caller-set limits"), so a
caller should release or budget old output before choosing this path
rather than routinely retrying it. The largest accepted case, that
overlap, plus the 64 MiB working allowance is 248.5 MiB, within the
roughly 300 MB target; observed RSS does not exhaust that allowance.
This is measured coverage, not a worst-case RSS proof.

Committed synthetic regressions exercise the exact 2 MiB prose/word
shapes, DOM byte-range validity and window equivalence, a smaller dense
block shape that refuses overlapping output under a lowered page budget,
and repeated global custom properties shared by differently colored
siblings. Timing and RSS are deliberately not assertions in the gate.

Naming each line's block and end (§4, "Line ends") costs a full layout
little: interleaved with the build before it, pinned to one CPU under a
load average of 4 to 6, medians of seven processes of nine layouts
(2026-11-16), 2 MiB each: prose 50.8 ms before and 51.1 after, one
word 49.1 and 50.0, 46-byte text-page lines at 128 px 82.0 and 83.2,
`ab <img width=20 height=10>` 23.0 and 24.0, and blocks, a block and a
line each, 71.1 and 74.8 (5%).

### Caller-set limits

Each `Limits` field defaults to its constant above and is clamped to it,
with one exception. `Limits::input` defaults to MAX_PAGE_INPUT, a page's
markup, and a caller may raise it to MAX_INPUT, td-html's own input
bound (`MAX_INPUT_BYTES`, from which it is taken), for a text page whose
text is one node (td-web/DESIGN.md §10, "Cuts"); a larger value is
clamped there. Input past the limit is refused `Limit("MAX_PAGE_INPUT")`,
the default's name, which covers a raised limit too. Nothing else is
sized from the input bound: text offsets are `usize` byte ranges and the
input is charged at its length. The other bounds (td-html's MAX_NODES
and MAX_WORK, MAX_BOXES, MAX_CASCADE_WORK, MAX_LAYOUT_WORK, MAX_LINES)
hold 4 MiB of markup as they hold 2 MiB; td-html's MAX_NODES refuses the
blocks shape doubled. What does not hold is a margin under
MAX_PAGE_BYTES: 4 MiB of markup within MAX_NODES can lay out denser than
any 2 MiB shape, and MAX_PAGE_BYTES then refuses it by name (below).

`Limits::styles` is td-css's PoolLimits, passed to the style pool:
its `seed`, None by default, keys the pool's style and custom-map
fingerprints and the slots of its index tables, which without one td-css
takes from addresses ASLR places, with no system call (td-css/DESIGN.md,
"Computed-style sharing"). A caller with entropy of its own passes it
there; td-layout adds none. The pool's work, and so whether a page meets
MAX_POOL_WORK, does not depend on it.

`Limits::style_attributes`, true by default, cascades every element as
if it had no `style` attribute when false: the attribute is not looked
up, so it is neither parsed nor cascaded, and the cascade charges nothing
for it (its bytes stay in the document's charge, as td-html counted
them). Each computed style, the pool and `CascadeWork` equal those of
the same page with the attribute renamed. Removing it instead can change
the tree: td-html's Noah's Ark clause compares formatting elements'
attributes. Selectors still see the attribute, as any other: `[style]`
matches, and td-css's attribute scans count it. td-web's second rung and
`F9` cascade so, with the user-agent sheets alone (td-web/DESIGN.md
§10).

`Request::bytes`, None by default, sets one layout's ceiling on charged
bytes (the tree's and styled document's, `retained_output_bytes` and
the new output together, `Layout::page_bytes`) in place of the styled
document's `Limits::bytes`, fixed when it was cascaded. It is clamped to
MAX_PAGE_BYTES and may lie above or below the styled document's limit;
past it the request is refused `Limit("MAX_PAGE_BYTES")`. It bounds the
conservative admission model (§2), not td-alloc storage. A document
built by `new` or `with_state` owns a budget sized from its own
`Limits::bytes` (§2), so a request raised past that can still fail
`Error::Memory`; only a caller that builds with `with_budget` or
`shared` and sizes that budget gets the whole raise.
`inline::wrap_text`, which takes no request, keeps `Limits::bytes`.

**Measured.** Release, the AMD Ryzen 7 5700G host, deterministic metrics
as above (a monospace advance of 8 px at the 16 px UA size). The 4 MiB
markup rows were measured once each on 2026-11-15 under a load average
of 1 to 2, at 1280 px and at 128 px, td-web's narrowest viewport
(td-web/DESIGN.md §10, "Pipeline and threads"), which HTML_UA_CSS's body
margin leaves 112 px of. Peak is the smallest td-alloc budget, found by
bisection to 64 KiB, under which styling and one layout succeed; the
tree keeps a budget of its own, 24 to 30 MiB for these shapes. A
paragraph shape repeats `<p>` and that many letters; the `style` shape
repeats `<p style="top:0;left:0;color:red">a`.

| 4 MiB markup | Nodes | Parse / cascade / layout ms at 1280 | Lines at 1280 / 128 | Charged MiB at 1280 / 128 | Peak MiB at 1280 / 128 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 26-letter paragraphs | 254,203 | 83.5 / 567.1 / 129.7 | 127,100 / 254,201 | 152.6 / 186.6 | 64.4 / 89.9 |
| 32-letter paragraphs | 215,097 | 76.3 / 507.6 / 128.0 | 107,547 / refused | 138.0 / refused | 58.7 / — |
| 48-letter paragraphs | 152,523 | 62.2 / 340.5 / 109.2 | 76,260 / 305,040 | 114.8 / 216.8 | 49.6 / 126.1 |
| `style` attributes | 239,678 | 59.7 / 740.2 / 43.9 | 119,837 / 119,837 | 172.6 / 172.6 | 62.3 / 62.3 |

"Refused" is `Limit("MAX_PAGE_BYTES")` on one layout. Twenty-six letters
are the densest paragraphs MAX_NODES admits at 4 MiB. Of two layouts
held together, the 48- and 32-letter shapes' at 1280 px fit, at 178.4
and 212.1 MiB; the 26-letter shape's would charge 233.2. Charges are
after §2's split (2026-11-16), which admits two things refused before:
the 48-letter shape at 128 px, whose tree, styled document and layout
hold 114.8 MiB of td-alloc storage, and the 32-letter pair, 106.5 MiB.
At 128 px admission is not monotonic in paragraph length: 26 to 28
letters fit, 29 to 40 are refused, 41 and 42 fit under the line array's
68 MiB step, and 43 and more fit. 43 letters charge the most measured,
222.4 MiB, 168.2 MiB of it the output of 335,544 lines.
4 MiB of prose or of one word charges 28.5 MiB and lays out in about
107 ms (2026-11-14, medians of three).

**Text pages.** A text page is `<!doctype html><plaintext>` and its
text, styled `plaintext{display:block;white-space:pre-wrap;
font-family:monospace}` as td-web's sheet styles it, its text within
td-web's `MAX_TEXT` (4 MiB) and cut where it would lay past
`MAX_TEXT_LINES` (262,144) lines at td-web's least width (below). A
layout's line array dominates its output, and is charged in capacity
steps: it grows by doubling, each step charging twice the capacity it
adds at 136 bytes a line, so up to 131,072 lines charge 34 MiB, to
262,144 68 MiB, to 524,288 136 MiB, and past 524,288 lines a layout is
always refused.
A per-line figure is therefore an artefact of where the count falls: 356
bytes a line at 199,999 lines is the 68 MiB step. A line holding more
than one fragment adds its fragment array, in the same steps: 224 bytes
for two fragments, 448 for three or four, 864 for five to eight. A
preserved tab coalesces with the source text around it (§4, "Tab
stops"), so a line of one text node is one fragment whatever its tabs
(one a 1,024 scalars, so at most `MAX_TEXT` / 1,024, 4,096, more in a
text page, under 1 MiB a layout),
and a tabbed page costs what an untabbed one of its laid lines does,
plus its bytes in the tree's and styled document's charges (about 1
MiB a tab a line at 199,999 lines). Measured on 2026-11-15 with the
deterministic metrics: one layout's charge in MiB, with its lines where
they differ from the source's (a page's lines end in a newline, so its
last line is empty and 199,999 lines lay 200,000), and whether a second
layout fits beside it (`retained_output_bytes`), else one, or refused
(`MAX_PAGE_BYTES`):

| Text page, 0.5 em (8 px) | 1280 px | 320 px | 128 px | 104 px |
| --- | ---: | ---: | ---: | ---: |
| 199,999 lines of 12 letters | 80.4, two | 80.4, two | 80.4, two | 148.4 (399,999), one |
| 199,999 lines of 19 letters | 87.1, two | 87.1, two | 155.1 (399,999), one | 155.1 (399,999), one |
| 46-byte lines, 4 MiB | 54.0 (91,180), two | 88.0 (182,360), two | 156.0 (364,720), one | 156.0 (455,899), one |
| a tab and 12 letters | 81.4, two | 81.4, two | 81.4, two | 149.4 (399,999), one |
| two tabs and 12 letters | 82.3, two | 82.3, two | 82.3, two | 150.3 (399,999), one |
| four tabs and 12 letters | 84.2, two | 84.2, two | 84.2, two | 152.2 (399,999), one |
| `aa\tbb\tcc\tdd\tee`, 199,999 | 82.3, two | 82.3, two | refused | refused |
| `name\tvalue\tthird\tx\ty`, 4 MiB (199,727) | 88.0, two | 88.0, two | refused | refused |
| `a\tb\t…\tj`, 199,999 | 87.1, two | 155.1 (399,999), one | refused | refused |
| 199,999 lines of 12 and one to 4 MiB | 88.0 (210,090), two | 88.0 (241,955), two | 156.0 (313,877), one | refused |

| Text page, 0.6 em (9.6 px) | 1280 px | 320 px | 128 px | 104 px |
| --- | ---: | ---: | ---: | ---: |
| 199,999 lines of 12 letters | 80.4, two | 80.4, two | 148.4 (399,999), one | 148.4 (399,999), one |
| 199,999 lines of 19 letters | 87.1, two | 87.1, two | 155.1 (399,999), one | refused |
| 46-byte lines, 4 MiB | 54.0 (91,180), two | 88.0 (182,360), two | 156.0 (455,899), one | refused |
| a tab and 12 letters | 81.4, two | 81.4, two | 149.4 (399,999), one | 149.4 (399,999), one |
| two tabs and 12 letters | 82.3, two | 82.3, two | 150.3 (399,999), one | 150.3 (399,999), one |
| four tabs and 12 letters | 84.2, two | 84.2, two | 152.2 (399,999), one | 152.2 (399,999), one |
| `aa\tbb\tcc\tdd\tee`, 199,999 | 82.3, two | 150.3 (399,999), one | refused | refused |
| `name\tvalue\tthird\tx\ty`, 4 MiB (199,727) | 88.0, two | 156.0 (399,455), one | refused | refused |
| `a\tb\t…\tj`, 199,999 | 87.1, two | refused | refused | refused |
| 199,999 lines of 12 and one to 4 MiB | 88.0 (212,170), two | 88.0 (251,428), two | refused | refused |

0.6 em is td-ui's monospace face (9.6 px; 9.59375 in 1/64 px). The
leading-tab rows fit at narrow widths because the all-space prefix
before a word that moves is dropped (§4). Each layout took 90 to 190
ms (medians of three), a tabbed page about as long as an untabbed one
(the TSV page at 1280 px 125 ms). Refused, at td-web's narrowest pane
(128 px, 14 columns at 0.5 em and 11 at 0.6 em), and at 320 px with
0.6 em (31 columns), are only pages whose source lines each lay as
several lines: no layout holds more than 524,288 laid lines, and the
count is not the source's. With n source lines, B bytes of
text and c columns an untabbed page lays up to (B + (c - 2) n) / c
lines (471,019 at 14 columns, about 563,000 at 11); a tab can stand
for up to eight columns of one byte, so a tabbed line lays up to its
expanded width over c, and short tab-separated lines wrapping into
three (`aa\tbb\tcc\tdd\tee` is 34 columns, `name\tvalue\tthird\tx\ty`
33, `a\tb\t…\tj` 73) pass 524,288 at 200,000 lines. So a full 4 MiB
untabbed page (199,999 lines of 12 letters and one long line to 4
MiB, or 55,066 of 12 letters and 144,934 of 23) is refused on one
layout at 128 px with 0.6 em and at 104 px with 0.5 em, and so is a
page of short tab-separated lines at 128 px. Two layouts
of any page past 262,144 laid lines do not fit.

So td-web cuts a text page by the lines it lays at its least width,
not by its source lines, and `text::fit` counts them. Given the styled
text page, its metrics, a viewport width and a cap, it sizes the text's
block as layout sizes blocks, through the same `dimensions` from the
viewport down, and lays the one text node through the inline flow
layout lays it with, the same breaking, tab stops and
`MAX_LINE_SCALARS` breaks, into a sink that keeps no line and builds
no fragment, charged work and read for cancellation as a layout is, its
only storage the flow's pending scratch. It lays a source line at a
time, a long one 16 KiB at a time, and stops within that of the cap.
Lines past `Limits::lines` refuse `MAX_LINES` as layout's do, so a cap
within that of them can be refused rather than cut. A text page here
is one text node laid `pre-wrap` or `break-spaces` in an ordinary
block, every other box an ordinary block (no inline box, image, `<br>`,
table or list marker, which would put items or lines in the flow the
count does not see), and its text holds no control scalar but tab,
newline and carriage return: any other shows as U+FFFD, a fragment of
its own (§3), so a line of them would hold several. `fit` refuses
anything else (`Invalid`); td-web's text, its controls but the tab
dropped, meets it.

It returns the lines laid, as `Layout::line_count` counts them for the
kept text, and where the text is cut, a byte offset in its text node:
within the source line that passes the cap, where its first laid line
past the cap would start, when that keeps at least one of its laid
lines; else at the end of the last source line kept, before its
newline. Text cut where a soft break starts a line lays the lines
before it, the line before keeping its hanging spaces, and drops its
source line's indent as it did, but at a word split after that dropped
indent: the kept part of the word then fits beside the indent, which
stays (` abcdefghijklm界` in 14 columns splits before `界`, but
` abcdefghijklm` is one line with its indent). The counts returned
are those the flow had when the cap was passed, with no second pass,
so at such a split `Fit::lines` counts the indent the kept text keeps
and the cut is conservative by that one line; it never counts fewer
than the kept text lays. A line whose
first laid line passes is left out whole, since text cut at its start
would lay one more line, empty or of its indent alone. A page whose
first laid line passes keeps nothing. td-html makes a text page's CR
and CRLF one LF and its NUL U+FFFD; td-web's text, its newlines LF
and its controls dropped, is then its node's, byte for byte, so the
offset is one in td-web's text.

The count td-web needs is one that no wider width exceeds, so a page
cut at its least width fits at every width. Laid lines alone are not
that. In `pre-wrap` a line from a given start ends at its last break
opportunity before the first non-space that does not fit, or before
that non-space when it has none, and a space always fits, hanging
(§4). Every advance but a tab's is fixed and a tab's runs to the next
stop after its pen, so an item's end is non-decreasing in its pen:
starting a line later never moves an item's end right, and a wider
line never fits less. A line's end is therefore non-decreasing in its
start and in the width, and by induction a wider width's k-th line
ends no earlier than a narrower one's, so within a source line it lays
no more lines; `MAX_LINE_SCALARS`'s breaks, which count items, not
widths, keep that (tested, below). The exception is a source line's
all-space start dropped before a word that moves (§4): `   ab c` at
4 columns drops its indent, `ab` moves, and `ab c` is one line; at 5,
`   ab` fits and `c` takes a second. A width drops a line's indent only
if every narrower one does, and keeping it costs at most that one line,
so `Fit::lines` counts the lines laid and each indent dropped, and it
is non-increasing in width and no less than what any wider width lays.
td-web cuts by it. Tests check, over random text pages of words,
runs of spaces and tabs, indents, newlines of every kind (a carriage
return kept in an escaped `pre-wrap` block, which td-html does not
normalise), long words, wide, zero-width and combining scalars, some
past small scalar bounds, under sheets that size the block through
margins, padding, a border and a percentage width, at 19 viewports
from 16 to 1,280 px, that `Fit::laid` equals block layout's line count,
that `Fit::lines` never rises with width and bounds every wider
width's lines, and that a cut text laid again lays what was counted,
or a line fewer within a line, within the cap at its width and every
wider one, while the next source line, or the passing line's next laid
line, would have passed it but for that line.

262,144 is the line array's 68 MiB step. A text page within it lays
at most 262,144 lines at every width, a fragment each at most, so two
layouts, the old beside the new, charge at most 136 MiB beside a tree
and styled document of at most 17 MiB measured, within MAX_PAGE_BYTES.
Counting in td-web's least viewport, 128 px, and laying the kept page at
128 and 1280 px, release, pinned, medians of seven counts and of three
layouts under a load average of 3 to 4 (2026-11-15), each count cut at
262,144 laid lines:

| Text page | Em | Count ms | Kept | Lines at 1280 px | Layout ms at 128 / 1280 px |
| --- | ---: | ---: | --- | ---: | ---: |
| 199,999 lines of 12 and one to 4 MiB | 0.5 | 83 | the lines of 12, 0.8 MiB of the last | 205,506 | 123 / 113 |
| the same | 0.6 | 44 | 131,072 lines | 131,072 | 79 / 58 |
| 55,066 lines of 12 and 144,934 of 23 | 0.5 | 75 | 158,605 lines | 158,605 | 117 / 100 |
| the same | 0.6 | 50 | 105,736 lines and a part | 105,737 | 86 / 54 |
| `aa\tbb\tcc\tdd\tee`, 200,000 | 0.5, 0.6 | 33 | 87,381 lines and a part | 87,382 | 69 / 36 |
| `a\tb\t…\tj`, 200,000 | 0.5, 0.6 | 28 | 52,428 lines and a part | 52,429 | 62 / 27 |
| 46-byte lines, 4 MiB | 0.5 | 73 | 65,536 lines | 65,536 | 116 / 77 |
| the same | 0.6 | 58 | 52,428 lines and a part | 52,429 | 100 / 62 |
| a banner and a 4 MiB minified line | 0.5 | 72 | the banner, 2.8 MiB of the line | 18,726 | 115 / 71 |
| the same | 0.6 | 57 | the banner, 2.1 MiB of the line | 16,385 | 97 / 54 |

Every kept page lays 262,144 lines at 128 px, 68 MiB of output, and
its whole is refused there. The count costs half to two thirds of a
layout of what it keeps at 128 px.

None of the three limits slows a page: interleaved and pinned to one
CPU, the 2 MiB shapes cascade within 2% of two identical builds of the
crate without them, which differ by 1% between themselves, and lay out
within 1% (2026-11-14). A 2 MiB page of 43,690 paragraphs, each with a
two-declaration `style` attribute, cascades in about 255 ms with them
and 200 ms without.

Tab stops and the line bound do not slow a page either. Pinned to one
CPU and interleaved with a build of the crate without them, medians of
27 layouts each at 1280 px under a load average of 1 to 2
(2026-11-15): 2 MiB prose 52.8 ms without and 51.3 with, one word 51.9
and 50.9, blocks 68.8 and 67.5, and 2 MiB of 46-byte text-page lines
54.3 and 53.2. Letter-spacing is read once a run, not once a scalar.
The queue keeps the bound's handling and a line's break out of line
(`#[cold]`, `#[inline(never)]`) and a pending tab's interval in its
advance until it is queued: inlining either, or a pending scalar 16
bytes wider, measured 1 to 40% slower.

Intrinsic sizes and ratios kept through min and max (§5) do not slow
a page without images: pinned to one CPU and interleaved with a build
of the crate without them, medians of five rounds of nine layouts at
1280 px under a load average of 2 to 3 (2026-11-15): 2 MiB prose 51.2
ms without and 51.1 with, one word 50.5 and 50.6, blocks 69.2 and
69.7. 2 MiB of `ab <img width=20 height=10> ` (74,898 images, each
now sized through its attributes' ratio) takes 22.6 and 23.5, or 23.6
with a lookup answering every image.

List markers (§5) do not slow a page's layout, and styling a page
without list items takes about 3% longer, which is accepted: medians of
nine rounds, as in §5, "List markers", 2 MiB prose laid out in 45.6 ms
without them and 45.5 with, one word 44.4 and 44.1, blocks 62.9 and
62.6; the blocks shape styled in 406 ms without and 417 to 420 with,
before td-css's faster cascade.
None of that is the new `list-style-position` longhand (the blocks
shape styled in 402.7 ms with td-css's change alone against 403.8
without it), and the box walk's numbering adds at most 0.3 ms. Built
with `codegen-units=1`, the parent commit, this change and this change
with its numbering removed style the blocks shape alike, in 363.4,
363.9 and 361.9 ms: the difference is where the default build's code
units place the code, not work the change does.

### Cascade work

td-css bounds each compute call, not a page (td-css/DESIGN.md §5), so a
cascade could cost its elements times td-css's bound for one. A styled
document therefore sums each element's `ComputeStats` into `CascadeWork`,
which `cascade_work()` reports, and refuses `Limit("MAX_CASCADE_WORK")`
once the weighted total passes `Limits::cascade_work`, whose default and
maximum is MAX_CASCADE_WORK. It is checked after each element, so the
total overshoots by at most one element's work. An element costs 64
units, plus:

- one for each match step that is not an inspected string byte (selector
  tests, tree hops, index lookups, gathered bucket entries, and the step
  td-css charges for every four attributes a lookup may compare and every
  four bytes a case-insensitive substring test searches), each candidate
  rule and each declaration taken into the cascade, custom ones included;
- one more for each candidate rule for every 2,048 the element gathers,
  since ordering them costs more as there are more;
- six more for each custom declaration and for each inherited custom
  entry copied into a changed map;
- one for every 16 inspected string bytes and every 64 substitution
  payload bytes;
- five for each substitution component, graph visit or reference;
- for each custom name the element declares itself, 6 times the bit
  length of their number less 12, at least 8, since a name costs more as
  the element's own map grows.

Review of the first ceiling found four costs td-css did not count: an
element that changed one custom name copied its whole inherited map
entry by entry, an override walked the replaced value's tokens to
subtract its bytes, an attribute lookup scanned the element's
attributes for one step, and custom winners were inserted one by one
into a sorted map, quadratic in names arriving in descending order.
td-css now merges the inherited map once and counts its copies, keeps
each entry's bytes beside it, charges the scan through
`attribute_count`, and sorts the custom declarations once
(td-css/DESIGN.md §2, §3 and §5). Elements with `display: none` are
still cascaded, since their descendants inherit from them, and are
charged like any other. A second review found the same pattern twice
more: a declaration's references were inserted one by one, in token
order, into a sorted map, and resolution and cycle detection filled
sorted maps in visiting order, so `p{--x:var(--z043000)…var(--z000001)}`
took 206 ms an element at 234 ns a unit. td-css now sorts and dedups each
declaration's references once and indexes the graph, cycle detection
and resolution by a name's position among the element's sorted names.

Every order-dependent structure reachable from one element's cascade,
its style's interning, or td-layout's per-element path, after that
audit:

| Site | Worst order cost | Now |
| --- | --- | --- |
| a declaration's `var()` references (td-css `dependencies`) | quadratic in references | collected, sorted, deduplicated |
| dependency graph, cycle detection, resolved values (`variables::compute`, `cycles`) | quadratic in names, by visiting order | indexed by position |
| the element's custom winners (`compute_inner`) | quadratic in names, descending | collected, sorted, last per name |
| the element's custom map, merged with the parent's | linear, appends | one ordered merge, copies charged |
| winning declarations, computed values, `font` prefix | by property | bounded by the 56-longhand roster |
| candidate rules (`RuleIndex::candidates`) | none: sorted and deduplicated once | charged per candidate, more past 2,048 |
| style pool buckets, custom buckets, map and token identities | a shift of the index's length per new key | charged that length against `MAX_POOL_WORK` |

td-layout's own per-element path and td-dom's projection keep no sorted
structure: they append. The rule index is built when a set compiles,
under compilation's bounds.

The weights are fitted so that no measured shape costs much more than
25 ns a unit; the corpus pages below cost 9 to 23 ns, counting td-dom's
projection and box generation, more for the smallest, whose fixed costs
dominate. Parsing a `style` attribute is outside the weights: the
declarations it gives are counted once taken, but its bytes and tokens
are not. It is linear in the attribute's bytes, which td-css's
MAX_TOKENS and MAX_DECLARATIONS bound for one element and the input for
a page: 4 MiB of `<p style="top:0;left:0;color:red">a` spends about
0.2 s more on its attributes than with them off (740 against 540 ms),
and 4 MiB of elements each holding the 32,767 declarations td-css
admits, about 176 ms of cascade for 395,369 units, some 450 ns a unit
(2026-11-15). Measured as
above (release, the same AMD Ryzen 7 5700G host,
2026-11-14, under a load average of 3 to 4 from other work), each
hostile row times one element computed repeatedly against its sheet,
then a page of 20,000 such paragraphs, or the elements the row names,
built to its result: "before" with the first ceiling (with the second
for the four reference and resolution rows, which shared their cost),
"after" with these fixes; the rows the second review's fixes changed were measured again
under a load average of 5 to 8. Where td-css's own caps now refuse a shape outright, the row
after it is its largest variant they accept.

| Shape, every element | µs an element | Units an element | ns a unit | Before | After |
| --- | ---: | ---: | ---: | ---: | ---: |
| 2,000 universal `:not()` rules | 144 | 16,150 | 8.9 | 1.1 s | 1.2 s |
| 10,000 universal `:not()` rules | 3,198 | 120,158 | 26.6 | 2.5 s | 3.1 s |
| `var()` of 30,000 tokens | 6,564 | 301,571 | 21.8 | 2.6 s | 2.7 s |
| `var()` of 40,000 tokens | 9,743 | 402,040 | 24.2 | 2.8 s | 2.7 s |
| 30,000 declarations in one rule | 715 | 30,154 | 23.7 | 3.2 s | 3.2 s |
| 4,000 custom names on `*` | 3,006 | 349,158 | 8.6 | 3.5 s | 1.2 s |
| 8,000 custom names on `*` | 6,474 | 746,283 | 8.7 | 3.3 s | 1.2 s |
| 8,000 descending custom names on `*` | 6,042 | 746,568 | 8.1 | 6.8 s | 1.2 s |
| 100 custom names declared 320 times each | 4,877 | 229,175 | 21.3 | 13.0 s | 2.6 s |
| 2,400 custom names each a `var()` of a 50-word value | 17,295 | 1,451,813 | 11.9 | — | 1.9 s |
| 2,000 of those, 5,000 inherited names, 15,000 `:not()` rules | 17,688 | 1,422,864 | 12.4 | — | 1.8 s |
| 43,000 descending `var()` references, 140 `<p>` | 1,834 | 882,276 | 2.1 | 27.1 s accepted | 0.25 s accepted |
| the same ascending, 140 `<p>` | 2,080 | 882,276 | 2.4 | 0.96 s accepted | 0.25 s accepted |
| a first name referring to 4,095 later ones, 4,094 between | 6,808 | 910,491 | 7.5 | 4.5 s | 1.0 s |
| the same, the referring name sorting last | 7,747 | 910,491 | 8.5 | 2.5 s | 1.1 s |
| 8,000 inherited names, one changed, 262,000 `<p>` | 1,128 | 48,199 | 23.4 | 242 s accepted | 3.0 s |
| an inherited 130,000-token value overridden, 262,000 `<p>` | 5.2 | 194 | 26.8 | 95 s accepted | 1.6 s accepted |
| 140 rules of 1,000 `[x255]`, 900 256-attribute `<p>` | — | — | — | 113 s accepted | refused at once |
| 500 rules of four `[x255]`, the same 900 `<p>` | 2,050 | 132,150 | 15.5 | — | 1.8 s accepted |
| 1,380 `[t*=aaab i]` against a 100-byte value | — | — | — | 5.6 s | refused at once |
| 1,000 such rules | 468 | 35,650 | 13.1 | — | 1.6 s |
| 470 such rules against 300 bytes, 13,490 nodes | — | — | — | 4.2 s accepted | refused at once |
| 100 `:is()` of 64 attribute tests, 900 256-attribute `<p>` | — | — | — | 1.1 s accepted | refused at once |
| 30 such rules | 402 | 126,990 | 3.2 | — | 0.3 s accepted |
| 2,000 `:not()` rules, 900 256-attribute `<p>` | 469 | 144,150 | 3.3 | 0.4 s accepted | 0.4 s |

"Refused at once" is td-css's `MAX_MATCH_STEPS` on the first element,
in about 2 ms; rows not marked accepted are refused by the ceiling. So
the worst accepted cascade is about 3.5 s on this host, beside what
parsing, td-dom's projection and `MAX_POOL_WORK` cost: the costliest
shapes per unit reach the ceiling in 3.0 to 3.4 s, and loaded runs vary
by a fifth. A lookup is charged for every attribute its element has,
so the 256-attribute shapes are charged more than they cost, at 3 to
15 ns a unit, and a real page's elements, with a handful, about one
step more a lookup. The pages, fetched again with their stylesheets and
cut at MAX_PAGE_INPUT where longer, under the desktop environment of §6:

| Page | Elements cascaded | Units before / after | Cascade ms before / after |
| --- | ---: | ---: | ---: |
| WHATWG HTML, cut | 39,890 | 49,593,658 / 49,593,538 | 657 / 649 |
| Wikipedia, World War II, cut | 16,287 | 36,092,394 / same | 420 / 423 |
| MDN | 5,041 | 31,941,580 / same | 491 / 283 |
| Wikipedia, Asha Bhosle's Hindi songs | 24,629 | 30,565,162 / same | 401 / 397 |
| Wikipedia, Donald Trump, cut | 14,634 | 25,862,994 / same | 317 / 316 |
| Wikipedia, Cascading Style Sheets | 5,834 | 11,195,997 / same | 138 / 136 |
| ECMAScript, cut | 35,164 | 9,609,835 / same | 216 / 216 |
| GitHub | 1,576 | 6,579,517 / same | 75 / 73 |
| Bootstrap | 1,053 | 1,451,233 / same | 31 / 29 |
| Debian, kernel.org, Rust Book | 298 at most | 47,666 at most | 1.3 at most |

These were fetched again for the second review and measured before and
after its fixes; MDN's 5,041 elements declare 229,820 custom names
between them, which position indexing makes cheaper.
The heaviest, the WHATWG specification, stays 2.6 times under the
ceiling, the longest Wikipedia article 3.5 times and MDN 4.0 times. MDN
is the densest, at 6,336 units an element and 98 a byte: at that
density the ceiling is reached at about 20,000 elements or 1.3 MB of
page, and half of it at about 10,000 elements or 650 KB, so a longer
MDN-shaped page has less than twice the margin and a much longer one
steps down. The mobile (390 by 844 px, dark) and print environments
raise no page by more than a fifth, and none nearer the ceiling than
WHATWG's 53.8 million units under mobile, 2.4 times under it. A
Tailwind-style sheet declaring 40 custom names on `*` costs 2,116 units
(22 µs) an element, so the ceiling refuses such a page past about
60,000 elements, in 1.3 s measured. The Guardian's front page is not in
the table; its cascade is measured below. The ceiling, the flag reads
and td-css's new charges cost nothing measurable: the corpus cascades
above agree within run-to-run noise, and when the ceiling was first measured the 2 MiB blocks shape
cascaded in 390 then 395 ms (medians of six interleaved rounds) and the
2 MiB shapes' layouts agreed within 1% with no flag and with an unset
one. Making td-dom's attribute counts inlinable keeps the `:link`
charge off the Wikipedia pages' time, which a call for it raised by a
tenth.

The attribute charge raises td-css's per-element match steps, which
`MAX_MATCH_STEPS` (150,000) bounds for each element, by the attributes
an element carries, which on these pages is 4 to 14 at most. Each
page's costliest element, without the charge and with it (every
element computed against the page's set):

| Page | Most match steps of one element | Under `MAX_MATCH_STEPS` |
| --- | ---: | ---: |
| Wikipedia, World War II, cut | 47,125 → 47,183 | 3.2 times |
| Wikipedia, Asha Bhosle's Hindi songs | 44,953 → 45,011 | 3.3 times |
| GitHub | 44,449 → 44,743 | 3.4 times |
| ECMAScript, cut | 44,212 → 44,212 | 3.4 times |
| Wikipedia, Donald Trump, cut | 43,973 → 44,033 | 3.4 times |
| Wikipedia, Cascading Style Sheets | 43,787 → 43,847 | 3.4 times |
| WHATWG HTML, cut | 15,521 → 15,521 | 9.7 times |
| MDN | 9,174 → 9,186 | 16 times |
| Bootstrap | 8,752 → 8,830 | 17 times |
| The Guardian | 4,042 → 4,094 | 37 times |
| Debian, kernel.org, Rust Book | 1,412 at most | 106 times or more |

No page comes within twice the bound: the charge adds at most 0.7%,
GitHub's, to a page's costliest element.

### Style-pool work on The Guardian

The Guardian's international front page fetched on 2026-11-15
(1,374,527 bytes, HTML SHA-256
`bab3d56cafd93b35c558e00dde7e2472140c4af33b6e1862918f56d25c837028`;
its 20 inline and linked sheets in source order, media-wrapped,
`eceef6f4f780023bd067700bff9e0b498d9e101b8aa413c01163035d54bbdccc`)
styles in 159 to 165 ms with 734 styles and 21,441,319 pool visits, 32%
of MAX_POOL_WORK, the same in every process and with fixed keys, and
lays out at 1280 px in 1.0 ms, 517 lines, 110,955 px tall, under §6's
deterministic metrics. Its elements declare their own custom
properties, so each such element brings a new map with new token arrays
even where its tokens equal an earlier element's: the pool keeps 70
maps and indexes 13,169 token arrays by address, each lookup and
insertion a fixed two visits (td-css/DESIGN.md, "Computed-style
sharing"). Most of the pool's work is traversing those maps and
comparing them with equal ones, linear in their size; a page reaches
MAX_POOL_WORK that way, or MAX_POOL_BYTES first, at some 5,000
distinct maps.

## 7. Independently landable increments

1. Design and AGENTS route, documentation only.
2. Standalone crate, shared computed-style pool and bounded box generation.
3. Source-preserving inline normalization, metrics and line breaking.
4. Block geometry, image placeholders, full/windowed layout and resize.
5. Real-page and 2 MiB cost validation, synthetic regressions and measured
   memory/latency table. Reconcile the 132 MiB baseline after interning.

Flexbox, float exclusion and advanced text need subsequent designs;
painting and td-web's integration are td-web/DESIGN.md §10's. Open for
tables: a windowed layout
holds the lines above its window in the row being laid and in rows a
rowspan holds open (§5, "Tables", "Windows"). A window past 524,288
lines held in one row is refused `MAX_PAGE_BYTES`, as the lines vector
would double past the page budget: on a page that is one cell of a
million lines, any window past 10,485,763 px, about 52% of it.
Filtering a cell's lines at its row's end whatever rowspans remain
open would bound that to the open spans' own lines; a row's own lines
above the window cannot be dropped before the row's height is known.
td-web consumes this library (td-web/DESIGN.md §10, increment 7); its
integration (increment 6) needed a style pool that admits The Guardian's
front page, as td-css's does (§6, "Style-pool work on The Guardian").
Basic table layout, the automatic and fixed algorithms,
`colspan` and `rowspan` within HTML's bounds and MAX_TABLE_COLUMNS,
separated borders with collapse approximated, vertical alignment and
captions, is §5's "Tables". List markers as a fragment kind of their own, outside and
inside, numbered as HTML numbers list items, are §5's "List markers".
Intrinsic sizes for replaced elements supplied by the caller, a change
needing a layout and no cascade, are §5's "Intrinsic sizes". The input
limit a caller sets, the
cascade without style attributes and the byte limit per layout request
are §6's "Caller-set limits"; each line's block and end (`Line::block`,
`Line::end`) are §3's and §4's "Line ends"; eight-column tab stops where
white space is preserved, a tab costing no fragment, and a line past
MAX_LINE_SCALARS cut and marked, or broken where it wraps, rather than
refused, are §4's "Tab stops" and "Long lines". A tabbed text page costs
what an untabbed one of its laid lines does. Counting a text page's
lines in a viewport through layout's own sizing and breaking without
laying it out, and cutting it where the count, its dropped indents
included, would pass a cap, within the source line that passes it, so
the page td-web cuts at its least width fits two layouts at every width,
is §6's "Text pages" (`text::fit`).
