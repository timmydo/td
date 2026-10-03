//! The list's rows: the expanded part of the tree in preorder, each
//! directory's entries sorted by the chosen column, with what the cells
//! say. Pure.

use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;

use td_ui::tree_table::{Column, Direction, Row, Sort, ROWS};

use crate::tree::{Kind, Measure, Node, NodeId, Tree, ROOT};

/// How many of a directory's entries the list shows before a row offering
/// more; activating that row shows this many more.
pub const SHOWN: usize = 500;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum RowId {
    Node(NodeId),
    /// The entries of this directory past those shown.
    More(NodeId),
}

pub const NAME: usize = 0;
pub const SIZE: usize = 1;
pub const PERCENT: usize = 2;
pub const FILES: usize = 3;
pub const MODIFIED: usize = 4;

pub const COLUMNS: [Column<'static>; 5] = [
    Column {
        title: "Name",
        minimum: 120,
        preferred: 360,
        numeric: false,
    },
    Column {
        title: "Size",
        minimum: 80,
        preferred: 104,
        numeric: true,
    },
    Column {
        title: "% of parent",
        minimum: 112,
        preferred: 112,
        numeric: true,
    },
    Column {
        title: "Files",
        minimum: 64,
        preferred: 96,
        numeric: true,
    },
    Column {
        title: "Modified",
        minimum: 96,
        preferred: 112,
        numeric: false,
    },
];

/// The direction a column sorts in when it is first chosen: names A to Z,
/// everything else largest or newest first.
pub fn first_direction(column: usize) -> Direction {
    if column == NAME {
        Direction::Ascending
    } else {
        Direction::Descending
    }
}

/// A directory's live entries in the list's order.
pub fn sorted_children(tree: &Tree, id: NodeId, sort: Sort, measure: Measure) -> Vec<NodeId> {
    let Some(node) = tree.get(id) else {
        return Vec::new();
    };
    let mut children: Vec<(NodeId, &Node)> = node
        .children
        .iter()
        .filter_map(|child| Some((*child, tree.get(*child)?)))
        .collect();
    children.sort_by(|(_, a), (_, b)| {
        let order = match sort.column {
            NAME => a.name.as_bytes().cmp(b.name.as_bytes()),
            FILES => a.files.cmp(&b.files),
            MODIFIED => a.modified.cmp(&b.modified),
            _ => a.total.get(measure).cmp(&b.total.get(measure)),
        };
        let order = match sort.direction {
            Direction::Ascending => order,
            Direction::Descending => order.reverse(),
        };
        order.then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
    });
    children.into_iter().map(|(id, _)| id).collect()
}

/// Whether a node has entries the list can disclose.
fn has_children(node: &Node) -> bool {
    node.kind == Kind::Dir && !node.children.is_empty()
}

#[derive(Debug, Default)]
pub struct Visible {
    pub rows: Vec<Row<RowId>>,
    /// The list reached the toolkit's row limit and stops short.
    pub truncated: bool,
}

/// The visible preorder: the root, and the entries of every expanded
/// directory, at most `limits[dir]` (else `SHOWN`) of them each.
pub fn visible(
    tree: &Tree,
    expanded: &HashSet<NodeId>,
    limits: &HashMap<NodeId, usize>,
    sort: Sort,
    measure: Measure,
) -> Visible {
    let mut out = Visible::default();
    if tree.root().is_none() {
        return out;
    }
    // (row, children still to visit, depth)
    let mut stack: Vec<(RowId, Option<NodeId>, u16)> = vec![(RowId::Node(ROOT), None, 0)];
    while let Some((id, parent, depth)) = stack.pop() {
        if out.rows.len() >= ROWS {
            out.truncated = true;
            break;
        }
        match id {
            RowId::More(_) => out.rows.push(Row {
                id,
                parent: parent.map(RowId::Node),
                depth,
                children: false,
                expanded: false,
            }),
            RowId::Node(node_id) => {
                let Some(node) = tree.get(node_id) else {
                    continue;
                };
                let children = has_children(node);
                let open = children
                    && expanded.contains(&node_id)
                    && usize::from(depth) < td_ui::tree_table::DEPTH;
                out.rows.push(Row {
                    id,
                    parent: parent.map(RowId::Node),
                    depth,
                    children,
                    expanded: open,
                });
                if open {
                    let entries = sorted_children(tree, node_id, sort, measure);
                    let limit = limits.get(&node_id).copied().unwrap_or(SHOWN);
                    if entries.len() > limit {
                        stack.push((RowId::More(node_id), Some(node_id), depth + 1));
                    }
                    for child in entries.iter().take(limit).rev() {
                        stack.push((RowId::Node(*child), Some(node_id), depth + 1));
                    }
                }
            }
        }
    }
    out
}

/// Bytes in binary units with three significant digits: `0 B`, `512 B`,
/// `1.50 KiB`, `12.3 MiB`, `456 GiB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    if value >= 100.0 {
        format!("{value:.0} {name}")
    } else if value >= 10.0 {
        format!("{value:.1} {name}")
    } else {
        format!("{value:.2} {name}")
    }
}

