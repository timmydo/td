//! The treemap: every directory's rectangle divided among its entries in
//! proportion to their size, by the squarified layout (Bruls, Huizing and
//! van Wijk), down to tiles too small to divide. A file's tile is coloured
//! by its extension, the extensions taking the most space first taking the
//! palette's colours, as WinDirStat does. Pure.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use td_ui::raster::{fill, Draw, Rect};

use crate::tree::{Kind, Measure, NodeId, Tree, ROOT};

/// The tile budget: a directory is divided only while the tiles made, the
/// rectangles still to lay and its own entries stay under it, so a layout
/// makes at most this many tiles.
pub const MAX_TILES: usize = 250_000;

/// The extensions' colours, in rank order.
pub const PALETTE: [u32; 12] = [
    0x3d6fb6, 0xc8463d, 0x4e9a4b, 0xe0962e, 0x8a56ac, 0x2a9d8f, 0xd2691e, 0xb8a21b, 0xd0508a,
    0x7b5e4f, 0x1f93b8, 0x6a8a3a,
];
/// A file whose extension ranks past the palette, or that has none.
pub const OTHER: u32 = 0x9a948a;
/// A directory too small to divide, or a directory's own blocks.
pub const DIRECTORY: u32 = 0x6e675c;
/// Links, devices and mounts.
pub const SPECIAL: u32 = 0xbdb6a8;
/// What the delete list holds.
pub const DOOMED: u32 = 0x3b3833;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tile {
    pub rect: Rect,
    pub node: NodeId,
    pub color: u32,
}

#[derive(Debug, Default)]
pub struct Treemap {
    pub area: Option<Rect>,
    pub tiles: Vec<Tile>,
    /// Each laid-out node's rectangle, directories that were divided too.
    pub frames: HashMap<NodeId, Rect>,
}

#[derive(Clone, Copy, Debug)]
struct F {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl F {
    /// Edges rounded on their own, so neighbours share a pixel edge and
    /// nothing gaps or overlaps.
    fn pixels(self) -> Option<Rect> {
        let (x0, y0) = (self.x.round() as i64, self.y.round() as i64);
        let (x1, y1) = (
            (self.x + self.w).round() as i64,
            (self.y + self.h).round() as i64,
        );
        (x1 > x0 && y1 > y0).then(|| Rect {
            x: x0,
            y: y0,
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        })
    }
}

/// A lowercased extension, held inline so ranking a tree's files makes no
/// allocation per file.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Ext {
    len: u8,
    bytes: [u8; 16],
}

