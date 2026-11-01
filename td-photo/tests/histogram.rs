//! The develop box's histogram, `td_photo::histogram`: each channel's
//! counts of an 8-bit frame, and the columns the pane paints of them.

use td_photo::histogram::{Histogram, BINS};
use td_photo::image::Rgb8;

fn image(pixels: &[[u8; 3]]) -> Rgb8 {
    Rgb8 {
        width: pixels.len(),
        height: 1,
        data: pixels.iter().flatten().copied().collect(),
    }
}

#[test]
fn each_channel_counts_its_values() {
    let counted = Histogram::of(&image(&[[0, 128, 255], [0, 128, 10], [7, 7, 7]])).unwrap();
    let counts = counted.counts();
    assert_eq!(counts.len(), BINS);
    assert_eq!(counts[0], [2, 0, 0]);
    assert_eq!(counts[7], [1, 1, 1]);
    assert_eq!(counts[128], [0, 2, 0]);
    assert_eq!(counts[255], [0, 0, 1]);
    assert_eq!(counts[10], [0, 0, 1]);
    let total: u32 = counts.iter().map(|bin| bin[0]).sum();
    assert_eq!(total, 3);
    // Nothing to count, or a buffer not its axes' size, is none.
    assert_eq!(Histogram::of(&image(&[])), None);
    let short = Rgb8 {
        width: 2,
        height: 1,
        data: vec![0; 5],
    };
    assert_eq!(Histogram::of(&short), None);
}

/// The columns: the tallest inner column full height, the others to
/// scale, a pile at either end held to full height rather than shrinking
/// the rest; a column spanning bins their mean.
#[test]
fn the_columns_scale_to_the_tallest_inner_one() {
    let mut pixels = vec![[0u8, 0, 0]; 1000];
    pixels.extend(vec![[100u8, 100, 100]; 40]);
    pixels.extend(vec![[200u8, 200, 200]; 20]);
    let histogram = Histogram::of(&image(&pixels)).unwrap();
    let heights = histogram.heights(BINS, 50);
    assert_eq!(heights.len(), BINS);
    assert_eq!(heights[100], [50, 50, 50]);
    assert_eq!(heights[200], [25, 25, 25]);
    // The black pile is held to the top, not 25 times it.
    assert_eq!(heights[0], [50, 50, 50]);
    assert_eq!(heights[50], [0, 0, 0]);
    // Four bins a column: 100's 40 over four, against 200's 20 over four.
    let quarter = histogram.heights(BINS / 4, 40);
    assert_eq!(quarter[25], [40, 40, 40]);
    assert_eq!(quarter[50], [20, 20, 20]);
    assert!(histogram.heights(0, 10).is_empty());
    // More columns than bins (a band at scale 2 and up): several columns
    // on black, and on white, are all ends, so a pile at either still
    // leaves the tallest mid-tone column full height.
    let mut piled = pixels.clone();
    piled.extend(vec![[255u8, 255, 255]; 1000]);
    let piled = Histogram::of(&image(&piled)).unwrap();
    for columns in [400usize, 512, 600, 800, 1024] {
        let heights = piled.heights(columns, 90);
        assert_eq!(heights.len(), columns);
        let at = |value: usize| {
            heights[(0..columns)
                .find(|&c| {
                    c * BINS / columns <= value
                        && value < ((c + 1) * BINS / columns).max(c * BINS / columns + 1)
                })
                .unwrap()]
        };
        assert_eq!(at(100), [90, 90, 90], "{columns}");
        assert_eq!(at(200), [45, 45, 45], "{columns}");
        assert_eq!(heights[0], [90, 90, 90], "{columns}");
        assert_eq!(heights[columns - 1], [90, 90, 90], "{columns}");
    }
    // A frame all at one end has no inner column to scale by: its ends
    // alone, to the top.
    let black = Histogram::of(&image(&[[0, 0, 0]; 4])).unwrap();
    let heights = black.heights(16, 8);
    assert_eq!(heights[0], [8, 8, 8]);
    assert!(heights[1..].iter().all(|column| *column == [0, 0, 0]));
}
