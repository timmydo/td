#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_ui::chrome::{Strip, TabHit, TabNavigation};
use td_ui::raster::{Draw, Primitive, Scale, Surface};
fn surface(width: usize, scale: u8) -> Surface {
    Surface::new(width, 100 * scale as usize, Scale::new(scale).unwrap()).unwrap()
}
fn draws(strip: Strip, surface: Surface) -> Vec<Draw> {
    let mut draws = Vec::new();
    strip.emit(
        [("abcdefghijklmnopqrst", false); 5],
        surface.bounds(),
        &mut |draw| draws.push(draw),
    );
    draws
}
#[test]
fn default_document_tabs_keep_close_marks_and_hits() {
    for scale in 1..=4 {
        let surface = surface(400 * scale as usize, scale);
        for active in 0..5 {
            let strip = Strip::new(surface, 0, active, 5).unwrap();
            let stream = draws(strip, surface);
            let close = strip.close(active).unwrap();
            assert_eq!(strip.hit(close.x, close.y), Some(TabHit::Close(active)));
            assert!(stream.iter().any(|draw| matches!(draw.primitive,
                Primitive::Glyph { x, scalar: 'x', .. } if x == close.x + 8 * scale as i64)));
        }
    }
}

#[test]
fn nonclosable_tabs_reuse_close_space_without_close_hits_or_glyphs() {
    for scale in 1..=4 {
        let surface = surface(400 * scale as usize, scale);
        let strip = Strip::new(surface, 0, 1, 5)
            .unwrap()
            .with_close_buttons(false);
        assert_eq!(strip.close(1), None);
        let old_close = td_ui::chrome::Strip::new(surface, 0, 1, 5)
            .unwrap()
            .close(1)
            .unwrap();
        assert_eq!(strip.hit(old_close.x, old_close.y), Some(TabHit::Select(1)));
        let draws = draws(strip, surface);
        assert!(!draws
            .iter()
            .any(|draw| matches!(draw.primitive, Primitive::Glyph { scalar: 'x', .. })));
        assert!(draws.iter().any(|draw| matches!(draw.primitive, Primitive::Glyph { x, scalar: 'r', .. } if x >= old_close.x)));
        for x in [i64::MIN, -1, surface.width as i64, i64::MAX] {
            assert_eq!(strip.hit(x, 1), None);
        }
    }
}
#[test]
fn keyboard_selection_wraps_and_reveals_overflowed_resource_tabs() {
    for scale in 1..=4 {
        let surface = surface(160 * scale as usize, scale);
        let mut active = 0;
        for expected in [1, 2, 3, 4, 0] {
            let strip = Strip::new(surface, 0, active, 5)
                .unwrap()
                .with_close_buttons(false);
            active = strip.selection(TabNavigation::Next).unwrap();
            assert_eq!(active, expected);
            let strip = Strip::new(surface, 0, active, 5)
                .unwrap()
                .with_close_buttons(false);
            assert!(strip.tab(active).is_some());
            assert_eq!(strip.hit(0, 1), Some(TabHit::Select(active)));
        }
        let middle = Strip::new(surface, 0, 2, 5).unwrap();
        assert_eq!(middle.selection(TabNavigation::Previous), Some(1));
        assert!(Strip::new(surface, 0, 5, 5).is_none());
        let huge = Strip::new(surface, 0, usize::MAX - 1, usize::MAX).unwrap();
        assert_eq!(huge.selection(TabNavigation::Next), Some(0));
        assert_eq!(
            huge.selection(TabNavigation::Previous),
            Some(usize::MAX - 2)
        );
        let strip = Strip::new(surface, 0, 0, 5).unwrap();
        assert_eq!(strip.selection(TabNavigation::Previous), Some(4));
        assert_eq!(strip.selection(TabNavigation::First), Some(0));
        assert_eq!(strip.selection(TabNavigation::Last), Some(4));
        assert_eq!(
            Strip::new(surface, 0, 0, 0)
                .unwrap()
                .selection(TabNavigation::Next),
            None
        );
        assert_eq!(Strip::new(surface, 0, 0, 0).unwrap().hit(0, 1), None);
    }
}
#[test]
fn a_partially_visible_tab_has_no_off_surface_hit() {
    for scale in 1..=4 {
        let s = i64::from(scale);
        let surface = surface(40 * scale as usize, scale);
        let document = Strip::new(surface, 0, 4, 5).unwrap();
        let close = document.close(4).unwrap();
        assert_eq!(close.x, 136 * s);
        assert_eq!(
            document.hit(40 * s - 1, 24 * s - 1),
            Some(TabHit::Select(4))
        );
        assert_eq!(document.hit(close.x, close.y), None);
        let strip = document.with_close_buttons(false);
        assert_eq!(strip.hit(40 * s - 1, 24 * s - 1), Some(TabHit::Select(4)));
        assert_eq!(strip.hit(40 * s, 24 * s - 1), None);
        assert_eq!(strip.hit(0, 24 * s), None);
        assert_eq!(strip.close(4), None);
    }
}
