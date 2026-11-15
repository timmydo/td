# td-layout

## 1. Boundary

td-layout lays out td-html's immutable document with td-css computed values.
It is a library with path dependencies on td-alloc, td-html, td-css and
td-dom, with no external dependencies. It does not fetch, decode images, paint, run scripts, handle input,
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

The caller supplies the Document and compiled StyleSet. StyledDocument
can borrow the document or retain a BudgetShared<Document> through shared().
The shared form has no self-referential borrow and can move to a worker.
It computes parents before children through td-dom's HTML projection and
owns a style pool and flat box/source records. Detached
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

The conservative charged_bytes/output_bytes admission model remains a
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
every possible 2 MiB input is not promised.

The previous td-dom corpus peaked at **131.9 MiB (about 132 MiB)** RSS,
with a full ComputedStyle retained per element. After shared styles and
layout, the new corpus peak is **37.9 MiB**, including compiled sheets,
input strings and resize overlap probes. MDN, the former peak, is now
**9.4 MiB** with byte-identical HTML and CSS. This is a whole-pipeline
comparison: interning, custom-map reuse and suppression of hidden subtrees
all contribute; it does not isolate the saving from interning alone.
Section 6 records the workload, capture identities and limits of this
measurement. The 2 MiB many-block case peaks at 93.3 MiB RSS and charges
167.4 MiB for one retained page, below the 224 MiB admission ceiling.

## 3. Public data and source identity

StyledDocument::new(document, sheets, input_bytes, limits) constructs shared styles and
box records. NodeId always belongs to the retained or borrowed document. StyleId is
local to this styled document; accessors return Option for invalid IDs.
`block::layout` takes the styled document, mutable FontMetrics and a
Request with viewport dimensions, an optional vertical window, the
charged bytes of any previous layout output retained during the call and
an optional cancel flag (below). Its Result owns
geometry and emitted lines, with resource usage and work counters.
BoxGeometry records are in postorder and name DOM elements; this is not
a paint-order API. `output_bytes` excludes the shared styled document,
so it can be passed as `retained_output_bytes` on a resize request. Refusal
is an error, never successful partial output. There is no implicit I/O.
`cascade_work()` reports what the cascade spent (§6, "Cascade work").

**Cancellation.** `Limits::cancel` and `Request::cancel` each take an
optional `&AtomicBool`, which another thread may set while the call runs.
Construction reads it before it starts, before each element's cascade and
every `CANCEL_INTERVAL` (4,096) nodes of box generation; layout reads it at
its first work tick and then once every 4,096 units of `MAX_LAYOUT_WORK`,
which every box, line and scalar visit charges. A set flag returns
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
it within 4,096 units, well under a millisecond. Limits are tested before
the flag, so a call past a limit refuses by name even when cancelled; and
a flag set while the last element cascades, or after layout's last read,
can still return Ok. A caller that must not use superseded work tests its
own flag after the call. td-dom's projection, which
construction builds before the cascade, and `inline::wrap_text`, which has
no request, read no flag. The flag is loaded with relaxed ordering: it
publishes nothing else. A styled document keeps no flag, as `limits()`
shows: each layout names its own.

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
the same units. Values must stay stable within a layout call. Calls may populate a
caller-bounded metrics cache. Font
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
nowrap collapse HTML ASCII whitespace across inline nodes and drop an
initial collapsed space; pre-line also drops collapsed spaces after a
newline, preserves newlines and collapses other spaces; pre and pre-wrap preserve spaces
and newlines. nowrap and pre suppress soft wrapping. Tabs use one space,
as prose does, not CSS tab stops. In preserving modes, CR and CRLF in
DOM text become one newline; prose instead displays a literal CR as
U+FFFD. Other control scalars display U+FFFD.
Preformatted trailing spaces are still dropped to preserve the prose
contract: this differs from CSS pre/pre-wrap. A bare empty inline sequence
does not generate a line; an explicit newline does, and preserves the
following empty line. This differs from prose's empty-input one-line rule.
Text-transform and text-indent are retained in the style but initially
not applied. Block content widths are floored to whole pixels for the
prose-compatible break test; output positions retain subpixel precision.