impl Ext {
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

/// A lowercased extension of at most 16 bytes after the last dot; a name
/// that only starts with a dot has none.
pub fn extension(name: &OsStr) -> Option<Ext> {
    let bytes = name.as_bytes();
    let dot = bytes.iter().rposition(|b| *b == b'.')?;
    let ext = bytes.get(dot + 1..)?;
    if dot == 0 || ext.is_empty() || ext.len() > 16 {
        return None;
    }
    let mut out = Ext {
        len: u8::try_from(ext.len()).ok()?,
        bytes: [0; 16],
    };
    for (slot, byte) in out.bytes.iter_mut().zip(ext) {
        *slot = byte.to_ascii_lowercase();
    }
    Some(out)
}

/// The palette's colour for each of the extensions holding the most space.
pub fn palette(tree: &Tree, measure: Measure) -> HashMap<Ext, u32> {
    let mut sizes: HashMap<Ext, u64> = HashMap::new();
    for id in 0..tree.len() {
        let Some(node) = tree.get(id as NodeId) else {
            continue;
        };
        if node.kind != Kind::File {
            continue;
        }
        if let Some(ext) = extension(&node.name) {
            let slot = sizes.entry(ext).or_default();
            *slot = slot.saturating_add(node.own.get(measure));
        }
    }
    let mut ranked: Vec<(Ext, u64)> = sizes.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .zip(PALETTE)
        .map(|((ext, _), color)| (ext, color))
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Item {
    /// An entry, laid out by its subtree.
    Child(NodeId),
    /// A directory's own blocks.
    Own(NodeId),
}

fn worst(max: f64, min: f64, sum: f64, side: f64) -> f64 {
    let (side2, sum2) = (side * side, sum * sum);
    if min <= 0.0 || sum2 <= 0.0 {
        return f64::INFINITY;
    }
    (side2 * max / sum2).max(sum2 / (side2 * min))
}

/// Lays `items`, sorted by weight descending, all positive, into `area`.
fn squarify(items: &[(f64, Item)], area: F, out: &mut Vec<(F, Item)>) {
    let sum: f64 = items.iter().map(|(w, _)| *w).sum();
    if sum <= 0.0 || area.w <= 0.0 || area.h <= 0.0 {
        return;
    }
    let scale = area.w * area.h / sum;
    let mut rect = area;
    let mut start = 0;
    while start < items.len() {
        let side = rect.w.min(rect.h);
        let mut end = start;
        let mut row = 0.0;
        let mut best = f64::INFINITY;
        let first = items.get(start).map_or(0.0, |(w, _)| w * scale);
        while let Some((weight, _)) = items.get(end) {
            let area = weight * scale;
            let next = worst(first, area, row + area, side);
            if end > start && next > best {
                break;
            }
            best = next;
            row += area;
            end += 1;
        }
        let row_items = items.get(start..end).unwrap_or(&[]);
        let last = end == items.len();
        if rect.w >= rect.h {
            // A column at the left, as wide as the row's share.
            let thick = if last || rect.h <= 0.0 {
                rect.w
            } else {
                row / rect.h
            };
            let mut y = rect.y;
            for (index, (weight, item)) in row_items.iter().enumerate() {
                let h = if index + 1 == row_items.len() {
                    rect.y + rect.h - y
                } else {
                    weight * scale / thick
                };
                out.push((
                    F {
                        x: rect.x,
                        y,
                        w: thick,
                        h,
                    },
                    *item,
                ));
                y += h;
            }
            rect.x += thick;
            rect.w -= thick;
        } else {
            let thick = if last || rect.w <= 0.0 {
                rect.h
            } else {
                row / rect.w
            };
            let mut x = rect.x;
            for (index, (weight, item)) in row_items.iter().enumerate() {
                let w = if index + 1 == row_items.len() {
                    rect.x + rect.w - x
                } else {
                    weight * scale / thick
                };
                out.push((
                    F {
                        x,
                        y: rect.y,
                        w,
                        h: thick,
                    },
                    *item,
                ));
                x += w;
            }
            rect.y += thick;
            rect.h -= thick;
        }
        start = end;
    }
}

fn file_color(tree: &Tree, id: NodeId, colors: &HashMap<Ext, u32>) -> u32 {
    let Some(node) = tree.get(id) else {
        return OTHER;
    };
    match node.kind {
        Kind::File => extension(&node.name)
            .and_then(|ext| colors.get(&ext).copied())
            .unwrap_or(OTHER),
        Kind::Dir => DIRECTORY,
        Kind::Symlink | Kind::Other | Kind::Mount => SPECIAL,
    }
}

/// Lays the whole tree into `area`. A directory narrower or shorter than
/// `min_side` pixels is one tile.
pub fn layout(tree: &Tree, measure: Measure, area: Rect, min_side: f64) -> Treemap {
    let colors = palette(tree, measure);
    let mut map = Treemap {
        area: Some(area),
        ..Treemap::default()
    };
    let mut stack = vec![(
        ROOT,
        F {
            x: area.x as f64,
            y: area.y as f64,
            w: f64::from(area.width),
            h: f64::from(area.height),
        },
    )];
    let mut items: Vec<(f64, Item)> = Vec::new();
    let mut placed: Vec<(F, Item)> = Vec::new();
    while let Some((id, rect)) = stack.pop() {
        let Some(node) = tree.get(id) else {
            continue;
        };
        let Some(pixels) = rect.pixels() else {
            continue;
        };
        map.frames.insert(id, pixels);
        let divisible = node.kind == Kind::Dir
            && rect.w >= min_side
            && rect.h >= min_side
            && map.tiles.len() + stack.len() + node.children.len() < MAX_TILES;
        if !divisible {
            map.tiles.push(Tile {
                rect: pixels,
                node: id,
                color: file_color(tree, id, &colors),
            });
            continue;
        }
        items.clear();
        items.extend(node.children.iter().filter_map(|child| {
            let size = tree.get(*child)?.total.get(measure);
            (size > 0).then_some((size as f64, Item::Child(*child)))
        }));
        let own = node.own.get(measure);
        if own > 0 {
            items.push((own as f64, Item::Own(id)));
        }
        if items.is_empty() {
            map.tiles.push(Tile {
                rect: pixels,
                node: id,
                color: DIRECTORY,
            });
            continue;
        }
        items.sort_by(|a, b| b.0.total_cmp(&a.0));
        placed.clear();
        squarify(&items, rect, &mut placed);
        for (rect, item) in placed.drain(..) {
            match item {
                Item::Child(child) => stack.push((child, rect)),
                Item::Own(dir) => {
                    if let Some(pixels) = rect.pixels() {
                        map.tiles.push(Tile {
                            rect: pixels,
                            node: dir,
                            color: DIRECTORY,
                        });
                    }
                }
            }
        }
    }
    map
}

impl Treemap {
    /// The node whose tile holds the point.
    pub fn hit(&self, x: i64, y: i64) -> Option<NodeId> {
        self.tiles
            .iter()
            .find(|tile| tile.rect.contains(x, y))
            .map(|tile| tile.node)
    }
}

fn blend(color: u32, toward: u32, part: u32) -> u32 {
    let channel = |shift: u32| {
        let a = (color >> shift) & 0xff;
        let b = (toward >> shift) & 0xff;
        ((a * (256 - part) + b * part) >> 8) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// A tile with a light top and left edge and a dark bottom and right one,
/// so neighbours of one colour still read apart.
pub fn emit_tile(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    fill(rect, color, damage, sink);
    if rect.width < 4 || rect.height < 4 {
        return;
    }
    let light = blend(color, 0xffffff, 80);
    let dark = blend(color, 0x000000, 90);
    fill(Rect { height: 1, ..rect }, light, damage, sink);
    fill(Rect { width: 1, ..rect }, light, damage, sink);
    fill(
        Rect {
            y: rect.y + i64::from(rect.height) - 1,
            height: 1,
            ..rect
        },
        dark,
        damage,
        sink,
    );
    fill(
        Rect {
            x: rect.x + i64::from(rect.width) - 1,
            width: 1,
            ..rect
        },
        dark,
        damage,
        sink,
    );
}

/// A frame `thick` pixels wide inside `rect`.
pub fn emit_outline(rect: Rect, thick: u32, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    let t = thick.min(rect.width / 2).min(rect.height / 2).max(1);
    let ti = i64::from(t);
    fill(Rect { height: t, ..rect }, color, damage, sink);
    fill(
        Rect {
            y: rect.y + i64::from(rect.height) - ti,
            height: t,
            ..rect
        },
        color,
        damage,
        sink,
    );
    fill(Rect { width: t, ..rect }, color, damage, sink);
    fill(
        Rect {
            x: rect.x + i64::from(rect.width) - ti,
            width: t,
            ..rect
        },
        color,
        damage,
        sink,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Identity, Node, Size};

    fn file(name: &str, bytes: u64) -> Node {
        Node::new(
            name.into(),
            Kind::File,
            Size {
                allocated: bytes,
                apparent: bytes,
            },
            0,
            Identity::default(),
        )
    }

    fn dir(name: &str) -> Node {
        Node::new(
            name.into(),
            Kind::Dir,
            Size::default(),
            0,
            Identity::default(),
        )
    }

    #[test]
    fn extensions() {
        assert_eq!(
            extension(OsStr::new("a.TXT")).map(|e| e.as_bytes().to_vec()),
            Some(b"txt".to_vec())
        );
        assert_eq!(extension(OsStr::new(".bashrc")), None);
        assert_eq!(extension(OsStr::new("noext")), None);
        assert_eq!(extension(OsStr::new("a.")), None);
    }

    #[test]
    fn tiles_cover_the_area_in_proportion() {
        let mut tree = Tree::new("/r".into(), dir("r"));
        let sub = tree.push(ROOT, dir("sub")).unwrap();
        for (index, size) in [600u64, 300, 100].iter().enumerate() {
            tree.push(ROOT, file(&format!("f{index}.a"), *size))
                .unwrap();
        }
        tree.push(sub, file("g.b", 1000)).unwrap();
        tree.push(sub, file("h.b", 1000)).unwrap();
        tree.sum();
        let area = Rect {
            x: 10,
            y: 20,
            width: 300,
            height: 100,
        };
        let map = layout(&tree, Measure::Allocated, area, 4.0);
        let covered: u64 = map
            .tiles
            .iter()
            .map(|t| u64::from(t.rect.width) * u64::from(t.rect.height))
            .sum();
        assert_eq!(covered, 300 * 100);
        for tile in &map.tiles {
            assert!(area.intersection(tile.rect) == Some(tile.rect));
        }
        for (i, a) in map.tiles.iter().enumerate() {
            for b in map.tiles.iter().skip(i + 1) {
                assert!(a.rect.intersection(b.rect).is_none(), "{a:?} {b:?}");
            }
        }
        // The sub directory holds two thirds of the bytes.
        let frame = map.frames[&sub];
        let share = f64::from(frame.width) * f64::from(frame.height) / 30_000.0;
        assert!((share - 2.0 / 3.0).abs() < 0.02, "{share}");
        // The .b extension holds the most, so it takes the first colour.
        let g = map
            .tiles
            .iter()
            .find(|t| tree.get(t.node).unwrap().name == "g.b")
            .unwrap();
        assert_eq!(g.color, PALETTE[0]);
        assert_eq!(map.hit(g.rect.x, g.rect.y), Some(g.node));
    }

    #[test]
    fn a_directory_past_the_tile_budget_is_one_tile() {
        let mut tree = Tree::new("/r".into(), dir("r"));
        let wide = tree.push(ROOT, dir("wide")).unwrap();
        for index in 0..MAX_TILES {
            tree.push(wide, file(&format!("{index}"), 1)).unwrap();
        }
        tree.sum();
        let area = Rect {
            x: 0,
            y: 0,
            width: 4096,
            height: 1024,
        };
        let map = layout(&tree, Measure::Allocated, area, 1.0);
        assert!(map.tiles.len() <= MAX_TILES);
        assert_eq!(map.tiles.len(), 1);
        assert_eq!(map.tiles[0].node, wide);
    }

    #[test]
    fn extreme_ratios_and_tiny_areas_stay_inside() {
        let mut tree = Tree::new("/r".into(), dir("r"));
        tree.push(ROOT, file("huge", u64::MAX / 4)).unwrap();
        tree.push(ROOT, file("tiny", 1)).unwrap();
        tree.push(ROOT, file("zero", 0)).unwrap();
        tree.sum();
        for (width, height) in [(1, 1), (1, 500), (500, 1), (3, 2), (800, 600)] {
            let area = Rect {
                x: 5,
                y: 7,
                width,
                height,
            };
            let map = layout(&tree, Measure::Allocated, area, 3.0);
            for tile in &map.tiles {
                assert_eq!(area.intersection(tile.rect), Some(tile.rect));
            }
        }
    }

    #[test]
    fn a_small_directory_is_one_tile() {
        let mut tree = Tree::new("/r".into(), dir("r"));
        let sub = tree.push(ROOT, dir("sub")).unwrap();
        tree.push(sub, file("a", 1)).unwrap();
        tree.push(sub, file("b", 1)).unwrap();
        tree.push(ROOT, file("big", 100_000)).unwrap();
        tree.sum();
        let area = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 100,
        };
        let map = layout(&tree, Measure::Allocated, area, 4.0);
        assert!(map.tiles.iter().all(|t| t.node != sub + 1));
    }
}
