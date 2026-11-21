# td-page

**Status: normative for the crate's boundary; the behaviour it makes is
td-web/DESIGN.md's (§1 below names the sections).** td-page is td-web's
page engine, taken out of the td-web binary so a second program can
show a styled page: td-mail's HTML mail is the one planned (§4).

## 1. What it is

A page made, laid out and painted, and the places over it:

- `tree`, `charset`, `gather`, `sheets`: td-html's tree under the page's
  tree term, a page's style sheets gathered from its `style` elements
  and `<link rel=stylesheet>`s and decoded, and compiled with td-css's
  user-agent sheet and td-page's own (`page.css`) for an environment;
- `pipeline`, `page`, `fonts`: parsed, cascaded and laid out by
  td-layout over td-ui's faces, stepping down the cuts' ladder on a
  refusal, and laid out again for a new viewport, scale, face or styles;
- `inline`, `memory`: the pictures a layout lays, and the terms a page is
  made in;
- `paint`: a layout painted into a td-ui raster, bounded;
- `text`, `search`, `view`: places, text, find, selection, links and
  scrolling over a layout.

td-web/DESIGN.md is the contract for what these do: §10's "Pipeline
and threads" (the steps; the threads are the caller's), "Memory" (a
page's terms), "Sources", "Subresources" (gathering and cascade order;
fetching is the caller's), "Places, find, selection and links", "Cuts"
and "Painting"; §5's "Encodings" for `charset`; and §8's motions for
`text`. A rule stated there is not restated here.

## 2. The boundary

td-page fetches nothing, starts no thread and holds no window. Its
caller brings:

- the markup, charged to the page's budget, and a cancel flag each step
  reads;
- the sheets a page names: `gather::Sheets` says which it wants, and the
  caller answers each `arrived` or `refused`, from wherever it likes;
- the pictures: `inline::Inline` from pixels the caller decoded, and an
  `inline::Set` of them by node for a layout;
- a `page::Want` (viewport, scale, body face, styles, pictures) and a
  `fonts::Measure`.

It gives back a `pipeline::Page` and its `page::Laid`, which the caller
paints, searches and scrolls on whatever thread it chooses. A page's
last drop is the caller's to place: td-web keeps it on its render
thread. An `inline::Witnessed` rides on a page, each decoded picture and
each fitted copy, and tells a witness, when one is given, what dropped
on the thread that dropped it; none is given in production.

## 3. Trust and build

A standalone `td-*` roster crate (AGENTS.md "Rust code"): `std` and
td's own crates by path (td-alloc, td-charset, td-css, td-html,
td-layout, td-ui, td-url), no external dependency, and
`#![forbid(unsafe_code)]`. Its inputs are hostile: every parser it
feeds is bounded by td-web/DESIGN.md §10's limits and the crates' own.
Its storage is td-alloc's, budgeted, and its sources are guarded paths
of the allocation guard (`builder/src/allocation_guard.rs`). It is
staged beside each program that builds it; it has no recipe of its own.

## 4. Callers

td-web, the only one today. td-mail's message view is the planned
second (td-web/DESIGN.md §11): a page made from a message's HTML with
remote content off, `cid:` pictures from the message's own parts,
links opened outside the pane, and terms of its own measured against
td-mail's jail. Each is its own amendment, and the one that makes a
rule here caller-neutral moves that rule from td-web/DESIGN.md to this
file.
