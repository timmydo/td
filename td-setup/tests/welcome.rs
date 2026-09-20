#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! Draw-stream and pixel oracles for the installer's welcome page: what it
//! fills, at which cells its glyphs sit and in which colour and weight, and
//! the same page rasterized whole to confirm the pixels. The page draws no
//! live compositor; these read the composition directly, exactly as the
//! chrome bands are tested.

use std::collections::BTreeMap;

use td_setup::welcome::{self, Welcome, BODY, FOOTER, HEADING};
use td_ui::raster::{
    Composition, Draw, Primitive, Rect, Scale, Surface, Weight, BORDER, CHROME, INK,
};

fn surface(width: usize, height: usize, scale: u8) -> Surface {
    Surface::new(width, height, Scale::new(scale).unwrap()).unwrap()
}

/// The draws the page emits over `damage`.
fn draws_over(surface: Surface, damage: Rect) -> Vec<Draw> {
    let page = Welcome::new(surface).unwrap();
    let mut out = Vec::new();
    page.emit(damage, &mut |draw| out.push(draw));
    out
}

/// The draws over the whole surface.
fn draws(surface: Surface) -> Vec<Draw> {
    draws_over(surface, surface.bounds())
}

fn fills(draws: &[Draw]) -> Vec<(Rect, u32)> {
    draws
        .iter()
        .filter_map(|d| match d.primitive {
            Primitive::Fill { rect, color } => Some((rect, color)),
            Primitive::Glyph { .. } | Primitive::Mark { .. } => None,
        })
        .collect()
}

/// Each glyph as (x, y, scalar, ink, weight).
fn glyphs(draws: &[Draw]) -> Vec<(i64, i64, char, u32, Weight)> {
    draws
        .iter()
        .filter_map(|d| match d.primitive {
            Primitive::Glyph {
                x,
                y,
                scalar,
                style,
            } => Some((x, y, scalar, style.ink, style.weight)),
            Primitive::Fill { .. } | Primitive::Mark { .. } => None,
        })
        .collect()
}

/// The glyphs grouped into rows by their y, each row read left to right.
/// Spaces are painted glyphs, so a row reads as its literal text.
fn rows(draws: &[Draw]) -> BTreeMap<i64, String> {
    let mut cells: BTreeMap<i64, Vec<(i64, char)>> = BTreeMap::new();
    for (x, y, scalar, _, _) in glyphs(draws) {
        cells.entry(y).or_default().push((x, scalar));
    }
    cells
        .into_iter()
        .map(|(y, mut row)| {
            row.sort_by_key(|&(x, _)| x);
            (y, row.into_iter().map(|(_, c)| c).collect())
        })
        .collect()
}

