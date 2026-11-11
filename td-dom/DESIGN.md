# HTML documents as CSS subjects

td-dom connects td-html's immutable document tree to td-css's selector
boundary. It is a standalone std-only library depending only on those two
crates. No application consumes it yet. It fetches nothing, performs no
layout or painting, and does not change the resident HTML renderer.
td-html itself has no td-css dependency, so its existing users gain no CSS
closure.

## Projection and API

`HtmlTree::new(&Document)` builds navigation for the HTML elements reachable
from the document root. `with_state` additionally borrows a `StateProvider`.
`element(NodeId)` validates membership; `elements()` visits actual document
order, parents first, which can differ from arena allocation order after
formatting repair. Handles are cheap `Copy` values borrowing the index and
original element. IDs remain local to their document. The document and
state provider must outlive the index; the API exposes no mutation.

The projection excludes foreign elements and their entire subtrees,
including HTML descendants of foreign integration points. Detached template
content fragments and detached formatting nodes are not reached. The HTML
template element itself remains a subject, with its ordinary empty child
list. All structural selectors, including `:empty`, describe this HTML
projection: an element with only a foreign child is empty in that model.
This is an explicit limitation until the styling boundary can address
foreign namespaces. It does not remove those nodes from the document API.

`ElementRef` implements `td_css::selectors::Element`. Parent, previous/next
sibling and first-child queries are element-only, excluding text and
comments. `has_text` observes nonempty direct text children, including
whitespace; comments do not count. HTML names and unnamespaced attributes
come from the parser. An `a` or `area` with an href attribute is a link,
even if the value is empty. No URL is fetched or validated here.

The default state provider returns false for hover, focus and checked.
A caller can supply those states through `StateProvider::state(NodeId,
State)` and must keep them stable throughout a matching/cascade operation.
It owns hover propagation to ancestors, focus, and checked/selected
host-language semantics; attributes alone do not select a dynamic state.
There is no visited-history query, and td-css's visited selectors remain
false.

The caller supplies stylesheet order, the media environment, inline style
text, the parent's computed values and the computed root font size to
td-css. The adapter neither stores computed styles nor silently substitutes
defaults after a refusal. A layout consumer chooses its own retention and
refusal policy. Source encoding and resource fetching remain caller work.

## Bounds and cost

`MAX_INDEX_NODES` is td-html's `MAX_NODES`, 262,144, including document,
comment, text, fragment and detached nodes. Membership/link storage has
one fixed entry per arena node, and a second preallocated vector holds
HTML element IDs in document order. These arrays are admitted before
allocation. `MAX_INDEX_BYTES` is the sum of their maximum payloads:
7 MiB on the current target (24-byte link entry plus a 4-byte NodeId),
excluding allocator metadata, the index object and the borrowed document.
`index_bytes()` reports the actual allocated array capacity cost.

Construction walks the ordinary document links once and charges each
visited node and ancestor ascent to `MAX_INDEX_WORK`, twice
`MAX_INDEX_NODES`. Refusal returns `Error::{Nodes, Work, InvalidTree}` and
no partial index. The immutable Document API cannot construct invalid or
cyclic links, but index lookups still validate IDs. No recursion, unsafe
code, unchecked indexing or production panic is used.

Selector navigation and text-presence callbacks are constant-time. They
never scan arbitrarily many comments or text nodes between elements.
Attribute lookup visits at most td-html's `MAX_ATTRIBUTES_PER_TAG` (256)
entries; that bound also applies after repeated html/body tags merge
attributes. Name/value length and document allocation/work bounds remain
owned by td-html/DESIGN.md. State-provider execution is caller code; the
adapter cannot bound time spent there. td-css retains its own selector,
matching, stylesheet and substitution bounds.

## Test contract

Small in-memory tests parse HTML and cascade `HTML_UA_CSS` with author and
inline declarations, asserting display, colors, inherited font size,
font-relative lengths, custom properties and calc(). Navigation tests
cover comment/text gaps, repaired parent order, templates and foreign
isolation. State tests cover caller-reported interaction and visited
privacy. Dense-document and counter-edge tests pin the index ceilings.
There are no filesystem test fixtures or external test dependencies.

The resident renderer remains covered by the frozen pre-workstream
plain/rich differential described in td-html/DESIGN.md. The adapter cannot
alter those render paths and none of their consumers changes here.

## Whole-page measurements

The scratch corpus recaptures the same eight public URLs as td-css's
corpus: Wikipedia's CSS article (including MediaWiki load.php sheets),
MDN CSS, Bootstrap's cheatsheet, Debian, kernel.org, the Rust Book chapter,
The Guardian and GitHub's Rust README. These are fresh captures, not an
assertion that a site's bytes stayed identical to the earlier CSS run.
Only HTML, inline styles and linked stylesheets are fetched; no script,
import, font or image resource is fetched. Stylesheet order follows source
order; link/style media attributes wrap their sheets in a media query.
Unsupported CSS retains td-css's documented behavior.

