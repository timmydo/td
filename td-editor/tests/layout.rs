use td_editor::layout::{Affinity, Break, Caret, Layout, Metrics, Position, Viewport, CELL_WIDTH};
use td_editor::{text, Error};

fn caret(byte: usize) -> Caret {
    Caret {
        byte,
        affinity: Affinity::Downstream,
    }
}

#[test]
fn wrapping_preserves_separators_and_exact_width_has_no_phantom_row() {
    let cases = [
        ("", 4, vec![""]),
        ("abcd", 4, vec!["abcd"]),
        ("abcd\n", 4, vec!["abcd", ""]),
        ("abcde", 4, vec!["abcd", "e"]),
        ("a bcde", 4, vec!["a ", "bcde"]),
        ("abcd ef", 4, vec!["abcd", " ef"]),
        ("abcd\tef", 4, vec!["abcd", "\t", "ef"]),
        (
            "aaaaaaaa bbbbbbbbbb ccc",
            8,
            vec!["aaaaaaaa", " bbbbbbb", "bbb ccc"],
        ),
        ("  abcdef", 4, vec!["  ab", "cdef"]),
        ("        ", 4, vec!["    ", "    "]),
        ("ab  cd", 4, vec!["ab  ", "cd"]),
        ("\n\n", 1, vec!["", "", ""]),
        ("é猫z", 2, vec!["é猫", "z"]),
        ("a\tbc", 8, vec!["a\t", "bc"]),
        ("\tx", 1, vec!["\t", "x"]),
        ("a\tb", 1, vec!["a", "\t", "b"]),
    ];
    for (input, width, expected) in cases {
        let layout = Layout::new(input, width, true).unwrap();
        let actual: Vec<_> = layout
            .rows()
            .map(|row| input.get(row.bytes()).unwrap())
            .collect();
        assert_eq!(actual, expected, "{input:?}, width {width}");
        assert_eq!(layout.rows().last().unwrap().ending(), Break::End);
    }
}

#[test]
fn unwrapped_rows_keep_logical_lines_and_eight_column_tabs() {
    let layout = Layout::new("a\tbé\n\tq", 1, false).unwrap();
    let rows: Vec<_> = layout.rows().collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.first().unwrap().columns(), 10);
    assert_eq!(rows.last().unwrap().columns(), 9);
    assert_eq!(rows.first().unwrap().ending(), Break::Newline);
    let cells: Vec<_> = rows.first().unwrap().cells().collect();
    let tab = cells.get(1).unwrap();
    assert_eq!((tab.bytes.clone(), tab.column, tab.width), (1..2, 1, 7));
    let unicode = cells.last().unwrap();
    assert_eq!(
        (unicode.bytes.clone(), unicode.column, unicode.width),
        (3..5, 9, 1)
    );
}

#[test]
fn soft_boundaries_have_two_positions_and_hits_retain_the_side() {
    let layout = Layout::new("ab cd", 3, true).unwrap();
    let upstream = Caret {
        byte: 3,
        affinity: Affinity::Upstream,
    };
    assert_eq!(
        layout.position(upstream).unwrap(),
        Position { row: 0, column: 3 }
    );
    assert_eq!(
        layout.position(caret(3)).unwrap(),
        Position { row: 1, column: 0 }
    );
    assert_eq!(layout.rows().next().unwrap().hit_test(usize::MAX), upstream);
    assert_eq!(layout.rows().nth(1).unwrap().hit_test(0), caret(3));
}

#[test]
fn hits_use_font_pixels_with_midpoint_ties_before_tabs_and_glyphs() {
    let layout = Layout::new("a\tz", 10, false).unwrap();
    let row = layout.rows().next().unwrap();
    assert_eq!(row.hit_test(4), caret(0));
    assert_eq!(row.hit_test(5), caret(1));
    assert_eq!(row.hit_test(36), caret(1));
    assert_eq!(row.hit_test(37), caret(2));
    assert_eq!(row.hit_test(64), caret(2));
    assert_eq!(row.hit_test(68), caret(2));
    assert_eq!(row.hit_test(69), caret(3));
    assert_eq!(row.hit_test(usize::MAX), caret(3));
}

