# td-layout

## 1. Boundary

td-layout lays out td-html's immutable document with td-css computed values.
It is a std-only library with path dependencies on td-html, td-css and
td-dom. It does not fetch, decode images, paint, run scripts, handle input,
or depend on td-ui. Nothing in td-web consumes it yet. The existing HTML
text and rich renderers remain independent and byte-identical.

The first formatting model is horizontal, left-to-right normal flow. Its
references are CSS 2.2 [box generation and normal flow][flow], [used sizes
and line height][sizes], and [CSS Text 3][text]. This is an explicitly
bounded subset, not a claim to implement browser layout generally.

[flow]: https://www.w3.org/TR/CSS22/visuren.html
[sizes]: https://www.w3.org/TR/CSS22/visudet.html
[text]: https://www.w3.org/TR/2026/CRD-css-text-3-20260814/

## 2. Page ownership and shared styles

The caller owns the Document and compiled StyleSet. StyledDocument borrows
the document, computes parents before children through td-dom's HTML
projection, and owns a style pool and flat box/source records. Detached
template contents and foreign subtrees have the same exclusion as td-dom.
The caller supplies HTML_UA_CSS explicitly. Inline style attributes join
the normal cascade. The root's computed font size supplies rem units.

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

The target is approximately 300 MB for a 2 MiB HTML page's DOM, retained
styles and layout, including a working allowance. This is the share used
by td-html rendering and the kept page in td-web/DESIGN.md §3, within the
640 MiB memory-high and 768 MiB memory-max jail. It is not an additional
300 MB on top of the old rendered page. A future consumer must release or
account for the old representation before admitting a replacement.

MAX_PAGE_BYTES is 224 MiB of charged retained storage. Admission includes
the document's conservative charged_bytes, td-dom's navigation arrays,
style-pool allocations, node/style/box arrays, line records and fragments.
The original 2 MiB input is charged too while retained by the caller.
An additional 64 MiB working allowance brings the target to 288 MiB
(302 MB). It covers one cascade candidate, wrapping scratch and allocator
overhead. Charging precedes retained allocations; capacities, map overhead
and nested token/string allocations count. Shared allocations count once.
Counts and byte limits both apply, and caller limits may only lower them.

This charge is an admission model, not an exact allocator/RSS measurement.
The separately owned compiled stylesheets, font caches, image pixels,
framebuffer and concurrent network bodies still belong in the consumer's
jail ledger. In particular, a stylesheet compilation peak is not hidden
inside the 300 MB claim. Validation measures both page storage and fresh
process RSS including sheets. An unbounded caller-owned font cache cannot
be made bounded by this library. Hostile pages can be refused; accepting
every possible 2 MiB input is not promised.

The previous td-dom corpus peaked at **131.9 MiB (about 132 MiB)** RSS,
with a full ComputedStyle retained per element. That is the comparison
baseline, not a result for shared styles. The validation increment must
restate this figure beside the measured shared-style peak, style counts,
charged storage and process RSS; it must not infer a saving from interning
alone. It also measures 2 MiB text and many-element shapes, including the
transient candidate and resize overlap. Any measured violation of the
working allowance requires a smaller admission limit or a bounded
allocation change before this workstream is complete.

## 3. Public data and source identity

StyledDocument::new(document, sheets, input_bytes, limits) constructs shared styles and
box records. NodeId always belongs to the borrowed document. StyleId is
local to this styled document; accessors return Option for invalid IDs.
The public layout entry takes the styled document, viewport dimensions,
mutable FontMetrics, and an optional vertical window. Its Result owns
geometry and emitted lines, with resource usage and work counters. Refusal
is an error, never successful partial output. There is no implicit I/O.

Every TextFragment carries a DOM **text NodeId and UTF-8 byte range** into
that node, its StyleId, position and extent. Offsets are into decoded DOM
text, not source HTML bytes. A fragment cannot cross text nodes. Contiguous
unaltered source may coalesce; a collapsed space or replacement scalar
retains the original byte range and an explicit display transformation.
Suppressed whitespace has no fragment. Generated markers, line breaks and
replaced-element records are distinct kinds and never pretend to be DOM
text. Selection/find can traverse the DOM independently of the viewport;
link focus can walk the fragment's ancestors. History keeps a node/range
place within this document, rather than a fragile wrapped-line number.
Reparsing creates new identities; cross-navigation place restoration is a
consumer policy. Layout does not implement td-web's selection or history.

Geometry uses CSS pixels with subpixel precision and finite checked bounds.
There is no per-glyph retained page array. A future painter can replay a
fragment through the same metrics/whitespace policy. Box geometry names
the originating element and its style, including backgrounds and borders
for a future painter; no display list or pixel buffer is allocated here.

## 4. Fonts and inline flow

FontMetrics has two operations: advance(style, scalar), returning an
advance in 1/64 CSS px, and line(style), returning height and baseline in
the same units. Calls may populate a caller-bounded metrics cache. Font
selection uses the computed family, size, weight and style. A deterministic
test provider makes layout tests independent of installed fonts.

