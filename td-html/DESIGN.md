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