/// The rendered disclosure prose: the body rows (below the rule, above the
/// footer) joined with spaces, so a phrase split across a wrap reads whole.
fn rendered_body(draws: &[Draw], height: i64) -> String {
    rows(draws)
        .into_iter()
        .filter(|&(y, _)| (64..height - 24).contains(&y))
        .map(|(_, line)| line)
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn wrap_keeps_words_whole_within_the_columns_and_separates_paragraphs() {
    let lines = welcome::wrap(&["alpha beta gamma", "delta"], 11);
    // Each line fits the column budget and no word is split.
    assert!(lines.iter().all(|line| line.chars().count() <= 11));
    assert_eq!(lines, ["alpha beta", "gamma", "", "delta"]);
    // A word longer than the budget is hard-broken rather than overrun.
    let broken = welcome::wrap(&["supercalifragilistic"], 8);
    assert!(broken.iter().all(|line| line.chars().count() <= 8));
    assert_eq!(broken.concat(), "supercalifragilistic");
    // A long word mid-paragraph breaks across lines with its characters
    // intact, and the following word continues after it.
    let mixed = welcome::wrap(&["hi enormouslylongword bye"], 8);
    assert!(mixed.iter().all(|line| line.chars().count() <= 8));
    assert_eq!(mixed.concat().replace(' ', ""), "hienormouslylongwordbye");
    assert!(mixed.iter().any(|line| line.contains("bye")));
    // The empty column budget is clamped, not a divide-by-zero.
    assert!(!welcome::wrap(&["x"], 0).is_empty());
}

#[test]
fn the_welcome_screen_discloses_unencrypted_storage_and_auto_login_and_no_pin() {
    // INSTALLER.md requires the welcome screen to disclose that storage is
    // unencrypted and the account signs in automatically, and forbids a PIN
    // field here. Pin the whole phrases in the content constant.
    let prose = BODY.join(" ");
    for phrase in [
        "not encrypted",
        "signs in automatically",
        "no password or PIN",
        "erased",
    ] {
        assert!(prose.contains(phrase), "content discloses {phrase:?}");
    }
    // And pin the same whole phrases in the rendered glyphs, so dropping a
    // word like "not" in rendering would fail even though the isolated
    // words survive.
    let draws = draws(surface(800, 600, 1));
    let rendered = rendered_body(&draws, 600);
    for phrase in [
        "not encrypted",
        "signs in automatically",
        "no password or PIN",
    ] {
        assert!(rendered.contains(phrase), "render discloses {phrase:?}");
    }
    // The page constructs no text entry: every fill is the chrome ground or
    // the border rule, never a text entry's paper ground or its ink caret,
    // and no masking glyph is drawn. There is no field, masked or not, to
    // spoof.
    assert!(
        fills(&draws)
            .iter()
            .all(|&(_, color)| color == CHROME || color == BORDER),
        "only chrome and border fills, no field ground or caret"
    );
    assert!(
        glyphs(&draws)
            .iter()
            .all(|&(_, _, c, _, _)| c != '\u{2022}'),
        "no masked field glyph"
    );
}

#[test]
fn the_page_fills_the_whole_surface_chrome_before_anything_else() {
    let surface = surface(800, 600, 1);
    let draws = draws(surface);
    // The very first draw is the chrome ground over the whole surface, so
    // no pixel is left unpainted and nothing paints under it.
    assert_eq!(
        draws.first().map(|d| d.primitive),
        Some(Primitive::Fill {
            rect: surface.bounds(),
            color: CHROME,
        })
    );
}

#[test]
fn the_heading_sits_at_the_top_inset_over_a_border_rule() {
    for scale in 1..=4u8 {
        let s = scale as i64;
        let surface = surface(800 * scale as usize, 600 * scale as usize, scale);
        let draws = draws(surface);
        // The heading is one cell in and one row down, in medium ink.
        assert_eq!(
            rows(&draws).get(&(24 * s)).map(String::as_str),
            Some(HEADING)
        );
        let heading: Vec<_> = glyphs(&draws)
            .into_iter()
            .filter(|&(_, y, _, _, _)| y == 24 * s)
            .collect();
        assert_eq!(
            heading.first().map(|&(x, ..)| x),
            Some(8 * s),
            "heading inset"
        );
        assert!(heading
            .iter()
            .all(|&(_, _, _, ink, weight)| ink == INK && weight == Weight::Medium));
        // A one-pixel border rule sits a row below the heading, inset both
        // sides.
        let width = (800 * scale as usize).saturating_sub(2 * 8 * scale as usize) as u32;
        assert!(fills(&draws).contains(&(
            Rect {
                x: 8 * s,
                y: 48 * s,
                width,
                height: scale as u32,
            },
            BORDER,
        )));
    }
}

#[test]
fn the_disclosure_prose_renders_below_the_rule() {
    let surface = surface(800, 600, 1);
    let draws = draws(surface);
    let rows = rows(&draws);
    // The body rows (below the rule at y = 48, above the footer) are exactly
    // the non-blank lines of the crate's own wrap of BODY at this width, in
    // order: the whole prose renders, unmodified. Block columns at width 800
    // scale 1 are (800 - 16) / 8 = 98.
    let rendered: Vec<String> = rows
        .iter()
        .filter(|(&y, _)| (64..576).contains(&y))
        .map(|(_, line)| line.clone())
        .collect();
    let expected: Vec<String> = welcome::wrap(&BODY, 98)
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(rendered, expected, "the whole prose renders, unmodified");
    // The first body row begins the first paragraph.
    assert!(rows
        .get(&72)
        .is_some_and(|line| line.starts_with("This installs td")));
}

#[test]
fn the_footer_names_the_step_on_the_bottom_row() {
    let surface = surface(800, 600, 1);
    let draws = draws(surface);
    // The status footer's line sits on the bottom row (height - ROW, then
    // the status inset of four pixels down).
    let rows = rows(&draws);
    assert_eq!(rows.get(&(600 - 24 + 4)).map(String::as_str), Some(FOOTER));
    // Its band carries a border over chrome, the shared status frame.
    let footer = Rect {
        x: 0,
        y: 600 - 24,
        width: 800,
        height: 24,
    };
    let fills = fills(&draws);
    assert!(fills.contains(&(footer, CHROME)));
    assert!(fills.contains(&(
        Rect {
            height: 1,
            ..footer
        },
        BORDER,
    )));
}

#[test]
fn new_refuses_a_surface_too_small_and_accepts_at_the_exact_boundary() {
    // The binding narrow-end limit is the block's row budget: at scale 1,
    // 39 columns (width 328) wrap the prose to exactly the block's rows and
    // end exactly where the footer begins (height 336).
    assert!(Welcome::new(surface(328, 336, 1)).is_some());
    // One column narrower wraps a row past the budget.
    assert!(Welcome::new(surface(327, 336, 1)).is_none());
    // One pixel shorter overlaps the footer.
    assert!(Welcome::new(surface(328, 335, 1)).is_none());
    // Grossly too narrow is refused.
    assert!(Welcome::new(surface(200, 600, 1)).is_none());
    // The height boundary at width 800: the prose is 7 rows, ending at
    // 72 + 7 * 16 = 184, so the footer needs the row from height - 24, i.e.
    // height 208 accepts and 207 overlaps.
    assert!(Welcome::new(surface(800, 208, 1)).is_some());
    assert!(Welcome::new(surface(800, 207, 1)).is_none());
    // Degenerate: narrower than the two insets, refused without underflow.
    assert!(Welcome::new(surface(8, 600, 1)).is_none());
    // An invalid literal surface (axis past the ceiling) is refused, not
    // rendered with truncated geometry.
    assert!(Welcome::new(Surface {
        width: u32::MAX as usize + 16,
        height: 600,
        scale: Scale::new(1).unwrap(),
    })
    .is_none());
    // A generous surface is accepted at every scale.
    for scale in 1..=4u8 {
        assert!(Welcome::new(surface(800 * scale as usize, 600 * scale as usize, scale)).is_some());
    }
}

#[test]
fn a_partial_damage_clips_every_draw_to_the_damaged_region() {
    let surface = surface(800, 600, 1);
    let damage = Rect {
        x: 100,
        y: 40,
        width: 200,
        height: 80,
    };
    let draws = draws_over(surface, damage);
    // Every draw stays inside the damage, and something inside it is painted.
    assert!(!draws.is_empty());
    for draw in &draws {
        assert!(
            draw.clip.intersection(damage) == Some(draw.clip),
            "draw clipped to damage"
        );
    }
}

#[test]
fn the_page_rasterizes_to_the_expected_grounds_and_ink() {
    let (width, height) = (800usize, 600usize);
    // Exercise the exported preview path, not a re-implemented raster.
    let pixels = td_setup::preview(width, height, 1).unwrap();
    let pixel = |x: usize, y: usize| -> u32 {
        let base = (y * width + x) * 4;
        u32::from_le_bytes([
            pixels[base],
            pixels[base + 1],
            pixels[base + 2],
            pixels[base + 3],
        ]) & 0xff_ffff
    };
    // The corners are the chrome ground.
    assert_eq!(pixel(0, 0), CHROME);
    assert_eq!(pixel(width - 1, 0), CHROME);
    // The heading paints ink somewhere on its row.
    assert!(
        (24..40).any(|y| (8..200).any(|x| pixel(x, y) == INK)),
        "heading ink"
    );
    // The rule row is the border colour at the inset.
    assert_eq!(pixel(8, 48), BORDER);
    // The disclosure prose paints ink below the rule.
    assert!(
        (72..560).any(|y| (8..400).any(|x| pixel(x, y) == INK)),
        "body ink"
    );
    // The footer band opens with the border over chrome.
    assert_eq!(pixel(8, height - 24), BORDER);
    assert!(
        (height - 20..height - 4).any(|y| (8..300).any(|x| pixel(x, y) == INK)),
        "footer ink"
    );
}

#[test]
fn preview_renders_a_tight_buffer_and_refuses_a_surface_too_small() {
    // The exported preview buffer is tight XRGB for the requested extent.
    let pixels = td_setup::preview(800, 600, 1).unwrap();
    assert_eq!(pixels.len(), 800 * 600 * 4);
    // A surface too small for the page is an error, not a truncated buffer.
    assert!(td_setup::preview(200, 600, 1).is_err());
    assert!(td_setup::preview(0, 600, 1).is_err());
    // An out-of-range scale is refused.
    assert!(td_setup::preview(800, 600, 5).is_err());
}

#[test]
fn preview_ppm_has_a_valid_p6_header_and_body_length() {
    let ppm = td_setup::preview_ppm(800, 600, 1).unwrap();
    let header = b"P6\n800 600\n255\n";
    assert!(ppm.starts_with(header), "PPM P6 header");
    // Header plus three bytes per pixel, no padding.
    assert_eq!(ppm.len(), header.len() + 800 * 600 * 3);
    // The first pixel is the chrome ground: RGB of CHROME (0xe1dbcf).
    assert_eq!(&ppm[header.len()..header.len() + 3], &[0xe1, 0xdb, 0xcf]);
}
