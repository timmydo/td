# HTML text rendering and bounded references

The existing resident renderer owns its parsed tree and formatted text. Its
plain and rich output share rendering and preserve the existing layout,
link/image and named-reference conventions. Those whole-document APIs allocate;
they are not mail-service memory-admission interfaces.

`reference::Reference` is a separate bounded streaming interface for decoding
one character reference after the caller has consumed its ampersand. The
resident renderer uses this same decoder; the old resident reference scanner
has been removed. Named-reference lookup and numeric codepoint replacement
share the existing table and implementation.

A Reference contains at most 128 bytes, allocates nothing and retains no
source, clock, work meter or authority. It may be cloned as pure parser
progress. The caller owns source identity, ordered input, work/output admission
and final publication. One poll consumes at most one input scalar or emits
at most one output scalar. Resolution performs at most one table lookup over
at most 32 name bytes over at most 512 table entries per turn; suffix flushing emits one byte per turn.
Long numeric runs saturate the codepoint accumulator without retaining digits.
Unknown names flush their bounded literal prefix and leave following source
characters to the caller. A single reference takes linear work in its input
plus at most a fixed number of name-resolution and flush turns.

Names::Html uses the renderer's complete existing table. Names::Basic admits
only amp, lt, gt, quot, apos and nbsp with the same legacy flags. Names are
case-sensitive. Numeric references use the renderer's HTML replacement for
zero, surrogate/out-of-range values and C1 codepoints. The semicolon may be
omitted after numeric references or a legacy named reference. In Attribute
context, a missing-semicolon legacy name is not decoded when its following
character is ASCII alphanumeric or equals; Text preserves the renderer's
existing longest-legacy-prefix behavior.

The caller retains an unconsumed input/last pair. A consumed last scalar closes
input; pending reference output can be drained with None. A nonfinal None
requests input unless the decoder already has output to drain. Complete can
carry the final output scalar, which must be handled before completion; cached
Complete consumes and emits nothing. Input beyond the reference is left
unconsumed. State refusal is terminal and is not an alternative interpretation
of malformed source; malformed references are ordinary literal text or the
specified replacement character.

This reference interface does not tokenize tags or sanitize markup. Its caller
funds every decoder turn under its own original admission. No td-mta dependency
or capability is introduced by this interface.

## Bounded text extraction

`extract::Extractor` implements a streaming text heuristic, independently of the
resident renderer. It retains at most 256 bytes including its shared Reference,
allocates nothing and owns no source or admission authority. Each poll consumes
at most one input scalar and emits at most one output scalar. Work is linear in
source scalars plus the fixed reference-resolution overhead. Callers must fund
all turns, including suppressed text, reference resolution and EOF draining.
They retain an unconsumed input/last pair and handle any emitted scalar
before acting on its accompanying Complete status. Yield asks for another turn;
NeedInput asks for another scalar or explicit EOF. A consumed last scalar closes input; None drains remaining output.
Cached completion consumes and emits nothing. Diagnostics are passive and sticky.

The heuristic strips tags, comments and declarations. Head contents are
suppressed until a closing head tag or a body start; nested head starts do not
create a stack. Inside head, title content waits for its own raw closing name,
so a literal head end inside title cannot release suppression.
Script/style contents use ASCII-insensitive raw closing names followed by HTML
space, slash or greater-than; false prefixes do not end suppression. A slash
on a non-void start tag does not close head/script/style. This is not a DOM,
HTML tree-repair algorithm, renderer or sanitizer, and it performs no fetching,
CSS or script interpretation. Raw text may end at a closing tag inside a script
string, following markup rather than JavaScript quoting.

Ordinary text and alt/title values are emitted in source order. Attributes of
closing tags or suppressed head/script/style starts are ignored. Quoted and
unquoted values each receive a surrounding SP; ordinary attribute names and
values are not output. Duplicate alt/title attributes are all included. Tag
and attribute names are ASCII-insensitive, with fixed ten-byte name retention;
longer names are unknown without retaining their suffix. Inline/unknown tags
produce no separator. Start/end tags address, article, aside, blockquote, br,
div, dl, dt, dd, fieldset, figcaption, figure, footer, form, h1 through h6,
header, hr, li, main, nav, ol, p, pre, section, table, td, th, tr and ul produce
one SP outside suppressed content. No whitespace collapsing occurs here.