`inline::wrap_text` accepts source TextRuns, a whole-pixel width and a
StyleId for the line strut. It returns bounded lines and byte/work charges.
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
replaced elements always read their own intrinsic attributes. Styling counts
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
padding, inline relative offsets, generated content and list markers are
deferred. An HTML br uses the explicit-newline rule, including its own
metrics and the following empty line.

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
dimension. Min/max sizes and box-sizing apply; the first model clamps the two
dimensions independently rather than preserving the ratio through those
constraints. Invalid or attribute dimensions over td-css MAX_ABSOLUTE_VALUE
(1,000,000 px) use the corresponding placeholder default. Inline images are atomic; wrapping modes allow a break before or after
them. No image decode or load can change geometry inside a layout call. SVG/MathML remain excluded with the
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
| MAX_CASCADE_WORK | 128,000,000 | weighted cascade units per styled document |
| MAX_LINE_SCALARS | 65,536 | pending single-line scratch |
| MAX_LINES | 1,048,576 | counted lines, emitted or not |
| MAX_FRAGMENTS | 1,048,576 | emitted text/replaced fragments |
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
measurement pass on every width change. This is **not** td-web's current
viewport-only resize cost. In the release-mode 2 MiB fixtures below,
a width-only resize from 1280 to 832 px took median **105–194 ms** with
synthetic font metrics; individual runs ranged from **82 to 226 ms** on
the measured host. These figures exclude HTML parsing, cascade and the
release of the previous output. A full recascade is much more expensive
for many-element pages. This is synchronous O(page) work, not a guarantee
of an interactive frame time; the caller's flag (§3) can stop it, and
scheduling belongs to the consumer.

Line boxes can be emitted lazily for a half-open vertical window: measure
the whole flow, retain block geometry, emit only intersecting logical line
boxes. Empty windows emit none. This is line-box intersection, not a glyph
ink query; a future painter must account for overhang in its overscan.
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
retention. The Guardian's front page fetched again on 2026-11-14 is
refused by `MAX_POOL_WORK` (below, "Style-pool work on The Guardian"). This
is a workload check, not a visual browser-equivalence claim. Unsupported
CSS and the fallback formatting in §5 still apply.

The table shows desktop values except peak RSS, which spans all four
configurations and all layout passes. Setup is parse + stylesheet compile
+ cascade; layout excludes setup. Charged page storage includes DOM,
original input, shared styles, full output and conservative capacities.
It can exceed process RSS because it deliberately overcharges allocations.

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
a 1000..1800 px vertical window at the resized width.

| Shape | Parse / cascade ms | Full / resize / window ms | Full / window lines | Full / resized / window charged MiB | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| Prose | 55.3 / 0.1 | 110.1 / 106.7 / 94.5 | 13,618 / 41 | 14.2 / 17.6 / 10.1 | 12.8 |
| Word | 67.4 / 0.1 | 136.0 / 105.0 / 102.9 | 13,274 / 41 | 14.2 / 17.5 / 10.1 | 12.6 |
| Blocks | 102.8 / 764.1 | 203.1 / 194.3 / 162.0 | 91,181 / 23 | 167.4 / 167.4 / 136.3 | 93.3 |

Resized full line counts are 21,184, 20,561 and 91,181 respectively.
Resized layout work is 6,333,820, 6,291,454 and 4,650,203 visits; windowed
work is identical, confirming that line retention is lazy but measurement
is not. Window output alone is 17,472 bytes for each text case and
56,176,704 bytes for blocks, whose whole-page box geometry remains.
These figures do not justify promising viewport-only CPU cost.

The overlap probe keeps the old full layout while constructing the new
one. It succeeds for prose and word at 21.8 and 21.7 MiB charged. Blocks
correctly refuses MAX_PAGE_BYTES: its 84.7 MiB old output plus a new full
page would exceed 224 MiB. After releasing that output, resize succeeds
at 167.4 MiB; the table times only this successful pass. The rejected
probe separately costs median 112.6 ms. A caller should release or budget
old output before choosing this path rather than routinely retrying it.
Even the largest accepted case plus the 64 MiB working allowance is
231.4 MiB, within the roughly 300 MB target; observed RSS does not exhaust
that allowance. This is measured coverage, not a worst-case RSS proof.

