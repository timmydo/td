# td-css

td-css is td's CSS engine: owned syntax, selector matching and computed
styles, without layout, painting, fetching or a document tree. It is a
standalone library using std and td-alloc, with no external dependencies,
no unsafe code and no
production panics or unchecked indexing. The standalone
[td-dom adapter](../td-dom/DESIGN.md) connects its Element boundary to
td-html documents. td-web consumes it (td-web/DESIGN.md §10): it
compiles a page's sheets and td-layout cascades them. td-css itself has
no dependency on td-html, td-web, td-news, td-mail or td-ui.

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
An initial BOM is removed. ASCII input with no CR, form feed or NUL is
borrowed during lexing; other input uses an owned normalized character
array. Both paths produce the same owned tokens. Comments disappear without
adding whitespace.
Tokens retain decoded names, strings and URLs, number representations and
integer flags, and the ID/unrestricted distinction on hash tokens.
Escapes, bad strings, bad URLs, CDO/CDC and EOF recovery follow Syntax §4.
Numbers are represented as text at the syntax layer; property parsing
refuses non-finite arithmetic rather than giving an overflowed float a
meaning.

The syntax layer implements the stylesheet, declaration-list and
component-value-list entry points of Syntax §5. Functions and simple
blocks own their component values. Rule bodies take ownership of their parsed arrays, and declaration validation
borrows candidate spans before copying only retained values. Unclosed blocks/functions end at EOF;
an incomplete qualified rule without a block is discarded. A declaration
without a name/colon is discarded through its next top-level semicolon.
Its terminal, ASCII-insensitive `!important` is removed into a flag.
Whitespace inside values is preserved except leading/trailing whitespace.
Parse errors are recovered; resource exhaustion is a distinct fatal error
and returns no partial result. Diagnostics count recovered errors without
retaining attacker-controlled error strings.