The shared Basic reference decoder handles text and attributes in their
respective contexts. Numeric replacement, legacy name prefixes and optional
semicolons follow its rules. Unknown names with no recognized legacy prefix
remain literal; amp/lt/gt/quot/apos/nbsp are the only recognized names. Emitted
NBSP becomes SP and literal NUL becomes U+FFFD with a malformed diagnostic. Decoded less-than and ampersand are text and never retokenized.

An ordinary less-than followed by no tag-opening character has not opened
markup; it is literal text, even though the heuristic reports its malformed
shape. Consuming malformed markup's remaining context applies after an actual
tag, closing-tag, declaration or comment opener. This distinction is the
boundary for the mail policy's bounded-context rule.
An invalid ordinary tag opener emits its less-than literally, leaves the
following scalar unconsumed and sets malformed_markup; a trailing less-than
at EOF is also preserved. Malformed closing openers and declarations consume
through the first greater-than, regardless of quotes. Comments end at two or
more hyphens followed by greater-than; abrupt initial greater-than, one initial
hyphen plus greater-than and --!> also end them, with malformed_markup.
Invalid tag/attribute syntax, including a missing separator after a quoted
value, and unterminated markup set the same sticky diagnostic. Partial visible
attribute values remain provisional output and receive their trailing
separator on EOF; other unfinished markup is suppressed. The diagnostic is
not a browser-conformance verdict: unknown elements and omitted ordinary end
tags do not themselves constitute errors. Unfinished suppressed head/raw-text
contexts do, except that body start closes head as stated above. Any internal
State refusal is terminal. A composing mail cursor must combine these
diagnostics with source decoding and original admission before publishing
output. Native mailbox composition and Mail advertisement remain separate.

## Resident rendering cost

The resident renderer's time and memory are linear in its input plus the
output it builds, which the budget below bounds, apart from two bounded
factors: each run of text and each piece of a wrapped line reckons the
tags around it, a stack of at most some two hundred (element nesting is
kept to 64, and an element adds at most three tags), and a superscript
looks at most 4096 nodes ahead for its digits.

A quote, list item, definition or heading renders its content once: each
of its lines is emitted once, with the prefix of every block enclosing it
composed in front of it as it is emitted, so nesting costs the prefixes
the output carries and no copy of a line per level. Rich output still
gives each enclosing prefix a span of its own on every line, each word of
a wrapped paragraph a span of its own, and each span every tag around
it, a link's or image's target in full, so rich output grows with nesting
depth and with tags around many spans. Plain output keeps no tags while
it renders, since only its text reaches the reader. Equal tag stacks
share an id, so whether text joins the span before it is decided without
comparing tags. A laid-out table cell renders into lines of its own,
which its row then copies side by side, so each of the at most four
levels of laid-out table copies its cells' lines once more. Preformatted
text keeps the column its line has reached for its tab stops.

