# td-markdown: Markdown structure as source ranges

This std-only crate parses Markdown, the subset a chat model writes, into
leaf blocks and inline spans that name byte ranges of the source. It owns
no rendering, no I/O and no clock, and it decodes nothing: a span is a
slice of the source, so a renderer draws the source's own characters and a
copy of a selection takes the source as written. td-agent's transcript is
its first consumer; td-ui's message list stays dependency-free, and the
consumer maps the document onto the list's styled text.

## Output

`parse(source)` answers a `Document`, or `Error::TooLong` past
`MAX_BYTES`, 1 MiB: as much as td-ui's message list holds in one section.
A document is:

- `blocks`, the leaf blocks in order. Each has a `kind`: `Paragraph`,
  `Heading(1..=6)`, `Code { info }` (fenced, its info string's range, or
  indented, an empty one), `Rule`, or `Table`; the range of its
  containers in `levels`; the range of its spans in `spans`; its `source`;
  and `blank_before`, whether a blank line came between it and the block
  before, which a renderer spaces by.
- `levels`, each block's containers, outermost first: `Quote`, or
  `Item { marker, first }`, its marker a `Bullet(char)` or
  `Ordered { number, delimiter }`, `first` on the item's first block, where
  the marker shows. An item closed with no block of its own has an empty
  paragraph, so its marker shows.
- `spans`, each block's in source order: `Text`, `Code` (a code span's
  content), `SoftBreak` (a line ending inside a paragraph, shown as a
  space), `HardBreak`, and `Image { source, open, close }` (its range
  the alt text as written up to its first line's end, `source` its
  destination in `links`, and its markup the source from `open`, its
  `![`, to `close`, past its closing parenthesis, so a renderer's
  stand-in can stand for the image whole). Each has a `Style` of
  `EMPHASIS`, `STRONG` and `STRIKE` bits and the `link` it is in, an
  image's too: a linked image is a badge its renderer can follow. A code
  block's or table's spans are its lines as written with a `HardBreak`
  at each line's end but the last; a rule has none.
- `links`, each a destination range and an optional title range, as
  written: an escape's backslash is kept there. A link or image inside an
  image's alt text names none.