#[test]
fn vertical_motion_retains_the_callers_desired_column_through_short_rows() {
    let layout = Layout::new("abcdef\nx\nabcdef", 8, true).unwrap();
    let short = layout.vertical(caret(5), 1, 5).unwrap();
    assert_eq!(short.byte, 8);
    assert_eq!(layout.vertical(short, 1, 5).unwrap().byte, 14);
    assert_eq!(layout.vertical(short, -1, 5).unwrap().byte, 5);
    assert_eq!(layout.vertical(short, isize::MIN, 5).unwrap().byte, 5);
    assert_eq!(layout.vertical(short, isize::MAX, 5).unwrap().byte, 14);
    assert_eq!(layout.vertical(short, 0, 5).unwrap(), short);
}

#[test]
fn vertical_motion_uses_visual_rows_and_preserves_upstream_affinity() {
    let layout = Layout::new("abcdef", 3, true).unwrap();
    let result = layout.vertical(caret(6), -1, 3).unwrap();
    assert_eq!(
        result,
        Caret {
            byte: 3,
            affinity: Affinity::Upstream
        }
    );
    assert_eq!(layout.position(result).unwrap().row, 0);
    assert_eq!(layout.vertical(result, 1, 3).unwrap().byte, 6);
}

#[test]
fn invalid_input_and_scalar_boundaries_fail_without_panics() {
    for width in [0, 1025, usize::MAX] {
        assert!(matches!(
            Layout::new("", width, true),
            Err(Error::InvalidArgument)
        ));
    }
    for input in ["\0", "\r", "\u{feff}x", "\u{7f}"] {
        assert!(matches!(
            Layout::new(input, 10, true),
            Err(Error::InvalidText)
        ));
    }
    let layout = Layout::new("猫", 1, true).unwrap();
    for byte in [1, 2, 4, usize::MAX] {
        assert_eq!(layout.position(caret(byte)), Err(Error::InvalidPosition));
        assert_eq!(
            layout.vertical(caret(byte), 1, 0),
            Err(Error::InvalidPosition)
        );
    }
    assert!(matches!(
        Layout::new(&"x".repeat(text::MAX_FILE_BYTES + 1), 1, true),
        Err(Error::Limit)
    ));
}

#[test]
fn row_stream_is_fused_and_does_not_store_per_character_or_per_row_maps() {
    let input = "\n".repeat(text::MAX_FILE_BYTES);
    let layout = Layout::new(&input, 1, true).unwrap();
    assert!(std::mem::size_of_val(&layout.rows()) <= 128);
    assert_eq!(layout.rows().count(), text::MAX_FILE_BYTES + 1);
    let empty = Layout::new("", 1, true).unwrap();
    let mut rows = empty.rows();
    assert!(rows.next().is_some());
    assert!(rows.next().is_none());
    assert!(rows.next().is_none());
    let short = Layout::new("abcde\nf", 2, true).unwrap();
    let mut rows = short.rows();
    assert_eq!(rows.next().unwrap().bytes(), 0..2);
    let checkpoint = rows.clone();
    assert_eq!(
        rows.map(|row| row.bytes()).collect::<Vec<_>>(),
        checkpoint.map(|row| row.bytes()).collect::<Vec<_>>()
    );
}

