#![allow(clippy::unwrap_used, clippy::panic)]
use td_ui::raster::{Rect, Scale, Surface};
use td_ui::tree_table::{Geometry, GeometryHit as Hit};
#[test]
fn headers_and_cells_scroll_together_and_hits_remain_clipped() {
    for scale in 1..=4 {
        let s = scale as u32;
        let surface = Surface::new(
            700 * scale as usize,
            400 * scale as usize,
            Scale::new(scale).unwrap(),
        )
        .unwrap();
        let rect = Rect {
            x: 20 * scale as i64,
            y: 20 * scale as i64,
            width: 640 * s,
            height: 320 * s,
        };
        for offset in [0, 200 * scale as usize, 700 * scale as usize, usize::MAX] {
            let geometry = Geometry::new(surface, rect, &[800, 400], 100, 20, offset)
                .unwrap()
                .unwrap();
            assert!(geometry.horizontal().is_some());
            for column in 0..2 {
                if let Some(header) = geometry.header_cell(column) {
                    let cell = geometry.cell(20, column).unwrap();
                    assert_eq!(header.rect.x, cell.rect.x);
                    assert_eq!(header.clip.x, cell.clip.x);
                    assert_eq!(header.clip.width, cell.clip.width);
                    assert_eq!(
                        geometry.hit(header.clip.x, header.clip.y),
                        Some(Hit::Header(column))
                    );
                    assert_eq!(
                        geometry.hit(cell.clip.x, cell.clip.y),
                        Some(Hit::Cell { row: 20, column })
                    );
                    assert_eq!(cell.clip.intersection(rect), Some(cell.clip));
                }
            }
            assert_eq!(geometry.hit(i64::MIN, i64::MAX), None);
            let thumb = geometry.horizontal().unwrap().thumb;
            assert_eq!(geometry.hit(thumb.x, thumb.y), Some(Hit::Horizontal));
        }
    }
}
#[test]
fn blank_rows_and_clipped_disclosures_have_no_hit() {
    let surface = Surface::new(700, 400, Scale::new(1).unwrap()).unwrap();
    let rect = Rect {
        x: 20,
        y: 20,
        width: 640,
        height: 320,
    };
    let geometry = Geometry::new(surface, rect, &[200, 80], 3, usize::MAX, usize::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(geometry.first(), 0);
    assert_eq!(geometry.offset(), 0);
    assert_eq!(geometry.horizontal(), None);
    assert_eq!(
        geometry.hit(geometry.body().x, geometry.body().y + 4 * 24),
        None
    );
    assert!(geometry.disclosure(0, 1).is_some());
    assert_eq!(geometry.disclosure(0, 30), None);
    assert_eq!(geometry.row(3), None);
    assert!(
        Geometry::new(surface, Rect { height: 20, ..rect }, &[200, 80], 3, 0, 0)
            .unwrap()
            .is_none()
    );
}

#[test]
fn empty_contained_rectangles_fall_back_but_outside_rectangles_are_errors() {
    let surface = Surface::new(700, 400, Scale::new(1).unwrap()).unwrap();
    for rect in [
        Rect {
            width: 0,
            ..surface.bounds()
        },
        Rect {
            height: 0,
            ..surface.bounds()
        },
        Rect {
            x: 700,
            y: 400,
            width: 0,
            height: 0,
        },
    ] {
        assert!(Geometry::new(surface, rect, &[100], 1, 0, 0)
            .unwrap()
            .is_none());
    }
    for rect in [
        Rect {
            x: i64::MAX,
            ..surface.bounds()
        },
        Rect {
            x: -1,
            ..surface.bounds()
        },
        Rect {
            width: u32::MAX,
            ..surface.bounds()
        },
    ] {
        assert!(Geometry::new(surface, rect, &[100], 1, 0, 0).is_err());
    }
}