A text span's range leaves markup out: emphasis delimiters, brackets and
destinations, an escape's backslash, a code span's backticks and the
single space each side CommonMark strips. A break's range is empty, at its
line's end (its newline's offset; a backslash break's after the
backslash). Every span lies in its block's `source`, which runs from its
first line's first byte past its containers' markers (and an indented
block's indentation) to its last line's end: a fenced block's closing
fence is in it, and so are its containers' markers on its later lines. An
item with no block of its own has its marker's range.

## Blocks

CommonMark's line-by-line container algorithm: each line matches the open
containers (a quote's `>`, an item's content indentation; a blank line
continues an item), then may open new ones, then continues or closes the
open leaf. Up to three spaces of indentation are allowed before a marker;
four or more outside a paragraph start indented code. Supported:

- ATX headings, a closing `#` run after a space removed; setext headings
  (`=` level 1, `-` level 2) under a paragraph its containers all matched;
- fenced code (three or more backticks or tildes, an info string, which a
  backtick fence's may not contain a backtick in, a closing fence of the
  same character at least as long, indented at most three; an unclosed
  fence runs to its container's end), its content de-indented by the
  fence's indentation; indented code, its trailing blank lines left out;
- thematic breaks (`***`, `---`, `___`, spaces allowed);
- block quotes, nested, with lazy paragraph continuation;
- bullet (`-`, `+`, `*`) and ordered (`1.` or `1)`, at most nine digits)
  list items, nested by content indentation, which is the marker's width
  and one to four spaces (one when the line is blank after the marker or
  five or more spaces follow, the rest then indented code). An item
  interrupts a paragraph its containers all matched only with content and,
  if ordered, at 1. An item that opens on a blank line and holds nothing
  by the next blank line closes there; an empty container inside it (a
  bare `>`) is nothing here, where CommonMark keeps the item open;
- GFM pipe tables: a paragraph's last line with a pipe, over a delimiter
  row of as many cells (`-` runs, colons either side; an escaped `\|` is
  no cell boundary, `\\|` is), the lines before it staying a paragraph; every line
  after is a row until a blank line or another block. A table is kept as
  its lines as written; its cells are not yet parsed.

Not supported, and so text: reference links and their definitions, HTML
blocks and inline HTML, entity and numeric character references, and a
lone carriage return, which ends no line (a `\r\n` does). A tab is taken
whole where a container's indentation reaches into it, where CommonMark
splits it into spaces. Nesting stops at `MAX_DEPTH` (32) containers: a
marker past it is text.

## Inlines

A paragraph's or heading's lines are joined by newlines and scanned once:

- backslash escapes of ASCII punctuation, and a backslash before a line
  ending, a hard break;
- code spans, a backtick run closed by the next run of the same length,
  found from a per-length index of the runs; an unclosed run is text;
- emphasis and strong emphasis by CommonMark's delimiter-run algorithm,
  `*` and `_` with its intraword rule, the rule of three, and matching
  over a doubly linked list with a per-character, per-can-open,
  per-length-mod-3 search floor; GFM strikethrough, `~` or `~~`, closed by
  a run of the same length (a longer run is text);
- inline links and images, `[text](destination "title")` with a `<...>`
  or bare destination (balanced parentheses to depth 32) and a quoted or
  parenthesised title, an angle destination and a title each on one line
  (an escaped line ending too), the parenthesised title with no
  unescaped parenthesis inside. Links do not nest: an inner autolink gives
  way, and a closed link leaves every `[` before it inactive, which one
  depth mark records. An image drops what its brackets held, the items,
  emphasis and links matched inside, at once;
- autolinks `<scheme:...>` (scheme of 2 to 32 characters), and bare
  `http://` and `https://` URLs after a space or an opening mark. A bare
  URL ends at whitespace, `<`, a backtick and, inside a bracket, `]`, so
  `[see https://a](https://b)` is one link; its trailing punctuation and
  unmatched closing parentheses are left out;
- line endings: a soft break, or a hard one after two or more spaces (not
  tabs), the trailing whitespace left out; a paragraph's own trailing
  whitespace too.

## Bounds

Every step is linear in the source: each item, delimiter and bracket is
visited a bounded number of times, and each scan forward is bounded: a
title, an autolink and a bare URL stop at a byte that also ends the next
one's, an angle destination at the next `<`, and a bare destination
within 32 nested parentheses, so a byte is rescanned at most that many
times. Memory is what the output holds and the scan's items, a few words
per source byte at worst: at `MAX_BYTES`, one-letter lines (one span per
two bytes) or alternating `*` and `_` peak at about 126 MB and parse in
about a quarter of a second optimised; deep quotes and list items peak
below 50 MB.

## Test contract

The tests write each document back in a normal form and pin: headings,
rules and setext against rules; soft and hard breaks, tabs making none,
and each break's offset after an image, an autolink, a backslash and
spaces; emphasis, strong, nested, unclosed, intraword `_`, the rule of
three and strikethrough; code spans, unmatched and spanning lines; links
with titles, angle and empty destinations, nesting refused, parentheses,
multi-line titles and angle destinations (an escaped line ending too)
and parenthesised titles refused, images, a linked image, an image
holding links, each image's markup from its `![` past its closing
parenthesis (quoted, parenthesised and angle destinations, across a
quote's lines and inside a link), autolinks and bare URLs with their
trailing punctuation, before a backtick and inside link text; escapes;
quotes and lazy lines; bullet, ordered and nested items, items
interrupting paragraphs, an item under a quote whose markers shift by a
tab, empty items and one opened blank; fenced, tilde, unclosed, indented
and in-item code; tables with an intro line, rows without pipes, escaped
pipes and an escaped backslash before a closing pipe; block sources with
a closing fence, an item's content, an empty item's marker and an
escaped closing `#`; the depth and the length limits. Every range lies
in the source on character boundaries, an image's markup holds its alt
text, and every block's spans lie in its source in order, for every case
and for 3000 seeded random inputs over a markup alphabet, and sixteen
pathological shapes of up to 1 MiB (emphasis, brackets, links,
backticks, autolinks, items, quotes, backslash and image breaks, nested
images, parenthesised titles and URL trailers) each parse in under a
second unoptimised, at sizes where quadratic work takes seconds
optimised.
