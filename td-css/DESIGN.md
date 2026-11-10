# td-css

td-css is td's CSS engine: owned syntax, selector matching and computed
styles, without layout, painting, fetching or a document tree. It is a
standalone, std-only library, with no dependencies, no unsafe code and no
production panics or unchecked indexing. It has no consumer yet. In
particular it neither depends on nor changes td-html, td-web, td-news,
td-mail or td-ui. A later td-web layout engine may consume its values.

## 1. Scope and standards

This is a bounded partial implementation, not a browser-conformance claim.
The algorithm references are [CSS Syntax Level 3, 24 December 2021](https://www.w3.org/TR/2021/CRD-css-syntax-3-20211224/),
[Selectors Level 3](https://www.w3.org/TR/2018/REC-selectors-3-20181106/),
the logical selector functions and attribute flags in
[Selectors Level 4, 22 January 2026](https://www.w3.org/TR/2026/WD-selectors-4-20260122/),
and [Cascade Level 4](https://www.w3.org/TR/2022/CR-css-cascade-4-20220113/).
The style-rule body uses declaration-list parsing rather than the
style-block entry point that also admits nesting. Rules inside style
rules are not interpreted as nested selectors.

Input is already decoded UTF-8 (`&str`). Encoding sniffing and decoding
bytes belong to the caller. Preprocessing folds CRLF, CR and form feed to
LF, and replaces NUL with U+FFFD. Rust strings cannot contain surrogates.
An initial BOM is removed. Comments disappear without adding whitespace.
Tokens retain decoded names, strings and URLs, number representations and
integer flags, and the ID/unrestricted distinction on hash tokens.
Escapes, bad strings, bad URLs, CDO/CDC and EOF recovery follow Syntax §4.
Numbers are represented as text at the syntax layer; property parsing
refuses non-finite arithmetic rather than giving an overflowed float a
meaning.

The syntax layer implements the stylesheet, declaration-list and
component-value-list entry points of Syntax §5. Functions and simple
blocks own their component values. Unclosed blocks/functions end at EOF;
an incomplete qualified rule without a block is discarded. A declaration
without a name/colon is discarded through its next top-level semicolon.
Its terminal, ASCII-insensitive `!important` is removed into a flag.
Whitespace inside values is preserved except leading/trailing whitespace.
Parse errors are recovered; resource exhaustion is a distinct fatal error
and returns no partial result. Diagnostics count recovered errors without
retaining attacker-controlled error strings.

At-rules retain their name, prelude and optional block as syntax, including
unknown rules. Declaration-list at-rules are consumed and ignored. Style
compilation skips every at-rule, including `@import`, `@media`, `@supports`,
`@font-face`, `@page`, `@namespace` and `@layer`; their contents never leak
into the surrounding cascade. No URL is fetched. Custom properties are
ordinary syntax but have no computed interpretation; `var()`, `calc()`,
animations, transitions, counters and generated content are outside this
contract. Unsupported selectors invalidate their rule, unsupported
properties or values only their declaration.

## 2. Selectors and the element boundary

Selectors address HTML elements in a tree supplied by the caller. Type
and attribute names compare ASCII-insensitively; IDs, classes and
attribute values are case-sensitive unless an explicit `i` flag requests
ASCII-insensitive matching (`s` explicitly requests sensitive matching).
Host-language enumerated attribute-value folding is not implemented.
There are no namespace selectors, pseudo-elements, interaction states,
language selectors or shadow trees.

Supported forms are the universal and type selectors, ID and class,
attribute presence and `=`, `~=`, `|=`, `^=`, `$=`, `*=`, descendant,
child, adjacent-sibling and subsequent-sibling combinators; `:root`,
`:empty`, `:first-child`, `:last-child`, `:only-child`, `:first-of-type`,
`:last-of-type`, `:only-of-type`, and all four `:nth-*` child/of-type
forms with the Syntax §6 An+B grammar (no `of <selector>` clause).
`:empty` follows Level 3: any nonempty text, including whitespace,
prevents a match; comments do not. Structural positions count only
elements; a root is an only child in the matching model.

The named Level 4 subset is `:is()`, `:where()`, selector-list `:not()`
and attribute `i`/`s`. `:is` and `:where` discard invalid list members;
`:not` and ordinary lists are strict. Forgiving functions still require
Syntax's `<any-value>`: bad strings, bad URLs and unmatched closers,
including inside nested components, invalidate the function. An empty
forgiving list matches nothing. Each may hold complex selectors. `:has`, relative selectors,
nesting `&`, and the column combinator are unsupported.

Specificity is an ordered `(ids, classes, types)` tuple, never a decimal
encoding. Attribute and ordinary pseudo-class selectors count as classes.
`:is` and `:not` take the maximum specificity of their arguments, even
when a different argument matches; `:where` contributes zero. A rule
with a selector list uses the greatest specificity of its matching
members. `:link` matches any link; `:visited` always fails. The interface
has no visited-history query, so history cannot change any computed value,
including through negation or logical functions.

`Element: Clone` is a cheap handle into the caller's stable tree. It
exposes `tag_name`, `attribute`, `parent`, `previous_sibling`,
`next_sibling`, `first_child`, `has_text`, and `is_link`. Attributes are
queried by ASCII-lowercase name, including `id` and `class`. The caller
ignores comments in `has_text`, supplies element-only sibling/child
links, and keeps these observations stable for one operation. It owns
HTML link semantics. No references or handles escape a matching call.
Callbacks must return promptly; td-css cannot bound time spent inside a
caller callback. Traversal is budgeted even if a faulty tree cycles.

## 3. Cascade

Sheets have an explicit `Origin`: user-agent, user or author. The order
from weakest to strongest is normal UA, normal user, normal author,
important author, important user, important UA. Inside a rank, inline
author declarations outrank selectors; otherwise specificity wins, then
later sheet/rule/declaration order. The caller supplies sheet order.
Inline styles have normal or important author origin, and do not outrank
important user or UA rules. There are no layers, scopes, presentational
hints, animations or transitions.

Only valid property values enter the cascade: a later invalid declaration
cannot erase an earlier valid one. Supported shorthands expand before
selection. Every implemented property accepts `initial`, `inherit` and
`unset`; `revert` and `revert-layer` are unsupported. Inheritance uses a
parent's computed value, never reparses the parent's specified value.
An absent parent supplies the initial value. Font size computes before
font-relative lengths; color computes before `currentcolor` references.
The caller computes parents before children and supplies the root's
computed font size for descendant `rem` values. An absent parent identifies
the root: its own `font-size` resolves `rem` against the initial 16px,
then its other properties use its newly computed font size, as specified
by [CSS Values Level 3 §5.1.1](https://www.w3.org/TR/2022/CR-css-values-3-20221201/#font-relative-lengths).

## 4. First computed property set

Values are typed and deterministic; absolute lengths use CSS px (96 per
inch), independent of display scale. Accepted units are px, pt, pc, in,
cm, mm, q, em and rem; bare zero is a length. Percentages on margin,
padding, width and height stay percentages for layout to resolve.
Font-size percentages and em use the parent's computed font size; other
em lengths use the element's computed font size. Unsupported units and
functions invalidate the declaration. Negative margins are valid;
negative font sizes, padding, border widths and dimensions are not.

| Property | Accepted values | Initial; inherited |
|---|---|---|
| display | none, inline, block, inline-block, list-item, table, inline-table, table-row-group, table-header-group, table-footer-group, table-row, table-cell, table-column-group, table-column, table-caption, flex, inline-flex | inline; no |
| color, background-color | sixteen CSS basic names plus orange and rebeccapurple, transparent, currentcolor, #rgb/#rgba/#rrggbb/#rrggbbaa | black / transparent; color only |
| font-family | comma-separated quoted strings or identifier sequences, including generic families | serif; yes |
| font-size | lengths, percentages, xx-small through xx-large, smaller, larger | 16px; yes |
| font-weight | normal, bold, bolder, lighter, integers 100 through 900 in steps of 100 | 400; yes |
| font-style | normal, italic, oblique | normal; yes |
| text-decoration | none or a nonrepeating combination of underline, overline, line-through | none; no |
| white-space | normal, pre, nowrap, pre-wrap, pre-line, break-spaces | normal; yes |
| margin-{top,right,bottom,left} | lengths, percentages, auto | 0; no |
| padding-{top,right,bottom,left} | lengths, percentages | 0; no |
| border-{top,right,bottom,left}-width | lengths, thin, medium, thick | 3px; no |
| width, height | lengths, percentages, auto | auto; no |
| list-style-type | none, disc, circle, square, decimal, decimal-leading-zero, lower-roman, upper-roman, lower-alpha, upper-alpha | disc; yes |
| text-align | start, end, left, right, center, justify | start; yes |

`margin`, `padding` and `border-width` accept one to four values in TRBL
order. Other shorthands are unsupported. `text-decoration` here is the
line-set subset, without style, color or thickness. Its propagation to
descendants is a layout/painting concern, not inheritance. Border widths
are computed nominal widths; border styles are not implemented, so the
future layout engine must not infer a visible border from width alone.
Display computes to the selected keyword; blockification needs the later
position/float/layout contract. Font keyword sizes are 9, 10, 13, 16,
18, 24 and 32px; smaller/larger multiply the parent's size by 1/1.2 or
1.2. Thin/medium/thick are 1/3/5px. These are td's UA choices.

Recognized generic font families are serif, sans-serif, monospace,
cursive, fantasy, system-ui, ui-serif, ui-sans-serif, ui-monospace and
ui-rounded. Quoted names remain named families even when they spell a
generic keyword. Unquoted identifier sequences are joined by single
spaces; CSS-wide and generic keywords cannot be words of such a name.

The embedded `HTML_UA_CSS` supplies common block elements, hidden metadata
and script/style, body inset, paragraphs/headings/lists, monospace code
and pre, emphasis, links and table display roles. It is an ordinary UA
sheet parsed through the same engine, explicitly supplied by the caller;
no hidden rules are injected. It does not replace HTML semantics or claim
the complete HTML rendering stylesheet.

## 5. Bounds and failure

All limits are compiled constants, not caller-adjustable. They apply
before the allocation or recursive step they guard. A fatal `Error`
names the exhausted bound; callers must report refusal rather than treat
it as a stylesheet matching nothing.

| Constant | Limit and accounting |
|---|---|
| MAX_INPUT_BYTES | 1 MiB per syntax input |
| MAX_TOKEN_BYTES | 16 KiB per decoded token string or numeric representation |
| MAX_TOKENS | 131,072 emitted tokens per input, whitespace included |
| MAX_NESTING | 32 function/block levels, also on skipped syntax |
| MAX_RULES | 8,192 top-level rules per sheet and compiled rules per style set |
| MAX_DECLARATIONS | 32,768 declarations per list, expanded declarations per style set, and expanded inline declarations per compute call |
| MAX_SELECTORS | 256 selector-list members, including nested lists |
| MAX_SELECTOR_PARTS | 1,024 simple selectors and combinators per parsed list |
| MAX_SELECTOR_DEPTH | 8 nested logical functions |
| MAX_COMPOUNDS | 64 compounds per complex selector |
| MAX_MATCH_STEPS | 100,000 selector tests, tree hops and inspected string bytes per public match or compute call |
| MAX_ELEMENT_BYTES | 65,536 bytes per observed tag or attribute string |
| MAX_NTH_COEFFICIENT | 1,000,000,000 absolute magnitude for each An+B coefficient |
| MAX_SHEETS | 64 sheets per style set |
| MAX_STYLE_BYTES | 4 MiB total sheet source bytes per style set |
| MAX_FAMILIES | 32 names in a font-family value |
| MAX_ABSOLUTE_VALUE | 1,000,000 for computed px, percentages and supplied root font size |

Recovered syntax errors use a saturating counter. Tokenization and syntax
parsing take linear work in admitted input plus owned output; selector
compilation is bounded by the token and selector limits. Matching can
backtrack over ancestors or siblings, but shares one work counter across
logical functions and every rule of a compute call. Recursive matching
depth is bounded by compounds and logical depth, not document depth.
Attribute scans are bounded by MAX_ELEMENT_BYTES per observed string and
charged by byte length to the same work budget. Substring matching uses
linear string search, ASCII-folding bounded strings for an `i` test.
Numeric input may be larger than a supported value; finite values beyond
the computed bound are clamped, without wrapping or creating NaN.
Memory allocation failure remains the Rust allocator's process-level
failure; this is an admission contract, not fallible global allocation.

## 6. Public API and tests

`syntax::tokenize`, `parse_stylesheet`, `parse_declarations` and
`parse_components` accept text and return owned, inspectable results.
Syntax objects expose immutable views; parsing never accepts an arbitrary
caller-constructed recursive AST. `selectors::SelectorList::parse` and
`matching_specificity` return a parsed list and `Result<Option<Specificity>>`.
`StyleSet::new` compiles ordered `(Origin, &str)` sheets; `compute` takes
an element, optional parent `ComputedStyle`, root font size and inline
style text. `ComputedStyle` exposes typed read-only property values.
No filesystem, network, clock, global mutable state or thread is needed.

`cascade` exports `StyleSet`, `Origin` and `HTML_UA_CSS`; these and
`values::{ComputedStyle, ComputedValue, Property}` are re-exported at the
crate root. `Property::ALL`, `name`, `inherited` and `initial` describe
the fixed longhand roster. `ComputedStyle::get` yields immutable typed
values (keywords, RGBA colors, px/percentage/auto lengths, family lists,
weights and decoration flags); `font_size` and `color` are convenient
typed accessors. A computed style owns its values and cannot be populated
with caller-constructed non-finite values. The crate documentation has
a compile-checked example implementing the element boundary; runtime
pipeline tests use the repository's ordinary Cargo test runner.

Tests carry small spec-derived examples with section references, written
locally rather than vendoring a third-party suite. They cover tokens,
escapes, normalization, malformed/EOF recovery, importance and skipped
blocks; selector grammar, specificity and matching against an in-crate
tree; all origin/importance/order tiers, inline precedence, inherited
computed values, invalid-value fallback and UA defaults. Hostile cases
exercise each bound at its edge and beyond, deep and wide syntax, logical
functions, cyclic tree handles and non-finite numbers. Deterministic
generated strings test termination and panic freedom. The manifest and
lock must stay dependency-free; Clippy denies production panics and
indexing. No recipe or application integration is part of these tests.

## 7. Delivery increments

1. Design and the AGENTS.md route, as a documentation-only commit.
2. Syntax crate, committed standalone lock, tokenizer, parser and tests.
3. Selector grammar, specificity, Element boundary and matching tests.
4. Cascade, computed properties, HTML UA sheet and end-to-end tests.

Each increment is independently green, reviewed, readied and pushed on
`td-css-rolling`. Integration into a document tree or renderer requires a
separate workstream and design amendment.