/// A part of a whole as a percentage with one decimal.
pub fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "-".to_owned();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

/// Seconds since the epoch as a UTC date, `YYYY-MM-DD`, or question marks
/// for an instant past the calendar's range of about a million years.
pub fn date(seconds: i64) -> String {
    td_civil::unix_to_civil_utc_checked(seconds)
        .map_or_else(|| "????-??-??".into(), |c| td_civil::format_ymd(&c))
}

/// A name as the list shows it: lossy UTF-8, each control character a
/// `?`, so the cell takes it.
pub fn display_name(name: &std::ffi::OsStr) -> String {
    name.to_string_lossy()
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// The five cells of a row; `shown` is how many of a directory's entries
/// the list shows, for the row offering more.
pub fn cells(tree: &Tree, id: RowId, measure: Measure, queued: bool, shown: usize) -> [String; 5] {
    match id {
        RowId::More(dir) => {
            let hidden = tree
                .get(dir)
                .map_or(0, |node| node.children.len().saturating_sub(shown));
            [
                format!("… {hidden} more: Enter shows them"),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ]
        }
        RowId::Node(node_id) => {
            let Some(node) = tree.get(node_id) else {
                return Default::default();
            };
            let whole = node
                .parent
                .and_then(|parent| tree.get(parent))
                .map_or(node.total.get(measure), |parent| parent.total.get(measure));
            let mut name = String::new();
            if queued {
                name.push_str("[delete] ");
            }
            name.push_str(&display_name(&node.name));
            match node.kind {
                Kind::Dir if node.unreadable => name.push_str("  (unreadable)"),
                Kind::Dir => name.push('/'),
                Kind::Symlink => name.push_str(" @"),
                Kind::Mount => name.push_str("  (other file system)"),
                Kind::File | Kind::Other => {}
            }
            [
                name,
                size(node.total.get(measure)),
                percent(node.total.get(measure), whole),
                if node.kind == Kind::Dir {
                    node.files.to_string()
                } else {
                    String::new()
                },
                date(node.modified),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Identity, Size};

    fn node(name: &str, kind: Kind, bytes: u64) -> Node {
        Node::new(
            name.into(),
            kind,
            Size {
                allocated: bytes,
                apparent: bytes,
            },
            0,
            Identity::default(),
        )
    }

    #[test]
    fn formats() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1536), "1.50 KiB");
        assert_eq!(size(12_900_000), "12.3 MiB");
        assert_eq!(size(500 * 1024 * 1024 * 1024), "500 GiB");
        assert_eq!(percent(1, 3), "33.3%");
        assert_eq!(percent(1, 0), "-");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(-86_400), "1969-12-31");
        // Past the calendar's range, a placeholder of a date's width.
        assert_eq!(date(i64::MAX), "????-??-??");
        assert_eq!(date(i64::MIN), "????-??-??");
        assert_eq!(display_name(std::ffi::OsStr::new("a\nb")), "a?b");
    }

    #[test]
    fn preorder_sorted_by_size_with_a_more_row() {
        let mut tree = Tree::new("/r".into(), node("r", Kind::Dir, 0));
        let a = tree.push(ROOT, node("a", Kind::Dir, 0)).unwrap();
        let small = tree.push(ROOT, node("small", Kind::File, 1)).unwrap();
        let big = tree.push(ROOT, node("big", Kind::File, 50)).unwrap();
        let x = tree.push(a, node("x", Kind::File, 10)).unwrap();
        let y = tree.push(a, node("y", Kind::File, 20)).unwrap();
        tree.sum();
        let sort = Sort {
            column: SIZE,
            direction: Direction::Descending,
        };
        let expanded: HashSet<NodeId> = [ROOT, a].into_iter().collect();
        let mut limits = HashMap::new();
        let ids = |v: &Visible| v.rows.iter().map(|r| r.id).collect::<Vec<_>>();
        let v = visible(&tree, &expanded, &limits, sort, Measure::Allocated);
        assert_eq!(
            ids(&v),
            [ROOT, big, a, y, x, small].map(RowId::Node).to_vec()
        );
        td_ui::tree_table::Model::new(&v.rows, &COLUMNS).unwrap();
        limits.insert(a, 1);
        let v = visible(&tree, &expanded, &limits, sort, Measure::Allocated);
        assert_eq!(
            ids(&v),
            vec![
                RowId::Node(ROOT),
                RowId::Node(big),
                RowId::Node(a),
                RowId::Node(y),
                RowId::More(a),
                RowId::Node(small)
            ]
        );
        td_ui::tree_table::Model::new(&v.rows, &COLUMNS).unwrap();
        let by_name = Sort {
            column: NAME,
            direction: Direction::Ascending,
        };
        let v = visible(
            &tree,
            &[ROOT].into_iter().collect(),
            &HashMap::new(),
            by_name,
            Measure::Allocated,
        );
        assert_eq!(ids(&v), [ROOT, a, big, small].map(RowId::Node).to_vec());
    }
}
