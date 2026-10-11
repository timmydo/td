#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]

//! `Rgb8`, the buffer every decode returns, held to its bounds: its fields
//! are public, so every accessor checks them rather than trusting them.

use td_jpeg::{Rgb8, MAX_AXIS};

#[test]
fn buffers_are_bounded_and_checked() {
    assert!(Rgb8::new(0, 1).is_none());
    assert!(Rgb8::new(1, 0).is_none());
    assert!(Rgb8::new(MAX_AXIS + 1, 1).is_none());
    assert!(Rgb8::new(1, MAX_AXIS + 1).is_none());
    assert!(Rgb8::new(usize::MAX, 2).is_none());
    // Within the axis ceiling but past the pixel budget.
    assert!(Rgb8::new(MAX_AXIS, MAX_AXIS).is_none());
    assert!(Rgb8::new(MAX_AXIS, 4097).is_none());
    assert_eq!(Rgb8::new(2, 2).unwrap().data.len(), 12);
    let edge = Rgb8::new(MAX_AXIS, 1).unwrap();
    assert!(edge.is_consistent());
    assert_eq!(edge.data.len(), MAX_AXIS * 3);
    assert_eq!(edge.pixel(MAX_AXIS - 1, 0), Some([0, 0, 0]));
    assert_eq!(edge.pixel(MAX_AXIS, 0), None);
    assert_eq!(edge.pixel(0, 1), None);
    let image = Rgb8 {
        width: 2,
        height: 2,
        data: (0..12).collect(),
    };
    assert_eq!(image.pixel(1, 1), Some([9, 10, 11]));
    assert_eq!(image.pixel(2, 0), None);
}

#[test]
fn a_buffer_whose_fields_disagree_is_refused() {
    for broken in [
        Rgb8 {
            width: 0,
            height: 2,
            data: vec![],
        },
        Rgb8 {
            width: MAX_AXIS + 1,
            height: 1,
            data: vec![],
        },
        Rgb8 {
            width: MAX_AXIS,
            height: 4097,
            data: vec![],
        },
        Rgb8 {
            width: 2,
            height: 2,
            data: vec![0; 11],
        },
        Rgb8 {
            width: usize::MAX,
            height: usize::MAX,
            data: vec![],
        },
    ] {
        assert!(!broken.is_consistent());
        assert_eq!(broken.pixel(1, 1), None);
    }
}