An adapter around td-ui::prose::Metrics maps family/style, calls advance,
and multiplies line().height and line().baseline by 64. At scale one and
16 px this is a direct adapter of a few lines. Other device scales divide
the returned device units by scale. Since prose::Metrics currently measures
only its fixed 16 CSS px face, an adapter for other font sizes must scale
both advances and the line box by font_size / 16, or supply a face measured
at that size. This mathematical adapter does not claim that td-ui's current
fixed-size painter can draw arbitrary sizes. Painting that agrees with
these metrics belongs to the later consumer workstream.

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
nowrap collapse HTML ASCII whitespace across inline nodes; pre-line
preserves newlines but collapses spaces; pre and pre-wrap preserve spaces
and newlines. nowrap and pre suppress soft wrapping. Tabs use one space,
as prose does, not CSS tab stops. Other control scalars display U+FFFD.
Preformatted trailing spaces are still dropped to preserve the prose
contract: this differs from CSS pre/pre-wrap. A bare empty inline sequence
does not generate a line; an explicit newline does, and preserves the
following empty line. This differs from prose's empty-input one-line rule.
Text-transform is retained in the style but initially not applied.

Line height uses the largest participating font ascent/descent and CSS
line-height, with half-leading around the baseline. Inline replaced elements
align to the baseline. Initial text-align supports left/start, right/end
and center; justify uses left. Vertical-align other than baseline is
deferred. Letter spacing contributes to scalar advances, clamped so an
advance cannot be negative. Inline non-replaced margins, borders and
padding, generated content and list markers are deferred.

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
and their margins add. Relative offsets move visual geometry, not the
space reserved in flow. Overflow is reported geometrically; clipping,
scroll containers and stacking contexts are consumer/future work.

Tables, flex and inline-block formatting are not implemented: those
containers use ordinary block flow and their descendants remain visible.
List items use block flow without synthetic markers. visibility:hidden
retains geometry and style so the eventual painter can suppress it.
Floats use their computed blockified display in ordinary flow with **no
exclusion region**; clear has no effect. Float placement is a later
increment, not silently approximated by text wrapping around placeholders.

HTML img is a replaced element even when no image bytes are available.
Non-negative finite width/height attributes provide placeholder intrinsic
dimensions. With neither dimension it uses 300 by 150 CSS px; a single
attribute replaces that dimension and leaves the other default. These are
placeholder dimensions, not a guessed decoded aspect ratio. CSS dimensions
override attributes; a known two-attribute ratio supplies an auto opposite
dimension. Min/max sizes and box-sizing apply. No image decode or load can
change geometry inside a layout call. SVG/MathML remain excluded with the
td-dom projection; object/embed/canvas have no content-specific renderer.

## 6. Work bounds, resize and validation

All limits are named constants and tested at and beyond their edges.
The layout limits supplement, not weaken, td-html and td-css limits.

| Limit | Ceiling | Resource |
| --- | ---: | --- |
| MAX_PAGE_INPUT | 2 MiB | original HTML admission |
| MAX_PAGE_BYTES | 224 MiB | combined charged retained page |
| WORKING_BYTES | 64 MiB | measured transient/allocator allowance |
| MAX_BOXES | 262,144 | box/source records |
| MAX_STYLES | 16,384 | distinct complete computed styles |
| MAX_POOL_BYTES | 64 MiB | style pool within page budget |
| MAX_POOL_WORK | 67,108,864 | pool field/component and string-byte visits |
| MAX_STYLE_COMPARISONS | 1,048,576 | pool equality probes |
| MAX_DEPTH | 256 | layout ancestor stack |
| MAX_LAYOUT_WORK | 64,000,000 | visited records/scalars per layout |
| MAX_LINE_SCALARS | 65,536 | pending single-line scratch |
| MAX_LINES | 1,048,576 | counted lines, emitted or not |
| MAX_FRAGMENTS | 1,048,576 | emitted text/replaced fragments |
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
measurement pass on every width change. This is **not** td-web's current
viewport-only resize cost. The validation increment measures release-mode
2 MiB prose, a 2 MiB word and many short blocks at two widths, reporting
wall time, work, peak RSS and the cost of retaining the old result while
building the new one. Numerical latency is deliberately not claimed before
that experiment; these measurements are a completion requirement.

Line boxes can be emitted lazily for a vertical window: measure the whole
flow, retain block geometry, emit only intersecting lines. This saves
output memory but does not skip the measurement work. The first API repeats
that pass for another window; paragraph checkpoints and incremental layout
are deferred. It has exact total height, unlike a progressively unknown
document tail. A consumer may discard the previous Layout before resize to
avoid overlapping outputs, and must account for overlap if it keeps both.

Tests cover source ranges and transformations, sibling style sharing and
custom-property differences, CSS used-size examples, inline/block splits,
hidden trees, image placeholders, every refusal edge and deterministic
repeated layout. A small independent prose-wrap oracle checks line breaks
across changing widths, node boundaries and fractional advances. Windowed
output must equal filtering a full layout. No live network test enters the
gate. Corpus HTML/CSS remains under agent scratch, never committed; only
small synthetic reproductions enter the repository.

## 7. Independently landable increments

1. Design and AGENTS route, documentation only.
2. Standalone crate, shared computed-style pool and bounded box generation.
3. Source-preserving inline normalization, metrics and line breaking.
4. Block geometry, image placeholders, full/windowed layout and resize.
5. Real-page and 2 MiB cost validation, synthetic regressions and measured
   memory/latency table. Reconcile the 132 MiB baseline after interning.

Tables, flexbox, float exclusion, advanced text, painting and td-web
integration need subsequent designs. No existing consumer switches to this
library as part of these increments.