#[test]
fn viewport_scrolls_clamps_and_reveals_without_affecting_other_tabs() {
    let mut viewport = Viewport::new(5, 3).unwrap();
    let other = viewport;
    viewport.scroll(isize::MAX, 10);
    assert_eq!(viewport.origin().row, 7);
    viewport.scroll(isize::MIN, 10);
    assert_eq!(viewport.origin().row, 0);
    viewport.reveal(Position { row: 5, column: 8 }, 10, false);
    assert_eq!(viewport.origin(), Position { row: 3, column: 4 });
    viewport.reveal(Position { row: 1, column: 2 }, 10, false);
    assert_eq!(viewport.origin(), Position { row: 1, column: 2 });
    viewport.reveal(Position { row: 1, column: 2 }, 2, true);
    assert_eq!(viewport.origin(), Position { row: 0, column: 0 });
    assert_eq!(other.origin(), Position { row: 0, column: 0 });
    assert_eq!(other.dimensions(), (5, 3));
    viewport.scroll(isize::MAX, 0);
    viewport.reveal(
        Position {
            row: usize::MAX,
            column: usize::MAX,
        },
        0,
        false,
    );
    assert_eq!(viewport.origin().row, 0);
    for (columns, rows) in [
        (0, 1),
        (1, 0),
        (1025, 1),
        (1, 513),
        (usize::MAX, usize::MAX),
    ] {
        assert_eq!(Viewport::new(columns, rows), Err(Error::InvalidArgument));
    }
}

#[test]
fn resizing_preserves_scroll_until_clamping_and_invalid_sizes_are_atomic() {
    let mut view = Viewport::new(5, 3).unwrap();
    view.scroll(10, 30);
    view.scroll_horizontal(10, 31, false);
    view.resize(10, 5, 30, 31, false).unwrap();
    assert_eq!(
        view.origin(),
        Position {
            row: 10,
            column: 10
        }
    );
    let before = view;
    assert_eq!(
        view.resize(0, 5, 30, 31, false),
        Err(Error::InvalidArgument)
    );
    assert_eq!(view, before);
    view.resize(30, 29, 30, 31, false).unwrap();
    assert_eq!(view.origin(), Position { row: 1, column: 1 });
    view.scroll_horizontal(isize::MIN, 31, false);
    assert_eq!(view.origin().column, 0);
    view.scroll_horizontal(isize::MAX, 31, false);
    assert_eq!(view.origin().column, 1);
    view.resize(5, 3, 30, 31, true).unwrap();
    assert_eq!(view.origin().column, 0);
    view.scroll_horizontal(isize::MAX, 31, true);
    assert_eq!(view.origin().column, 0);
}

// Exhaustively score every prefix, recomputing its width from scratch.
// Deliberately slow and unlike the streaming production traversal.
#[allow(
    clippy::unwrap_used,
    reason = "test reference enumerates only valid vector boundaries"
)]
fn reference_rows(input: &str, width: usize) -> Vec<String> {
    let mut result = Vec::new();
    for logical in input.split('\n') {
        let mut scalars: Vec<char> = logical.chars().collect();
        if scalars.is_empty() {
            result.push(String::new());
        }
        while !scalars.is_empty() {
            let fitting = (1..=scalars.len())
                .filter(|end| {
                    let columns = scalars.get(..*end).unwrap().iter().fold(0, |col, c| {
                        if *c == '\t' {
                            (col / 8 + 1) * 8
                        } else {
                            col + 1
                        }
                    });
                    columns <= width || *end == 1
                })
                .max()
                .unwrap();
            let end = if fitting == scalars.len() {
                fitting
            } else {
                (1..=fitting)
                    .filter(|end| {
                        let prefix = scalars.get(..*end).unwrap();
                        matches!(prefix.last(), Some(' ' | '\t'))
                            && prefix.iter().any(|c| *c != ' ' && *c != '\t')
                    })
                    .max()
                    .unwrap_or(fitting)
            };
            result.push(scalars.drain(..end).collect());
        }
    }
    result
}