Each rendering has two limits, `Limits`: the bytes of input it reads and
an output budget. `to_text` and `to_rich` read the whole input within
`OUTPUT_BUDGET`, 128 MiB; `to_text_within` and `to_rich_within` take a
reader's own, sized as below. The output budget is what plain and rich
rendering both spend: a span costs 64 bytes plus its text and, in rich
output, 32 bytes per tag plus a link's or image's target; a line costs
32 bytes plus its spans; a quote, list item, definition or heading costs,
as it opens, its prefixes' text and, in rich output, its tags. Each is
charged before it is built: text as it joins the line being gathered, a
block as it opens, and a finished line, with its prefixes, before any
span of it is made: a rule, a table's border, a link reference, a
laid-out table's row line before its cells' spans are copied into it (the
cells' own lines having been charged as they were made), and each line
of a word too long for the width, split one line at a time. Wrapping
gathers a line's words as pieces before the line is charged, and stops
once those pieces, at 64 bytes each plus their text, pass what the budget
has left, since that line could not be kept. So nothing is built past
the budget but the pieces of the one line being gathered, within what is
left and one word, and what is bounded by the input: its text, the
element tree, plain output's link list and the table of tag stacks seen.

When a charge does not fit, rendering stops. The output is then the lines
kept before that charge, trimmed of trailing blank lines as usual,
followed by one line, `CUT_NOTICE`, saying the rest of the document is
not shown: a single plain span, unwrapped at any width, in no output that
was not cut. The text being gathered when the charge failed is dropped,
and plain output's link references are not appended if they had not
begun. The notice's own cost is set aside from the budget before
rendering, so a cut output still fits it. A rendering within the budget
is therefore unchanged by it, and one past it returns a prefix of the
lines it would have returned and the notice; a single paragraph whose own
spans pass the budget contributes none of its lines, so the notice may
follow only the lines before it, or stand alone. When what the refused
charge was for would have added no line, the notice follows every line
the rendering without a budget has: a trailing blank line, which the
trim drops, or a trailing block that emits none, such as an empty list
item. A caller can tell a cut by the last line, though a document may
itself end with the notice's text.

An input longer than its limit is read to the limit, less a character
the limit splits, rendered as a whole document within the output budget,
and followed by the notice, once if the budget cut it too. What is kept
is then the rendering of the part read, not a prefix of the whole
document's: a table or paragraph the limit splits is laid out as what
was read of it.

Measured on a development host, release build: 2 MiB of one-letter
paragraphs rendered rich at width 0 peaks at some 140 MB flat and some
260 MB inside eight quotes or more, where the budget cuts it. The
costliest hostile shapes tried, plain and rich at widths 0 and 80, each
peak at 365 MB at most and take 0.8 s at most: 2 MiB of one-letter
paragraphs left open (`<p>x`) inside 62 quotes (364 MB; 357 MB inside 8,
278 MB in none, which nothing cuts), or list items (`<li>x`) inside 62
quotes (323 MB); 2 MiB of words inside a link with a 16 KiB target, in a
laid-out table cell or not; 62 quotes inside a link with an 8 MiB target;
alternating bold and italic words inside a 16 KiB link; a link with a
1 MiB target around 130,000 bold letters, 87,000 superscripts or 40,000
images; a 2 MiB preformatted run of bold letters; an 8 MiB table cell; a
2 MiB word with no break in a laid-out table of 64 columns, at width 1,
or inside 38 quotes and a list item (8 MiB there too); 2 MiB of empty
`<style>` elements, in any case, which are skipped to their closing tag
without a copy of the rest of the document (0.03 s). The budget bounds
what a rendering builds, not the memory a reader allows it: these peaks,
some 365 MB at most, are what a caller of `to_text` or `to_rich` with
2 MiB of input must leave room for.

A reader with less room sizes its limits by two terms, measured the same
way. The output term is at most 1.25 bytes of peak per byte of output
budget: the peak's growth from an 8 to a 128 MiB budget, on 2 MiB of the
shapes the budget cuts (short paragraphs in 8 or 62 quotes, a long word
in a 64-column table at width 80 or alone at width 1, a word inside 38
quotes, links with 16 KiB targets around words or a 1 MiB target around
images), was 1.0 to 1.25 times the budget's. The input term is the
element tree, built whole before rendering starts, which the budget does
not bound: at most some 100 bytes of peak per byte of input read, for
dense markup of one-letter elements (`<p>x`, 99 MB a MiB; `<li>x` or
`<tr><td>x`, 80; `<p>x</p>`, 50; `<br>`, 40), some 20 for ordinary
documents (Apache's and Jim Tcl's HTML manuals) and 2 to 3 for text
with little markup. So a rendering's peak is at most some 100 times its
input limit plus 1.25 times its output budget, beside the input the
caller holds; the terms overlap, so the sum overstates it. Measured worst
peaks, with the generated input of four times the limit included, each
of the shapes above tried at widths 0, 20 and 80, plain and rich:

| input limit | output budget | worst peak |
|---|---|---|
| 256 KiB | 6 MiB | 36 MB |
| 256 KiB | 8 MiB | 38 MB |
| 256 KiB | 16 MiB | 48 MB |
| 256 KiB | 32 MiB | 69 MB |
| 512 KiB | 6 MiB | 62 MB |
| 512 KiB | 8 MiB | 64 MB |
| 512 KiB | 16 MiB | 74 MB |
| 512 KiB | 32 MiB | 94 MB |
| 1 MiB | 8 MiB | 116 MB |
| 1 MiB | 16 MiB | 126 MB |
| 1 MiB | 32 MiB | 146 MB |

The worst shape at every pair is one-letter paragraphs left open
(`<p>x`) inside 62 quotes, whose tree is as dense as any and whose lines
each carry 62 marks.

An ordinary document spends at most some 11 bytes of budget per byte of
input at width 80, rich or plain: an 8 MiB budget rendered the 0.72 MiB of two
copies of Jim Tcl's manual whole, and a 16 MiB budget four (1.45 MiB),
but not eight. A reader whose memory cannot hold its largest input's tree
therefore has its input limit, not its budget, cut ordinary documents,
and a budget of some 11 times its input limit cuts only hostile ones.

## Document tree

The document API is a separate, bounded HTML parser for future layout.
The resident renderer and streaming extractor keep their existing parsers:
`to_text`, `to_rich` and both `_within` forms retain byte-identical output,
including malformed markup, invalid UTF-8, nesting and budget cutoffs.
Moving them onto the document tree is outside this contract. Building a
new tree must not silently change mail, news, agent or browser rendering.
The resident cost figures above apply to those APIs, not to this tree.

### Parsing model

The algorithm reference is the [WHATWG HTML parsing chapter](https://html.spec.whatwg.org/multipage/parsing.html),
with [html5lib's tokenizer and tree-construction cases](https://github.com/html5lib/html5lib-tests)
as independent test examples. This is a documented subset, not a claim
of browser conformance. Input is decoded UTF-8 (`&str`); encoding sniffing
belongs to the caller. CRLF and lone CR become LF; an initial BOM is
ignored. NUL handling follows the tokenizer state and tree context.
Scripts never run, the scripting flag is off, and there is no
`document.write`, network access, speculative parser, custom-element
callback, XML parser, shadow DOM or public fragment-parser entry point.

The tokenizer implements data, RCDATA, RAWTEXT, script data (including
escaped and double-escaped forms), PLAINTEXT, tag/end-tag and all attribute
states, comments including bogus and abruptly closed comments, DOCTYPE
including public/system identifiers and force-quirks, and foreign CDATA.
Character references use the complete WHATWG named-reference data,
including two-scalar replacements, longest matches and attribute-context
ambiguity, and numeric replacement rules. This table is separate from the
resident renderer's deliberately frozen reference vocabulary. Malformed
input recovers through the corresponding states; errors are counted,
without storing source strings per diagnostic. Processing instructions
use the historical HTML bogus-comment recovery, including in foreign
content; the newer processing-instruction nodes are outside this subset.

Tree construction supplies implied html/head/body elements, the initial,
before-html, before-head, in-head, in-head-noscript, after-head, in-body,
text, in-table, in-table-text, in-caption, in-column-group, in-table-body,
in-row, in-cell, in-template, after-body and after-after-body modes.
Scope checks, implied end tags, active-formatting reconstruction and the
adoption agency algorithm repair misnested formatting. The adoption
agency's outer loop is bounded to eight iterations as in the algorithm;
formatting duplicates use its three-entry rule. Table text is buffered
through the next non-character token: a non-whitespace run is foster
parented as a whole. Non-table tokens in table context use the same
foster-parent insertion location. The historical in-select and
in-select-in-table modes are retained for ordinary select/option markup;
customizable select parsing is outside this subset.

Template elements own a detached DocumentFragment containing their
contents, with a template insertion-mode stack and formatting marker.
The fragment has no parent and is not among the template's ordinary
children. There is no template instantiation. Frameset construction is
excluded: frameset/frame start and end tags are ignored, their other
contents processed using the current mode; no in-frameset,
after-frameset or after-after-frameset mode is advertised.

Integration-point status is cached when the immutable element is built;
text scanning never looks up the same encoding attribute per character.
SVG and MathML elements retain namespaces, attributes and text, with
foreign self-closing tags and CDATA. HTML integration points and MathML
text integration points switch back to HTML token handling; HTML breakout
tags leave foreign content. SVG element/attribute case adjustments and
foreign xlink/xml/xmlns attribute namespaces are retained. These are tree
semantics only: there is no SVG/MathML renderer, geometry, resource loading
or XML well-formedness check. Unknown HTML elements remain elements.

### API and ownership

`document::parse(&str)` and `parse_with_limits(&str, ParseLimits)` return
`Result<Document, ParseError>`. A refusal returns no partial tree. Nodes
are arena-indexed by opaque `NodeId`; no Rc, recursive node ownership or
mutation API escapes parsing. `Document::node` validates an id and returns
an immutable node. IDs are local to their document, not global identity.
The document owns all names, attributes and text and does not borrow the
input. Iteration in document order follows links, not allocation order:
repair may move a node or allocate an ancestor after its descendants.

Node kinds are Document, DocumentFragment, Element, Text, Comment and
Doctype. Elements expose their local name, namespace (HTML, SVG or
MathML), attributes and optional template-content id. An attribute has a
local name, optional fixed `AttributePrefix` and namespace (none, XML, XMLNS or XLink),
and value; duplicate token attributes keep the first. Text and comment
nodes own their contents. Doctypes retain name, optional public/system
identifiers and force-quirks; the document reports its quirks mode.
Nodes expose parent, first/last child and previous/next sibling ids;
children are iterated without copying a vector. Invalid ids return None.
Text nodes coalesce with the adjacent text at the insertion location.

`html5::Tokenizer` is an incremental token iterator over a bounded `&str`,
with explicit text-mode and foreign-CDATA controls for the tree builder.
It emits owned tags, text, comments and doctypes. It does not guess a
context from a start tag: only the builder knows whether that start tag
was accepted. EOF and terminal refusal remain terminal on repeated calls.
Callers retaining every token must budget that retained storage themselves.

### Admission and work bounds

`html5::ParseLimits` lets callers lower each of the following ceilings;
values above a ceiling are clamped, zero is a valid restrictive limit.
All arithmetic used in admission is checked or saturating. Refusal names
its limit. Budgets are charged before allocating or extending the item
which would exceed them, including discarded duplicate attributes,
formatting clones and implicit elements. They are cumulative, never
refunded after a node is moved or parsing drops a temporary token.

| Constant | Ceiling | What it bounds |
|---|---:|---|
| `MAX_INPUT_BYTES` | 4 MiB | UTF-8 input before preprocessing/allocation |
| `MAX_NODES` | 262,144 | All arena nodes, including document, fragments and repair clones |
| `MAX_ATTRIBUTES` | 524,288 | Token attributes and attributes copied into tree/repair nodes |
| `MAX_TREE_BYTES` | 128 MiB | Cumulative allocation charge described below |
| `MAX_DEPTH` | 256 | Open elements and final parent-chain depth, fragment roots included |
| `MAX_ATTRIBUTES_PER_TAG` | 256 | Attributes encountered on one tag (duplicates included), and attributes retained on one element |
| `MAX_NAME_BYTES` | 1,024 | Tag, attribute and doctype names after preprocessing |
| `MAX_ATTRIBUTE_BYTES` | 1 MiB | Each decoded attribute value |
| `MAX_WORK` | 64 million | Tokenizer scalar transitions plus tree traversal/repair steps |
| `MAX_ADOPTION_PASSES` | 8 | Adoption-agency outer iterations per end tag |
| `MAX_FORMATTING_DUPLICATES` | 3 | Identical active formatting entries after the last marker |

The allocation charge is `NODE_CHARGE` (256 bytes) per arena node,
`ATTRIBUTE_CHARGE` (128 bytes) per token or copied attribute, and
`BYTE_CHARGE` (2 bytes) per byte appended to owned strings. Token text,
comments, names and doctype identifiers use that same string charge;
moving a token's string into a node costs no second string charge.
Attribute prefixes use a fixed enum rather than separately allocated strings.
Attribute and node vector growth is covered by their fixed charges,
parser stacks by the depth/node ceilings. The byte charge allows ordinary
geometric string capacity growth; it is an admission model, not a promise
about the allocator's metadata or the caller's retained source. Text and
comment lengths need no independent ceiling beyond input, work and bytes.
Tree edits that increase depth check the moved subtree as well as the
insertion point. Parent links must remain acyclic on every successful
return; exceeding depth is a refusal, never silent flattening.

The tokenizer consumes input monotonically; reconsume/state transitions
and entity scans cost work too. Tree scans (scope, formatting, ancestors,
reparented descendants) cost work per visited entry. There is no unbounded
recursive call chain or recursive destruction. Limits bound hostile input
independently of how forgiving HTML recovery is. Parse-time and retained
memory measurements on real pages supplement, rather than replace, these
admission rules.

### Styling boundary

A separate std-only `td-dom` crate depends on `td-html` and `td-css` and
owns the adapter described in [td-dom/DESIGN.md](../td-dom/DESIGN.md).
td-html has no CSS dependency; existing consumers gain
no CSS closure. The adapter borrows an immutable Document and exposes
HTML elements through `td_css::Element`, with element-only parent, child
and sibling traversal and text-child detection excluding comments.
Foreign elements and template fragments do not become HTML selector
subjects or leak their descendants into that tree. Structural selectors
observe this projection, so an element with only foreign children is empty.
Navigation and direct-text presence are indexed once under td-dom's own
node, byte and traversal ceilings; CSS callbacks do not scan comments.
HTML a/area elements
with href are links. Caller-supplied hover/focus/checked state remains
stable during a cascade; visited history is never exposed.

An end-to-end test parses a document and computes parents before children,
using td-css's HTML user-agent sheet, an author sheet, inline styles and
inherited values. No existing application consumes this adapter yet.
The future layout consumer chooses when to compute styles and retains
only the computed values it needs; the adapter does not cache every
property on every node.

### Test contract and work order

Before any implementation, a frozen copy of the current resident renderer
is compared byte-for-byte against the working crate over deterministic
malformed/generated documents, depth edges, references, invalid UTF-8,
plain/rich output, widths and input/output limits, plus real pages. Run
that scratch differential at every commit. Its baseline and downloaded
pages are not committed. New small tests are original Rust fixtures
adapted from named html5lib cases, with WHATWG section references; no test
suite is vendored. Tokenizer expectations include duplicates, EOF in each
state family, references in text/attributes, raw-text closing prefixes,
script escapes, comments, doctypes and CDATA. Tree tests report case counts
by insertion mode and explicitly identify excluded frameset, fragment,
scripting and newer customizable-select/processing-instruction cases.

Each configurable bound has below/at/above tests; fixed algorithm bounds
have fixtures exercising the boundary. Tree tests assert bidirectional
links, unique reachability, namespace handling, formatting repair,
foster parenting and template isolation. Real-page measurements use the
eight sites named in td-css/DESIGN.md, with page capture identity, node
and attribute counts, parse-plus-cascade time and peak process memory.

The independently landable work is: (1) this contract and renderer
baseline; (2) tokenizer and references; (3) document tree and mode
fixtures; (4) td-dom adapter and whole-page measurements. A renderer
migration is deferred and requires its own deliberate compatibility
proposal; it is not a fifth implementation step in this workstream.

### Tree-construction coverage

The immutable arena API exposes `Document::root`, checked `node`, `children`
and allocation-order `nodes`, plus `quirks_mode`, `work`, `charged_bytes`
and `errors`. `NodeId::index` is a document-local index for callers' tables.
`Document::errors` combines tokenizer diagnostics with the tree builder's
reported recoveries; it is not a full WHATWG error-code or location oracle.
Element prefixes and namespaces are enums; ordinary attributes have no
namespace. Repeated html/body tags cannot grow an element past the
per-element attribute bound, even when every individual tag fits.

The scratch tree oracle compares full tree shapes against html5lib-tests
revision `9329e64694e7835d0dcff9811e22856ef6ad16f9`. Of 1,488 document
cases run, 1,462 match. Twenty-five exercise newer select-content rules
excluded above; one intentionally exceeds `MAX_NAME_BYTES`. Another
192 fragment, 104 frameset and eight scripting-enabled cases are excluded
before running. Raw fixture bytes preserve CR and trailing input newlines.
The suite and its generated harness are not shipped.

Passing cases that visit each insertion mode are listed below; one case
may visit several modes. The mode instrumentation is scratch-only.
These counts describe tested tree shapes, not exhaustive algorithm or
parse-error conformance. Original small Rust fixtures hold the repair,
foreign-content and template contracts in the crate, beside generated
link-invariant tests and resource edges.

| Insertion mode | Passing cases |
|---|---:|
| Initial / before html / before head / in head / after head / in body (each) | 1,462 |
| In head noscript | 26 |
| Text | 310 |
| In table | 223 |
| In table text | 45 |
| In caption | 21 |
| In column group | 27 |
| In table body | 132 |
| In row | 128 |
| In cell | 97 |
| In select | 37 |
| In select in table | 22 |
| In template | 105 |
| After body | 54 |
| After after body | 76 |

The frameset modes are excluded, and the historical select modes are
intentional. Template contents are separate fragment roots; no fragment
entry point is implied by their presence. The tree destructor drops a flat
arena, including detached formatting nodes, without following child links.
