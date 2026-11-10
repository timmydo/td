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
