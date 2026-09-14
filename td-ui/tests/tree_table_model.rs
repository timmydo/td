#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_ui::tree_table::{Cell, Column, Model, ModelError as Error, Row, DEPTH, ROWS};
const COLUMNS: [Column<'static>; 2] = [
    Column {
        title: "Process",
        minimum: 80,
        preferred: 200,
        numeric: false,
    },
    Column {
        title: "PID",
        minimum: 48,
        preferred: 80,
        numeric: true,
    },
];
fn row(id: u32, parent: Option<u32>, depth: u16) -> Row<u32> {
    Row {
        id,
        parent,
        depth,
        children: true,
        expanded: true,
    }
}
#[test]
fn lookup_and_navigation_preserve_preorder_with_unsorted_ids() {
    let rows = [
        row(10, None, 0),
        row(90, Some(10), 1),
        row(1, Some(10), 1),
        row(40, None, 0),
    ];
    let model = Model::new(&rows, &COLUMNS).unwrap();
    assert_eq!(model.find(1), Some(2));
    assert_eq!(model.find(90), Some(1));
    assert_eq!(model.parent(2), Some(0));
    assert_eq!(model.first_child(0), Some(1));
    assert_eq!(model.first_child(1), None);
    assert_eq!(model.find(9), None);
}
#[test]
fn invalid_and_revived_parent_links_are_refused() {
    for rows in [
        vec![row(1, None, 0), row(1, None, 0)],
        vec![row(1, Some(1), 0)],
        vec![row(1, Some(2), 1), row(2, None, 0)],
        vec![row(1, None, 0), row(2, Some(1), 2)],
        vec![
            row(1, None, 0),
            row(2, Some(1), 1),
            row(3, None, 0),
            row(4, Some(2), 2),
        ],
        vec![
            Row {
                expanded: false,
                ..row(1, None, 0)
            },
            row(2, Some(1), 1),
        ],
        vec![
            Row {
                children: false,
                ..row(1, None, 0)
            },
            row(2, Some(1), 1),
        ],
    ] {
        assert_eq!(
            Model::new(&rows, &COLUMNS).unwrap_err(),
            Error::InvalidHierarchy
        );
    }
}
#[test]
fn row_depth_and_text_bounds_are_explicit() {
    let mut rows: Vec<Row<u32>> = (0..ROWS as u32).map(|id| row(id, None, 0)).collect();
    assert_eq!(Model::new(&rows, &COLUMNS).unwrap().rows().len(), ROWS);
    rows.push(row(ROWS as u32, None, 0));
    assert_eq!(Model::new(&rows, &COLUMNS).unwrap_err(), Error::Limit);
    let mut deep: Vec<Row<u32>> = (0..=DEPTH as u32)
        .map(|id| row(id, id.checked_sub(1), id as u16))
        .collect();
    assert!(Model::new(&deep, &COLUMNS).is_ok());
    deep.push(row(DEPTH as u32 + 1, Some(DEPTH as u32), DEPTH as u16 + 1));
    assert_eq!(Model::new(&deep, &COLUMNS).unwrap_err(), Error::Limit);
    assert_eq!(Cell::new("bad\ntext"), Err(Error::InvalidText));
    assert_eq!(Cell::new(&"a".repeat(4097)), Err(Error::Limit));
    assert_eq!(Cell::empty().text(), "");
}
#[test]
fn columns_and_owned_storage_are_bounded_before_copying() {
    let rows = [row(1, None, 0)];
    assert_eq!(Model::new(&rows, &[]).unwrap_err(), Error::InvalidColumn);
    for column in [
        Column {
            minimum: 16,
            ..COLUMNS[0]
        },
        Column {
            preferred: 79,
            ..COLUMNS[0]
        },
        Column {
            preferred: 8193,
            ..COLUMNS[0]
        },
    ] {
        assert_eq!(
            Model::new(&rows, &[column]).unwrap_err(),
            Error::InvalidColumn
        );
    }
    for title in ["", "bad\nheading"] {
        assert_eq!(
            Model::new(
                &rows,
                &[Column {
                    title,
                    ..COLUMNS[0]
                }]
            )
            .unwrap_err(),
            Error::InvalidText
        );
    }
    let wide = Column {
        preferred: 8192,
        ..COLUMNS[0]
    };
    let columns = [wide; 16];
    let model = Model::new(&rows, &columns).unwrap();
    assert_eq!(model.columns().len(), 16);
    assert!(model.storage_bytes() <= td_ui::tree_table::MODEL_BYTES);
    assert_eq!(Model::new(&rows, &[wide; 17]).unwrap_err(), Error::Limit);
    let large = Row {
        id: [0u8; 1024],
        parent: None,
        depth: 0,
        children: false,
        expanded: false,
    };
    let oversized = vec![large; 8192];
    // The byte budget refuses before allocating the ID index or finding duplicates.
    assert_eq!(Model::new(&oversized, &COLUMNS).unwrap_err(), Error::Limit);
    assert_eq!(Cell::new(&"x".repeat(4096)).unwrap().text().len(), 4096);
}