Committed synthetic regressions exercise the exact 2 MiB prose/word
shapes, DOM byte-range validity and window equivalence, a smaller dense
block shape that refuses overlapping output under a lowered page budget,
and repeated global custom properties shared by differently colored
siblings. Timing and RSS are deliberately not assertions in the gate.

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
| winning declarations, computed values, `font` prefix | by property | bounded by the 55-longhand roster |
| candidate rules (`RuleIndex::candidates`) | none: sorted and deduplicated once | charged per candidate, more past 2,048 |
| style pool buckets, custom buckets, map and token identities | a shift of the index's length per new key | charged that length against `MAX_POOL_WORK` |

td-layout's own per-element path and td-dom's projection keep no sorted
structure: they append. The rule index is built when a set compiles,
under compilation's bounds.

The weights are fitted so that no measured shape costs much more than
25 ns a unit; the corpus pages below cost 9 to 23 ns, counting td-dom's
projection and box generation, more for the smallest, whose fixed costs
dominate. Measured as above (release, the same AMD Ryzen 7 5700G host,
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
the table: `MAX_POOL_WORK` refuses it with or without this ceiling
(below). The ceiling, the flag reads and td-css's new charges cost
nothing measurable: the corpus cascades above agree within run-to-run
noise, and when the ceiling was first measured the 2 MiB blocks shape
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

The Guardian's international front page fetched on 2026-11-14 (1,404,591
bytes, HTML SHA-256 `9e295b8cbef09d6f5b6f490febc4582902bbafb7d521d3f5e58e226de2835278`;
its 20 inline and linked sheets concatenated in source order without media
wrappers, `8f3ffe4ea4dda34b6c2dd4313564ad1fae15ce9d2fd69f714c353cf6e4a62eef`)
is refused `Css(Limit("MAX_POOL_WORK"))` in the desktop environment, with
or without the cascade ceiling, as was an earlier fetch that day. Its
cascade is not costly: with the pool's work counter unbounded, in scratch
instrumentation only, the styled document completes in 264 ms with 804
styles and 8,856,857 cascade units. The pool's work comes to 123,125,413
visits, 1.83 times MAX_POOL_WORK, and 102,452,455 of them are its
token-identity index. The pool records each distinct custom-token
allocation by address and charges each insertion the index's current
length, the linear shift of its sorted map, so the page's 14,314 distinct
allocations cost about 14,314²/2. They come from the 78 custom maps the
pool retains of the 543 that were new by address (465 equalled one already
kept), about 183 fresh allocations each: an element that declares custom
properties itself (88,322 local names over 4,176 elements) gets new token
arrays even where their tokens equal an earlier element's, since identity
is by address. Traversing those maps (10.7 million visits) and comparing
them with equal-hash maps (9.1 million) make most of the rest. A page
reaches the bound at about 10,000 such allocations; the 2026-11-11 capture
stayed under it.

## 7. Independently landable increments

1. Design and AGENTS route, documentation only.
2. Standalone crate, shared computed-style pool and bounded box generation.
3. Source-preserving inline normalization, metrics and line breaking.
4. Block geometry, image placeholders, full/windowed layout and resize.
5. Real-page and 2 MiB cost validation, synthetic regressions and measured
   memory/latency table. Reconcile the 132 MiB baseline after interning.

Tables, flexbox, float exclusion, advanced text, painting and td-web
integration need subsequent designs. No existing consumer switches to this
library as part of these increments. td-web's integration
(td-web/DESIGN.md §10, increment 6) needs intrinsic sizes for replaced
elements supplied by the caller, list markers as a fragment kind, a
basic automatic table layout (rows and cells, colspan and rowspan
within bounds), an input limit the caller sets (up to 4 MiB for a text
page), a style pool that admits The Guardian's front page, whose
token-identity index charges each insertion its length (§6, "Style-pool
work on The Guardian"), a line past MAX_LINE_SCALARS cut and marked rather than
refused, eight-column tab stops where white space is preserved, a
cascade without style attributes, and a byte limit per layout request.