At-rules retain their name, prelude and optional block as syntax, including
unknown rules. Declaration-list at-rules are consumed and ignored. Style
compilation evaluates nested `@media` and `@supports` groups as described
below. Other at-rules, including `@import`, `@font-face`, `@page`,
`@namespace`, `@container` and `@layer`, are skipped whole; their contents
never leak into the surrounding cascade. No URL is fetched: a caller
that fetches, as td-web does, reads a sheet's imports itself and gives
the sheets they name to the set (`StyleSet::from_sheets`, below). Custom properties and `var()` follow the bounded
[CSS Variables Level 1 subset](https://www.w3.org/TR/2022/CR-css-variables-1-20220616/)
in §3. Length arithmetic uses the bounded calc() subset in §4; animations,
transitions, counters and generated content are outside this contract. Unsupported selectors invalidate their rule, unsupported
properties or values only their declaration.

## 2. Selectors and the element boundary

Selectors address HTML elements in a tree supplied by the caller. Type
and attribute names compare ASCII-insensitively; IDs, classes and
attribute values are case-sensitive unless an explicit `i` flag requests
ASCII-insensitive matching (`s` explicitly requests sensitive matching).
Host-language enumerated attribute-value folding is not implemented.
There are no namespace selectors, pseudo-elements, language selectors
or shadow trees. :hover, :focus and :checked use caller-reported state.

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

`Element: Copy` is a cheap handle into the caller's stable tree. It
exposes `tag_name`, `attribute`, `parent`, `previous_sibling`,
`next_sibling`, `first_child`, `has_text`, and `is_link`. Attributes are
queried by ASCII-lowercase name, including `id` and `class`.
`attribute_count`, zero by default, tells td-css how many attributes an
`attribute` lookup may compare, and `link_attribute_count`, also zero
by default, how many an `is_link` call may; each such lookup, the
index's `id` and `class` reads included, costs a match step for every
four of them. td-dom reports the element's attribute count, since
td-html's lookup scans them in turn, and for `is_link` only on `a` and
`area`, the elements it looks for `href` on. The caller
ignores comments in `has_text`, supplies element-only sibling/child
links, and keeps these observations stable for one operation. It owns
HTML link semantics and `state(State::{Hover, Focus, Checked})`, whose
default is false. The caller supplies hover propagation, focus and
checked/selected host-language semantics; td-css does not infer these
from attributes. There is no visited State variant. No references or handles escape a matching call.
Callbacks must return promptly; td-css cannot bound time spent inside a
caller callback beyond what `attribute_count` and `link_attribute_count`
report. Traversal is budgeted even if a faulty tree cycles.

## 3. Cascade

Sheets have an explicit `Origin`: user-agent, user or author. The order
from weakest to strongest is normal UA, normal user, normal author,
important author, important user, important UA. Inside a rank, inline
author declarations outrank selectors; otherwise specificity wins, then
later sheet/rule/declaration order. The caller supplies sheet order.
Inline styles have normal or important author origin, and do not outrank
important user or UA rules. There are no layers, scopes, presentational
hints, animations or transitions.

Compiled rules are indexed by a required id, class or tag in each
rightmost compound, preferring id then class then tag. In their absence,
a positive :is/:where supplies the union of its argument keys; negation,
attributes and structural/state tests use a universal fallback. A list
with any universal branch is wholly universal. An empty forgiving list
contributes no key because it cannot match. Keys only select candidates:
the complete selector list still determines matching and specificity.
Candidate rule numbers are deduplicated and visited in original order.
Index construction appends shared key/rule pairs, then sorts each key class
once. Binary lookup finds its contiguous matching range. Construction is
O(entries log entries), without quadratic sorted-map insertion.

Ordinary invalid property values are discarded before the cascade. A
syntactically valid var() value instead defers property grammar until
substitution: a winning value invalid then becomes unset, without
resurrecting an earlier declaration. Supported shorthands expand before
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

### Custom properties and substitution

Custom names are case-sensitive dashed identifiers other than `--`.
Their token values inherit; `initial` is guaranteed-invalid, while
`inherit` and `unset` use the parent's computed tokens. Empty values are
valid and distinct from missing/invalid values. The ordinary origin,
importance, specificity, inline and order rules also select custom
properties. Invalid local values shadow inherited values. No registered
custom-property syntax, animation taint or attr() is implemented.

var() requires a custom name and optionally a comma followed by fallback
tokens; the fallback may be empty or contain more commas. Its own
top-level ! and semicolon tokens are invalid even when unused; those
tokens remain valid inside nested fallback blocks/functions. Substitution
copies tokens, never concatenates and retokenizes text. All references,
including those in unused fallbacks, form the element's dependency graph;
every member of a cyclic strongly connected component is invalid. An
invalid dependency can use a fallback. Parent values are already resolved
before inheritance, so a child's override cannot change their references.
Font-relative units in inherited token values resolve when an ordinary
property consumes them on that child.

Property grammar is checked after substituting each winning pending
longhand. Shorthands reserve all of their longhands during selection,
including those later invalid at computed-value time. Invalid winning
values use unset, including the inherited/initial distinction. Substitution
may itself produce initial/inherit/unset, including on custom properties;
custom inherit/unset then uses the same name in the parent computed map.
Unsupported revert/revert-layer produced by substitution is invalid. Unknown properties never enter
the cascade. Supports accepts valid custom declarations and syntactically
valid var() on implemented properties without requiring a variable value.

Computed custom maps share immutable parent data when there are no local
changes; changed maps share token arrays, built by one ordered merge of
the parent's entries with the element's resolved ones, and count each
parent entry they copy. Each entry keeps its stored bytes beside it, so
an override subtracts the replaced value's bytes without walking it.
The element's own custom declarations are collected, sorted by name and
priority once, and each name's last kept; each declaration's references
are collected, sorted and deduplicated once; and the dependency graph,
cycle detection and resolution are indexed by a name's position among
the element's sorted names. So the order names are declared, referenced
or visited in costs nothing more. Invalid entries count toward
the map limit so repeated invalid names cannot accumulate without bound.
Graph visits, reference names and copied components share one substitution
budget per compute call, including pending shorthand reparsing. Payload
bytes count decoded token strings/numeric representations plus one byte
per token, function-name bytes plus two, or two per block; map bytes also
include names. These are admission units, not allocator byte estimates.
Nested copies and fallback recursion also obey MAX_NESTING. Fatal limits
refuse the whole compute call, distinct from an invalid CSS value.

### Conditional rules

[Media Queries Level 4, 19 February 2026](https://www.w3.org/TR/2026/CRD-mediaqueries-4-20260219/)
supplies query lists, not/only media-type prefixes, grouped not/and/or
conditions and feature ranges. The named Level 5 subset is
[prefers-color-scheme and prefers-reduced-motion](https://www.w3.org/TR/2026/WD-mediaqueries-5-20260219/).
[Conditional Rules Level 3](https://www.w3.org/TR/2024/CRD-css-conditional-3-20240815/)
supplies supports conditions. Nested active groups flatten in source
order. Rules inside inactive groups are counted and their nested
conditions parsed for admission, without compiling their selectors or
values. Bodies of other at-rules remain opaque skipped syntax.

`conditions::Environment` supplies `MediaType::{Screen, Print}`, finite
nonnegative CSS-pixel width/height, finite nonnegative resolution in dppx,
`ColorScheme::{Light, Dark}` and a reduced-motion boolean. It reads no
host state. `StyleSet::new_with_environment` validates that environment
and compiles the applicable rules. Changing it requires recompilation;
the result does not retain source text. `StyleSet::new` uses the explicit
default: screen, 1280×720 CSS px, 1 dppx, light, no reduced motion.

Supported features are width, height and resolution (boolean, exact,
min-/max- prefixes and single or same-direction chained comparisons),
orientation (portrait includes square), color scheme (light/dark) and
reduced motion (reduce/no-preference). Length comparisons accept the
absolute length units in §4 and em/rem at the initial 16px, or bare zero;
resolution accepts dpi, dpcm, dppx and x. Negative comparison values are
permitted and compared to nonnegative environment metrics. Other units,
features and values are unknown. Range operators <, <=, =, >=, > preserve
adjacency of the two-token operators; mixing comparison directions is
invalid. Aspect ratio, interaction capability and other media features
are outside this subset.

Media conditions use three-valued logic: unknown remains unknown under
not, false dominates and, true dominates or, and only a final true query
matches. Unknown media types are false and can be negated. Invalid query
members recover at the next top-level comma; an empty list matches. An
and/or level cannot mix operators; a media-type suffix cannot have a
top-level or. Supports uses boolean logic and queries exactly one
implemented property/value declaration; unknown/general-enclosed syntax
is false, including selector() queries. It tests computation grammar,
not whether a layout engine exists. Both public text entry points,
`conditions::media_matches` and `supports_matches`, have the same bounds
and syntax handling used for compiled groups.

A sheet given to `StyleSet::from_sheets` may carry media conditions, a
slice of media query lists that must all match: the `media` attribute of
the `<style>` or `<link>` that supplied it, then the media list of each
`@import` on the way to it, since a list has no conjunction of lists.
HTML applies such a sheet when each text
[matches the environment](https://html.spec.whatwg.org/multipage/common-microsyntaxes.html#matches-the-environment):
empty or ASCII whitespace alone matches, as an absent attribute (an
empty slice) does, and anything else is a media query list evaluated as
above, so an invalid member is `not all` (Media Queries 4 §3.2) and a
list matches when any member does. An attribute is text, not a decoded
sheet, so a leading U+FEFF is no byte order mark but part of an
identifier, and matches nothing. (A `style` attribute's declarations
still go through the decoded-sheet path, which strips a leading U+FEFF,
so an attribute `\u{feff}color: red` applies its colour where keeping
the U+FEFF, as for media lists, would make the property unknown.) Each
list is
parsed as a component list
of its own, never spliced into the sheet's text, so a stray `}` in the
sheet cannot close a condition, nor can a list add rules; every list is
evaluated, so each list's bounds hold whatever the others answer. A
sheet whose conditions fail is parsed and walked as the rules of a false
`@media` block are: counted, its nested conditions parsed, its selectors
and values not compiled. It contributes no rule, and the other sheets
keep their order. The conditions are no group level: the sheet's own
groups may still nest MAX_GROUP_DEPTH deep. A sheet without any compiles
as before.

Imports remain unfetched. A future fetch-owning caller may resolve an
import under its own URL, depth and byte policy, then supply its sheets
at the import's source position, with the chain's media lists, outermost
first, as the sheet's conditions. The ordered-sheet API is not an import
resolver and carries no import layer or supports() condition.

## 4. Computed property set

Values are typed and deterministic; absolute lengths use CSS px (96 per
inch), independent of display scale. Accepted units are px, pt, pc, in,
cm, mm, q, em and rem; bare zero is a length. Percentages on margin,
padding, width and height stay percentages for layout to resolve.
Font-size percentages and em use the parent's computed font size; other
em lengths use the element's computed font size. Other units, and
functions other than calc() and the colour functions below, invalidate
the declaration, but in the `background` shorthand's position and size
slots, which nothing stores (below). Negative margins are valid;
negative font sizes, padding, border widths and dimensions are invalid
as bare values; calc() clamps its result to the property range.

| Property | Accepted values | Initial; inherited |
|---|---|---|
| display | none, inline, block, inline-block, list-item, table, inline-table, table-row-group, table-header-group, table-footer-group, table-row, table-cell, table-column-group, table-column, table-caption, flex, inline-flex, grid, inline-grid | inline; no |
| color, background-color | CSS Color 4's 148 named colours, transparent, currentcolor, #rgb/#rgba/#rrggbb/#rrggbbaa, rgb()/rgba(), hsl()/hsla() (below) | black / transparent; color only |
| font-family | comma-separated quoted strings or identifier sequences, including generic families | serif; yes |
| font-size | lengths, percentages, xx-small through xx-large, smaller, larger | 16px; yes |
| font-weight | normal, bold, bolder, lighter, integers 100 through 900 in steps of 100 | 400; yes |
| font-style | normal, italic, oblique | normal; yes |
| text-decoration | none or a nonrepeating combination of underline, overline, line-through | none; no |
| white-space | normal, pre, nowrap, pre-wrap, pre-line, break-spaces | normal; yes |
| margin-{top,right,bottom,left} | lengths, percentages, auto | 0; no |
| padding-{top,right,bottom,left} | lengths, percentages | 0; no |
| border-{top,right,bottom,left}-width | lengths, thin, medium, thick | medium (3px), computed zero for none/hidden style; no |
| width, height | lengths, percentages, auto | auto; no |
| list-style-type | none, disc, circle, square, decimal, decimal-leading-zero, lower-roman, upper-roman, lower-alpha, upper-alpha, lower-latin, upper-latin | disc; yes |
| text-align | start, end, left, right, center, justify | start; yes |

`margin`, `padding`, `border-width`, `border-style` and `border-color`
accept one to four values in TRBL order. `border` and each border-side
shorthand accept up to one width, style and color in any order, resetting
omitted components to their initials. Border-image is outside this subset.
`overflow` accepts one value for both axes or two in x/y order. `font`
accepts optional style, variant and weight (each at most once, with normal
filling omitted slots), a required size, optional slash line-height, and
a required family list. It resets all six implemented font longhands;
system-font keywords, stretch and other font extension/reset properties
are unsupported. `background` sets the colour of its final layer and
`list-style` its type and position (both below); `flex`, `flex-flow`
and `gap` set the flex longhands (below). Other shorthands remain
unsupported. `text-decoration`
here is the line-set subset, without style, color or thickness. Its propagation to
descendants is a layout/painting concern, not inheritance. Border widths
compute to zero for none/hidden styles. Border colors default to the
computed color and resolve currentcolor after color. Display blockifies
for the root (absent parent style), floats, and flex and grid items,
the children of a box whose computed display is flex, inline-flex
(Flexbox 1 §4), grid or inline-grid (Grid 2 §6.1): inline/inline-block
and table-internal displays become block, inline-table becomes table,
inline-flex becomes flex and inline-grid becomes grid. None, list-item,
table, block, flex and grid remain. A flex or grid item's `float` computes to none, so no item
floats, and `clear` is left as it is, which td-layout does not read for
one (td-layout/DESIGN.md §5, "Flex", "Grid"); the root's `float` is its
own. Unsupported positioning modes are deferred.
Overflow visible/clip computes to auto/hidden when the opposite axis is
neither visible nor clip. No scrolling or box generation occurs here.
Font keyword sizes are 9, 10, 13, 16,
18, 24 and 32px; smaller/larger multiply the parent's size by 1/1.2 or
1.2. Thin/medium/thick are 1/3/5px. These are td's UA choices.

Colours follow the sRGB part of
[CSS Color 4, 12 November 2026](https://www.w3.org/TR/2026/CRD-css-color-4-20261112/):
§6.1's 148 named colours, ASCII case-insensitive and held in a sorted
table searched by bisection, `transparent` (§6.3), `currentcolor`
(§6.4), the hex notations (§5.2), and rgb()/rgba() (§5.1) and
hsl()/hsla() (§7), each pair aliases. The legacy syntax (§4.1.2)
separates three components and an optional alpha with commas and takes
no `none`; its rgb() channels are all numbers or all percentages and
its hsl() saturation and lightness are percentages. The modern syntax
separates components with whitespace and an optional `/` before alpha,
mixes numbers and percentages, and reads `none` as zero. A hue is a
number of degrees or a deg, grad, rad or turn angle, normalized into
[0, 360); alpha is a number or a percentage. A wrong type, a missing or
extra component, a mixed separator, or more than seven significant
components makes the value invalid. rgb() channels and alpha clamp to
their ranges and negative saturation to 0%; the other hsl() components
convert by §7.1 unclamped, their magnitudes bounded by
MAX_ABSOLUTE_VALUE, and the channels that result clamp to sRGB's gamut.
Each channel and alpha then rounds to eight bits: the nearest integer on
the 0–255 scale, halves toward +∞ (§5.1), so 50% and an alpha of 0.5
are both 128. The computed colour is that RGBA; a consumer that paints
on opaque fills blends it itself. A calc() component counts as a number
or a percentage when it types as that alone; one with a length, an
angle or a mix of the two makes the colour invalid, so a hue angle
cannot come from calc(). hwb(), lab(), lch(), oklab(), oklch(),
color(), color-mix(), relative colours and the system colours (§6.2)
are unsupported, so invalid. `currentcolor` computes to the element's
computed `color` (for `color` itself, the parent's) rather than staying
the keyword §6.4 specifies: the two differ only where a non-inherited
colour property is explicitly inherited from a parent whose value was
`currentcolor`, and the child then takes the parent's colour.

The `background` shorthand follows
[Backgrounds 3 §2.10, 11 March 2024](https://www.w3.org/TR/2024/CRD-css-backgrounds-3-20240311/#background):
comma-separated layers of which only the last may carry a colour. It
sets `background-color` to that colour, or to the initial transparent
when there is none; td-css has no other background longhand, so it
resets nothing else. A layer is a nonempty set, in any order, of at
most one image, one position with an optional `/` size, one repeat
style, one attachment, two boxes and, in the last layer, one colour. An
image is `none`, a URL token, url() or src(); td-css fetches nothing.
Gradients (linear, radial and conic, their repeating forms and every
`-webkit-` form) and image-set(), cross-fade() and element() are
unsupported, so a layer naming one makes the declaration invalid, as
CSS Conditional 3's usable support requires of `@supports`: td-web
paints no image, and accepting one would reset the colour of a page's
usual fallback, `background: #1e5799; background:
linear-gradient(...)`, to transparent. The cost is that a gradient
in the same declaration as its colour loses that colour too. A
position is §2.6's `<bg-position>`, one to four keywords and
length-percentages; a size is cover, contain, or one or two of auto and
nonnegative length-percentages. Since nothing stores a position or a
size, their length-percentages are any CSS Values 4 length unit
(absolute, font-relative, viewport and container units, ASCII
case-insensitive), a percentage, bare zero, or a calc(), min(), max()
or clamp() stepped over whole; other units and functions are invalid.
A repeat style is repeat-x, repeat-y or
one or two of repeat, space, round and no-repeat; an attachment is
scroll, fixed or local; a box is border-box, padding-box or
content-box. Anything else, a repeated component, an empty layer or a
colour before the last layer invalidates the declaration; a var() in it
defers as in any shorthand.

Additional values use [CSS 2.2](https://www.w3.org/TR/CSS22/) property
semantics, the two-axis visible/clip rule from
[Overflow Level 3](https://www.w3.org/TR/2025/WD-css-overflow-3-20251007/),
and opacity's number/percentage range from
[Color Level 4](https://www.w3.org/TR/2026/CRD-css-color-4-20261103/#transparency).
The flex longhands and shorthands follow
[Flexbox Level 1](https://www.w3.org/TR/css-flexbox-1/) and the
alignment longhands [Box Alignment Level
3](https://www.w3.org/TR/css-align-3/), as far as flex layout reads them,
and the grid longhands and shorthands [CSS Grid Level
2](https://www.w3.org/TR/css-grid-2/) without subgrid or masonry, which
no layout reads yet (below, "Grid").

| Property | Accepted values | Initial; inherited |
|---|---|---|
| line-height | normal, nonnegative number, length or percentage | normal; yes |
| font-variant | normal, small-caps | normal; yes |
| border-{top,right,bottom,left}-style | none, hidden, dotted, dashed, solid, double, groove, ridge, inset, outset | none; no |
| border-{top,right,bottom,left}-color | the color grammar above | currentcolor; no |
| vertical-align | baseline, sub, super, top, text-top, middle, bottom, text-bottom, signed length/percentage | baseline; no |
| text-indent | signed length/percentage | 0; yes |
| text-transform | none, capitalize, uppercase, lowercase | none; yes |
| visibility | visible, hidden, collapse | visible; yes |
| overflow-x, overflow-y | visible, hidden, clip, scroll, auto | visible; no |
| float | none, left, right | none; no |
| clear | none, left, right, both | none; no |
| position | static, relative | static; no |
| top, right, bottom, left | signed length/percentage, auto | auto; no |
| box-sizing | content-box, border-box | content-box; no |
| min-width, min-height | auto, nonnegative length/percentage | auto; no |
| max-width, max-height | nonnegative length/percentage, none | none; no |
| letter-spacing | normal, signed length (no percentage) | normal; yes |
| opacity | number or percentage, clamped to [0,1] | 1; no |
| list-style-position | outside, inside | outside; yes |
| border-collapse | separate, collapse | separate; yes |
| border-spacing | one nonnegative length for both axes, or the horizontal then the vertical; no percentage | 0; yes |
| table-layout | auto, fixed | auto; no |
| flex-direction | row, row-reverse, column, column-reverse | row; no |
| flex-wrap | nowrap, wrap, wrap-reverse | nowrap; no |
| flex-grow, flex-shrink | nonnegative number | 0 / 1; no |
| flex-basis | auto, content, nonnegative length/percentage | auto; no |
| justify-content | normal, space-between, space-around, space-evenly, stretch, and center, start, end, flex-start, flex-end, left, right, each after an optional safe or unsafe | normal; no |
| align-items | normal, stretch, baseline, first baseline, last baseline, and center, start, end, self-start, self-end, flex-start, flex-end, each after an optional safe or unsafe | normal; no |
| align-self | auto and align-items' values | auto; no |
| align-content | normal, space-between, space-around, space-evenly, stretch, baseline, first baseline, last baseline, and center, start, end, flex-start, flex-end, each after an optional safe or unsafe | normal; no |
| row-gap, column-gap | normal, nonnegative length/percentage | normal; no |
| order | integer | 0; no |
| grid-template-columns, grid-template-rows | none, or a track list | none; no |
| grid-template-areas | none, or one or more strings | none; no |
| grid-auto-columns, grid-auto-rows | one or more track sizes | auto; no |
| grid-auto-flow | row or column, dense, or both | row; no |
| grid-row-start, grid-row-end, grid-column-start, grid-column-end | auto, a custom-ident, a nonzero integer and an optional custom-ident, or span with a positive integer, a custom-ident or both | auto; no |
| justify-items | normal, stretch, baseline, first baseline, last baseline, legacy, legacy with left, right or center, and center, start, end, self-start, self-end, flex-start, flex-end, left, right, each after an optional safe or unsafe | legacy, computing to normal; no |
| justify-self | auto and justify-items' values but the legacy ones | auto; no |

Line-height numbers stay numbers through inheritance; lengths and
percentages compute against the declaring element's font size and then
inherit as px. Normal stays a keyword for later font metrics. Vertical-align
percentages retain their line-height basis for layout; all other newly
admitted percentages except line-height remain unresolved. Min/max sizes
use the CSS 2.2 subset, without intrinsic sizing or fit-content, and
with Flexbox 1's `auto` as the minimum's initial value, computing to
`Length::Auto`; td-layout reads it as CSS 2.2's initial 0 but in a flex
item's main axis, where it is the automatic minimum (td-layout/DESIGN.md
§5, "Flex").

The flex and alignment longhands compute to keywords, numbers and
lengths only: no td-css computation depends on them but blockification
on a parent's `display`. With `clear`, `opacity`, the four offsets,
`table-layout` and the grid longhands (below), they are the rare group
a style shares until a rule sets one ("Computed-style sharing"). td-layout lays rows, wrapping rows and
columns (td-layout/DESIGN.md §5, "Flex"), `wrap-reverse` as `wrap` and
`column-reverse` as `column`, and reads `order` to place the items of a
row laid on one line (td-layout/DESIGN.md §5, "Order"). A flex factor
clamps to MAX_ABSOLUTE_VALUE and
`order`, an integer token, to the 32-bit range browsers keep it in.
`flex-basis: content` is a keyword, auto and a length-percentage are
lengths, as width's are; a gap's `normal` stays a keyword. Box Alignment
3's `first baseline` computes to `baseline` and `last baseline` stays
two words; a `safe` or `unsafe` overflow position is accepted before a
positional keyword (none other) and dropped, as td-layout will not tell
them apart. Left and right are the justify longhands' alone, and
self-start and self-end the items' and self's. The `flex` shorthand is
`none` (0 0 auto) or `<grow> <shrink>? || <basis>`: an omitted factor
is 1 and an omitted basis 0%, so `auto` is 1 1 auto, a lone number n is
n 1 0% and a lone basis b is 1 1 b; `flex: initial`, the CSS-wide
keyword, gives the initials 0 1 auto. A unitless zero is a factor unless
two factors precede it, and the shrink factor must follow the grow
factor directly. `flex-flow` is a direction and a wrap, each at most
once in either order, the other reset; `gap` is a row gap and an
optional column gap that otherwise takes the row's. Anything else
invalidates the declaration, as any property's invalid value does.
Accepting these grammars changes what `@supports` reports for them, and
`min-width: auto`, which was invalid and dropped, now wins its cascade
and lays as 0 (Vector 2022's `.skin--responsive .mw-page-container`
below 300 px, which the earlier `min-width: 18.75em` held before).
Relative offsets retain auto and percentages: choosing the containing
block, resolving opposing offsets and applying position are layout work.
Text-transform is a keyword, not an operation on strings. Visibility,
overflow, float, clear, box-sizing and opacity likewise compute values
without painting, formatting, clipping or compositing.

The `list-style` shorthand follows
[CSS Lists 3 §3.4](https://www.w3.org/TR/2020/WD-css-lists-3-20201117/#list-style-property):
a position, an image and a type, each at most once and in any order,
resetting `list-style-type` and `list-style-position` to their initials
where omitted. No `list-style-image` is computed, so its slot takes
`none`, a URL token or any CSS Images 4 `<image>` function (url(),
src(), image(), image-set(), cross-fade(), element() and the six
gradients), checked by name only, and stores nothing; td-css fetches
nothing. The type's keywords are the longhand's, from one list;
lower-latin and upper-latin are Counter Styles 3's aliases of the alphas
and compute as written.
`none` fills an empty slot, the type's first: `list-style: none` sets
the type to none, `none none` the type and image, and `none circle`
the image, keeping circle; a `none` with no slot left, a repeated slot
or any other component invalidates the declaration. td-layout draws
markers from the two longhands (td-layout/DESIGN.md §5, "List
markers").

The length calc() grammar follows [CSS Values Level 3 §8.1](https://www.w3.org/TR/2024/CRD-css-values-3-20240322/#calc-notation):
parentheses and nested calc(), left-associative + and -, and higher-priority
* and /. Addition/subtraction require whitespace on both sides. Products
require a number operand; division requires a nonzero number divisor.
Unitless zero inside calc() is a number, not a length. Unsupported units,
dimensionally invalid expressions, non-finite arithmetic, min/max/clamp
and numeric constants are invalid. No pure-number property calc() grammar
is claimed by this length subset; a colour component's calc() may be a
bare number or a bare percentage (above). Bounds also inspect invalid
tails, except in a colour function or a `background` layer: each stops
at its first invalid component, so an over-limit calc() after one is
merely invalid there, not fatal, and the math functions a background
position or size steps over are bounded by MAX_NESTING and MAX_TOKENS
alone.

Absolute units combine into px; em/rem compute with the same font bases
as ordinary lengths. Font-size percentages resolve against the parent.
Other admitted percentages remain `Length::Calc { px, percent,
non_negative }`, even when their coefficient cancels to zero. Border
width arithmetic forbids percentages, including canceled ones. Negative
results clamp to zero only where the property forbids negative lengths;
unresolved mixed values retain that requirement until their basis is known.
Finite magnitude is clamped to MAX_ABSOLUTE_VALUE at computation, including
unresolved coefficients. Floating-point evaluation is deterministic, not
exact real arithmetic; scaled summation avoids infinity-minus-infinity
when resolving finite font coefficients.

`Length::resolve(basis)` is a convenience for a caller that has determined
the appropriate percentage basis. It accepts finite nonnegative CSS px up
to MAX_ABSOLUTE_VALUE and returns a clamped px value, or None for auto or
invalid caller-constructed values. It does not choose the basis, establish
a containing block, or perform layout. The caller must apply any property's
additional used-value rules.

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
the complete HTML rendering stylesheet. Its lists follow HTML §15.3.7:
dir and menu are block lists like ol and ul, with their 1em block
margins, 40px start padding and disc; a dir, dl, menu, ol or ul inside
another has no block margins (written as margin-top and margin-bottom,
since td-css has no logical margins); and nested lists' bullets are its
two `:is()` rules, circle and then square. Physical left stands for
inline-start, and `counter-reset` and `text-align: match-parent` are
absent: td-layout numbers by HTML's list owners itself.
Presentational hints such as a list's `type` attribute are not mapped.
`border-spacing` is one longhand, as [CSS 2.2
§17.6.1](https://www.w3.org/TR/CSS22/tables.html#separated-borders) and
CSS Tables 3 define it, computing to `ComputedValue::Spacing`, both axes
in px, each em or rem against the element's font sizes as any length's;
td-layout lays tables from it (td-layout/DESIGN.md §5, "Tables"). The
per-axis `-webkit-border-horizontal-spacing` and
`-webkit-border-vertical-spacing`, which Chrome and Safari expose, are
not parsed: a single longhand cannot take one axis alone, and one
longhand costs each element's style one value where two cost two
(below, "Computed-style sharing").
Its hidden elements follow HTML §15.3.1 as far as the rest of the
sheet's tags: a hidden `input` (`type=hidden`, any case) is not
rendered anywhere, over any author display.
Its tables follow HTML §15.3.9 without the presentational hints of
`cellspacing`, `cellpadding`, `align` or `valign`: a table is
border-box with 2px spacing, separate borders and an initial
text-indent, cells have 1px padding, rows inherit `vertical-align:
middle` from their group (or table, for a row HTML left there) and cells
from their rows, and a `form` that HTML's parser left directly in a
table, group or row is not rendered. Tables and cells have gray
borders, which row groups and rows inherit. Of `border`, attribute
selectors read only whether it is `0`: any other value gives the table
a 1px outset border and the cells of its rows, its own and its groups',
1px inset ones, where HTML takes the table's width from the value. Every rule the sheet adds for
tables, forms and hidden inputs is keyed by tag in the rule index, so a
page without those elements tests none of them; a test holds that the
sheet has no rule the index tests against every element.

### Grid

The twelve grid longhands (the table above) are parsed and computed for
td-layout's grid layout (td-layout/DESIGN.md §5, "Grid"). `display:
grid` and `display: inline-grid` are valid and their children are
blockified as grid items (above), so `@supports (display: grid)` and
`@supports (display: inline-grid)` are true, as is `@supports` of every
`grid*` property (the `grid-*gap` aliases among them), `justify-items`,
`justify-self`, `place-items` and `place-self`, which the layout reads.
td-layout lays an `inline-grid` whole in its line, as an inline-block
holding a grid (td-layout/DESIGN.md §5, "Grid", "Inline grid and
baselines"). `grid-template-*: subgrid` and the masonry values are not
parsed, so `@supports` of them is false.

A track list is line-name lists, track sizes and `repeat()`. A name is
a custom-ident other than `span` and `auto`, kept as written. A size is
a nonnegative length, percentage or calc(), a nonnegative `fr` flex
factor clamped to MAX_ABSOLUTE_VALUE, `auto`, `min-content`,
`max-content`, `minmax(min, max)` whose minimum is no flex factor, or
`fit-content()` of a length or percentage. `repeat()` takes a positive
integer, `auto-fill` or `auto-fit`, then names and sizes and no
`repeat()`. A list holds at most one automatic repeat, and then every
size in it, inside the repeat or out, must be fixed: a length or
percentage, or a `minmax()` with one. Two name lists side by side are
invalid but in `grid-template`'s areas form, where a row's trailing
names and the next row's leading ones are one line, and one list. The
`grid-auto-*` longhands take sizes alone. `subgrid` and `masonry` are
invalid. The templates' `none` and the auto longhands' `auto` compute to
keywords, any other list to `ComputedValue::GridTracks`, a `TrackList`
of items in order (a name list's range of the names stored beside them,
a size, or a `repeat()` with the length of the body after it), its
repeats kept compact. Lengths compute as flex-basis's: px at the
element's font sizes, percentages and calc()'s percentage kept, calc()
clamped to nonnegative. A list with no em or rem computes once, as
parsed, and every element it applies to shares it; one with them
computes per element, and ComputeStats' `grid_items` counts its items
each time, which td-layout weighs a cascade work unit each (its
DESIGN.md §6), even for an element `display: none` keeps out of the
pool. A dimension past f64's range, `1e400px` or `1e400fr`, is invalid,
as td's every length is, where browsers clamp it.

`grid-template-areas` tokenizes each string as Grid 2 §7.3.1 does: a
run of ident code points is a named cell, a run of `.` a null cell,
whitespace separates them, and any other character invalidates the
declaration, as do a string of no cells, strings of unequal cell
counts, and a name whose cells do not fill a rectangle. It computes to
`ComputedValue::GridAreas`: the row and column counts and each named
area's zero-based rows and columns, ends exclusive, sorted by name. A
placement longhand computes to `auto` or `ComputedValue::GridLine`, a
span flag, an integer and a name: a lone name has integer 0, a span
without an integer 1. `span` comes first or last, never between the
integer and the name (§8.3's `span && [<integer> || <custom-ident>]`).
Integers are nonzero and spans positive, both clamped as parsed to
±MAX_GRID_LINE (10,000), as Gecko clamps them, at any length: one past
f64's range clamps as its sign's infinity.
`grid-auto-flow` computes to `row`, `column`, `row dense` or `column
dense` (`dense` alone is `row dense`), dense kept though nothing reads
it yet.

`justify-items` and `justify-self` take `align-items`' and
`align-self`'s values as above, `safe` and `unsafe` dropped and `first
baseline` computed to `baseline`, and add `left` and `right`.
`justify-items` adds Box Alignment 3 §6.1's `legacy`: with `left`,
`right` or `center`, in either order, it computes to `legacy left`,
`legacy right` or `legacy center`, which a child's `legacy`, `initial`
or `unset`, or an unset `justify-items`, takes from its parent; under
any other parent they compute to `normal`. The cascade reads the parent
for it only when the parent's rare group is not its style set's
initial one, so a page without it reads nothing more. `anchor-center`
is not parsed. The other alignment longhands and the gaps already held
the values grid uses (`normal`, `stretch` and `space-evenly` for
content), so they are unchanged.

`grid-template` is `none`, `<rows> / <columns>`, or the areas form:
rows each of optional names, a string, an optional size (`auto`
otherwise) and optional names, then an optional `/` and columns of
names and sizes alone (`none` without them); it sets its three
longhands. `grid` is a `grid-template`, `<rows> / auto-flow dense?
<auto-columns>?` or `auto-flow dense? <auto-rows>? / <columns>`, with
`auto-flow` and `dense` in either order, and resets the others of its
six longhands (templates, auto sizes and flow), not the gaps (Grid 2
§7.8); its auto sizes are `grid-auto-*`'s, so a lone `auto` computes to
the keyword and leaves the initial group shared.
`grid-row` and `grid-column` take one or two lines and `grid-area` one
to four, `/` apart; an omitted line copies a lone custom-ident and is
`auto` otherwise (§8.4): the end copies the start, and `grid-area`'s
column start and row end copy the row start and its column end the
column start. `place-content`, `place-items` and `place-self` take an
align value then an optional justify value, which copies the align one
when omitted, but that `place-content`'s baselines, which
justify-content lacks, give it `start`. `grid-gap`, `grid-row-gap` and
`grid-column-gap` are Box Alignment 3 §8.4's legacy names of `gap`,
`row-gap` and `column-gap`. A list past MAX_GRID_TRACKS tracks or
names, or areas past it in rows or columns or past MAX_GRID_AREAS
names, is invalid, as an out-of-range value is: the declaration is
dropped and its sheet compiles on.

A sheet's areas strings are tokenized once, with it. One a `var()`
brings is reparsed for each element and each longhand the declaration
sets (three for `grid-template`, six for `grid`), and the areas grammar
reads it a byte at a time, looks each named cell's name up and
allocates each area's name, where the substitution counts a string as
one component and a 64th of its bytes: `grid_items` counts each such
byte and named cell each time, and two more for each area a valid value
builds, so a page of 200 thousand-cell rows, or of one row of 1,000
distinct names, reparsed on every paragraph is refused
MAX_CASCADE_WORK, not run for minutes. The bytes one element can
reparse are bounded by its MAX_SUBSTITUTION_BYTES (4 MiB), some 75 ms,
before td-layout weighs them.

A name is looked up by binary search among the distinct names seen,
kept sorted, and updated in place. A cell whose name is new is
appended to a pending run, which is sorted once and merged into a new
sorted list, equal names joined, when it holds 64 cells and as many as
the list: inserting each new name into the sorted list shifted its
tail, quadratic in names arriving in descending order, which a
reparsed row of 1,000 descending names paid on every element (17.5 s
to refusal, against 6.3 s ascending). Every rectangle is checked before
any name is allocated, so an invalid value allocates none.

## 5. Bounds and failure

Syntax and work limits are compiled constants. Caller-supplied memory
budgets and PoolLimits may impose lower storage bounds. Limits apply
before the allocation or recursive step they guard. A fatal `Error`
names the exhausted bound; callers must report refusal rather than treat
it as a stylesheet matching nothing.

`StyleSet::from_sheets` leaves out a sheet that a bound it meets alone
refuses, and compiles on: it returns the set with `Refused`, each sheet
left out as a `Refusal` (its index in the caller's list, its origin and
the `Error`), in input order, at most MAX_SHEETS, held inline. Those
bounds are the per-input syntax bounds (MAX_INPUT_BYTES, MAX_TOKEN_BYTES,
MAX_TOKENS, MAX_NESTING, MAX_RULES), MAX_GROUP_DEPTH, MAX_DECLARATIONS in
one list, MAX_FAMILIES, and the selector (MAX_SELECTORS,
MAX_SELECTOR_PARTS, MAX_SELECTOR_DEPTH, MAX_COMPOUNDS,
MAX_NTH_COEFFICIENT), condition and calc() bounds, met in the sheet's
text or its media lists. A refused sheet is rolled back whole: its
compiled rules, index entries and declaration and visited-rule counts
return to what they were before it, so the set equals the one the other
sheets make alone, but that the arrays keep the capacity it grew.
Whether a sheet meets these bounds depends on its text, its conditions
and the environment alone, never on the sheets before it: a sheet past
its own MAX_INPUT_BYTES is refused, and kept out of MAX_STYLE_BYTES,
whatever the others hold; and a sheet that overfills a set-wide total
on its way to a bound of its own is rolled back and compiled once more
alone, from empty totals, against the same budget, that storage dropped
at once. Refused alone, it is left out like any other; compiled alone,
or exceeding a total alone, the set is refused. The environment matters
because an inactive group, or a sheet whose conditions fail, compiles no
selector or value, so a width change can newly refuse a sheet.

A call's work is bounded by MAX_STYLE_BYTES of input: each sheet's text
and lists are parsed and walked once, and once more alone only when it
overfills a total. Rollback restores the totals, so the rolled-back work
of the refused sheets can reach about MAX_SHEETS times what one set's
totals admit; review measured 38 refused sheets of 108,699 bytes in all
in 298 ms. Each media list counts at least one byte toward its sheet's
MAX_INPUT_BYTES, so a sheet holds at most that many lists, and counting
stops once past it.

The Err is a set-wide refusal: MAX_SHEETS and MAX_STYLE_BYTES, checked
before any sheet is parsed; the totals MAX_GROUP_RULES,
MAX_COMPILED_RULES, MAX_DECLARATIONS per set and MAX_INDEX_ENTRIES, when
a sheet that meets no bound alone overfills them; an invalid
environment; and the memory budget, the compile ceiling every sheet
spends. A sheet that alone exceeds a total is a set-wide refusal all the
same. Inactive sheets and groups spend MAX_GROUP_RULES as active ones
do, so a page heavy with print-only rules can exhaust it and lose its
screen styles with them. The `(Origin, &str)` constructors stop at the
first refusal, a sheet's or the set's, and return its bare `Error`; as
sheets past their own MAX_INPUT_BYTES are left out of MAX_STYLE_BYTES,
such a set reports MAX_INPUT_BYTES where it once reported
MAX_STYLE_BYTES.

| Constant | Limit and accounting |
|---|---|
| MAX_INPUT_BYTES | 1 MiB per syntax input, and per sheet's text and media lists together |
| MAX_TOKEN_BYTES | 16 KiB per decoded token string or numeric representation |
| MAX_TOKENS | 131,072 emitted tokens per input, whitespace included |
| MAX_NESTING | 32 function/block levels, also on skipped syntax |
| MAX_RULES | 8,192 rules per parsed syntax rule list |
| MAX_COMPILED_RULES | 16,384 active compiled rules per style set |
| MAX_GROUP_RULES | 32,768 visited rules across top-level and media/supports lists per style set, inactive lists included |
| MAX_GROUP_DEPTH | 16 nested media/supports groups |
| MAX_CONDITION_PARTS | 2,048 component values, whitespace and nested contents included, per condition prelude or public query |
| MAX_CONDITION_DEPTH | 16 nested blocks/functions per condition |
| MAX_MEDIA_QUERIES | 64 comma-list members per media prelude or query |
| MAX_RESOLUTION_DPPX | 1,000 supplied environment resolution |
| MAX_DECLARATIONS | 32,768 declarations per list, expanded declarations per style set, and expanded inline declarations per compute call |
| MAX_SELECTORS | 256 selector-list members, including nested lists |
| MAX_SELECTOR_PARTS | 1,024 simple selectors and combinators per parsed list |
| MAX_SELECTOR_DEPTH | 8 nested logical functions |
| MAX_COMPOUNDS | 64 compounds per complex selector |
| MAX_MATCH_STEPS | 150,000 selector tests, tree hops, inspected string bytes, index lookups, gathered bucket entries, every four attributes a lookup may compare and every four bytes a case-insensitive substring test searches, per public match or compute call |
| MAX_INDEX_ENTRIES | 262,144 rule-to-bucket entries per style set |
| MAX_ELEMENT_BYTES | 65,536 bytes per observed tag or attribute string |
| MAX_NTH_COEFFICIENT | 1,000,000,000 absolute magnitude for each An+B coefficient |
| MAX_SHEETS | 64 sheets per style set |
| MAX_STYLE_BYTES | 4 MiB total sheet text and media list bytes per style set, sheets past their own MAX_INPUT_BYTES excluded |
| MAX_CALC_PARTS | 1,024 recursive component values per calc(), root and whitespace included |
| MAX_CALC_DEPTH | 16 nested functions/blocks per calc(), root included |
| MAX_CUSTOM_PROPERTIES | 8,192 distinct computed custom names per element, invalid entries included |
| MAX_CUSTOM_BYTES | 1 MiB computed custom names and token payload admission bytes per element |
| MAX_VARIABLE_DEPTH | 64 recursive dependency-graph or resolver levels |
| MAX_SUBSTITUTION_COMPONENTS | 262,144 charged component/reference/graph visits per compute |
| MAX_SUBSTITUTION_BYTES | 4 MiB cumulative charged payload/name bytes per compute |
| MAX_FAMILIES | 32 names in a font-family value |
| MAX_GRID_TRACKS | 1,000 explicit tracks per track list, integer `repeat()`s expanded and an automatic repeat's tracks counted once; 1,000 line names per list as written; 1,000 rows and 1,000 columns per `grid-template-areas`; past it the declaration is invalid, not a refusal |
| MAX_GRID_AREAS | 1,000 named areas per `grid-template-areas`; past it the declaration is invalid |
| MAX_GRID_LINE | 10,000, the magnitude grid line integers and spans clamp to as parsed |
| MAX_ABSOLUTE_VALUE | 1,000,000 for computed px, percentages and supplied root font size |

Recovered syntax errors use a saturating counter. Tokenization and syntax
parsing take linear work in admitted input plus owned output; selector
compilation is bounded by the token and selector limits. Matching can
backtrack over ancestors or siblings, but shares one work counter across
logical functions and every candidate rule of a compute call. Index
lookups charge observed tag/id/class bytes before scanning or folding,
one step per lookup and one per bucket entry before deduplication.
Binary index lookups and candidate sorting have an additional logarithmic factor
bounded by MAX_INDEX_ENTRIES and MAX_COMPILED_RULES; the work counter is an
admission metric, not a CPU instruction count. Duplicate class words
still charge repeated bucket visits. Only indexed fields are observed. Recursive matching
depth is bounded by compounds and logical depth, not document depth.
Attribute scans are bounded by MAX_ELEMENT_BYTES per observed string and
charged by byte length to the same work budget. Substring matching uses
linear string search; an `i` selector compiles a bounded KMP prefix array
and folds ASCII bytes while scanning, without allocating in the match.
Numeric input may be larger than a supported value; finite values beyond
the computed bound are clamped, without wrapping or creating NaN.
A colour function stops at its eighth significant component, and a
background layer's url(), src() and math functions are stepped over
whole, their depth already bounded by MAX_NESTING; neither stores
anything.
All td-css-owned heap storage uses td-alloc. Token text up to 14 UTF-8
bytes lives inline in its enclosing owner.
Text from 15 through 62 bytes uses one shared fixed SmallText allocation;
longer text uses a shared BudgetString. Custom maps, retained custom-token
arrays, font families, rare-property groups and grid track lists, areas, lines and a track list's names use immutable BudgetShared owners; nested component arrays use BudgetVec.
Component construction counts immediate children with a fixed bounded
delimiter stack before reserving exact capacity.
Mutable arrays and small ordered maps use BudgetVec and BudgetMap.
Error::Memory distinguishes budget exhaustion, overflow and allocator
refusal with no heap-backed diagnostic. Failure drops temporary storage;
no partial stylesheet or computed style escapes. Element callbacks and
caller-provided formatting/hashing remain outside this storage contract.
The source guard rejects standard owners and recognized unchecked growth
throughout production td-css, apart from the CSS token's String variant.

Standalone syntax, selector and stylesheet constructors create a 256 MiB
root plus its budget metadata. Their with_budget variants instead share
an explicit caller budget; all scratch and returned storage keep that
identity. StyleSet::compute uses the stylesheet's budget by default;
compute_with_budget and compute_with_stats_and_budget let a page choose a
separate child. These are simultaneous owned-layout limits, independent
of the cumulative syntax/substitution admission counters. Shared strings
and tokens retain their original admission while a computed style or
interner still references them, even after the StyleSet drops. Callers
must not release or subtract an estimated stylesheet charge at that point.
Small ordered maps have logarithmic lookup and linear shifting insertion;
custom-property counts and pool work bounds limit their construction.

## 6. Public API and tests

`syntax::tokenize`, `parse_stylesheet`, `parse_declarations` and
`parse_components` accept text and return owned, inspectable results.
Syntax objects expose immutable views; parsing never accepts an arbitrary
caller-constructed recursive AST. `selectors::SelectorList::parse` and
`matching_specificity` return a parsed list and `Result<Option<Specificity>>`.
`StyleSet::new` compiles ordered `(Origin, &str)` sheets, and
`from_sheets` ordered `Sheet`s (`Sheet::new(origin, text)`, then
`with_media(lists)`; the type is non-exhaustive so later fields do not
break callers) under an environment and a budget; `compute` takes
an element, optional parent `ComputedStyle`, root font size and inline
style text. `ComputedStyle` exposes typed read-only property values.
`compute_with_stats` has the same arguments plus a mutable `ComputeStats`.
It resets and returns candidate-rule, matched-rule and consumed match-step
counts, the inspected string bytes among those steps, the declarations of
matched rules and inline style taken into the cascade (custom ones
included) and the custom ones among them, the distinct custom names
declared on the element itself, the parent entries its changed custom
map copied, and substitution component and byte charges, even on
refusal. Candidate
count is zero if gathering fails before completion; consumed steps
exclude the charge that refused. td-layout sums them into its page-wide
cascade ceiling (td-layout/DESIGN.md §6). `compute`
uses the same implementation. The unindexed implementation is test-only.
No filesystem, network, clock, global mutable state or thread is needed.
td-web's styled pages (td-web/DESIGN.md §10) compile through
`from_sheets`, and would use an answer to whether a changed environment
changes which of a set's rules apply (td-web/DESIGN.md §11), which
td-css does not give yet.

`cascade` exports `StyleSet`, `Sheet`, `Refused`, `Refusal`, `Origin`,
`ComputeStats` and `HTML_UA_CSS`; these and
`values::{ComputedStyle, ComputedValue, Property}` are re-exported at the
crate root. `Property::ALL`, `name`, `inherited` and `initial` describe
the fixed 83-longhand roster, in declaration order: the 52 a style holds
itself, then the 31 of its shared rare group ("Computed-style
sharing"), which no caller sees but in that order. `initial` supplies context-free seeds; border
colors, border widths, overflow and display also undergo the contextual
computation above. `ComputedStyle::new(&Budget)` fallibly builds initial values with invisible
borders. `try_clone` replaces owned Clone, and `Property::initial(&Budget)`
is fallible for owning values. `ComputedStyle::get` yields immutable typed
values (keywords, RGBA colors, px/percentage/auto and unresolved calc() lengths, family lists,
weights, scalar numbers, decoration flags, border spacing's px pair, and the grid track lists (`TrackList`), areas (`GridAreas`) and lines (`GridLine`) §4 describes, read through accessors); `font_size` and `color` are convenient
typed accessors. `custom_property(name)` exposes an immutable computed
token slice, or None for missing/guaranteed-invalid values. A computed style owns its values and cannot be populated
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
5. Real-stylesheet admission and coverage measurements. Keep downloaded
   Wikipedia/MediaWiki, MDN, Bootstrap, Debian/kernel.org, Rust Book,
   news and GitHub README pages and sheets only in the owning worktree's
   `.td-build-cache/agent-scratch/`. Record URLs, capture dates, hashes,
   source bytes, tokens, rules and declarations, nested conditional-rule
   shares, unsupported selectors/properties and custom-property, var()
   and calc() use. An approximate HTML extraction supplies tags, ids,
   classes and tree relationships without depending on td-html. Report
   accepted and skipped coverage beside median/p95/maximum matching work,
   tree time and named refusals, so discarded CSS cannot masquerade as
   good performance. Commit a short summary here and small original
   synthetic regressions, never downloaded corpus material. These
   measurements determine the tuning and property priorities below.
6. A conservative rule index on rightmost-compound ids, classes, tags
   and a universal fallback. Lists and logical functions must never lose
   candidates; deduplication must preserve source order and specificity.
   Charge candidate gathering as well as matching. Differential tests
   compare values with an unindexed reference, including adversarial
   universal rules. Tune admission and matching bounds from the corpus,
   name remaining refusals and retain bounded worst-case work.
7. Conditional rules. Bounded media-query evaluation takes an explicit
   caller environment: screen/print, CSS-pixel viewport width/height,
   resolution, preferred color scheme and reduced motion. Unsupported
   features evaluate through the specification's unknown-query semantics;
   nested conditions preserve order and are bounded even when false.
   Supports conditions query the implemented property/value grammar.
   Imports remain unfetched; a future caller may resolve imports under
   its own fetch policy and supply ordered sheets with inherited import
   conditions. td-css itself never follows a URL.
8. Custom properties and arithmetic. Custom properties inherit computed
   token values, with bounded maps, cycle detection, substitution depth,
   expanded bytes and work. var() includes fallback and shorthand
   substitution; a winning value invalid at computed-value time does not
   resurrect an earlier declaration. calc() supports typed lengths and
   percentages with +, -, *, / and bounded nesting; unresolved mixed
   px/percentage values remain typed expressions for layout. This
   increment may land as separate green custom-property and arithmetic
   commits. Each amends the current cascade/value contract above.
9. Layout-facing values chosen from the corpus frequencies: line-height,
   border styles/colors and border/font shorthands, vertical-align,
   text-indent/transform, visibility, overflow, float/clear, static and
   relative positioning, box-sizing, min/max-width, letter-spacing and
   opacity are the initial candidates. State selectors use explicit
   caller observations for hover, focus and checked state. Visited
   history remains absent and :visited always false; changing that needs
   a separate privacy design. Compute values only, without layout or
   painting. Repeat the corpus after conditional rules, variables and
   added properties: the extra admitted work must also fit the bounds.

The numbered units are independently landable work. The current accepted
grammars, public interfaces, initial/inherited values and named limits
are specified in §§1–6.
Corpus results describe the captured sample, not general Web conformance;
ad-hoc HTML extraction is not an HTML5 tree-builder correctness oracle.

Each increment is independently green, reviewed, readied and pushed on
`td-css-rolling`. Integration into a document tree or renderer requires a
separate workstream and design amendment.

## 8. Corpus evidence

The 2026-11-10 UTC capture contains the HTML and its inline and linked
stylesheets, without scripts, imported sheets or font/image downloads.
Wikipedia includes both MediaWiki `load.php` responses. A scratch-only
Python HTMLParser extraction retains tags, attributes, text presence and
parent/sibling links; it does not repair HTML as an HTML5 tree builder
would. Inline sheets precede linked sheets in this baseline harness;
link media attributes are not applied. These are workload measurements,
not a rendering or stylesheet-order oracle.

Counts use td-css tokenization and recursively inspect grouping at-rules
(media, supports, layer, container and related groups); keyframe bodies
and font-face descriptors are not style rules/declarations. Tokens include
whitespace. All eight complete sheet sets pass syntax and compilation
admission with the original bounds (100,000 matching steps); per-input bounds do not limit the
sum of tokens over separate sheets. The compiled count excludes skipped
at-rules and invalid selectors, but includes rules with no valid values.
Value acceptance below counts declarations, not expanded longhands, and
includes values inside skipped grouping rules. No corpus bytes are tests.

| Capture | CSS bytes | Tokens | Style rules / compiled | Declarations / accepted | Rules in media | Custom declarations / var() / calc() |
|---|---:|---:|---:|---:|---:|---:|
| [Bootstrap cheatsheet](https://getbootstrap.com/docs/5.3/examples/cheatsheet/) | 237,445 | 48,098 | 2,583 / 1,005 | 5,642 / 1,654 | 1,368 (53%) | 1,198 / 1,380 / 134 |
| [Debian](https://www.debian.org/) | 20,075 | 5,218 | 133 / 88 | 417 / 212 | 14 (11%) | 0 / 0 / 0 |
| [GitHub Rust README](https://github.com/rust-lang/rust) | 2,532,428 | 390,843 | 16,135 / 5,896 | 43,118 / 6,385 | 5,185 (32%) | 11,127 / 21,768 / 1,171 |
| [kernel.org](https://www.kernel.org/) | 13,860 | 3,577 | 108 / 55 | 321 / 169 | 50 (46%) | 0 / 0 / 0 |
| [MDN CSS](https://developer.mozilla.org/en-US/docs/Web/CSS) | 116,139 | 22,884 | 743 / 350 | 1,953 / 479 | 108 (15%) | 373 / 1,235 / 25 |
| [The Guardian](https://www.theguardian.com/international) | 639,068 | 95,371 | 1,133 / 433 | 18,473 / 1,952 | 477 (42%) | 13,292 / 190 / 44 |
| [Rust Book](https://doc.rust-lang.org/book/ch03-01-variables-and-mutability.html) | 50,083 | 11,116 | 272 / 221 | 917 / 238 | 13 (5%) | 252 / 137 / 16 |
| [Wikipedia CSS](https://en.wikipedia.org/wiki/Cascading_Style_Sheets) | 252,655 | 45,142 | 1,516 / 564 | 3,585 / 1,260 | 766 (51%) | 421 / 565 / 139 |

The unindexed matcher charges selector tests, hops and inspected string
bytes. Per-element work below is median/p95/maximum; refused calls show
consumed work before the failing charge, not their unbounded demand.
Compute time is one release-mode whole-tree pass on the development host,
excluding the separate work-counter pass, parsing and I/O. It is an
observation, not a portable timing threshold. Failed parent computations
supply no parent to later calls, so timings with refusals do not measure a
successfully styled tree.

| Capture | Elements | Match steps p50 / p95 / max | Refused elements | Compute ms |
|---|---:|---:|---:|---:|
| Bootstrap cheatsheet | 1,422 | 14,559 / 66,295 / 84,313 | 0 | 49.54 |
| Debian | 267 | 525 / 1,850 / 2,587 | 0 | 1.22 |
| GitHub Rust README | 2,246 | 99,978 / 100,000 / 100,000 | 1,465 | 390.78 |
| kernel.org | 325 | 238 / 449 / 715 | 0 | 1.17 |
| MDN CSS | 5,239 | 2,661 / 7,007 / 12,534 | 0 | 55.83 |
| The Guardian | 4,831 | 5,425 / 12,627 / 46,519 | 0 | 71.64 |
| Rust Book | 272 | 1,244 / 6,212 / 10,796 | 0 | 2.15 |
| Wikipedia CSS | 6,404 | 10,110 / 32,086 / 99,883 | 1 | 152.88 |

Both observed refusals are `MAX_MATCH_STEPS`: GitHub on 1,465 elements
and Wikipedia on one. A synthetic 5,000 unrelated class rules against an
element with a 33-byte class attribute reproduces that failure. The rule
index must remove that unrelated work before considering a budget increase.
Universal and structural rules must retain a finite refusal path.

Skipped groups include media/supports on most sites, GitHub's layers and
containers, and font-face/keyframes. Common unsupported selector names
in the outer preludes are hover (932), before (849), after (730), focus
(442), lang (410), active (206), focus-visible (190) and checked (145).
These occurrence counts do not recursively enumerate logical arguments.
Properties with high frequency outside the first set include position
(1,356), border (694), line-height (663), border-color (551), max-width
(425), overflow (416), opacity (328), min-width (310), vertical-align
(239) and float (215). The layout-facing work in §7 includes these, plus
their companion longhands, font shorthand, clear, visibility, text-indent,
text-transform, box-sizing and letter-spacing. Relative offsets are needed
with relative positioning; absolute/fixed/sticky positioning, flex/grid
layout, border-radius, shadows, generated content and images remain beyond
that property increment. These captures, and the runs below, predate
§4's functional colours and `background` shorthand, which they counted
as discarded; the corpus has not been replayed since.

Capture identities below are SHA-256. The CSS identity hashes, in capture
order, each filename, NUL, its file SHA-256 in lowercase hex, and LF; it
identifies the bundle without retaining downloaded bytes in git.

| Capture | HTML SHA-256 | CSS bundle SHA-256 |
|---|---|---|
| Bootstrap cheatsheet | `01594c4e79d9a6c259ece6618555f77f683559c8a98777c5333784dd12dcf807` | `5a76551aee0901464d76b18160e0824dd57a746b69a3dba5a5be132d9d16b242` |
| Debian | `06bdc137f5ecc3dfc8ec29ce208b46dd1e1baa82812e52c06aae1987adc1757d` | `85f29ed36e7490d65e4dfcf355f9d8dae41a60a8c9af63da7a20b5af0238e4b0` |
| GitHub Rust README | `a1d78d00873a2803489148f478f44a4e01a2da8179d29beae35177b8586b3188` | `b20b69b4d29b7e7ac1091c280cbb80d36f1d24962e049e7579d188343a87e0c6` |
| kernel.org | `33bf3d63d934d94a82ae68bd86985f818c2775717ab9d54c46d364c0d2cf320a` | `00ea93489686e927d119403897a53c01552283207a68f73c2b30e2623edf25a5` |
| MDN CSS | `5900f37fc4684a75ccbc81affdcbcdd477a56983a2dacc873b4b4b97151510aa` | `558f2503a24d994262998c0d006fe74e2ef3975d1806b619f49f846e5265480b` |
| The Guardian | `1ef64be3026b305e77e319bba9be5996d21fe7c0fae9223b04364d361ad13a59` | `d41245206e720d343d3f3c0c564f52b58bb750f885054e337ad0ba8859b141bd` |
| Rust Book | `f06a10a850287ef35db88d0f0080d269eea9f736f0e295e22d803ab7b635b7ce` | `caba047cab8ac3c227028122608833285f7a7ba0a26bb9228bfc0664aa016802` |
| Wikipedia CSS | `7ac5bb7612c91b2b48ba95553b1eb8b4abeaa26deef7dc3a54e11a9f82694f6d` | `60b1fc167a3aea176e67ef38396975aa953885f2df4defbc7e57b5f09a4d0873` |

### Indexed workload

The same captured trees and sheets, with candidate collection charged,
produce no element refusals at MAX_MATCH_STEPS = 150,000. At 100,000 the
index alone left eight GitHub refusals; a diagnostic run with a larger
scratch-only cap measured the full worst cost as 105,426. The chosen cap
leaves about 42% headroom over that observed maximum; other existing
admission bounds remain unchanged because no capture exhausted them.
This is sample-based tuning, not a claim that arbitrary pages fit.

| Capture | Candidate rules p95 / max | Match steps p50 / p95 / max | Compute ms |
|---|---:|---:|---:|
| Bootstrap | 57 / 61 | 914 / 3409 / 6394 | 9.69 |
| Debian | 5 / 10 | 66 / 462 / 807 | 0.89 |
| GitHub | 220 / 234 | 13083 / 78717 / 105426 | 67.63 |
| kernel.org | 7 / 7 | 48 / 88 / 129 | 1.07 |
| MDN | 40 / 97 | 1220 / 3294 / 4764 | 32.95 |
| The Guardian | 35 / 39 | 847 / 1613 / 6122 | 32.86 |
| Rust Book | 9 / 13 | 124 / 541 / 1429 | 1.03 |
| Wikipedia | 20 / 121 | 2335 / 18048 / 28482 | 69.57 |

Universal/structural rules, cyclic caller trees, repeated class words
with large buckets, overlong observed fields and index-entry exhaustion
still refuse under named bounds. Synthetic and deterministic generated
differential tests compare computed values, list specificity and order
against the unindexed path where both complete; admission errors may
change because unrelated work is removed and candidate gathering is
charged. The 5,000-rule fixture succeeds with zero candidates while its
unindexed reference still exhausts the budget.

### Conditional workload

The same corpus also runs with desktop screen (1280×720, 1 dppx, light),
mobile screen (375×812, 2 dppx, dark), and print (800×1100, 3 dppx, light),
all without reduced motion. Every sheet set and every element completes.
GitHub's desktop set needs 8,259 compiled rules, exceeding the former
8,192 aggregate cap; MAX_COMPILED_RULES is therefore 16,384 while the
per-list syntax bound remains 8,192. False groups still contribute to
the independent visited-rule and nesting bounds. Matching stays at
150,000 steps; the largest measured call is 107,059.

| Capture | Compiled rules desktop / mobile / print | Maximum steps across environments |
|---|---:|---:|
| bootstrap | 2036 / 1066 / 1556 | 8,345 |
| debian | 93 / 97 / 92 | 807 |
| github | 8259 / 6205 / 7185 | 107,059 |
| kernel | 55 / 99 / 55 | 311 |
| mdn | 382 / 397 / 400 | 5,251 |
| news | 733 / 533 / 633 | 7,518 |
| rust-book | 221 / 225 / 221 | 1,429 |
| wikipedia | 1019 / 1084 / 664 | 49,863 |

### Custom-property workload

The same desktop, mobile-dark and print configurations complete every
captured element without refusal after admitting custom properties and
pending var() values. Largest per-element substitution charges across
those runs are below; matching maxima remain under 108,000 steps. These
costs include graph construction and copies, not just final value size.

| Capture | Substitution bytes / component visits, maximum |
|---|---:|
| Bootstrap | 7,919 / 1,026 |
| Debian | 0 / 0 |
| GitHub | 132,742 / 8,737 |
| kernel.org | 0 / 0 |
| MDN | 20,837 / 1,594 |
| The Guardian | 37,950 / 2,584 |
| Rust Book | 3,329 / 435 |
| Wikipedia | 15,744 / 982 |

Desktop whole-tree release compute times were 24.44ms (Bootstrap),
102.24ms (GitHub), 593.88ms (MDN), 246.00ms (Guardian), 141.13ms
(Wikipedia), and under 2ms for the remaining three. Custom maps and
substitution add real work, especially where many elements declare local
variables. These host timings are observations, not thresholds or visual
correctness claims. Functional colors and unsupported arithmetic units still limit how many
substituted values become implemented property values.

### Arithmetic workload

Replaying the same three environments with length calc() enabled admits
all sheet sets and computes every captured element without refusal.
Candidate counts and matching work are unchanged (maximum 107,059 steps),
as are the substitution maxima above. Desktop whole-tree compute ranged
from 0.98ms to 749.38ms. Unsupported viewport/font-metric units, numeric
calc() values and functional colors still discard declarations; successful
admission does not imply complete styling coverage.

### Layout-value and state workload

All eight complete sheet sets and extracted trees pass the desktop,
mobile-dark and print runs with the 55-longhand grammar. A separate
interactive desktop run reports focus on the first link, hover on that
link and its ancestors, and checked/selected input/option attributes as
caller state; it also has no refusals. This is synthetic state supplied
by the harness, not browser event processing. Source sizes, tokens and
all-rule/declaration counts are unchanged from the capture table.

| Capture | Compiled rules desktop / mobile / print | Accepted declarations / all | Maximum match steps across four runs | Desktop compute ms |
|---|---:|---:|---:|---:|
| bootstrap | 2097 / 1127 / 1617 | 2818 / 5642 | 8,788 | 34.41 |
| debian | 113 / 117 / 112 | 307 / 417 | 889 | 1.69 |
| github | 8617 / 6558 / 7541 | 20177 / 43118 | 138,862 | 118.58 |
| kernel | 55 / 101 / 55 | 227 / 321 | 311 | 1.89 |
| mdn | 451 / 467 / 470 | 1066 / 1953 | 9,308 | 599.21 |
| news | 837 / 646 / 747 | 2860 / 18473 | 7,526 | 261.12 |
| rust-book | 245 / 247 / 241 | 439 / 917 | 1,432 | 2.14 |
| wikipedia | 1045 / 1110 / 671 | 2168 / 3585 | 50,042 | 165.18 |

Acceptance counts ordinary declarations whose property grammar parses,
including pending var() declarations and declarations in skipped groups;
custom definitions are counted separately in the capture table. A pending
value may still become invalid after substitution. No assertion of visual
correctness follows from these counts. All source/admission/substitution
bounds remain unchanged: largest substitution charges are below 133KB
and 8,800 components. The largest matching demand is 138,862, leaving
about 8% headroom under 150,000; universal selectors, different states or
larger caller fields can still refuse. Layers, containers, pseudo-elements,
language selectors, functional colors, images and unsupported positioning
and sizing values account for substantial remaining discarded CSS.

## Computed-style sharing

`sharing::StylePool` is an optional document-local owner of complete
computed styles. `intern` returns an opaque StyleId; `get` borrows the
canonical value. Equal values with equal custom properties share an ID,
including separately computed siblings. Custom maps are canonicalized
independently, and inherited token allocations retain their BudgetShared sharing.
A fingerprint only selects an equality bucket. Signed zero hashes alike.
No global cache or tree dependency is introduced. A custom map is keyed
by the same fingerprint over its names and tokens. A complete
style's fingerprint takes about 245 writes (some 320 when a style held
every longhand, below), which SipHash-1-3
would run a round per 8-byte block over and buffer write by write; it is
instead a fold of 64-bit words, each with the state and the pool's own
key multiplied out to 128 bits and the halves combined (wyhash's mum
step). A string is its 8-byte words, its tail zero-padded to one, and
then its length, so two strings make one word sequence only if equal.
No input difference survives the mixing step with certainty, as one
did a multiply-rotate fold's (a sign and an ulp, `calc(3px + 50%)`
against `calc(-3px + 50.000000000000114%)`, always shared its key, so
eleven such properties made 2,048 styles one bucket), and a page cannot
search offline for a pool's key: `PoolLimits::seed`, which a caller with
entropy sets, else one mixed from the addresses ASLR gave the pool's
budget, the constructing frame and a static. td-css makes no system call
for it (§6): RandomState would read the OS's entropy and std panics where
that fails. The default is as strong as ASLR and no stronger; a page that
learns an address can collide styles. The defence either way is that a
collided bucket is bounded by MAX_STYLE_COMPARISONS and MAX_POOL_WORK,
and equality, not the key, decides sharing.

A style holds its 52 common longhands itself and shares a group of
the 31 rarely set ones, the roster's last (`Property::FIRST_RARE` on):
`clear`, `opacity`, `top`, `right`, `bottom`, `left`, `table-layout`,
the twelve flex and alignment longhands and the twelve grid ones (§4,
"Grid"). None inherits, and most elements set none: §8's corpus lists
`opacity` (328 declarations over eight sites) among its frequent
properties but neither `clear` nor the offsets, which only positioned
boxes read, and only tables, flex and grid boxes set the rest. No
common longhand's computation reads one, nor does `finish`; the one
rare value read from a parent is `justify-items`' legacy position (§4,
"Grid"), and only from a parent whose group is not the initial one.
A style set holds one initial group, which every style it computes
shares until a rule sets one of them, so an element that sets none
never computes, stores, fingerprints or compares them. A winning rare
declaration is substituted and computed as any is, after the common
longhands and so with the element's own font size; `inherit` reads
the parent's group and `initial` and `unset` the initial values. The
first value that differs from the initial group's copies the group
(31 values, a grid value's storage shared by its handle, not copied)
and writes the copy; one that computes to the initial value copies
nothing. A group marks, in a 64-bit `set`, the slots whose value is not
its initial one to the bit, kept by every write, so the pool's
fingerprints and comparisons visit the marked slots alone and a
group's cost does not grow with the roster: adding the grid longhands
added nothing to the hashing of a flex group. `get` takes one branch to the group, and
`set`, which `finish` and tests write through, copies a group before
writing a rare value, so no group is written once shared. Display,
float, position, overflow, box-sizing, the minimum and maximum sizes
and the border longhands stay common: the UA sheet or the usual author
resets set them on many elements, and `finish` writes four of them.

The pool canonicalizes groups by contents. No rare longhand inherits,
so the one group that reaches it already canonical is a style set's
initial group: the pool keeps the last group that arrived canonical,
or became so, with its fingerprint, and takes a style whose group is
that same allocation at once, without a visit. Its fingerprint is kept
there, not in the group: the key is the pool's, a group is shared by
pools of different keys (its style set's documents), and an unkeyed
digest a group carried would let a page search for collisions offline.
Any other group is fingerprinted over its mark and its marked slots,
each slot's number and value, a grid value by its contents (a track
list's items and names, an areas value's names and ranges, a line's
name), with the pool's key, looked up, and compared with its bucket's
(a visit, and a visit a marked value and a track list's item or a name
byte, as its work, and a comparison each against
MAX_STYLE_COMPARISONS), then either replaced by its canonical equal or
retained, charged once: 128 bytes for its index entry and its doubled
storage, 1,632 bytes in all (1,040 with 19 longhands and no mark), and
its grid values' own storage, doubled as a style's font families are:
a track list's or an areas value's owner and its arrays at their
capacity, and each name twice its heap capacity and 32 bytes. A group
holding a value is charged it however many groups share its handle, so
two distinct groups with one track list are charged it twice. Nothing is
looked up by a group's address, which a freed group's successor
reuses, so the pool's work on groups depends on the page alone. Groups
compare to the bit, as the cascade does a value against the initial
group's, so a group shared between styles never gives one another's
signed zero: `order:-0` keeps its sign, where `==` would take it for
the initial 0; a track list compares by address, then its items to the
bit. Its bucket is keyed to the bit as well: eleven rare longhands
(and any track list's sizes) can be a zero of either sign, and a
fingerprint that hashed
signed zeros alike gave a group's 2,048 variants one bucket a page
could fill without the key (1,500 elements over them, or 10,000 over
256, were refused MAX_STYLE_COMPARISONS; both are now admitted, three
styles, with each variant retained). Each variant is charged its
1,632 bytes though its style merges, so a page of tens of thousands
of distinct signed-zero combinations (64,000 elements over 64 `order`
classes and 1,024 variants, about 1.7 MB) is refused MAX_POOL_BYTES
where a style holding every longhand admitted it in 6.5 MB. The one
pass over its marked slots also gives a second fingerprint that hashes
signed zeros alike and passes over a slot `==` its initial value (the
initial zero's other sign, marked though `==`), kept beside the group
and in `known`; a style's fingerprint folds that one as one word, as
styles' `==` does, and the pool compares two styles' canonical groups
by address, then, for groups apart by a signed zero alone (a visit and
one a slot either marks), by `==`, so such styles are one style, the
first, as when a style held every longhand. `ComputedStyle`'s own
equality compares groups by address, then by `==`.

An element that sets a rare value pays for a copy, an allocation, and
its group's hashing, lookup and comparison or insertion in the pool,
where every element once paid for those longhands' computation,
hashing and comparison. A novel style with a novel group costs about
98 visits, against 88 when a style held every longhand (`<p
style="order:N">`, each distinct: 196,524 visits for 2,000 against
176,386), so pages of mostly distinct groups take 11 to 14% more pool
work: 5,000 `<p style="order:N;width:Npx">`, each distinct, 515,676
against 452,962. Pages whose elements share a few groups take less:
2 MiB of flex bars, three elements in four setting one, 22,024,593
against 21,745,786 (+1.3%), and fewer instructions; the Wikipedia
pages, a fifth less (below). An element whose group is not canonical
spends one equality probe more against MAX_STYLE_COMPARISONS than when
a style held every longhand: at most 786,432 of its 1,048,576 at
td-html's MAX_NODES, three a node (style, group and custom map), so
another canonicalized group a style carried would reach the limit.

Each element's style holds every common longhand, so a common longhand
costs every element its computation, its insertion, and its share of
the pool's fingerprint and equality test. The cascade reads the style's own font
size and colour once each is computed, and an unset property, most of
every element's, copies its parent's value or its initial one inline,
with no Result between. Measured per cascade, release, pinned to one
CPU, user-mode `perf stat` (median of five, four cascades less one),
2026-11-15 under a load average of 7 to 12, against the 56-longhand
cascade before these:

| Page | Instructions before / now (M) | Cycles before / now (M) |
| --- | ---: | ---: |
| 2 MiB blocks (td-layout §6; 91,183 elements, HTML_UA_CSS alone) | 5,541 / 4,140 | 1,847 / 1,699 |
| Wikipedia, *Cascading Style Sheets* (637,078 bytes) | 1,870 / 1,790 | 747 / 698 |
| Bootstrap 5.3 examples index (65,263 bytes) | 120 / 113 | 58 / 53 |

The blocks cascade's wall time is 398 to 402 ms against 435 to 449
(fastest of five, three processes each).

The pool's five indexes (style buckets by fingerprint, custom-map
buckets by fingerprint, canonical custom maps by address and their
token arrays by address, rare-group buckets by bitwise fingerprint) are
open-addressed tables of td-alloc storage, at most half full, their
slots chosen by the key mixed twice with the pool's key, which enters
each round, so a slot is no fixed function of the key XOR the pool's
key. A lookup or an insertion is charged two visits whatever it probes,
so a new key costs four (a lookup, then an insertion), and a table that
would pass half full doubles, charged its new size, the slots it
initialises, before anything is allocated or moved, so a refusal leaves
it whole. Probes, which depend on the key and the addresses ASLR gives
the pool, are bounded by construction instead: no run of occupied slots
reaches PROBE_LIMIT (64), so a probe for any key, present or absent,
reads at most 64 slots however often it is made. An insertion that
would make such a run doubles the table first, as does a growth whose
move made one, up to SPREAD (4) times the half-full size its entries
need, each size charged as any growth's; past that the insertion is
refused `MAX_POOL_WORK` with the table whole, which only keys built
against the pool's key can bring about, and a probe is refused before
it would read a 65th slot. An allowance averaged over operations, as
there was (eight an operation and 4,096), does not bound one key looked
up again and again: an address a freed custom map or group left, which
the next element's took, missed once an element over the same long
run, and refused up to 4% of the loads of a page of many elements over
a few values. Keyed homes make a run of 64 at most half full all but
impossible (14,000 addresses at strides of 8 to 4,096 bytes made none
longer than 31), so the pool's work is a function of the page alone in
practice, not of its key or of ASLR. Bucket order, and so every StyleId
and every shared map, does not depend on the key either. Measured 2026-11-15, release, pinned
(fastest of five cascades, three processes; pool work identical in
every process and with fixed keys):

| Page | Cascade ms | Pool work |
| --- | ---: | ---: |
| The Guardian, front page (1,374,527 bytes, its 20 sheets) | 159 to 165 | 21,441,319 (32% of MAX_POOL_WORK), 734 styles |
| Wikipedia, *Cascading Style Sheets* | 132 to 133 | 858,460 |
| Bootstrap 5.3 examples index | 10.2 to 10.4 | 955,481 |
| 2 MiB blocks | 388 to 404 | 12,035,902 |
| 1,000 / 2,000 / 4,000 elements, each its own one-property map | 6.0 to 6.2, 12.4 to 13.4, 25.8 to 27.0 | 112,364, 225,748, 452,516 |
| 6,000 such elements | `MAX_POOL_BYTES` at 41 | the pool's 2 KiB a map and a style's charge bind first |
| 4,000 elements, each its own 21-property map | `MAX_POOL_BYTES` at 95 | |
| 10,000 elements declaring one equal 20-property map | 330 to 334 | 7,540,032 |
| 5,000 elements reading a 60-deep `var()` chain | 72 to 74 | 6,650,828 |
| an 8,000-property map and 2,000 children adding one each | `MAX_POOL_BYTES` at 100 | |

A style is charged 9,728 bytes with its 52 common longhands and its
group's handle (13,056 when it held all 71, 10,944 with 59), and each
distinct rare group 1,632 bytes once and its grid values' storage, so
MAX_POOL_BYTES admits some 6,900 distinct styles (6,896 of `<p
style="width:Npx">`, each distinct; 5,137 when a style held every
longhand, some 6,100 before the flex longhands), 5,905 that each bring
a group of their own as well (`<p style="order:N">`; 6,230 with 19
rare longhands, 5,137 before the group), and some 5,500 elements each
with its own custom map (5,495 of `<p style="--x:N">`, as with 19;
4,317 before the group). Each figure is a page of such paragraphs
through td-layout's StyledDocument at default limits, HTML_UA_CSS its
sheet; a bare pool of the paragraphs' styles alone admits one or two
more.
Real pages hold a few hundred (*Germany* 466, The Guardian
734). Each common longhand added lowers both by 176 bytes a style; a
rare one adds 48 bytes to each distinct group alone, the grid ones 576
and the mark 16.

A lookup or an insertion on these pages probes 1.0 to 1.9 slots on
average, growth's moves aside (The Guardian's 13,169 token addresses
1.25, its longest run of occupied slots 18 of 32,768). For 14,000
addresses at strides of 8 to 4,096 bytes under three keys, moves
included: 1.54 to 1.58 probes an operation over their insertions (two
operations each), 1.35 to 1.40 a lookup that finds one, 1.97 to 2.08
one of 14,000 absent addresses, and a longest run of 19 to 31 of 32,768
slots. Custom maps that collide under every key cannot be written, since
a string's length follows its bytes; maps equal by value and new by
address, the case that does reach a bucket, are traversed and compared
once each, linearly.

Table layout's three longhands (border-collapse, border-spacing,
table-layout) and the sheet's table rules cost some of that back,
measured as above (median of five, 2026-11-15, load average 3 to 5):

| Page | Instructions before / faster / with tables (M) | Cycles (M) |
| --- | ---: | ---: |
| 2 MiB blocks | 5,541 / 4,140 / 4,324 | 1,801 / 1,615 / 1,723 |
| Wikipedia, *Cascading Style Sheets* | 1,870 / 1,784 / 1,801 | 666 / 657 / 669 |
| Wikipedia, *Germany* (1,647,153 bytes) | 4,076 / 3,901 / 3,929 | 1,461 / 1,431 / 1,465 |

The blocks cascade's wall time is 392 to 397 ms with them, 371 to 374
without, 404 to 408 before both (fastest of five, three processes
each). The longhands' own cost needed styles that do not hold every
longhand: `table-layout` has since moved to the rare group (above),
and the two inheriting ones stay common.

The twelve flex and alignment longhands (59 to 71) cost each element
twelve more computed values, insertions, fingerprint words and equality
tests, about as much again per longhand as the table ones. Measured
2026-11-24 as td-layout's `StyledDocument::new` (cascade, boxes and
pool), release, pinned to one CPU, user-mode `perf stat`, four
cascades less one, median of five (seven for blocks, *Cascading Style
Sheets* and *Germany*), interleaved with the parent build, load average
3 to 4. The 2 MiB shapes use HTML_UA_CSS alone but flex, whose 2 MiB of
`<nav>` bars carry a sheet of every new property; the Wikipedia pages
were captured that day with their two linked sheets and inline styles:

| Page | Instructions before / with flex (M) | Cycles before / with flex (M) |
| --- | ---: | ---: |
| 2 MiB blocks | 4,384 / 5,022 (+14.6%) | 1,794 / 2,029 |
| 2 MiB table rows | 5,109 / 5,816 (+13.8%) | 2,124 / 2,444 |
| 2 MiB nested lists | 4,472 / 5,139 (+14.9%) | 1,896 / 2,225 |
| 2 MiB flex bars | 5,108 / 6,411 (+25.5%) | 2,204 / 2,799 |
| Wikipedia, *Cascading Style Sheets* (637,055 bytes) | 1,810 / 1,851 (+2.2%) | 602 / 626 |
| Wikipedia, *Germany* (1,645,458 bytes) | 3,945 / 4,009 (+1.6%) | 1,305 / 1,340 |
| Wikipedia, *List of countries and dependencies by population* | 3,355 / 3,415 (+1.8%) | 1,143 / 1,179 |
| Wikipedia, *List of chemical elements* | 2,117 / 2,163 (+2.2%) | 705 / 752 |

The blocks cascade's wall time is 475 to 485 ms with them, 416 to 424
without (fastest of four, seven processes each). The flex bars also
parse and cascade the declarations the parent dropped. Each style is
charged 2,112 bytes more (176 a longhand), so a page's styled charge
grows with its distinct styles: *Germany*'s 465 by 1,036,800 bytes
(8.5 to 9.6 MB, 3.3% of its page charge), the 2 MiB shapes' three to
eight by 6 to 17 KB. Pool work grows with the fingerprint and equality
words: blocks 12,035,902 to 14,224,234 (+18%), *Germany* 1,669,380 to
1,929,851.

The rare-property group (above) recovers it. Measured 2026-11-25 as
above (`StyledDocument::new`, release, pinned, user-mode `perf stat`,
four less one, median of five, the two builds interleaved, load average
3.2 to 7.9) on this change's own 2 MiB shapes, HTML_UA_CSS with a small
author sheet where named (blocks are `<div><p>` pairs, 124,017
elements; the flex bars set flex longhands on three elements in four;
the cards and the Bootstrap-like grid set them on fewer), and the same
four Wikipedia pages fetched again that day:

| Page | Instructions every longhand / group (M) | Pool work every longhand / group |
| --- | ---: | ---: |
| 2 MiB blocks | 6,479 / 5,448 (−15.9%) | 19,346,338 / 14,633,828 |
| 2 MiB table rows | 7,952 / 6,743 (−15.2%) | 21,696,106 / 16,411,221 |
| 2 MiB nested tables | 10,051 / 8,460 (−15.8%) | 28,647,778 / 21,669,537 |
| 2 MiB floats | 5,588 / 4,874 (−12.8%) | 16,444,126 / 13,844,073 |
| 2 MiB inline-blocks | 2,994 / 2,481 (−17.2%) | 9,966,454 / 7,538,789 |
| 2 MiB nested lists | 8,800 / 7,500 (−14.8%) | 24,790,690 / 18,751,999 |
| 2 MiB inline tables | 2,952 / 2,467 (−16.4%) | 9,673,102 / 7,316,896 |
| 2 MiB flex bars | 7,327 / 6,902 (−5.8%) | 21,745,786 / 22,024,593 (+1.3%) |
| 2 MiB flex cards | 5,372 / 4,929 (−8.2%) | 15,476,470 / 14,682,926 |
| 2 MiB Bootstrap-like grid | 3,738 / 3,452 (−7.6%) | 10,182,810 / 9,940,463 |
| 20,000 `<p style="order:K">`, 30 values | 1,219 / 1,133 (−7.0%) | 3,118,162 / 3,158,381 (+1.3%) |
| 100,000 `<p style="--x:K">`, 126 values | 6,667 / 5,837 (−12.5%) | 17,618,808 / 13,821,205 |
| Wikipedia, *Cascading Style Sheets* | 1,851 / 1,798 (−2.9%) | 985,959 / 791,352 |
| Wikipedia, *Germany* | 4,025 / 3,917 (−2.7%) | 1,929,774 / 1,535,926 |
| Wikipedia, *List of countries and dependencies by population* | 3,414 / 3,335 (−2.3%) | 1,623,272 / 1,278,589 |
| Wikipedia, *List of chemical elements* | 2,164 / 2,106 (−2.7%) | 1,222,049 / 967,584 |

The blocks cascade's fastest run takes 499 ms against 625, the
Wikipedia pages' cycles and times move within run-to-run noise, and
their instructions fall. Every style is charged 3,328 bytes less, so a
page's styled charge falls with its distinct styles: *Germany*'s 466
from 9,565,490 to 8,060,402 bytes, *Cascading Style Sheets*' 365 from
6,451,863 to 5,264,183, the 2 MiB shapes' three to nine by 9 to 25 KB.
Every element's computed values, to the bit, its cascade work and
boxes, every style and its number, and the layouts at 1280 and 832 px
and windowed, are the same with the group as without it on all of
these and on 3,000 generated pages setting rare longhands, signed zeros
among them, through rules, inline styles, shorthands, `var()`,
CSS-wide keywords and `!important` (their layouts on 500 of them).
Pool work, and whether a page is admitted, is the same in every
process: 300 to 1,000 loads each of the pages that refused up to 4% of
loads under the averaged probe allowance (above) were all admitted.

The grid longhands grew the group from 19 values to 31, and with it
every rare group's hashing and comparison by 63%, the flex bars' pool
work by 15% and their instructions by 13%, until the group's mark
(above) limited both to its marked slots. The same change computes a
winning declaration's specified value in place, where the cascade
cloned each before computing it. Measured 2026-11-25 against the parent
(the cascade, boxes and pool over a parsed document, release, pinned
to one CPU, user-mode `perf stat`, four cascades less one, median of
five, the builds interleaved, load average 3.9 to 4.7) on the 2 MiB
shapes above and the two Wikipedia pages fetched that day with their
sheets:

| Page | Instructions parent / grid (M) | Pool work parent / grid |
| --- | ---: | ---: |
| 2 MiB blocks | 5,906 / 5,596 (−5.2%) | 15,733,116 / 15,733,098 |
| 2 MiB table rows | 4,934 / 4,696 (−4.8%) | 12,206,881 / 12,206,863 |
| 2 MiB nested tables | 8,028 / 7,634 (−4.9%) | 20,344,397 / 20,344,379 |
| 2 MiB floats | 4,682 / 4,444 (−5.1%) | 12,102,439 / 12,102,421 |
| 2 MiB inline-blocks | 2,680 / 2,523 (−5.9%) | 8,137,928 / 8,137,910 |
| 2 MiB nested lists | 5,756 / 5,456 (−5.2%) | 15,324,677 / 15,324,659 |
| 2 MiB inline tables | 5,936 / 5,646 (−4.9%) | 14,984,247 / 14,984,229 |
| 2 MiB flex bars | 6,490 / 6,057 (−6.7%) | 20,385,253 / 16,643,683 (−18.4%) |
| 2 MiB flex cards | 5,999 / 5,668 (−5.5%) | 15,514,244 / 14,605,158 (−5.9%) |
| 2 MiB Bootstrap-like grid | 4,915 / 4,548 (−7.5%) | 14,422,630 / 11,961,185 (−17.1%) |
| 20,000 `<p style="order:K">` | 1,159 / 1,070 (−7.7%) | 3,131,756 / 2,474,873 (−21.0%) |
| 100,000 `<p style="--x:K">` | 5,267 / 5,049 (−4.1%) | 12,492,179 / 12,492,161 |
| Wikipedia, *Cascading Style Sheets* | 1,796 / 1,785 (−0.6%) | 791,352 / 785,313 |
| Wikipedia, *Germany* | 3,737 / 3,709 (−0.7%) | 1,536,223 / 1,520,808 |

A page setting no grid longhand computes the same values, to the bit,
the same styles and numbers, cascade work and layouts at 1280 and 832
px and windowed, on all of these and on 3,000 generated pages; its
charge grows 592 bytes a distinct group (`known` initial group
included) and its pool work falls with the rare values it sets (18
visits less where it sets none: the initial group's 19 visits are one).
The Wikipedia pages' sheets set grid longhands on a few elements, so
their declaration counts grow and five styles more are retained (365
to 370 and 457 to 462, *Germany*'s charge 7,971,810 to 8,065,970
bytes); every other longhand's value and every box is unchanged. On a
page that sets them, the styles the pool retains change, and with them
which of two styles `==` but apart by a signed zero comes first: the
pool merges such styles into the first (above), so an element may take
the other zero, `margin:-0` for `0`, as it may whenever a page's styles
change. Review found 9 of 1,500 generated pages setting grid longhands
laid out apart from the parent by that alone. A group's grid values
are hashed by their contents, not their handles, so a page that sets a
1,000-track list on every element pays some 1,000 visits an element:
2 MiB of `<div><p>` with four such lists in em and a 20 by 50 areas
value on `*` is refused MAX_POOL_WORK in 0.63 s, the parent admitting
it, without them, in 0.56; 2 MiB of inline styles each with its own
1,000-track list (some 500 styles, 55 MB charged) is admitted in 160
ms, and of 300-cell inline areas values (776 styles) in 81 ms.

PoolLimits may lower MAX_STYLES (16,384), MAX_POOL_BYTES (64 MiB),
MAX_POOL_WORK (67,108,864 visits: fields, components and string bytes
traversed, two an index lookup or insertion, and each slot an index's
growth initialises), and
MAX_STYLE_COMPARISONS (1,048,576 equality probes). These caller-adjustable
pool limits are separate from the fixed parser/cascade limits in §5.
Storage charges precede pool retention and include conservative map/node
allowances, doubled vector/string capacity and nested token storage.
Canonical custom maps and shared token allocations count once. Pool
metadata stays charged even when a failed insertion retained only a new
custom map. A refusal never returns an ID for an incomplete style.
The newly computed candidate and cascade scratch remain caller costs.

StylePool::new is fallible; with_budget shares a page's chosen owner.
Exact owned allocations and the conservative PoolLimits admission model
are separate constraints. A refused insertion may retain valid custom-map
or empty-bucket cache entries, and consumes work/admission units already
used. Published pointer identities always have a retained owner; style IDs
are published only after their style is stored. Dropping the pool releases
its allocations, while shared owners elsewhere retain their own charges.
Failure tests inject refusal at every allocation in compilation, computation
and interning, check release or valid partial cache ownership, and retry.
