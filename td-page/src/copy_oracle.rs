//! A generated oracle for copying (td-web/DESIGN.md §10, "Copying"):
//! seeded pages of floats, inline-blocks, tables, inline tables, lists
//! and flex rows, wrapping rows and columns, and what a copy of them holds, worked from the lines and the
//! boxes they lie in rather than from what `copy` keeps as it walks; the
//! marker checks alone read the walk, for the order lines begin in. A
//! failing check names the generator, the page's seed and the selection,
//! with the page reduced to the tags that still fail it.
//!
//! The checks, each over every page of each generator:
//! - a copy holds the letters painted selected (`Highlight`);
//! - two scalars next to each other in tree order are parted as their
//!   paragraphs, floats and cells ask (`SeparatorMissing`,
//!   `SeparatorSpurious`, `CellTab`, `RowNewline`);
//! - a list marker starts its line and its text follows it at once
//!   (`MarkerGlued`, `MarkerParted`);
//! - a copy ending where a line starts ends in a newline exactly when the
//!   next scalar would be put after one (`EndNewlineMissing`,
//!   `EndNewlineSpurious`).
//!
//! `TD_COPY_ORACLE_PAGES` sets the pages each generator makes: 150
//! unset, 3,000 in a release build for a stress run.
//!
//! Left out, as what copying did before floats and inline-blocks were
//! walked in tree order and no rule here pins:
//! - a row with a cell holding a table of its own, whose rows' newlines
//!   stand where the outer row's tab would;
//! - what a line showing no text (an inline-block's empty first
//!   paragraph, a blank `pre` line, an empty inline-block alone on its
//!   line) or a box showing none adds between two scalars: a separator is
//!   still owed where the rules ask one, but none is held spurious there;
//! - a list item whose line begins with a box or shows nothing, and a
//!   list in a cell: its marker is not looked at;
//! - an empty item's marker, a paragraph of its own, and the line it is
//!   put with; an item holding only a float is such an item, its marker
//!   put after the float's lines.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use crate::page::tests::laid;
    use crate::page::Laid;
    use crate::text::{
        bounds, copy, fragment_text, hidden, items, page_bounds, paragraph, walk, Event, Nested,
        Place,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt::Write as _;
    use std::ops::ControlFlow;
    use td_css::values::{ComputedValue, Property};
    use td_layout::inline::DisplayText;
    use td_layout::TableRole;

    /// Pages per generator; `TD_COPY_ORACLE_PAGES` overrides it.
    const PAGES: u64 = 150;

    const WORDS: &[&str] = &["ab", "cde", "fghij", "k", "lmno", "pq"];

    struct Rng(u64);

    impl Rng {
        fn new(seed: u64, salt: u64) -> Self {
            Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ salt)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }

        fn word(&mut self) -> &'static str {
            WORDS[self.below(WORDS.len() as u64) as usize]
        }
    }

    fn words(r: &mut Rng, out: &mut String) {
        for _ in 0..1 + r.below(3) {
            out.push_str(r.word());
            out.push(' ');
        }
    }

    fn wrapped(out: &mut String, open: &str, close: &str, inner: impl FnOnce(&mut String)) {
        out.push_str(open);
        inner(out);
        out.push_str(close);
    }

    fn body(r: &mut Rng, least: u64, span: u64) -> String {
        format!("<body style=margin:0;width:{}px>", least + r.below(span))
    }

    // Nested pages: text, pictures, links, hidden and preserved text, and
    // floats and inline-blocks to three deep, in paragraphs, lists,
    // tables, `pre`, floats and inline-blocks.

    /// A float or an inline-block met in a line.
    fn boxed(r: &mut Rng, depth: usize, out: &mut String) {
        let (open, close) = match r.below(10) {
            0 => ("<span style=float:left;width:40px>", "</span>"),
            1 => ("<span style=float:right>", "</span>"),
            2 => ("<span style=float:left>", "</span>"),
            3..=5 => ("<span style=display:inline-block>", "</span>"),
            6 => ("<span style=display:inline-block;width:50px>", "</span>"),
            7 => ("<a href=/x style=display:inline-block>", "</a>"),
            8 => ("<span style=float:left;visibility:hidden>", "</span>"),
            _ => (
                "<span style=display:inline-block;visibility:hidden>",
                "</span>",
            ),
        };
        out.push_str(open);
        match r.below(8) {
            0 | 1 => {
                for _ in 0..1 + r.below(3) {
                    wrapped(out, "<span style=display:block>", "</span>", |out| {
                        inline(r, depth + 1, out);
                    });
                }
            }
            // An empty box, an icon's.
            2 => {}
            _ => inline(r, depth + 1, out),
        }
        if r.below(5) == 0 {
            out.push_str("<br>");
        }
        out.push_str(close);
        if r.below(2) == 0 {
            out.push(' ');
        }
    }

    fn inline(r: &mut Rng, depth: usize, out: &mut String) {
        for _ in 0..2 + r.below(if depth == 0 { 9 } else { 5 }) {
            match r.below(30) {
                0 => out.push_str("<br>"),
                1 => out.push_str("<img width=12 height=8 alt=IMG>"),
                2 => out.push_str("<img width=30 height=8>"),
                3 => out.push_str("<span style=visibility:hidden>hid den </span>"),
                4 => {
                    let _ = write!(out, "<a href=/l>{} </a>", r.word());
                }
                5 => {
                    let _ = write!(out, "<b>{}</b>", r.word());
                }
                6 => out.push_str("<span style=white-space:pre-wrap>a  b\n c </span>"),
                7 => out.push_str("<span style=white-space:nowrap>no wrap here</span> "),
                8..=13 if depth < 3 => boxed(r, depth, out),
                _ => {
                    out.push_str(r.word());
                    out.push(' ');
                }
            }
        }
    }

    fn block(r: &mut Rng, depth: usize, out: &mut String) {
        match r.below(13) {
            0..=2 => wrapped(out, "<p style=margin:0>", "</p>", |out| inline(r, 0, out)),
            3 => wrapped(out, "<div>", "</div>", |out| {
                inline(r, 0, out);
                if depth < 2 {
                    block(r, depth + 1, out);
                }
                inline(r, 0, out);
            }),
            4 => wrapped(out, "<pre>", "</pre>", |out| {
                for _ in 0..1 + r.below(4) {
                    out.push_str(r.word());
                    out.push_str(if r.below(3) == 0 { "\n\n" } else { "\n" });
                }
            }),
            5 | 6 => {
                let (open, close) = if r.below(2) == 0 {
                    ("<ul>", "</ul>")
                } else {
                    ("<ol>", "</ol>")
                };
                wrapped(out, open, close, |out| {
                    for _ in 0..1 + r.below(4) {
                        wrapped(out, "<li>", "</li>", |out| match r.below(5) {
                            0 => {}
                            1 if depth < 2 => block(r, depth + 1, out),
                            _ => inline(r, 1, out),
                        });
                    }
                });
            }
            7 | 8 => wrapped(out, "<table>", "</table>", |out| {
                for _ in 0..1 + r.below(3) {
                    wrapped(out, "<tr>", "</tr>", |out| {
                        for _ in 0..1 + r.below(3) {
                            wrapped(out, "<td>", "</td>", |out| {
                                if r.below(4) != 0 {
                                    inline(r, 2, out);
                                }
                            });
                        }
                    });
                }
            }),
            9 => wrapped(
                out,
                "<p style=margin:0;white-space:pre-wrap>",
                "</p>",
                |out| inline(r, 0, out),
            ),
            10 => wrapped(out, "<div style=float:left;width:120px>", "</div>", |out| {
                for _ in 0..1 + r.below(3) {
                    block(r, 2, out);
                }
            }),
            _ => wrapped(
                out,
                "<div style=display:inline-block;width:45%>",
                "</div>",
                |out| {
                    for _ in 0..1 + r.below(3) {
                        block(r, 2, out);
                    }
                },
            ),
        }
    }

    fn nested_page(seed: u64) -> String {
        let mut r = Rng::new(seed, 0x1234_5678);
        let mut out = body(&mut r, 60, 340);
        for _ in 0..1 + r.below(3) {
            block(&mut r, 0, &mut out);
        }
        out
    }

    // Table pages: tables in floats and inline-blocks; floats,
    // inline-blocks and tables in cells; floated tables.

    fn cell(r: &mut Rng, depth: usize, out: &mut String) {
        for _ in 0..r.below(4) {
            match r.below(21) {
                0..=3 => words(r, out),
                4 => out.push_str("<br>"),
                5 => wrapped(out, "<span style=float:left>", "</span>", |out| {
                    words(r, out);
                }),
                6 => wrapped(
                    out,
                    "<span style=float:right;width:40px>",
                    "<br></span>",
                    |out| words(r, out),
                ),
                7 => out.push_str("<span style=float:left><br></span>"),
                8 => wrapped(
                    out,
                    "<span style=float:left;visibility:hidden>",
                    "</span>",
                    |out| words(r, out),
                ),
                9 => wrapped(out, "<div style=float:left>", "</div>", |out| {
                    for _ in 0..1 + r.below(2) {
                        wrapped(out, "<p style=margin:0>", "</p>", |out| words(r, out));
                    }
                }),
                10 => wrapped(out, "<span style=display:inline-block>", "</span>", |out| {
                    words(r, out);
                }),
                11 => out.push_str("<span style=display:inline-block;width:9px;height:9px></span>"),
                12 => wrapped(
                    out,
                    "<span style=display:inline-block;visibility:hidden>",
                    "</span>",
                    |out| words(r, out),
                ),
                13 => wrapped(out, "<div style=display:inline-block>", "</div>", |out| {
                    for _ in 0..1 + r.below(2) {
                        wrapped(out, "<p style=margin:0>", "</p>", |out| words(r, out));
                    }
                }),
                14 if depth < 2 => wrapped(out, "<div style=float:left>", "</div>", |out| {
                    table(r, depth + 1, out);
                }),
                15 if depth < 2 => {
                    wrapped(out, "<div style=display:inline-block>", "</div>", |out| {
                        table(r, depth + 1, out);
                    });
                }
                16 => out.push_str("<img width=12 height=8 alt=IMG>"),
                17 => wrapped(out, "<p style=margin:0>", "</p>", |out| words(r, out)),
                18 => out.push_str("<span style=visibility:hidden>hid den </span>"),
                19 => wrapped(out, "<span style=float:left>", "</span>", |out| {
                    wrapped(out, "<span style=float:left>", "</span>", |out| {
                        words(r, out);
                    });
                    words(r, out);
                }),
                _ => wrapped(out, "<span style=display:inline-block>", "</span>", |out| {
                    words(r, out);
                    wrapped(out, "<span style=float:left>", "</span>", |out| {
                        words(r, out);
                    });
                }),
            }
        }
    }

    fn table(r: &mut Rng, depth: usize, out: &mut String) {
        let open = match r.below(8) {
            0 => "<table style=float:left>",
            1 => "<table style=float:right>",
            _ => "<table>",
        };
        wrapped(out, open, "</table>", |out| {
            for _ in 0..1 + r.below(3) {
                wrapped(out, "<tr>", "</tr>", |out| {
                    for _ in 0..1 + r.below(4) {
                        wrapped(out, "<td>", "</td>", |out| cell(r, depth, out));
                    }
                });
            }
        });
    }

    fn table_page(seed: u64) -> String {
        let mut r = Rng::new(seed, 0x7a_b1e5);
        let mut out = body(&mut r, 60, 400);
        for _ in 0..1 + r.below(3) {
            match r.below(12) {
                0..=2 => table(&mut r, 0, &mut out),
                3 => wrapped(&mut out, "<div style=float:left>", "</div>", |out| {
                    table(&mut r, 0, out);
                }),
                4 => wrapped(
                    &mut out,
                    "<div style=float:right;width:150px>",
                    "</div>",
                    |out| {
                        words(&mut r, out);
                        table(&mut r, 0, out);
                        words(&mut r, out);
                    },
                ),
                5 => wrapped(
                    &mut out,
                    "<div style=display:inline-block>",
                    "</div>",
                    |out| table(&mut r, 0, out),
                ),
                6 => wrapped(&mut out, "<p style=margin:0>", "</p>", |out| {
                    words(&mut r, out);
                    wrapped(out, "<span style=float:left>", "</span>", |out| {
                        table(&mut r, 0, out);
                    });
                    words(&mut r, out);
                }),
                7 => wrapped(&mut out, "<p style=margin:0>", "</p>", |out| {
                    words(&mut r, out);
                    wrapped(out, "<span style=display:inline-block>", "</span>", |out| {
                        table(&mut r, 0, out);
                    });
                    words(&mut r, out);
                }),
                8 => wrapped(&mut out, "<div style=float:left>", "</div>", |out| {
                    wrapped(out, "<div style=float:left>", "</div>", |out| {
                        table(&mut r, 0, out);
                    });
                    table(&mut r, 0, out);
                }),
                9 => wrapped(&mut out, "<p style=margin:0>", "</p>", |out| {
                    words(&mut r, out);
                }),
                10 => wrapped(&mut out, "<div style=float:left>", "</div>", |out| {
                    words(&mut r, out);
                }),
                _ => wrapped(
                    &mut out,
                    "<div style=display:inline-block>",
                    "</div>",
                    |out| {
                        wrapped(out, "<div style=float:left>", "</div>", |out| {
                            table(&mut r, 0, out);
                        });
                    },
                ),
            }
        }
        out
    }

    // Inline-table pages: inline tables in paragraphs, cells, floats and
    // inline-blocks, captions above and below their rows, and in their
    // cells what table pages' cells hold and inline tables again.

    fn inline_table(r: &mut Rng, depth: usize, out: &mut String) {
        let open = match r.below(8) {
            // Floated: a block-level table, as td-css blockifies it.
            0 => "<table style=display:inline-table;float:left>",
            1 => "<table style=display:inline-table;width:120px>",
            2 => "<table style=display:inline-table;visibility:hidden>",
            _ => "<table style=display:inline-table>",
        };
        wrapped(out, open, "</table>", |out| {
            let caption = r.below(4);
            if caption == 0 {
                wrapped(out, "<caption>", "</caption>", |out| words(r, out));
            }
            for _ in 0..r.below(3) {
                wrapped(out, "<tr>", "</tr>", |out| {
                    for _ in 0..1 + r.below(3) {
                        wrapped(out, "<td>", "</td>", |out| {
                            if depth < 2 && r.below(4) == 0 {
                                words(r, out);
                                inline_table(r, depth + 1, out);
                                words(r, out);
                            } else {
                                cell(r, depth, out);
                            }
                        });
                    }
                });
            }
            if caption == 1 {
                wrapped(
                    out,
                    "<caption style=caption-side:bottom>",
                    "</caption>",
                    |out| words(r, out),
                );
            }
        });
    }

    fn inline_table_page(seed: u64) -> String {
        let mut r = Rng::new(seed, 0x1_7ab1e);
        let mut out = body(&mut r, 60, 400);
        for _ in 0..1 + r.below(3) {
            match r.below(6) {
                0..=2 => wrapped(&mut out, "<p style=margin:0>", "</p>", |out| {
                    words(&mut r, out);
                    inline_table(&mut r, 0, out);
                    words(&mut r, out);
                    if r.below(2) == 0 {
                        inline_table(&mut r, 0, out);
                        words(&mut r, out);
                    }
                }),
                3 => wrapped(&mut out, "<div style=float:left>", "</div>", |out| {
                    words(&mut r, out);
                    inline_table(&mut r, 0, out);
                }),
                4 => wrapped(&mut out, "<p style=margin:0>", "</p>", |out| {
                    words(&mut r, out);
                    wrapped(out, "<span style=display:inline-block>", "</span>", |out| {
                        words(&mut r, out);
                        inline_table(&mut r, 0, out);
                    });
                    words(&mut r, out);
                }),
                _ => table(&mut r, 0, &mut out),
            }
        }
        out
    }

    // List pages: lists in floats, floats before and after lists, floats
    // first, among and last in an item, and lists within those.

    fn item(r: &mut Rng, depth: usize, out: &mut String) {
        for _ in 0..r.below(4) {
            match r.below(17) {
                0..=4 => words(r, out),
                5 => wrapped(out, "<span style=float:left>", "</span>", |out| {
                    words(r, out);
                }),
                6 => wrapped(
                    out,
                    "<span style=float:right;width:40px>",
                    "<br></span>",
                    |out| words(r, out),
                ),
                7 => wrapped(out, "<div style=float:left>", "</div>", |out| {
                    wrapped(out, "<p style=margin:0>", "</p>", |out| words(r, out));
                }),
                8 => wrapped(
                    out,
                    "<span style=float:left;visibility:hidden>",
                    "</span>",
                    |out| words(r, out),
                ),
                9 => wrapped(out, "<span style=display:inline-block>", "</span>", |out| {
                    words(r, out);
                }),
                10 => out.push_str("<br>"),
                11 => out.push_str("<img width=12 height=8 alt=IMG>"),
                12 => out.push_str("<img width=20 height=20 alt=PIC style=float:left>"),
                13 if depth < 2 => list(r, depth + 1, out),
                14 if depth < 2 => wrapped(out, "<div style=float:left>", "</div>", |out| {
                    list(r, depth + 1, out);
                }),
                15 => wrapped(out, "<p style=margin:0>", "</p>", |out| words(r, out)),
                _ => words(r, out),
            }
        }
    }

    fn list(r: &mut Rng, depth: usize, out: &mut String) {
        let (open, close) = if r.below(2) == 0 {
            ("<ul>", "</ul>")
        } else {
            ("<ol>", "</ol>")
        };
        wrapped(out, open, close, |out| {
            for _ in 0..1 + r.below(3) {
                wrapped(out, "<li>", "</li>", |out| item(r, depth, out));
            }
        });
    }

    fn list_page(seed: u64) -> String {
        let mut r = Rng::new(seed, 0x0001_1575);
        let mut out = body(&mut r, 80, 320);
        for _ in 0..1 + r.below(3) {
            match r.below(12) {
                0 | 1 => list(&mut r, 0, &mut out),
                // A list in a float.
                2 => wrapped(&mut out, "<div style=float:left>", "</div>", |out| {
                    list(&mut r, 0, out);
                }),
                3 => wrapped(
                    &mut out,
                    "<div style=float:right;width:150px>",
                    "</div>",
                    |out| {
                        words(&mut r, out);
                        list(&mut r, 0, out);
                        words(&mut r, out);
                    },
                ),
                // A float before a list.
                4 => {
                    wrapped(
                        &mut out,
                        "<div style=float:left;width:100px>",
                        "</div>",
                        |out| {
                            words(&mut r, out);
                        },
                    );
                    list(&mut r, 0, &mut out);
                }
                // A float after one.
                5 => {
                    list(&mut r, 0, &mut out);
                    wrapped(&mut out, "<div style=float:left>", "</div>", |out| {
                        words(&mut r, out);
                    });
                    wrapped(&mut out, "<p style=margin:0>", "</p>", |out| {
                        words(&mut r, out);
                    });
                }
                // A float in the line before one.
                6 => {
                    wrapped(&mut out, "<p style=margin:0>", "</p>", |out| {
                        words(&mut r, out);
                        wrapped(out, "<span style=float:left>", "</span>", |out| {
                            words(&mut r, out);
                        });
                        words(&mut r, out);
                    });
                    list(&mut r, 0, &mut out);
                }
                7 => wrapped(
                    &mut out,
                    "<div style=display:inline-block>",
                    "</div>",
                    |out| list(&mut r, 0, out),
                ),
                8 => wrapped(&mut out, "<div style=float:left>", "</div>", |out| {
                    wrapped(out, "<div style=float:left>", "</div>", |out| {
                        list(&mut r, 0, out);
                    });
                    list(&mut r, 0, out);
                }),
                9 => wrapped(&mut out, "<p style=margin:0>", "</p>", |out| {
                    words(&mut r, out);
                }),
                10 => {
                    out.push_str("<img width=20 height=20 alt=PIC style=float:right>");
                    list(&mut r, 0, &mut out);
                }
                _ => wrapped(&mut out, "<div style=float:left>", "</div>", |out| {
                    wrapped(out, "<p style=margin:0>", "</p>", |out| words(&mut r, out));
                    list(&mut r, 0, out);
                    wrapped(out, "<p style=margin:0>", "</p>", |out| words(&mut r, out));
                }),
            }
        }
        out
    }

    // Flex pages: rows of links, words and boxes, items moved by their
    // alignment and auto margins, nested rows, and rows in floats, cells,
    // inline-blocks and list items, with floats, inline-blocks, tables
    // and lists in their items.

    fn flex_item(r: &mut Rng, depth: usize, out: &mut String) {
        match r.below(16) {
            0..=2 => words(r, out),
            3 | 4 => {
                let _ = write!(out, "<a href=/l>{}</a>", r.word());
            }
            5 => wrapped(out, "<span>", "</span>", |out| {
                words(r, out);
                out.push_str("<br>");
                words(r, out);
            }),
            6 => wrapped(out, "<div style=margin:auto>", "</div>", |out| {
                words(r, out)
            }),
            7 => wrapped(out, "<p style=margin:0;align-self:center>", "</p>", |out| {
                inline(r, 2, out);
            }),
            8 if depth < 2 => flex(r, depth + 1, out),
            9 => wrapped(out, "<div>", "</div>", |out| {
                words(r, out);
                wrapped(out, "<span style=float:left>", "</span>", |out| {
                    words(r, out)
                });
                words(r, out);
            }),
            10 => wrapped(out, "<div>", "</div>", |out| {
                words(r, out);
                wrapped(out, "<span style=display:inline-block>", "</span>", |out| {
                    words(r, out);
                });
                words(r, out);
            }),
            11 => wrapped(out, "<ul style=margin:0>", "</ul>", |out| {
                for _ in 0..1 + r.below(3) {
                    wrapped(out, "<li>", "</li>", |out| words(r, out));
                }
            }),
            12 if depth < 2 => wrapped(out, "<table>", "</table>", |out| {
                wrapped(out, "<tr>", "</tr>", |out| {
                    for _ in 0..1 + r.below(2) {
                        wrapped(out, "<td>", "</td>", |out| {
                            if r.below(3) == 0 {
                                flex(r, depth + 1, out);
                            } else {
                                words(r, out);
                            }
                        });
                    }
                });
            }),
            13 => wrapped(
                out,
                "<span style=visibility:hidden;align-self:end>",
                "</span>",
                |out| words(r, out),
            ),
            14 => out.push_str("<img width=12 height=8 alt=IMG>"),
            _ => wrapped(out, "<b style=flex:1;min-width:0>", "</b>", |out| {
                inline(r, 2, out);
            }),
        }
        if r.below(3) == 0 {
            out.push(' ');
        }
    }

    fn flex(r: &mut Rng, depth: usize, out: &mut String) {
        let align = ["center", "end", "start", "stretch"][r.below(4) as usize];
        let justify = ["normal", "center", "space-between", "end"][r.below(4) as usize];
        // Rows, from either side, wrapping rows whose lines a height may
        // spread, and columns a height or least height may grow.
        let lay = [
            "",
            "",
            "flex-direction:row-reverse;",
            "flex-wrap:wrap;",
            "flex-flow:row-reverse wrap;",
            "flex-wrap:wrap;height:120px;align-content:space-around;",
            "flex-wrap:wrap;min-height:90px;",
            "flex-direction:column;",
            "flex-direction:column;min-height:120px;",
            "flex-direction:column;height:150px;",
        ][r.below(10) as usize];
        let _ = write!(
            out,
            "<div style=display:flex;{lay}align-items:{align};justify-content:{justify};column-gap:{}px;row-gap:{}px>",
            r.below(3) * 4,
            r.below(3) * 3
        );
        for _ in 0..1 + r.below(5) {
            flex_item(r, depth, out);
        }
        out.push_str("</div>");
    }

    fn flex_page(seed: u64) -> String {
        let mut r = Rng::new(seed, 0xf1e_c5ed);
        let mut out = body(&mut r, 60, 400);
        for _ in 0..1 + r.below(3) {
            match r.below(10) {
                0..=3 => flex(&mut r, 0, &mut out),
                4 => wrapped(&mut out, "<div style=float:left>", "</div>", |out| {
                    flex(&mut r, 1, out);
                }),
                5 => wrapped(&mut out, "<p style=margin:0>", "</p>", |out| {
                    words(&mut r, out);
                    wrapped(out, "<span style=display:inline-block>", "</span>", |out| {
                        flex(&mut r, 1, out);
                    });
                    words(&mut r, out);
                }),
                6 => wrapped(&mut out, "<ul>", "</ul>", |out| {
                    for _ in 0..1 + r.below(3) {
                        wrapped(out, "<li>", "</li>", |out| {
                            if r.below(2) == 0 {
                                words(&mut r, out);
                            }
                            flex(&mut r, 1, out);
                        });
                    }
                }),
                7 => wrapped(&mut out, "<table>", "</table>", |out| {
                    wrapped(out, "<tr>", "</tr>", |out| {
                        wrapped(out, "<td>", "</td>", |out| flex(&mut r, 1, out));
                        wrapped(out, "<td>", "</td>", |out| words(&mut r, out));
                    });
                }),
                _ => wrapped(&mut out, "<p style=margin:0>", "</p>", |out| {
                    inline(&mut r, 0, out);
                }),
            }
        }
        out
    }

    // The model: what is painted, and the boxes each line lies in.

    /// A scalar painted, with its places and its line.
    #[derive(Clone, Copy)]
    struct Scalar {
        start: Place,
        end: Place,
        c: char,
        line: usize,
    }

    struct Page<'a> {
        laid: &'a Laid,
        seed: u64,
        /// Scalars shown, in tree order.
        scalars: Vec<Scalar>,
        /// Pictures shown, with their `alt`.
        alts: Vec<(Place, Place, &'a str)>,
        /// Each line's floats and inline-blocks, outermost first, a
        /// float's `true`.
        boxes: Vec<Vec<(usize, bool)>>,
        /// Each line's (table, row, cell) for every table it lies in,
        /// outermost first.
        cells: Vec<Vec<(usize, usize, usize)>>,
        /// Lines an empty item's marker, a paragraph of its own, is put
        /// with: the first begun at or after the item once the markers
        /// before its own are put, as `copy` puts them.
        with_empty_item: BTreeSet<usize>,
        /// How many of the markers are put or passed once each line is
        /// begun: an empty item's among the rest is put with a later line,
        /// though the item hold this one (an item of a float alone).
        markers_by: Vec<usize>,
    }

    impl<'a> Page<'a> {
        fn new(laid: &'a Laid, seed: u64) -> Self {
            let lines = laid.lines();
            let mut scalars = Vec::new();
            let mut alts = Vec::new();
            for (at, line) in lines.iter().enumerate() {
                for f in &line.fragments {
                    if hidden(laid, f.style) {
                        continue;
                    }
                    if f.display == DisplayText::Space {
                        scalars.push(Scalar {
                            start: Place::new(laid, f.node, f.range.start),
                            end: Place::new(laid, f.node, f.range.end),
                            c: ' ',
                            line: at,
                        });
                        continue;
                    }
                    for (o, c) in fragment_text(laid, f).char_indices() {
                        scalars.push(Scalar {
                            start: Place::new(laid, f.node, f.range.start + o),
                            end: Place::new(laid, f.node, f.range.start + o + c.len_utf8()),
                            c,
                            line: at,
                        });
                    }
                }
                for r in &line.replaced {
                    if !hidden(laid, r.style) && !r.atomic {
                        alts.push((
                            Place::new(laid, r.node, 0),
                            Place::new(laid, r.node, 1),
                            laid.tree().alt(r.node).unwrap_or(""),
                        ));
                    }
                }
            }
            scalars.sort_by_key(|s| s.start);
            alts.sort_by_key(|alt| alt.0);
            let block = |at: usize| lines[at].block().map(|b| b.index());
            let mut with_empty_item = BTreeSet::new();
            let mut markers_by = vec![0; lines.len()];
            let markers = &laid.layout.markers;
            let mut next = 0;
            let _ = walk(laid, None, 0, |event| {
                let Event::Begin(at) = event else {
                    return ControlFlow::Continue(());
                };
                let first = bounds(laid, &lines[at]).map(|b| b.0);
                while let Some(m) = markers.get(next) {
                    match m.line {
                        Some(line) if line as usize > at => break,
                        None if first.is_none_or(|s| laid.tree().rank(m.item) > s.rank) => break,
                        None => {
                            with_empty_item.insert(at);
                        }
                        Some(_) => {}
                    }
                    next += 1;
                }
                markers_by[at] = next;
                ControlFlow::Continue(())
            });
            Self {
                laid,
                seed,
                scalars,
                alts,
                boxes: (0..lines.len())
                    .map(|at| boxes_from(laid, block(at)))
                    .collect(),
                cells: (0..lines.len())
                    .map(|at| cells_from(laid, block(at)))
                    .collect(),
                with_empty_item,
                markers_by,
            }
        }

        /// A place as a reader finds it: what is shown from it on.
        fn at(&self, place: Place) -> String {
            let mut shown: Vec<(Place, String)> = self
                .scalars
                .iter()
                .filter(|s| s.start >= place)
                .take(8)
                .map(|s| (s.start, s.c.to_string()))
                .collect();
            shown.extend(
                self.alts
                    .iter()
                    .filter(|alt| alt.0 >= place)
                    .take(2)
                    .map(|alt| (alt.0, format!("<{}>", alt.2))),
            );
            shown.sort();
            let next: String = shown.into_iter().take(8).map(|piece| piece.1).collect();
            if next.is_empty() {
                format!("the end ({}.{})", place.rank, place.offset as i32)
            } else {
                format!("before {next:?} ({}.{})", place.rank, place.offset as i32)
            }
        }

        /// A selection and what it copied, for a failure's message.
        fn said(&self, from: Place, to: Place, text: &str) -> String {
            format!(
                "copying from {} to {} gave {:?}",
                self.at(from),
                self.at(to),
                tail(text)
            )
        }
    }

    fn parent(laid: &Laid, index: usize) -> Option<usize> {
        laid.styled()
            .node_of(index)
            .and_then(|b| b.parent)
            .map(|p| p.index())
    }

    /// Whether box `index` floats, or is an inline-block (`false`). A
    /// table keeps its place in flow whatever its `float`
    /// (td-layout/DESIGN.md §5, "Floats").
    fn nested_kind(laid: &Laid, index: usize) -> Option<bool> {
        let styled = laid.styled();
        let style = styled.style(styled.node_of(index)?.style)?;
        if matches!(
            style.get(Property::Float),
            Some(ComputedValue::Keyword("left" | "right"))
        ) && styled.table_role(index).is_none()
        {
            Some(true)
        } else if matches!(
            style.get(Property::Display),
            Some(ComputedValue::Keyword("inline-block" | "inline-table"))
        ) {
            Some(false)
        } else {
            None
        }
    }

    fn boxes_from(laid: &Laid, block: Option<usize>) -> Vec<(usize, bool)> {
        let mut out = Vec::new();
        let mut at = block;
        while let Some(index) = at {
            if let Some(floats) = nested_kind(laid, index) {
                out.push((index, floats));
            }
            at = parent(laid, index);
        }
        out.reverse();
        out
    }

    fn cells_from(laid: &Laid, block: Option<usize>) -> Vec<(usize, usize, usize)> {
        let styled = laid.styled();
        let mut out = Vec::new();
        let (mut cell, mut row) = (None, None);
        let mut at = block;
        while let Some(index) = at {
            match styled.table_role(index) {
                Some(TableRole::Cell) if cell.is_none() => cell = Some(index),
                Some(TableRole::Row) if cell.is_some() && row.is_none() => row = Some(index),
                Some(TableRole::Table) => {
                    if let (Some(cell), Some(row)) = (cell, row) {
                        out.push((index, row, cell));
                    }
                    (cell, row) = (None, None);
                }
                _ => {}
            }
            at = parent(laid, index);
        }
        out.reverse();
        out
    }

    /// The place before box `index`, which its own places follow.
    fn box_place(laid: &Laid, index: usize) -> Option<Place> {
        Some(Place::new(laid, laid.styled().node_of(index)?.node, 0))
    }

    fn copied(laid: &Laid, from: Place, to: Place) -> String {
        let mut out = String::new();
        let _ = copy(laid, from, to, &mut |text| {
            out.push_str(text);
            ControlFlow::Continue(())
        });
        out
    }

    fn letters(text: &str) -> String {
        text.chars().filter(char::is_ascii_alphabetic).collect()
    }

    /// The end of a copy, enough to read.
    fn tail(text: &str) -> String {
        let n = text.chars().count();
        if n <= 48 {
            text.to_string()
        } else {
            format!("…{}", text.chars().skip(n - 48).collect::<String>())
        }
    }

    // The checks.

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum Check {
        Highlight,
        SeparatorMissing,
        SeparatorSpurious,
        MarkerGlued,
        MarkerParted,
        CellTab,
        RowNewline,
        EndNewlineMissing,
        EndNewlineSpurious,
    }

    impl Check {
        fn rule(self) -> &'static str {
            match self {
                Self::Highlight => {
                    "a copy holds the letters painted selected, those starting from its start \
                     to before its end, and a picture's alt held whole"
                }
                Self::SeparatorMissing => {
                    "two paragraphs' text, and text on either side of a float's edge, are a \
                     newline apart (in a table, at least a space or a tab)"
                }
                Self::SeparatorSpurious => {
                    "text of one paragraph, an inline-block's with its line's, has no newline \
                     or tab within it, and two paragraphs' have one newline between them"
                }
                Self::MarkerGlued => {
                    "a list marker starts its line in the copy: nothing but a newline, or the \
                     copy's start, comes before it"
                }
                Self::MarkerParted => {
                    "a list marker is followed at once by the text its line begins with"
                }
                Self::CellTab => "two cells of one row are a tab apart, with no newline",
                Self::RowNewline => {
                    "two rows of one table are a newline apart: one when the second follows \
                     the first, and no tab from a row's last cell to the next's first"
                }
                Self::EndNewlineMissing => {
                    "a copy ending where a line starts ends in a newline when the next scalar \
                     would be put after one"
                }
                Self::EndNewlineSpurious => {
                    "a copy ending where a line starts ends in no newline unless the next \
                     scalar would be put after one"
                }
            }
        }

        /// The checks one pass makes together.
        fn pass(self) -> u8 {
            match self {
                Self::Highlight => 0,
                Self::SeparatorMissing
                | Self::SeparatorSpurious
                | Self::CellTab
                | Self::RowNewline => 1,
                Self::MarkerGlued | Self::MarkerParted => 2,
                Self::EndNewlineMissing | Self::EndNewlineSpurious => 3,
            }
        }
    }

    /// The first failure of each check on a page.
    #[derive(Default)]
    struct Faults(BTreeMap<Check, String>);

    impl Faults {
        fn add(&mut self, check: Check, detail: impl FnOnce() -> String) {
            self.0.entry(check).or_insert_with(detail);
        }
    }

    /// Copies of the whole page and of pairs of places against what is
    /// painted between them.
    fn highlight(page: &Page, faults: &mut Faults) {
        let laid = page.laid;
        let mut places = Vec::new();
        let mut edges = Vec::new();
        for line in laid.lines() {
            for item in items(laid, line) {
                places.extend([item.start, item.end]);
            }
            if let Some((start, end)) = bounds(laid, line) {
                edges.extend([start, end]);
            }
        }
        places.extend(page.scalars.iter().flat_map(|s| [s.start, s.end]));
        places.sort();
        places.dedup();
        if places.is_empty() {
            return;
        }
        let mut r = Rng::new(page.seed, 0xab_cdef);
        let mut pick = |from: &[Place]| {
            let a = from[r.below(from.len() as u64) as usize];
            let b = from[r.below(from.len() as u64) as usize];
            (a.min(b), a.max(b))
        };
        let mut pairs = vec![page_bounds(laid)];
        for _ in 0..24 {
            pairs.push(pick(&places));
        }
        for _ in 0..12 {
            pairs.push(pick(&edges));
        }
        for (from, to) in pairs {
            let mut want: Vec<(Place, String)> = page
                .scalars
                .iter()
                .filter(|s| s.start >= from && s.start < to)
                .map(|s| (s.start, s.c.to_string()))
                .collect();
            want.extend(
                page.alts
                    .iter()
                    .filter(|alt| alt.0 >= from && alt.1 <= to)
                    .map(|alt| (alt.0, alt.2.to_string())),
            );
            want.sort_by_key(|w| w.0);
            let want: String = want.into_iter().map(|w| w.1).collect();
            let text = copied(laid, from, to);
            if letters(&text) != letters(&want) {
                faults.add(Check::Highlight, || {
                    format!(
                        "{}: its letters are {:?}, those painted {:?}",
                        page.said(from, to, &text),
                        tail(&letters(&text)),
                        tail(&letters(&want))
                    )
                });
            }
        }
    }

    /// The copy of each two scalars next to each other in tree order,
    /// white space aside: what parts them against the boxes, paragraphs
    /// and cells they lie in.
    fn separators(page: &Page, faults: &mut Faults) {
        let laid = page.laid;
        let lines = laid.lines();
        // Boxes with lines, and lines showing no text, by where they lie.
        let nested: Vec<(usize, Place)> = page
            .boxes
            .iter()
            .flatten()
            .map(|&(index, _)| index)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|index| Some((index, box_place(laid, index)?)))
            .collect();
        let solid: Vec<Scalar> = page
            .scalars
            .iter()
            .copied()
            .filter(|s| !s.c.is_whitespace())
            .collect();
        let texted: BTreeSet<usize> = solid.iter().map(|s| s.line).collect();
        let blank: Vec<(usize, [Option<Place>; 2])> = lines
            .iter()
            .enumerate()
            .filter(|(at, _)| !texted.contains(at))
            .map(|(at, line)| {
                let block = line.block().and_then(|b| box_place(laid, b.index()));
                (at, [block, bounds(laid, line).map(|b| b.0)])
            })
            .collect();
        // Rows with a cell holding a table of its own.
        let mut holders = BTreeSet::new();
        for index in 0..laid.styled().styled().boxes().len() {
            if laid.styled().table_role(index) == Some(TableRole::Table) {
                let mut at = parent(laid, index);
                while let Some(up) = at {
                    at = parent(laid, up);
                    if laid.styled().table_role(up) == Some(TableRole::Cell) {
                        holders.extend(at);
                    }
                }
            }
        }
        let climb = |mut line: usize, steps: usize| {
            for _ in 0..steps {
                line = laid.outer(line)?;
            }
            Some(line)
        };
        for pair in solid.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if page
                .alts
                .iter()
                .any(|alt| alt.0 > a.start && alt.0 < b.start)
            {
                continue;
            }
            let text = copied(laid, a.start, b.end);
            let said = || page.said(a.start, b.end, &text);
            // Markers before the first, and the newline a copy ending at
            // a line's start closes with, are other checks'.
            let held = text
                .trim_start_matches(|c: char| !c.is_ascii_alphabetic())
                .trim_end_matches('\n');
            let mut chars = held.chars();
            if chars.next() != Some(a.c) || chars.next_back() != Some(b.c) {
                continue;
            }
            let between = chars.as_str();
            let marker = between.chars().any(|c| !c.is_whitespace());
            let (newlines, tab) = (between.matches('\n').count(), between.contains('\t'));

            let (ba, bb) = (&page.boxes[a.line], &page.boxes[b.line]);
            let common = ba.iter().zip(bb).take_while(|(x, y)| x == y).count();
            let (ra, rb) = (&ba[common..], &bb[common..]);
            let float_edge = ra.iter().chain(rb).any(|b| b.1);
            let (ca, cb) = (&page.cells[a.line], &page.cells[b.line]);
            let cell = |cells: &[(usize, usize, usize)]| cells.last().map(|c| c.2);
            let in_cell = !ca.is_empty() || !cb.is_empty();
            // Whether the two are of two paragraphs, at the boxes they
            // share: an inline-block's text is its line's.
            let apart = match (climb(a.line, ra.len()), climb(b.line, rb.len())) {
                (Some(ha), Some(hb)) => Some(paragraph(laid, ha) != paragraph(laid, hb)),
                _ => None,
            };
            // Something between them that no rule here pins.
            let (lo, hi) = (a.line.min(b.line), a.line.max(b.line));
            let within = |place: Place| a.start < place && place < b.start;
            let unpinned = cell(ca) != cell(cb)
                || nested.iter().any(|&(index, place)| {
                    within(place) && !ba.iter().chain(bb).any(|b| b.0 == index)
                })
                || blank.iter().any(|&(at, places)| {
                    (lo < at && at < hi) || places.into_iter().flatten().any(within)
                });

            if float_edge || apart == Some(true) {
                let parted = if in_cell {
                    !between.is_empty()
                } else {
                    newlines > 0
                };
                if !parted {
                    faults.add(Check::SeparatorMissing, || {
                        format!(
                            "{}: {:?} and {:?} are {}",
                            said(),
                            a.c,
                            b.c,
                            if float_edge {
                                "on either side of a float's edge"
                            } else {
                                "of two paragraphs"
                            }
                        )
                    });
                }
                // White space between two floats is its own line's.
                if !in_cell && !unpinned && between.contains("\n\n") {
                    faults.add(Check::SeparatorSpurious, || {
                        format!("{}: two newlines between two paragraphs", said())
                    });
                }
            } else if apart == Some(false) && !unpinned && !marker && (newlines > 0 || tab) {
                faults.add(Check::SeparatorSpurious, || {
                    format!(
                        "{}: {:?} and {:?} are of one paragraph, no float's edge between",
                        said(),
                        a.c,
                        b.c
                    )
                });
            }

            // Cells of one table, in rows no cell of which holds a table.
            if ca.len() == cb.len() && !ca.is_empty() {
                let k = ca.len() - 1;
                let (ta, tb) = (ca[k], cb[k]);
                if ca[..k] == cb[..k]
                    && ta.0 == tb.0
                    && ta.2 != tb.2
                    && !holders.contains(&ta.1)
                    && !holders.contains(&tb.1)
                {
                    if ta.1 == tb.1 && (!tab || newlines > 0) {
                        faults.add(Check::CellTab, said);
                    }
                    // A row between them, and a cell after the first's or
                    // before the second's, may show white space alone.
                    let styled = laid.styled();
                    let next_row = styled
                        .node_of(ta.1)
                        .and_then(|row| row.next_sibling)
                        .is_some_and(|next| next.index() == tb.1);
                    let last_cell = styled
                        .node_of(ta.2)
                        .is_some_and(|cell| cell.next_sibling.is_none());
                    let first_cell = styled
                        .node_of(tb.1)
                        .and_then(|row| row.first_child)
                        .is_some_and(|cell| cell.index() == tb.2);
                    if ta.1 != tb.1
                        && (newlines == 0
                            || (next_row && newlines > 1)
                            || (last_cell && first_cell && tab))
                    {
                        faults.add(Check::RowNewline, said);
                    }
                }
            }
        }
    }

    /// What a walk of the page gives, as far as markers need it.
    enum Step {
        Begin(usize),
        Piece(Place, String),
        /// An inline-block entered, and left.
        Inline,
        Left,
        Other,
    }

    /// Each marker whose line begins with text or a picture: a copy from
    /// the page's start to that piece's end ends in the marker and the
    /// piece, the marker first on its line; one from the line's start is
    /// the two alone.
    fn markers(page: &Page, faults: &mut Faults) {
        let laid = page.laid;
        let lines = laid.lines();
        let mut steps = Vec::new();
        let _ = walk(laid, None, 0, |event| {
            steps.push(match event {
                Event::Begin(at) => Step::Begin(at),
                Event::Scalar(c, _, end, _) => Step::Piece(end, c.to_string()),
                Event::Alt(text, _, end) => Step::Piece(end, text.to_string()),
                Event::Enter(Nested::Inline) => Step::Inline,
                Event::Leave(Nested::Inline, ..) => Step::Left,
                _ => Step::Other,
            });
            ControlFlow::Continue(())
        });
        let (first, last) = page_bounds(laid);
        // Empty items past the page's last line are copied with a
        // selection held to its end.
        let trailing = laid.layout.markers.iter().any(|m| m.line.is_none());
        for (at, step) in steps.iter().enumerate() {
            let &Step::Begin(line) = step else {
                continue;
            };
            let Some((start, _)) = bounds(laid, &lines[line]) else {
                continue;
            };
            let marker: String = laid
                .layout
                .markers
                .iter()
                .filter(|m| m.line == Some(line as u32) && !hidden(laid, m.style))
                .map(|m| m.text().as_str().to_string())
                .collect();
            let Some(Step::Piece(end, piece)) = steps.get(at + 1) else {
                continue;
            };
            let late = laid
                .layout
                .markers
                .iter()
                .skip(page.markers_by[line])
                .any(|m| m.line.is_none() && laid.tree().rank(m.item) < end.rank);
            if marker.is_empty()
                || piece.is_empty()
                || late
                || page.with_empty_item.contains(&line)
                || !page.cells[line].is_empty()
                || (trailing && *end >= last)
            {
                continue;
            }
            let want = format!("{marker}{piece}");
            let text = copied(laid, first, *end);
            if !text.ends_with(want.as_str()) {
                faults.add(Check::MarkerParted, || {
                    format!(
                        "{}: it should end in {want:?}",
                        page.said(first, *end, &text)
                    )
                });
            }
            // An inline-block's first line lies in its holder's, whatever
            // showing nothing (an empty float's lines) the walk passes
            // between the inline-block's start and the line.
            let own_line = !steps[..at]
                .iter()
                .rev()
                .find_map(|step| match step {
                    Step::Inline => Some(true),
                    Step::Piece(..) | Step::Left => Some(false),
                    _ => None,
                })
                .unwrap_or(false);
            let before = text
                .rfind(marker.as_str())
                .map_or("", |found| &text[..found]);
            if own_line && !before.is_empty() && !before.ends_with('\n') {
                faults.add(Check::MarkerGlued, || {
                    format!(
                        "{}: {marker:?} follows {:?}",
                        page.said(first, *end, &text),
                        tail(before)
                    )
                });
            }
            let text = copied(laid, start, *end);
            if text != want {
                faults.add(Check::MarkerParted, || {
                    format!("{}: it should be {want:?}", page.said(start, *end, &text))
                });
            }
        }
    }

    /// A copy ending where a line starts, from the page's start, from the
    /// scalar before and from a dozen before: its last newline against the
    /// copy one scalar longer.
    fn end_newlines(page: &Page, faults: &mut Faults) {
        let laid = page.laid;
        let starts: BTreeSet<Place> = laid
            .lines()
            .iter()
            .filter_map(|line| bounds(laid, line).map(|b| b.0))
            .collect();
        let (first, _) = page_bounds(laid);
        for (n, next) in page.scalars.iter().enumerate() {
            if !starts.contains(&next.start) || page.with_empty_item.contains(&next.line) {
                continue;
            }
            let back = |k: usize| n.checked_sub(k).map(|at| page.scalars[at].start);
            for from in [Some(first), back(1), back(12)].into_iter().flatten() {
                if from >= next.start {
                    continue;
                }
                let text = copied(laid, from, next.start);
                if text.is_empty() {
                    continue;
                }
                let longer = copied(laid, from, next.end);
                let Some(rest) = longer.strip_prefix(text.strip_suffix('\n').unwrap_or(&text))
                else {
                    continue;
                };
                let said = || {
                    format!(
                        "{}; one scalar on, the copy goes on {rest:?}",
                        page.said(from, next.start, &text)
                    )
                };
                let (has, owed) = (text.ends_with('\n'), rest.starts_with('\n'));
                if owed && !has {
                    faults.add(Check::EndNewlineMissing, said);
                }
                if has && !owed {
                    faults.add(Check::EndNewlineSpurious, said);
                }
            }
        }
    }

    /// The checks `markup` fails; with `only`, those of its pass alone.
    fn faults(markup: &str, seed: u64, only: Option<Check>) -> Faults {
        let laid = laid(markup);
        let page = Page::new(&laid, seed);
        let mut faults = Faults::default();
        let passes: [fn(&Page, &mut Faults); 4] = [highlight, separators, markers, end_newlines];
        for (pass, run) in passes.into_iter().enumerate() {
            if only.is_none_or(|check| usize::from(check.pass()) == pass) {
                run(&page, &mut faults);
            }
        }
        faults
    }

    // Reduction: a failing page with every tag pair, tag and word taken
    // out that it fails without.

    fn tokens(markup: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = markup;
        while !rest.is_empty() {
            let end = if rest.starts_with('<') {
                rest.find('>').map_or(rest.len(), |end| end + 1)
            } else {
                // A word with the white space after it, or that alone.
                let text = &rest[..rest.find('<').unwrap_or(rest.len())];
                let word = text.find(char::is_whitespace).unwrap_or(text.len());
                text[word..]
                    .find(|c: char| !c.is_whitespace())
                    .map_or(text.len(), |next| word + next)
            };
            out.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        out
    }

    /// A tag's name, and whether it closes.
    fn tag(token: &str) -> Option<(&str, bool)> {
        let inner = token.strip_prefix('<')?.strip_suffix('>')?;
        let (inner, closes) = match inner.strip_prefix('/') {
            Some(inner) => (inner, true),
            None => (inner, false),
        };
        Some((inner.split(' ').next()?, closes))
    }

    /// The tag closing the one opened at `at`.
    fn closing(tokens: &[String], at: usize) -> Option<usize> {
        let (name, closes) = tag(&tokens[at])?;
        if closes || name == "img" || name == "br" {
            return None;
        }
        let mut depth = 0usize;
        for (i, token) in tokens.iter().enumerate().skip(at) {
            match tag(token) {
                Some((other, true)) if other == name => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                Some((other, false)) if other == name => depth += 1,
                _ => {}
            }
        }
        None
    }

    fn reduced(markup: &str, seed: u64, check: Check) -> String {
        let fails = |tokens: &[String]| {
            faults(&tokens.concat(), seed, Some(check))
                .0
                .contains_key(&check)
        };
        let without = |tokens: &[String], gone: &[usize]| -> Vec<String> {
            tokens
                .iter()
                .enumerate()
                .filter(|(i, _)| !gone.contains(i))
                .map(|(_, token)| token.clone())
                .collect()
        };
        let mut tokens = tokens(markup);
        // Laying a page is the cost: enough tries for a page, no more.
        let mut tries = 0;
        let mut changed = true;
        while changed && tries < 3000 {
            changed = false;
            // The body's own tag stays.
            let mut at = 1;
            while at < tokens.len() && tries < 3000 {
                let candidates = match closing(&tokens, at) {
                    Some(end) => vec![
                        without(&tokens, &(at..=end).collect::<Vec<_>>()),
                        without(&tokens, &[at, end]),
                    ],
                    None if tag(&tokens[at]).is_some_and(|(_, closes)| closes) => Vec::new(),
                    None => vec![without(&tokens, &[at])],
                };
                let mut took = false;
                for candidate in candidates {
                    tries += 1;
                    if fails(&candidate) {
                        tokens = candidate;
                        (took, changed) = (true, true);
                        break;
                    }
                }
                if !took {
                    at += 1;
                }
            }
        }
        tokens.concat()
    }

    fn pages() -> u64 {
        std::env::var("TD_COPY_ORACLE_PAGES")
            .ok()
            .and_then(|pages| pages.parse().ok())
            .unwrap_or(PAGES)
    }

    /// Every check on `generate`'s pages from seed `base`: each one that
    /// fails, with how many pages fail it and the first of them reduced.
    fn run(name: &str, base: u64, generate: fn(u64) -> String) {
        let count = pages();
        let mut failed: BTreeMap<Check, (u64, u64)> = BTreeMap::new();
        for seed in base..base + count {
            for &check in faults(&generate(seed), seed, None).0.keys() {
                failed.entry(check).or_insert((0, seed)).0 += 1;
            }
        }
        if failed.is_empty() {
            return;
        }
        let mut report = format!(
            "{} of the copy oracle's checks fail on {name} pages (seeds {base}..{}):\n",
            failed.len(),
            base + count
        );
        for (check, (pages, seed)) in failed {
            let small = reduced(&generate(seed), seed, check);
            let detail = faults(&small, seed, Some(check))
                .0
                .remove(&check)
                .unwrap_or_default();
            let _ = write!(
                report,
                "\n{check:?}: {}.\n  {pages} of {count} pages; first seed {seed}, reduced to\n  \
                 {small:?}\n  {detail}\n",
                check.rule()
            );
        }
        panic!("{report}");
    }

    #[test]
    fn nested_pages_copy_as_they_are_painted() {
        run("nested", 0, nested_page);
    }

    #[test]
    fn table_pages_copy_as_they_are_painted() {
        run("table", 1_000_000, table_page);
    }

    #[test]
    fn list_pages_copy_as_they_are_painted() {
        run("list", 2_000_000, list_page);
    }

    #[test]
    fn inline_table_pages_copy_as_they_are_painted() {
        run("inline-table", 3_000_000, inline_table_page);
    }

    #[test]
    fn flex_pages_copy_as_they_are_painted() {
        run("flex", 4_000_000, flex_page);
    }
}