#[test]
fn generated_rows_match_reference_and_every_boundary_roundtrips() {
    let alphabet = ['x', ' ', '\t', '\n', 'é', '猫', '\u{301}'];
    let mut seed = 9u64;
    for _ in 0..300 {
        let mut input = String::new();
        for _ in 0..80 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            input.push(
                *alphabet
                    .get((seed >> 32) as usize % alphabet.len())
                    .unwrap(),
            );
        }
        for width in [1, 2, 7, 8, 9, 16, 1024] {
            let layout = Layout::new(&input, width, true).unwrap();
            let rows: Vec<_> = layout.rows().collect();
            let actual: Vec<_> = rows
                .iter()
                .map(|row| input.get(row.bytes()).unwrap().to_owned())
                .collect();
            assert_eq!(actual, reference_rows(&input, width));
            for row in &rows {
                for cell in row.cells() {
                    for pixel in 0..=cell.width * CELL_WIDTH {
                        let expected = if pixel <= cell.width * CELL_WIDTH / 2 {
                            cell.bytes.start
                        } else {
                            cell.bytes.end
                        };
                        assert_eq!(
                            row.hit_test(cell.column * CELL_WIDTH + pixel).byte,
                            expected
                        );
                    }
                }
            }
            for byte in input
                .char_indices()
                .map(|(byte, _)| byte)
                .chain([input.len()])
            {
                for affinity in [Affinity::Upstream, Affinity::Downstream] {
                    let pos = layout.position(Caret { byte, affinity }).unwrap();
                    let row = rows.get(pos.row).unwrap();
                    let hit = row.hit_test(pos.column * CELL_WIDTH);
                    assert_eq!(
                        hit.byte, byte,
                        "{input:?} width {width}, {byte}, {affinity:?}"
                    );
                    assert_eq!(layout.position(hit).unwrap(), pos);
                }
            }
        }
    }
}

#[test]
fn model_layout_uses_viewport_width_and_metrics_are_cacheable() {
    let mut editor = td_editor::model::Editor::default();
    let tab = editor.load_bytes(b"abcde\n\tq").unwrap();
    let view = Viewport::new(4, 2).unwrap();
    let layout = view.layout(editor.document(tab).unwrap(), true).unwrap();
    assert_eq!(
        layout.metrics(),
        Metrics {
            rows: 4,
            columns: 9
        }
    );
    assert_eq!(layout.rows().next().unwrap().columns(), 4);
    let unwrapped = view.layout(editor.document(tab).unwrap(), false).unwrap();
    assert_eq!(
        unwrapped.metrics(),
        Metrics {
            rows: 2,
            columns: 10
        }
    );
    let maximum = Viewport::new(1024, 512).unwrap();
    assert_eq!(maximum.dimensions(), (1024, 512));
    assert!(Layout::for_document(editor.document(tab).unwrap(), 0, true).is_err());
    assert!(Layout::for_document(editor.document(tab).unwrap(), 1025, true).is_err());
}

#[test]
fn caret_pixels_clip_soft_row_ends_and_oversized_tabs_but_not_unwrapped_carets() {
    let mut view = Viewport::new(3, 2).unwrap();
    assert_eq!(
        view.caret_pixel(Position { row: 0, column: 3 }, true),
        Some((23, 0))
    );
    assert_eq!(
        view.caret_pixel(Position { row: 1, column: 1 }, true),
        Some((8, 16))
    );
    assert_eq!(view.caret_pixel(Position { row: 2, column: 0 }, true), None);
    assert_eq!(
        view.caret_pixel(Position { row: 0, column: 3 }, false),
        None
    );
    view.reveal(Position { row: 2, column: 3 }, 4, false);
    assert_eq!(
        view.caret_pixel(Position { row: 2, column: 3 }, false),
        Some((16, 16))
    );
    assert_eq!(
        view.caret_pixel(Position { row: 0, column: 3 }, false),
        None
    );
    assert_eq!(
        view.caret_pixel(Position { row: 2, column: 0 }, false),
        None
    );
    view.resize(1, 1, 2, 9, true).unwrap();
    assert_eq!(
        view.caret_pixel(Position { row: 1, column: 8 }, true),
        Some((7, 0))
    );
    assert_eq!(
        view.caret_pixel(
            Position {
                row: 1,
                column: usize::MAX
            },
            true
        ),
        Some((7, 0))
    );
}