Each sample is a separate release process on the development host, with
file reads outside the timer. Total time includes HTML parsing, index
construction, CSS compilation and one parent-first cascade of every
indexed element, retaining all computed styles in this harness. RSS is
Linux VmHWM for the whole process, including input HTML, stylesheet text,
compiled rules, the document, index and retained styles. Parse-only RSS
comes from a separate process without stylesheet loading. Measurements
are observations under host load, not portable performance thresholds or
allocator bounds.

Desktop uses screen 1280x720, 1 dppx, light and no reduced motion. Other
runs use screen 390x844 dark, print, and desktop interaction (first link
focused and hovered with its ancestors, checked inputs/selected options
reported from attributes). Every sheet set compiles and every element
computes in all four configurations, with zero refusals. Unsupported
selectors/properties still get skipped; this is a workload test, not a
visual rendering oracle. The maximum match work observed is 50,047 steps
on Wikipedia mobile, below td-css's 150,000-step ceiling.

| Page | HTML bytes | Nodes / attributes | HTML subjects | Parse ms | Parse-only peak MiB | Desktop total ms | Peak MiB across configurations |
|---|---:|---:|---:|---:|---:|---:|---:|
| Wikipedia | 635,870 | 12,607 / 13,871 | 6,404 | 14.2 | 7.5 | 276.0 | 26.9 |
| MDN | 328,385 | 8,164 / 3,411 | 5,063 | 11.8 | 4.9 | 934.9 | 131.9 |
| Bootstrap | 96,197 | 3,770 / 1,999 | 1,364 | 2.8 | 3.4 | 52.8 | 16.5 |
| Debian | 15,863 | 713 / 388 | 267 | 0.6 | 2.6 | 3.4 | 4.0 |
| kernel.org | 20,436 | 857 / 322 | 321 | 0.8 | 2.6 | 4.6 | 4.1 |
| Rust Book | 34,512 | 696 / 285 | 237 | 0.9 | 2.8 | 5.3 | 4.4 |
| Guardian | 1,543,653 | 5,794 / 9,573 | 4,608 | 56.8 | 7.8 | 563.3 | 102.3 |
| GitHub | 320,616 | 3,099 / 5,748 | 1,854 | 11.8 | 4.4 | 299.5 | 38.0 |

MDN and Guardian's stylesheet/custom-property retention dominates their
full-process peaks. The largest index in this corpus is 352,996 bytes
(Wikipedia). The separate resident renderer's measured cost model remains
unchanged; these figures must not be substituted for its input/output
budget estimates.

Capture identity (HTML fetched 2026-11-11 02:03 UTC); no corpus bytes are committed.
CSS bundle hashes concatenate each fetched sheet in source order, without
media wrappers.

| Page URL | HTML SHA-256 | CSS bundle SHA-256 |
|---|---|---|
| [wikipedia](https://en.wikipedia.org/wiki/Cascading_Style_Sheets) | `7ac5bb7612c91b2b48ba95553b1eb8b4abeaa26deef7dc3a54e11a9f82694f6d` | `207a52c3cc03872c29c56db5a75f7967cfe42a57e0992a8b9ae91630424bdb3f` |
| [mdn](https://developer.mozilla.org/en-US/docs/Web/CSS) | `9b160b4a7eebfc6af06bfb21c692906e2a1dd21f47057c21c23dc4fbb436e12f` | `3ee720105e4b74b0bb001a59cedabc70ae12ee48e3aa8959219e38375b16f853` |
| [bootstrap](https://getbootstrap.com/docs/5.3/examples/cheatsheet/) | `01594c4e79d9a6c259ece6618555f77f683559c8a98777c5333784dd12dcf807` | `59a1c805b99de2d0323ff0871a4b7d0f52feb3de3cbf30b5e5dbb89afabefed3` |
| [debian](https://www.debian.org/) | `06bdc137f5ecc3dfc8ec29ce208b46dd1e1baa82812e52c06aae1987adc1757d` | `d6bc6ba5aac22ba1665b81f68917b0b9b538f38fe17f14695ec7cb5068cabffc` |
| [kernel](https://www.kernel.org/) | `10287f13e3e67c2acc89374f191087a61481478c41438446df91637f80eec29a` | `03b2a8c8c1f8a5b1f286e4c267b3a0d8e07c94aa470f4f23a4b72be926216075` |
| [rust](https://doc.rust-lang.org/book/ch03-01-variables-and-mutability.html) | `f06a10a850287ef35db88d0f0080d269eea9f736f0e295e22d803ab7b635b7ce` | `0c5acf47d333c75690d853a4d78b9657ac2e87a8745f3f7a4a67768182f787d6` |
| [guardian](https://www.theguardian.com/international) | `f14b58518043958d441f24f8a67f74fc9dae607617b903c07e2e9762aa83b0b5` | `10fcc965c474f69bf7aa60d4ab955ef786fbb003684576892b71278f41e47aae` |
| [github](https://github.com/rust-lang/rust) | `774b91d32b26d5551e53e567ae8b9250853920f593084a5d4c9e4c7441b6028f` | `5ffb4bf52419c47f0e3ef2353ec2075e939761ed09399e2bc36ef95e15b2e4f0` |
