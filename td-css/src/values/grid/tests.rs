//! The grid longhands and shorthands (DESIGN.md §4, "Grid").
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use super::super::*;
    use crate::test_tree::{tree, Handle};
    use crate::StyleSet;

    fn style(input: &str, parent: Option<&ComputedStyle>) -> ComputedStyle {
        let nodes = tree();
        StyleSet::new(&[])
            .unwrap()
            .compute(
                &Handle {
                    nodes: &nodes,
                    index: 3,
                },
                parent,
                16.0,
                input,
            )
            .unwrap()
    }
    fn child(input: &str) -> ComputedStyle {
        style(
            input,
            Some(&ComputedStyle::new(&crate::memory().unwrap()).unwrap()),
        )
    }
    fn length(l: &Length) -> String {
        match l {
            Length::Px(n) => format!("{n}px"),
            Length::Percent(n) => format!("{n}%"),
            Length::Calc { px, percent, .. } => format!("calc({px}px + {percent}%)"),
            Length::Auto => "auto".into(),
        }
    }
    fn breadth(b: &Breadth) -> String {
        match b {
            Breadth::Length(l) => length(l),
            Breadth::Flex(n) => format!("{n}fr"),
            Breadth::Auto => "auto".into(),
            Breadth::MinContent => "min-content".into(),
            Breadth::MaxContent => "max-content".into(),
        }
    }
    /// A computed list written back, its repeats compact.
    fn list(list: &TrackList) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut open = 0usize;
        for item in list.items() {
            let part = match item {
                TrackItem::Names { start, len } => {
                    let names: Vec<&str> = list
                        .line_names(*start, *len)
                        .unwrap()
                        .iter()
                        .map(|n| n.as_str())
                        .collect();
                    format!("[{}]", names.join(" "))
                }
                TrackItem::Size(TrackSize::Breadth(b)) => breadth(b),
                TrackItem::Size(TrackSize::MinMax(a, b)) => {
                    format!("minmax({}, {})", breadth(a), breadth(b))
                }
                TrackItem::Size(TrackSize::FitContent(l)) => format!("fit-content({})", length(l)),
                TrackItem::Repeat { repeat, len } => {
                    open = *len + 1;
                    match repeat {
                        Repeat::Count(n) => format!("repeat({n},"),
                        Repeat::AutoFill => "repeat(auto-fill,".into(),
                        Repeat::AutoFit => "repeat(auto-fit,".into(),
                    }
                }
            };
            parts.push(part);
            if open > 0 {
                open -= 1;
                if open == 0 {
                    parts.last_mut().unwrap().push(')');
                }
            }
        }
        parts.join(" ")
    }
    fn written(style: &ComputedStyle, p: Property) -> String {
        match style.get(p) {
            Some(ComputedValue::Keyword(k)) => (*k).into(),
            Some(ComputedValue::GridTracks(l)) => list(l),
            Some(ComputedValue::GridAreas(a)) => {
                let areas: Vec<String> = a
                    .areas()
                    .iter()
                    .map(|a| {
                        format!(
                            "{} {}-{} {}-{}",
                            a.name.as_str(),
                            a.row_start,
                            a.row_end,
                            a.column_start,
                            a.column_end
                        )
                    })
                    .collect();
                format!("{}x{}: {}", a.rows(), a.columns(), areas.join(", "))
            }
            Some(ComputedValue::GridLine(l)) => {
                let mut parts = Vec::new();
                if l.span {
                    parts.push("span".to_string());
                }
                if l.integer != 0 {
                    parts.push(l.integer.to_string());
                }
                if let Some(name) = &l.name {
                    parts.push(name.as_str().into());
                }
                parts.join(" ")
            }
            other => panic!("{p:?}: {other:?}"),
        }
    }
    /// `p`'s value after `5px` (or `p`'s `seed`) then `value`: an
    /// invalid value leaves the first.
    fn after(p: Property, seed: &str, value: &str) -> String {
        written(&child(&format!("{0}:{seed};{0}:{value}", p.name())), p)
    }
    const GRID: &[Property] = &[
        GridTemplateColumns,
        GridTemplateRows,
        GridTemplateAreas,
        GridAutoColumns,
        GridAutoRows,
        GridAutoFlow,
        GridRowStart,
        GridRowEnd,
        GridColumnStart,
        GridColumnEnd,
        JustifyItems,
        JustifySelf,
    ];

    #[test]
    fn grid_longhands_start_at_their_initials_rare_and_uninherited() {
        let memory = crate::memory().unwrap();
        let want = [
            "none", "none", "none", "auto", "auto", "row", "auto", "auto", "auto", "auto",
            "normal", "auto",
        ];
        let parent = child(
            "grid-template-columns:1px;grid-template-rows:2px;grid-template-areas:'a';\
             grid-auto-columns:3px;grid-auto-rows:4px;grid-auto-flow:column;grid-area:1/2/3/4;\
             justify-items:center;justify-self:end",
        );
        let s = style("", Some(&parent));
        for (p, want) in GRID.iter().zip(want) {
            assert!(!p.inherited(), "{p:?}");
            assert!(p.rare().is_some(), "{p:?}");
            assert_eq!(written(&child(""), *p), want, "{p:?}");
            assert_eq!(written(&s, *p), want, "{p:?}");
            assert_ne!(written(&parent, *p), want, "{p:?}");
            assert_eq!(
                ComputedStyle::new(&memory).unwrap().get(*p),
                Some(&p.initial(&memory).unwrap())
            );
        }
    }

    #[test]
    fn track_lists_take_sizes_names_and_repeats() {
        for (input, want) in [
            ("1px 2fr auto", "1px 2fr auto"),
            ("10% min-content max-content", "10% min-content max-content"),
            ("2em 1rem", "32px 16px"),
            ("0 1.5FR", "0px 1.5fr"),
            ("[a b] 1px [c] 2px [d]", "[a b] 1px [c] 2px [d]"),
            ("[] 1px", "[] 1px"),
            ("minmax(10px, 1fr)", "minmax(10px, 1fr)"),
            ("minmax(auto, max-content)", "minmax(auto, max-content)"),
            ("fit-content(20%)", "fit-content(20%)"),
            ("calc(50% - 1em)", "calc(-16px + 50%)"),
            ("calc(1px - 2em)", "0px"),
            ("repeat(3, 1fr)", "repeat(3, 1fr)"),
            ("repeat(2, [a] 1px [b] auto)", "repeat(2, [a] 1px [b] auto)"),
            ("[x] repeat(+2, 1fr) [y]", "[x] repeat(2, 1fr) [y]"),
            (
                "repeat(auto-fill, minmax(100px, 1fr))",
                "repeat(auto-fill, minmax(100px, 1fr))",
            ),
            (
                "10px repeat(AUTO-FIT, [a] 5em) repeat(2, 1px)",
                "10px repeat(auto-fit, [a] 80px) repeat(2, 1px)",
            ),
            (
                "repeat(auto-fill, minmax(min-content, 3px))",
                "repeat(auto-fill, minmax(min-content, 3px))",
            ),
            ("auto", "auto"),
            ("none", "none"),
            ("1e9px 1e9fr", "1000000px 1000000fr"),
            ("[Name] 1px", "[Name] 1px"),
        ] {
            assert_eq!(after(GridTemplateColumns, "5px", input), want, "{input}");
            assert_eq!(after(GridTemplateRows, "5px", input), want, "{input}");
        }
    }

    #[test]
    fn invalid_track_lists_are_dropped() {
        for input in [
            "subgrid",
            "subgrid [a]",
            "masonry",
            "-1px",
            "-1fr",
            "1",
            "[a]",
            "[a] [b] 1px",
            "1px [a] [b]",
            "[span] 1px",
            "[auto] 1px",
            "[inherit] 1px",
            "[1] 1px",
            "minmax(1fr, 2px)",
            "minmax(1px)",
            "minmax(1px, 2px, 3px)",
            "minmax(-1px, 2px)",
            "fit-content(1fr)",
            "fit-content(auto)",
            "fit-content(-1px)",
            "repeat(0, 1px)",
            "repeat(-1, 1px)",
            "repeat(1.5, 1px)",
            "repeat(2)",
            "repeat(2, [a])",
            "repeat(2, repeat(2, 1px))",
            "repeat(2, [a] [b] 1px)",
            "repeat(auto-fill, 1fr)",
            "repeat(auto-fill, auto)",
            "repeat(auto-fill, fit-content(10px))",
            "repeat(auto-fill, minmax(auto, 1fr))",
            "repeat(auto-fill, 1px) 1fr",
            "repeat(auto-fill, 1px) auto",
            "min-content repeat(auto-fit, 1px)",
            "repeat(2, 1fr) repeat(auto-fill, 1px)",
            "repeat(auto-fill, 1px) repeat(auto-fit, 1px)",
            "1px, 2px",
            "1px / 2px",
            "1deg",
            "'a'",
            "none 1px",
            "auto-fill",
        ] {
            assert_eq!(after(GridTemplateColumns, "5px", input), "5px", "{input}");
        }
    }

    #[test]
    fn auto_tracks_take_sizes_alone() {
        for (input, want) in [
            ("auto", "auto"),
            ("1px", "1px"),
            ("1fr 2em minmax(0, 1fr)", "1fr 32px minmax(0px, 1fr)"),
            (
                "min-content fit-content(1px)",
                "min-content fit-content(1px)",
            ),
            ("auto auto", "auto auto"),
        ] {
            assert_eq!(after(GridAutoRows, "5px", input), want, "{input}");
            assert_eq!(after(GridAutoColumns, "5px", input), want, "{input}");
        }
        for input in ["none", "[a] 1px", "repeat(2, 1px)", "-1px", "subgrid"] {
            assert_eq!(after(GridAutoRows, "5px", input), "5px", "{input}");
        }
    }

    #[test]
    fn areas_past_a_pending_run_merge_in_order_whatever_their_arrival() {
        // More names than a run holds, descending, repeated, and in two
        // rows, so names merge from several runs and later rows find
        // them; the result is the same in any arrival order.
        let names: Vec<String> = (0..300).map(|i| format!("n{i:03}")).collect();
        let mut sorted = names.clone();
        sorted.sort();
        let want = {
            let areas: Vec<String> = sorted
                .iter()
                .map(|n| {
                    let i: usize = n[1..].parse().unwrap();
                    format!("{n} 0-2 {i}-{}", i + 1)
                })
                .collect();
            format!("2x300: {}", areas.join(", "))
        };
        let row = |order: &[String]| format!("'{}'", order.join(" "));
        let two = |order: &[String]| format!("{} {}", row(order), row(order));
        assert_eq!(after(GridTemplateAreas, "none", &two(&names)), want);
        // Descending arrival, in two rows.
        let reversed: Vec<String> = sorted.iter().rev().cloned().collect();
        let got = after(GridTemplateAreas, "none", &two(&reversed));
        assert!(
            got.starts_with("2x300: n000 0-2 299-300, n001 0-2 298-299"),
            "{got}"
        );
        assert!(got.ends_with("n299 0-2 0-1"), "{got}");
        // A name split by another row is no rectangle, across runs too.
        let mut gap = names.clone();
        gap[0] = "other".into();
        let input = format!("{} {} {}", row(&names), row(&gap), row(&names));
        assert_eq!(after(GridTemplateAreas, "none", &input), "none");
        // A run of one name across a row is one area.
        let wide = vec!["w".to_string(); 1000];
        assert_eq!(
            after(GridTemplateAreas, "none", &row(&wide)),
            "1x1000: w 0-1 0-1000"
        );
    }

    #[test]
    fn template_areas_compute_to_rectangles() {
        for (input, want) in [
            ("'a'", "1x1: a 0-1 0-1"),
            ("'a a b' 'c c b'", "2x3: a 0-1 0-2, b 0-2 2-3, c 1-2 0-2"),
            (
                "\"head head\" \". main\"",
                "2x2: head 0-1 0-2, main 1-2 1-2",
            ),
            ("'a...b'", "1x3: a 0-1 0-1, b 0-1 2-3"),
            ("'  x  '  ' .  '", "2x1: x 0-1 0-1"),
            (
                "'1a -b _c é'",
                "1x4: -b 0-1 1-2, 1a 0-1 0-1, _c 0-1 2-3, é 0-1 3-4",
            ),
            ("'. .' '. .'", "2x2: "),
            ("'A a'", "1x2: A 0-1 0-1, a 0-1 1-2"),
        ] {
            assert_eq!(after(GridTemplateAreas, "'z'", input), want, "{input}");
        }
        for input in [
            "'a b a'",
            "'a a' 'a b'",
            "'a b' 'b a'",
            "'a' 'b b'",
            "'a b' 'c'",
            "''",
            "'   '",
            "'a' ''",
            "'a#'",
            "'a,b'",
            "a",
            "'a' none",
            "'a' 1px",
        ] {
            assert_eq!(
                after(GridTemplateAreas, "'z'", input),
                "1x1: z 0-1 0-1",
                "{input}"
            );
        }
    }

    #[test]
    fn auto_flow_takes_an_axis_and_dense() {
        for (input, want) in [
            ("row", "row"),
            ("COLUMN", "column"),
            ("dense", "row dense"),
            ("dense column", "column dense"),
            ("row dense", "row dense"),
        ] {
            assert_eq!(after(GridAutoFlow, "column", input), want, "{input}");
        }
        for input in ["row column", "dense dense", "auto", "row row", "1"] {
            assert_eq!(after(GridAutoFlow, "column", input), "column", "{input}");
        }
    }

    #[test]
    fn placement_lines_take_integers_names_and_spans() {
        for (input, want) in [
            ("auto", "auto"),
            ("3", "3"),
            ("-2", "-2"),
            ("foo", "foo"),
            ("2 foo", "2 foo"),
            ("foo -1", "-1 foo"),
            ("span 2", "span 2"),
            ("span foo", "span 1 foo"),
            ("2 span", "span 2"),
            ("span 3 foo", "span 3 foo"),
            ("Span 4", "span 4"),
            // Clamped as parsed, to ±MAX_GRID_LINE.
            ("99999", "10000"),
            ("-99999 a", "-10000 a"),
            ("span 99999", "span 10000"),
            ("span foo 2", "span 2 foo"),
            ("2 foo span", "span 2 foo"),
            ("foo span", "span 1 foo"),
        ] {
            for p in [GridRowStart, GridRowEnd, GridColumnStart, GridColumnEnd] {
                assert_eq!(after(p, "7", input), want, "{input}");
            }
        }
        // An integer of any length clamps, as the column count's does not
        // need to: past f64's range it is still an integer.
        let digits = |n: usize| "9".repeat(n);
        for (input, want) in [
            (digits(64), "10000".to_string()),
            (digits(130), "10000".to_string()),
            (digits(400), "10000".to_string()),
            (format!("-{} a", digits(400)), "-10000 a".to_string()),
            (format!("+{}", digits(400)), "10000".to_string()),
            (format!("span {}", digits(400)), "span 10000".to_string()),
        ] {
            assert_eq!(after(GridRowStart, "7", &input), want, "{input}");
        }
        for input in [
            "0",
            "0 foo",
            "span 0",
            "span -1",
            "span",
            "span auto",
            "auto 2",
            "span span 2",
            "2 3",
            "a b",
            "1.5",
            "span 2 a b",
            "inherit 2",
            "2 default",
            "'a'",
            "2 / 3",
            // `span` comes first or last (Grid 2 §8.3).
            "foo span 2",
            "2 span foo",
        ] {
            assert_eq!(after(GridRowStart, "7", input), "7", "{input}");
        }
    }

    #[test]
    fn justify_items_and_self_take_their_alignment_values() {
        for (input, want) in [
            ("normal", "normal"),
            ("stretch", "stretch"),
            ("first baseline", "baseline"),
            ("last baseline", "last baseline"),
            ("safe center", "center"),
            ("unsafe LEFT", "left"),
            ("right", "right"),
            ("self-end", "self-end"),
            ("legacy left", "legacy left"),
            ("center legacy", "legacy center"),
            // `legacy` alone takes a parent's legacy position, else normal.
            ("legacy", "normal"),
        ] {
            assert_eq!(after(JustifyItems, "end", input), want, "{input}");
        }
        for input in [
            "auto",
            "legacy start",
            "legacy legacy",
            "legacy safe",
            "safe stretch",
            "space-between",
            "left right",
        ] {
            assert_eq!(after(JustifyItems, "end", input), "end", "{input}");
        }
        for (input, want) in [
            ("auto", "auto"),
            ("left", "left"),
            ("safe right", "right"),
            ("baseline", "baseline"),
        ] {
            assert_eq!(after(JustifySelf, "end", input), want, "{input}");
        }
        for input in ["legacy", "legacy left", "space-around"] {
            assert_eq!(after(JustifySelf, "end", input), "end", "{input}");
        }
    }

    #[test]
    fn justify_items_legacy_inherits_its_position() {
        let parent = child("justify-items:legacy right");
        for (input, want) in [
            ("", "legacy right"),
            ("justify-items:legacy", "legacy right"),
            ("justify-items:initial", "legacy right"),
            ("justify-items:unset", "legacy right"),
            ("justify-items:inherit", "legacy right"),
            ("justify-items:center", "center"),
            ("--x:legacy;justify-items:var(--x)", "legacy right"),
            ("place-items:center legacy", "legacy right"),
            ("place-items:center", "center"),
        ] {
            assert_eq!(
                written(&style(input, Some(&parent)), JustifyItems),
                want,
                "{input}"
            );
        }
        let grandchild = style("", Some(&style("", Some(&parent))));
        assert_eq!(written(&grandchild, JustifyItems), "legacy right");
        // A plain parent: `legacy` is normal, and children share the default.
        let plain = child("justify-items:end");
        let s = style("justify-items:legacy", Some(&plain));
        assert_eq!(written(&s, JustifyItems), "normal");
        let other = style("", Some(&plain));
        assert!(
            td_alloc::BudgetShared::ptr_eq(&other.rare, &child("").rare)
                || *other.rare == *child("").rare
        );
    }

    #[test]
    fn grid_template_sets_rows_columns_and_areas() {
        let case = |input: &str| {
            let s = child(&format!("grid-auto-flow:column;grid-template:{input}"));
            [
                GridTemplateRows,
                GridTemplateColumns,
                GridTemplateAreas,
                GridAutoFlow,
            ]
            .map(|p| written(&s, p))
        };
        assert_eq!(case("none"), ["none", "none", "none", "column"]);
        assert_eq!(case("1px 2px / 3fr"), ["1px 2px", "3fr", "none", "column"]);
        assert_eq!(
            case("repeat(2, 1px) / none"),
            ["repeat(2, 1px)", "none", "none", "column"]
        );
        assert_eq!(
            case("'a a' 'b c'"),
            [
                "auto auto",
                "none",
                "2x2: a 0-1 0-2, b 1-2 0-1, c 1-2 1-2",
                "column"
            ]
        );
        assert_eq!(
            case("[top] 'a' 10px [mid] [mid2] 'b' [end] / [l] 1fr [r]"),
            [
                "[top] 10px [mid mid2] auto [end]",
                "[l] 1fr [r]",
                "2x1: a 0-1 0-1, b 1-2 0-1",
                "column"
            ]
        );
        assert_eq!(
            case("'a' 2em 'b' minmax(1px, 1fr) / 1px 2px"),
            [
                "32px minmax(1px, 1fr)",
                "1px 2px",
                "2x1: a 0-1 0-1, b 1-2 0-1",
                "column"
            ]
        );
        // Invalid forms leave all three as they were.
        for input in [
            "1px",
            "1px / 2px / 3px",
            "none / ",
            "'a' / repeat(2, 1px)",
            "'a' / none",
            "'a' 1px 2px",
            "1px 'a'",
            "'a' [x] [y] [z] 'b'",
            "[x] [y] 'a'",
            "'a' [x]  / 1px [y] [z]",
            "'a b' 'c'",
            "'a' (x)",
            "subgrid / 1px",
            "'a' / 1px, 2px",
            "[x]",
        ] {
            let s = child(&format!(
                "grid-template-rows:9px;grid-template-columns:8px;grid-template-areas:'z';\
                 grid-template:{input}"
            ));
            assert_eq!(
                [GridTemplateRows, GridTemplateColumns, GridTemplateAreas].map(|p| written(&s, p)),
                ["9px", "8px", "1x1: z 0-1 0-1"],
                "{input}"
            );
        }
    }

    #[test]
    fn reparsed_area_bytes_and_computed_relative_tracks_are_counted() {
        let nodes = tree();
        let set = StyleSet::new(&[]).unwrap();
        let memory = crate::memory().unwrap();
        let items = |inline: &str| {
            let mut stats = crate::ComputeStats::default();
            let node = Handle {
                nodes: &nodes,
                index: 3,
            };
            set.compute_with_stats_and_budget(&node, None, 16.0, inline, &mut stats, &memory)
                .unwrap();
            stats.grid_items
        };
        // Each string byte and named cell, and AREA_ITEMS an area built,
        // each time a longhand reparses its value: 7 bytes, 4 cells and 4
        // areas.
        let rows = "--t:'a b' 'c  d';";
        let each = 7 + 4 + 4 * super::super::AREA_ITEMS;
        assert_eq!(each, 19);
        assert_eq!(items(&format!("{rows}grid-template-areas:var(--t)")), each);
        assert_eq!(items(&format!("{rows}grid-template:var(--t)")), 3 * each);
        assert_eq!(items(&format!("{rows}grid:var(--t)")), 6 * each);
        // Null cells are no named cells; an invalid value builds no area.
        assert_eq!(
            items("--t:'a . bc';grid-template-areas:var(--t)"),
            6 + 2 + 4
        );
        assert_eq!(items("--t:'ab c ab';grid-template-areas:var(--t)"), 7 + 3);
        assert_eq!(items(&format!("{rows}width:var(--t)")), 0);
        assert_eq!(items(&format!("{rows}--u:var(--t)")), 0);
        // Parsed once with its sheet, not per element.
        assert_eq!(items("grid-template-areas:'a b' 'c d'"), 0);
        // A font-relative list's items, each element; one in px none.
        assert_eq!(items("grid-template-columns:1em [a] repeat(3, 2em 1px)"), 5);
        assert_eq!(items("grid-template-columns:1px [a] repeat(3, 2px)"), 0);
        assert_eq!(items("--l:1em 2em;grid-auto-rows:var(--l)"), 2);
        assert_eq!(items("grid:none/auto-flow 1rem"), 1);
    }

    #[test]
    fn grid_auto_flow_with_auto_tracks_computes_the_keyword() {
        let nodes = tree();
        let set = StyleSet::new(&[]).unwrap();
        let at = |input: &str| {
            let node = Handle {
                nodes: &nodes,
                index: 3,
            };
            set.compute(&node, None, 16.0, input).unwrap()
        };
        let initial = at("");
        let auto = at("grid-auto-rows:auto;grid-auto-columns:auto");
        for input in ["grid:auto-flow auto / none", "grid:none/auto-flow AUTO"] {
            let s = at(input);
            assert_eq!(s.get(GridAutoRows), auto.get(GridAutoRows), "{input}");
            assert_eq!(s.get(GridAutoColumns), auto.get(GridAutoColumns));
            assert_eq!(s.get(GridAutoRows), Some(&ComputedValue::Keyword("auto")));
        }
        // Every value initial: the initial group itself, unmarked.
        let s = at("grid:auto-flow auto / none");
        assert_eq!(s.rare.set, 0);
        assert!(td_alloc::BudgetShared::ptr_eq(&s.rare, &initial.rare));
    }

    #[test]
    fn grid_sets_the_template_or_the_flow_and_resets_the_rest() {
        let all = [
            GridTemplateRows,
            GridTemplateColumns,
            GridTemplateAreas,
            GridAutoRows,
            GridAutoColumns,
            GridAutoFlow,
            RowGap,
        ];
        let case = |input: &str| {
            let s = child(&format!(
                "grid-auto-rows:7px;grid-auto-columns:6px;grid-auto-flow:column;row-gap:5px;\
                 grid:{input}"
            ));
            all.map(|p| match s.get(p) {
                Some(ComputedValue::Length(l)) => length(l),
                _ => written(&s, p),
            })
        };
        assert_eq!(
            case("1px / 2px"),
            ["1px", "2px", "none", "auto", "auto", "row", "5px"]
        );
        assert_eq!(
            case("none"),
            ["none", "none", "none", "auto", "auto", "row", "5px"]
        );
        assert_eq!(
            case("'a' / 1fr"),
            [
                "auto",
                "1fr",
                "1x1: a 0-1 0-1",
                "auto",
                "auto",
                "row",
                "5px"
            ]
        );
        assert_eq!(
            case("auto-flow / 1fr 1fr"),
            ["none", "1fr 1fr", "none", "auto", "auto", "row", "5px"]
        );
        assert_eq!(
            case("dense auto-flow 3px 4px / repeat(2, 1fr)"),
            [
                "none",
                "repeat(2, 1fr)",
                "none",
                "3px 4px",
                "auto",
                "row dense",
                "5px"
            ]
        );
        assert_eq!(
            case("1fr auto / auto-flow dense 1em"),
            [
                "1fr auto",
                "none",
                "none",
                "auto",
                "16px",
                "column dense",
                "5px"
            ]
        );
        assert_eq!(
            case("none / auto-flow"),
            ["none", "none", "none", "auto", "auto", "column", "5px"]
        );
        let unchanged = ["none", "none", "none", "7px", "6px", "column", "5px"];
        for input in [
            "auto-flow",
            "auto-flow / auto-flow",
            "auto-flow auto-flow / 1px",
            "dense / 1px",
            "1px / dense",
            "auto-flow [a] 1px / 1px",
            "auto-flow repeat(2, 1px) / 1px",
            "1px / auto-flow 1px / 2px",
            "3px auto-flow / 1px",
            "1px",
        ] {
            assert_eq!(case(input), unchanged, "{input}");
        }
    }

    #[test]
    fn grid_row_column_and_area_copy_a_lone_name() {
        let lines = [GridRowStart, GridColumnStart, GridRowEnd, GridColumnEnd];
        let case = |input: &str| {
            let s = child(input);
            lines.map(|p| written(&s, p))
        };
        assert_eq!(case("grid-row:1 / 3"), ["1", "auto", "3", "auto"]);
        assert_eq!(case("grid-row:a"), ["a", "auto", "a", "auto"]);
        assert_eq!(case("grid-row:2"), ["2", "auto", "auto", "auto"]);
        assert_eq!(
            case("grid-row:span 2 a"),
            ["span 2 a", "auto", "auto", "auto"]
        );
        assert_eq!(case("grid-row:2 a"), ["2 a", "auto", "auto", "auto"]);
        assert_eq!(
            case("grid-column:a / span 2"),
            ["auto", "a", "auto", "span 2"]
        );
        assert_eq!(case("grid-column:b"), ["auto", "b", "auto", "b"]);
        assert_eq!(case("grid-area:a"), ["a", "a", "a", "a"]);
        assert_eq!(case("grid-area:a / b"), ["a", "b", "a", "b"]);
        assert_eq!(case("grid-area:a / 2"), ["a", "2", "a", "auto"]);
        assert_eq!(case("grid-area:2 / b"), ["2", "b", "auto", "b"]);
        assert_eq!(case("grid-area:1 / 2 / 3"), ["1", "2", "3", "auto"]);
        assert_eq!(case("grid-area:a / b / c"), ["a", "b", "c", "b"]);
        assert_eq!(case("grid-area:1 / 2 / 3 / 4"), ["1", "2", "3", "4"]);
        assert_eq!(case("grid-area:2"), ["2", "auto", "auto", "auto"]);
        assert_eq!(case("grid-area:auto"), ["auto", "auto", "auto", "auto"]);
        let set = "grid-area:9/9/9/9;";
        for input in [
            "grid-row:1 / 2 / 3",
            "grid-row:/ 2",
            "grid-row:1 /",
            "grid-row:0",
            "grid-area:1/2/3/4/5",
            "grid-area:span 0",
            "grid-column:span auto",
        ] {
            assert_eq!(
                case(&format!("{set}{input}")),
                ["9", "9", "9", "9"],
                "{input}"
            );
        }
    }

    #[test]
    fn place_shorthands_set_align_then_justify() {
        let case = |input: &str, a: Property, j: Property| {
            let s = child(&format!("{}:end;{}:end;{input}", a.name(), j.name()));
            [written(&s, a), written(&s, j)]
        };
        let content = |input: &str| case(input, AlignContent, JustifyContent);
        let items = |input: &str| case(input, AlignItems, JustifyItems);
        let selfs = |input: &str| case(input, AlignSelf, JustifySelf);
        assert_eq!(content("place-content:center"), ["center", "center"]);
        assert_eq!(content("place-content:center start"), ["center", "start"]);
        assert_eq!(content("place-content:baseline"), ["baseline", "start"]);
        assert_eq!(
            content("place-content:last baseline"),
            ["last baseline", "start"]
        );
        assert_eq!(
            content("place-content:first baseline right"),
            ["baseline", "right"]
        );
        assert_eq!(
            content("place-content:safe center unsafe end"),
            ["center", "end"]
        );
        assert_eq!(
            content("place-content:space-evenly stretch"),
            ["space-evenly", "stretch"]
        );
        assert_eq!(items("place-items:center"), ["center", "center"]);
        assert_eq!(items("place-items:baseline"), ["baseline", "baseline"]);
        assert_eq!(
            items("place-items:stretch legacy left"),
            ["stretch", "legacy left"]
        );
        assert_eq!(items("place-items:start right"), ["start", "right"]);
        assert_eq!(selfs("place-self:auto"), ["auto", "auto"]);
        assert_eq!(selfs("place-self:center left"), ["center", "left"]);
        for input in [
            "place-content:left",
            "place-content:center baseline",
            "place-content:a",
            "place-content:center start end",
        ] {
            assert_eq!(content(input), ["end", "end"], "{input}");
        }
        for input in ["place-items:legacy", "place-items:left", "place-items:auto"] {
            assert_eq!(items(input), ["end", "end"], "{input}");
        }
        assert_eq!(selfs("place-self:left"), ["end", "end"]);
    }

    #[test]
    fn grid_gap_names_alias_the_gaps() {
        let gaps = |input: &str| {
            let s = child(input);
            [RowGap, ColumnGap].map(|p| match s.get(p) {
                Some(ComputedValue::Length(l)) => length(l),
                other => format!("{other:?}"),
            })
        };
        assert_eq!(gaps("grid-gap:1px 2px"), ["1px", "2px"]);
        assert_eq!(gaps("grid-gap:3px"), ["3px", "3px"]);
        assert_eq!(
            gaps("grid-row-gap:4px;grid-column-gap:1em"),
            ["4px", "16px"]
        );
        assert_eq!(gaps("--g:5px;grid-gap:var(--g)"), ["5px", "5px"]);
        assert_eq!(
            gaps("--g:6px;grid-row-gap:var(--g);column-gap:1px"),
            ["6px", "1px"]
        );
    }

    #[test]
    fn var_and_css_wide_keywords_reach_every_grid_longhand() {
        let s = child(
            "--c:[a] 1fr repeat(2, 2em);--r:'x y';--l:2 / span b;--f:column;\
             grid-template-columns:var(--c);grid-template-areas:var(--r);\
             grid-row:var(--l);grid-auto-flow:var(--f) dense;\
             justify-self:var(--missing, left)",
        );
        assert_eq!(written(&s, GridTemplateColumns), "[a] 1fr repeat(2, 32px)");
        assert_eq!(written(&s, GridTemplateAreas), "1x2: x 0-1 0-1, y 0-1 1-2");
        assert_eq!(written(&s, GridRowStart), "2");
        assert_eq!(written(&s, GridRowEnd), "span 1 b");
        assert_eq!(written(&s, GridAutoFlow), "column dense");
        assert_eq!(written(&s, JustifySelf), "left");
        // A shorthand through var(), and a longhand after it.
        let s = child("--t:'a b' 1em / 1fr 2fr;grid-template:var(--t);grid-template-rows:3px");
        assert_eq!(written(&s, GridTemplateRows), "3px");
        assert_eq!(written(&s, GridTemplateColumns), "1fr 2fr");
        assert_eq!(written(&s, GridTemplateAreas), "1x2: a 0-1 0-1, b 0-1 1-2");
        let s = child("--g:1px / auto-flow 2px;grid:var(--g)");
        assert_eq!(written(&s, GridAutoColumns), "2px");
        assert_eq!(written(&s, GridAutoFlow), "column");
        // Invalid at computed-value time: unset, the initial value.
        let s = child("--c:subgrid;--z:span 0;grid-template-columns:var(--c);grid-area:var(--z)");
        assert_eq!(written(&s, GridTemplateColumns), "none");
        assert_eq!(written(&s, GridColumnEnd), "auto");
        // inherit, initial and unset; an inherited list is the parent's.
        let parent = child(
            "grid-template-columns:1px 2em;grid-auto-rows:3px;grid-area:a;\
             grid-template-areas:'q';grid-auto-flow:dense",
        );
        let s = style(
            "grid-template-columns:inherit;grid-auto-rows:initial;grid-row-start:inherit;\
             grid-template-areas:unset;grid-auto-flow:inherit;grid:inherit",
            Some(&parent),
        );
        for p in GRID.iter().take(6) {
            assert_eq!(s.get(*p), parent.get(*p), "{p:?}");
        }
        let s = style(
            "grid-template-columns:inherit;grid-auto-rows:initial;grid-row-start:inherit;\
             grid-template-areas:unset",
            Some(&parent),
        );
        let (Some(ComputedValue::GridTracks(a)), Some(ComputedValue::GridTracks(b))) =
            (s.get(GridTemplateColumns), parent.get(GridTemplateColumns))
        else {
            panic!()
        };
        assert!(td_alloc::BudgetShared::ptr_eq(a, b));
        assert_eq!(written(&s, GridTemplateColumns), "1px 32px");
        assert_eq!(written(&s, GridAutoRows), "auto");
        assert_eq!(written(&s, GridRowStart), "a");
        assert_eq!(written(&s, GridTemplateAreas), "none");
        let s = style("grid-row:inherit;grid-template:initial", Some(&parent));
        assert_eq!(written(&s, GridRowEnd), "a");
        assert_eq!(written(&s, GridTemplateAreas), "none");
    }

    #[test]
    fn font_relative_lists_compute_per_element_and_absolute_ones_as_parsed() {
        let sheet = "p{grid-template-columns:1px 2fr;grid-template-rows:2em}";
        let set = StyleSet::new(&[(crate::Origin::Author, sheet)]).unwrap();
        let nodes = tree();
        let at = |inline: &str| {
            set.compute(
                &Handle {
                    nodes: &nodes,
                    index: 3,
                },
                None,
                16.0,
                inline,
            )
            .unwrap()
        };
        let (a, b) = (at(""), at("font-size:10px"));
        let tracks = |s: &ComputedStyle, p| match s.get(p) {
            Some(ComputedValue::GridTracks(l)) => l.try_clone().unwrap(),
            other => panic!("{other:?}"),
        };
        // The absolute list is the rule's one allocation.
        assert!(td_alloc::BudgetShared::ptr_eq(
            &tracks(&a, GridTemplateColumns),
            &tracks(&b, GridTemplateColumns)
        ));
        assert_eq!(written(&a, GridTemplateRows), "32px");
        assert_eq!(written(&b, GridTemplateRows), "20px");
        // At the root, rem is its own font size.
        let r = at("font-size:20px;grid-template-rows:1rem");
        assert_eq!(written(&r, GridTemplateRows), "20px");
    }

    #[test]
    fn supports_reports_display_grid_and_the_grid_properties() {
        use crate::conditions::supports_matches;
        // td-layout lays `display: grid`, so its properties are supported.
        for query in [
            "(display: grid)",
            "(grid-template-columns: repeat(2, 1fr))",
            "(GRID-TEMPLATE-COLUMNS: 1fr)",
            "(grid-template-areas: 'a b')",
            "(grid-area: auto)",
            "(grid-row: 1)",
            "(grid: auto-flow / 1fr)",
            "(grid-auto-flow: dense)",
            "(justify-items: legacy center)",
            "(justify-self: start)",
            "(place-items: center)",
            "(place-self: end)",
            "(grid-gap: 1px)",
            "(grid-row-gap: 1px)",
            "(grid-column-gap: 1px)",
            "(display: flex) and (grid-area: a)",
            "(display: flex)",
            "(gap: 1px)",
            "(place-content: center)",
        ] {
            assert_eq!(supports_matches(query), Ok(true), "{query}");
        }
        // `inline-grid` waits for inline grid layout; `subgrid` is invalid.
        for query in [
            "(display: inline-grid)",
            "(grid-template-columns: subgrid)",
            "not (grid-area: a)",
        ] {
            assert_eq!(supports_matches(query), Ok(false), "{query}");
        }
    }

    #[test]
    fn display_grid_parses_and_inline_grid_waits() {
        let grid = child("display:grid");
        assert_eq!(grid.get(Display), Some(&ComputedValue::Keyword("grid")));
        let inline = child("display:inline-grid");
        assert_eq!(inline.get(Display), Some(&ComputedValue::Keyword("inline")));
    }

    #[test]
    fn the_bounds_refuse_a_declaration_cleanly_at_their_edge() {
        let px = |n: usize| vec!["1px"; n].join(" ");
        let names = |n: usize| {
            let names: Vec<String> = (0..n).map(|i| format!("n{i}")).collect();
            format!("[{}] 1px", names.join(" "))
        };
        let tracks = |input: &str| after(GridTemplateColumns, "5px", input) != "5px";
        assert!(tracks(&px(MAX_GRID_TRACKS)));
        assert!(!tracks(&px(MAX_GRID_TRACKS + 1)));
        assert!(tracks("repeat(1000, 1px)"));
        assert!(!tracks("repeat(1001, 1px)"));
        assert!(!tracks("repeat(99999999999, 1px)"));
        assert!(tracks("repeat(500, 1px [a] 1px)"));
        assert!(!tracks("repeat(500, 1px 1px) 1px"));
        assert!(!tracks("repeat(2, 1px) repeat(999, 1px)"));
        assert!(tracks("repeat(auto-fill, 1px) repeat(999, 1px)"));
        assert!(!tracks("repeat(auto-fill, 1px 1px) repeat(999, 1px)"));
        assert!(tracks(&names(MAX_GRID_TRACKS)));
        assert!(!tracks(&names(MAX_GRID_TRACKS + 1)));
        assert!(!after(GridAutoRows, "5px", &px(MAX_GRID_TRACKS + 1)).ends_with("1px"));
        let area = |input: &str| after(GridTemplateAreas, "'z'", input) != "1x1: z 0-1 0-1";
        let row = |n: usize| format!("'{}'", vec!["."; n].join(" "));
        assert!(area(&row(MAX_GRID_TRACKS)));
        assert!(!area(&row(MAX_GRID_TRACKS + 1)));
        assert!(area(&vec!["'.'"; MAX_GRID_TRACKS].join(" ")));
        assert!(!area(&vec!["'.'"; MAX_GRID_TRACKS + 1].join(" ")));
        let named = |n: usize| {
            let cells: Vec<String> = (0..n).map(|i| format!("a{i}")).collect();
            format!("'{}'", cells.join(" "))
        };
        assert!(area(&named(MAX_GRID_AREAS)));
        let second = format!("'b {}'", vec!["."; MAX_GRID_AREAS - 1].join(" "));
        assert!(!area(&format!("{} {second}", named(MAX_GRID_AREAS))));
        // The same cells with `a0` for `b`: a taller area, no new name.
        let again = format!("'a0 {}'", vec!["."; MAX_GRID_AREAS - 1].join(" "));
        assert!(area(&format!("{} {again}", named(MAX_GRID_AREAS))));
    }
}
