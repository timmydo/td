#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The TrueType reader against fonts this file encodes: the table
//! directory, metrics, both character-map formats and their preference,
//! every coordinate encoding of a simple glyph, composite transforms and
//! point matching, and each refusal with the item it names.

use td_ui::coverage::{Mask, Rasterizer};
use td_ui::sfnt::{Error, Font, Outline, Point, MAX_COMPONENTS, MAX_POINTS, MAX_TABLES};

mod fonts;

use fonts::{assemble, encode, offset, square, Builder, Component, Glyph, Segment};

fn points(outline: &Outline) -> Vec<(f32, f32, bool)> {
    outline.points().iter().map(|p| (p.x, p.y, p.on)).collect()
}

fn outline_of(font: &[u8], glyph: u16) -> Result<Outline, Error> {
    let font = Font::parse(font).unwrap();
    let mut outline = Outline::new();
    font.outline(glyph, &mut outline).map(|()| outline)
}

fn base() -> Builder {
    Builder::new(vec![
        Glyph::Empty,
        Glyph::Simple(vec![square(0, 0, 500)]),
        Glyph::Simple(vec![square(100, -100, 300), square(-400, 1000, 20)]),
        Glyph::Empty,
    ])
}

#[test]
fn metrics_and_both_character_map_formats() {
    let mut builder = base();
    builder.format4 = vec![
        Segment::Delta(0x41, 0x42, 1u16.wrapping_sub(0x41)),
        Segment::Array(0x61, vec![2, 0, 3, 9]),
        Segment::Delta(0x100, 0x100, 0u16.wrapping_sub(0x100)),
    ];
    let bytes = builder.font();
    let font = Font::parse(&bytes).unwrap();
    assert_eq!(font.units_per_em(), 1000);
    assert_eq!(
        (font.ascender(), font.descender(), font.line_gap()),
        (800, -200, 90)
    );
    assert_eq!(font.glyph_count(), 4);
    assert_eq!(font.glyph('A'), Some(1));
    assert_eq!(font.glyph('B'), Some(2));
    assert_eq!(font.glyph('C'), None, "past the segment");
    assert_eq!(font.glyph('a'), Some(2), "through the glyph array");
    assert_eq!(font.glyph('b'), None, "array zero is missing");
    assert_eq!(font.glyph('c'), Some(3));
    assert_eq!(font.glyph('d'), None, "past the face");
    assert_eq!(font.glyph('\u{100}'), None, "delta to glyph zero");
    assert_eq!(font.glyph('\u{1f600}'), None, "format 4 is the BMP");
    assert_eq!(font.glyph('\u{ffff}'), None);

    builder.format12 = Some(vec![(0x41, 0x41, 3), (0xf0001, 0xf0002, 2)]);
    let bytes = builder.font();
    let font = Font::parse(&bytes).unwrap();
    assert_eq!(font.glyph('A'), Some(3), "format 12 preferred");
    assert_eq!(font.glyph('B'), None, "format 4 no longer consulted");
    assert_eq!(font.glyph('\u{f0001}'), Some(2));
    assert_eq!(font.glyph('\u{f0002}'), Some(3));
    assert_eq!(font.glyph('\u{f0003}'), None);
    assert_eq!(font.glyph('\u{10ffff}'), None);
}

#[test]
fn advances_past_the_long_metrics_share_the_last() {
    let mut builder = base();
    builder.long_metrics = 2;
    let bytes = builder.font();
    let font = Font::parse(&bytes).unwrap();
    assert_eq!(
        (0..5).map(|g| font.advance(g)).collect::<Vec<_>>(),
        [Some(500), Some(501), Some(501), Some(501), None]
    );
}

#[test]
fn simple_glyphs_decode_every_coordinate_encoding() {
    let contour = vec![
        (0, 0, true),
        (0, 0, false),
        (255, -255, true),
        (256, 1000, false),
        (-2000, 1000, false),
        (-2000, 1000, true),
        (-2001, 999, true),
        (-2001, 999, true),
        (-2001, 999, true),
        (30000, -30000, true),
    ];
    for short_offsets in [false, true] {
        let mut builder = Builder::new(vec![
            Glyph::Empty,
            Glyph::Simple(vec![contour.clone(), square(5, 5, 1)]),
        ]);
        builder.short_offsets = short_offsets;
        let outline = outline_of(&builder.font(), 1).unwrap();
        let expected: Vec<(f32, f32, bool)> = contour
            .iter()
            .chain(&square(5, 5, 1))
            .map(|&(x, y, on)| (x as f32, y as f32, on))
            .collect();
        assert_eq!(points(&outline), expected);
        let lengths: Vec<usize> = outline.contours().map(<[Point]>::len).collect();
        assert_eq!(lengths, [10, 4]);
    }
}

#[test]
fn empty_glyphs_are_empty_outlines() {
    let mut builder = base();
    builder.glyphs[3] = Glyph::Raw(vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let bytes = builder.font();
    let font = Font::parse(&bytes).unwrap();
    let mut outline = Outline::new();
    font.outline(1, &mut outline).unwrap();
    assert!(!outline.is_empty());
    for glyph in [0, 3] {
        font.outline(glyph, &mut outline).unwrap();
        assert!(outline.is_empty(), "glyph {glyph}");
        assert_eq!(outline.contours().count(), 0);
    }
}

#[test]
fn composites_place_scale_and_match_their_components() {
    let f2 = |v: f32| (v * 16384.0) as i16;
    let mut builder = base();
    builder.glyphs.push(Glyph::Composite(vec![
        offset(1, 10, -20),
        Component {
            glyph: 1,
            flags: 0x0002 | 0x0008,
            args: (-5, 7),
            transform: vec![f2(0.5)],
        },
        Component {
            glyph: 1,
            flags: 0x0001 | 0x0002 | 0x0040 | 0x0800,
            args: (100, 100),
            transform: vec![f2(2.0 - 1.0 / 16384.0), f2(0.25)],
        },
        Component {
            glyph: 1,
            flags: 0x0001 | 0x0002 | 0x0080,
            args: (0, 0),
            transform: vec![0, f2(1.0 - 1.0 / 16384.0), f2(-1.0), 0],
        },
    ]));
    // A nested composite, and point matching: glyph 1's point 2 of the
    // second component lands on this glyph's point 0.
    builder.glyphs.push(Glyph::Composite(vec![
        offset(4, 1000, 0),
        Component {
            glyph: 1,
            flags: 0,
            args: (0, 2),
            transform: vec![],
        },
    ]));
    let bytes = builder.font();

    let outline = outline_of(&bytes, 4).unwrap();
    let got = points(&outline);
    assert_eq!(got.len(), 16);
    assert_eq!(outline.contours().count(), 4);
    let near = |(x, y, _): (f32, f32, bool), (ex, ey): (f32, f32)| {
        (x - ex).abs() < 0.2 && (y - ey).abs() < 0.2
    };
    // Offset only.
    assert!(near(got[2], (510.0, 480.0)));
    // Uniform half scale, offset unscaled.
    assert!(near(got[6], (245.0, 257.0)));
    // x and y scales, offset scaled by them.
    assert!(near(got[10], (1200.0, 150.0)));
    // A quarter turn: (x, y) -> (-y, x).
    assert!(near(got[14], (-500.0, 500.0)));
    assert!(got.iter().all(|p| p.2));

    let nested = points(&outline_of(&bytes, 5).unwrap());
    assert_eq!(nested.len(), 20);
    for (a, b) in nested.iter().zip(&got) {
        assert!(near(*a, (b.0 + 1000.0, b.1)));
    }
    assert!(near(nested[18], (1010.0, -20.0)), "{:?}", nested[18]);
}

#[test]
fn a_real_outline_rasterizes_through_the_font() {
    let bytes = base().font();
    let font = Font::parse(&bytes).unwrap();
    let mut outline = Outline::new();
    font.outline(2, &mut outline).unwrap();
    let mut mask = Mask::default();
    Rasterizer::new()
        .rasterize(&outline, 0.01, &mut mask)
        .unwrap();
    assert_eq!(
        (mask.width, mask.height, mask.left, mask.top),
        (8, 12, -4, 11)
    );
    assert_eq!(mask.get(5, 9), 255);
    assert_eq!(mask.get(4, 0), 0);
    assert_eq!(mask.get(4, 7), 0);
}

fn parse_error(bytes: &[u8]) -> Error {
    Font::parse(bytes).unwrap_err()
}

#[test]
fn the_directory_and_required_tables_are_checked() {
    let good = base().font();
    assert_eq!(parse_error(&good[..3]), Error::Truncated("table directory"));
    assert_eq!(
        parse_error(&good[..20]),
        Error::Truncated("table directory")
    );
    let mut version = good.clone();
    version[..4].copy_from_slice(b"OTTO");
    assert_eq!(parse_error(&version), Error::Unsupported("CFF outlines"));
    version[..4].copy_from_slice(b"ttcf");
    assert_eq!(parse_error(&version), Error::Unsupported("font collection"));
    version[..4].copy_from_slice(b"wOFF");
    assert_eq!(parse_error(&version), Error::Malformed("sfnt version"));
    version[..4].copy_from_slice(b"true");
    assert!(Font::parse(&version).is_ok());

    let mut record = good.clone();
    let length_at = 12 + 16 * 6 + 12;
    record[length_at..length_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(parse_error(&record), Error::Truncated("table record"));

    let many: Vec<([u8; 4], Vec<u8>)> = (0..=MAX_TABLES)
        .map(|i| {
            (
                [b'x', b'x', b'0' + (i / 10) as u8, b'0' + (i % 10) as u8],
                vec![],
            )
        })
        .collect();
    assert_eq!(
        parse_error(&assemble(0x0001_0000, &many)),
        Error::Limit("tables")
    );

    let mut builder = base();
    builder.extra = vec![(*b"head", vec![0; 54])];
    assert_eq!(
        parse_error(&builder.font()),
        Error::Malformed("duplicate table")
    );

    for (tag, name) in [
        (b"head", "head"),
        (b"maxp", "maxp"),
        (b"hhea", "hhea"),
        (b"hmtx", "hmtx"),
        (b"loca", "loca"),
        (b"cmap", "cmap"),
        (b"glyf", "glyf"),
    ] {
        let mut builder = base();
        builder.omit = vec![tag];
        assert_eq!(parse_error(&builder.font()), Error::Missing(name), "{name}");
    }
    let mut builder = base();
    builder.omit = vec![b"glyf"];
    builder.extra = vec![(*b"CFF ", vec![1, 0, 4, 4])];
    assert_eq!(
        parse_error(&builder.font()),
        Error::Unsupported("CFF outlines")
    );
}

#[test]
fn fixed_table_fields_are_checked() {
    let tamper = |tag: &[u8; 4], edit: &dyn Fn(&mut Vec<u8>)| {
        let builder = base();
        let mut tables = vec![];
        for name in [
            b"cmap", b"glyf", b"head", b"hhea", b"hmtx", b"loca", b"maxp",
        ] {
            let mut table = builder.table(name).unwrap();
            if name == tag {
                edit(&mut table);
            }
            tables.push((*name, table));
        }
        parse_error(&assemble(0x0001_0000, &tables))
    };
    assert_eq!(
        tamper(b"head", &|t| t.truncate(53)),
        Error::Truncated("head")
    );
    assert_eq!(
        tamper(b"head", &|t| t[12] = 0),
        Error::Malformed("head magic")
    );
    assert_eq!(
        tamper(b"head", &|t| t[18..20]
            .copy_from_slice(&15u16.to_be_bytes())),
        Error::Malformed("units per em")
    );
    assert_eq!(
        tamper(b"head", &|t| t[18..20]
            .copy_from_slice(&16385u16.to_be_bytes())),
        Error::Malformed("units per em")
    );
    assert_eq!(
        tamper(b"head", &|t| t[51] = 2),
        Error::Malformed("index to location format")
    );
    assert_eq!(
        tamper(b"maxp", &|t| t.truncate(5)),
        Error::Truncated("maxp")
    );
    assert_eq!(
        tamper(b"maxp", &|t| t[4..6].fill(0)),
        Error::Malformed("glyph count")
    );
    assert_eq!(
        tamper(b"hhea", &|t| t.truncate(35)),
        Error::Truncated("hhea")
    );
    assert_eq!(
        tamper(b"hhea", &|t| t[34..36].fill(0)),
        Error::Malformed("horizontal metric count")
    );
    assert_eq!(
        tamper(b"hhea", &|t| t[34..36].copy_from_slice(&5u16.to_be_bytes())),
        Error::Malformed("horizontal metric count")
    );
    assert_eq!(
        tamper(b"hmtx", &|t| {
            t.pop();
        }),
        Error::Truncated("hmtx")
    );
    assert_eq!(
        tamper(b"loca", &|t| {
            t.pop();
        }),
        Error::Truncated("loca")
    );
    assert_eq!(
        tamper(b"cmap", &|t| t.truncate(3)),
        Error::Truncated("cmap")
    );
    // The one subtable's platform is not Unicode.
    assert_eq!(
        tamper(b"cmap", &|t| t[4..6].copy_from_slice(&1u16.to_be_bytes())),
        Error::Missing("unicode cmap")
    );
    assert_eq!(
        tamper(b"cmap", &|t| t[12 + 6..12 + 8]
            .copy_from_slice(&3u16.to_be_bytes())),
        Error::Malformed("cmap format 4 segments")
    );
    assert_eq!(
        tamper(b"cmap", &|t| {
            let len = t.len();
            t[12 + 2..12 + 4].copy_from_slice(&((len - 12 + 1) as u16).to_be_bytes());
        }),
        Error::Truncated("cmap format 4")
    );

    let mut builder = base();
    builder.format12 = Some(vec![(0x41, 0x41, 3)]);
    // A format 12 whose groups run past it, beside the format 4 on the
    // platform given: a broken candidate is passed over, and the font is
    // refused with its error only when no Unicode subtable is usable.
    let broken = |format4_platform: u16| {
        let mut tables = vec![];
        for name in [
            b"cmap", b"glyf", b"head", b"hhea", b"hmtx", b"loca", b"maxp",
        ] {
            let mut table = builder.table(name).unwrap();
            if name == b"cmap" {
                let at = table.len() - 12 - 4;
                table[at..at + 4].copy_from_slice(&2u32.to_be_bytes());
                table[4..6].copy_from_slice(&format4_platform.to_be_bytes());
            }
            tables.push((*name, table));
        }
        assemble(0x0001_0000, &tables)
    };
    let bytes = broken(3);
    assert_eq!(Font::parse(&bytes).unwrap().glyph('A'), Some(1), "format 4");
    assert_eq!(parse_error(&broken(1)), Error::Truncated("cmap format 12"));
    // The one Unicode record's offset past the table.
    let mut tables = vec![];
    for name in [
        b"cmap", b"glyf", b"head", b"hhea", b"hmtx", b"loca", b"maxp",
    ] {
        let mut table = base().table(name).unwrap();
        if name == b"cmap" {
            table[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        }
        tables.push((*name, table));
    }
    assert_eq!(
        parse_error(&assemble(0x0001_0000, &tables)),
        Error::Truncated("cmap")
    );
}

#[test]
fn glyph_refusals_name_the_item_and_leave_the_outline_empty() {
    let refuse = |glyphs: Vec<Glyph>, glyph: u16| {
        let bytes = Builder::new(glyphs).font();
        let font = Font::parse(&bytes).unwrap();
        let mut outline = Outline::new();
        outline.push_contour(&[Point::default()]).unwrap();
        let error = font.outline(glyph, &mut outline).unwrap_err();
        assert!(outline.is_empty(), "{error}");
        error
    };
    let good = Glyph::Simple(vec![square(0, 0, 10)]);
    let raw = |bytes: &[u8]| Glyph::Raw(bytes.to_vec());

    assert_eq!(
        refuse(vec![good.clone()], 1),
        Error::Malformed("glyph index")
    );
    assert_eq!(refuse(vec![raw(&[0])], 0), Error::Truncated("glyph header"));
    assert_eq!(
        refuse(vec![raw(&[0, 0, 0, 0, 0, 0, 0, 0, 0])], 0),
        Error::Truncated("glyph header")
    );
    assert_eq!(
        refuse(vec![raw(&[0xff, 0xfe, 0, 0, 0, 0, 0, 0, 0, 0])], 0),
        Error::Malformed("contour count")
    );
    // Contour ends must rise.
    let mut header = vec![0, 2, 0, 0, 0, 0, 0, 0, 0, 0];
    header.extend_from_slice(&[0, 3, 0, 3]);
    assert_eq!(
        refuse(vec![raw(&header)], 0),
        Error::Malformed("contour ends")
    );
    // A flag run past the point count.
    let mut header = vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0];
    header.extend_from_slice(&[0x39, 5]);
    assert_eq!(
        refuse(vec![raw(&header)], 0),
        Error::Malformed("flag repeat")
    );
    // Coordinates cut short.
    let mut short = encode(&good);
    short.truncate(short.len() - 1);
    assert_eq!(
        refuse(vec![raw(&short)], 0),
        Error::Truncated("simple glyph")
    );
    // Instructions past the end.
    let mut header = vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    header.extend_from_slice(&[0xff, 0xff]);
    assert_eq!(
        refuse(vec![raw(&header)], 0),
        Error::Truncated("simple glyph")
    );
    // One contour over the point budget, and the budget itself admitted.
    let over = (MAX_POINTS - 1) as u16 + 1;
    let mut header = vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
    header.extend_from_slice(&over.to_be_bytes());
    assert_eq!(refuse(vec![raw(&header)], 0), Error::Limit("points"));
    let budget: Vec<(i32, i32, bool)> = (0..MAX_POINTS as i32)
        .map(|i| (i % 7, i % 5, true))
        .collect();
    let bytes = Builder::new(vec![Glyph::Simple(vec![budget])]).font();
    assert_eq!(outline_of(&bytes, 0).unwrap().points().len(), MAX_POINTS);

    // A composite that names itself runs into the depth bound; one that
    // fans out runs into the component budget first.
    assert_eq!(
        refuse(
            vec![good.clone(), Glyph::Composite(vec![offset(1, 0, 0)])],
            1
        ),
        Error::Limit("composite depth")
    );
    let fan = |glyph| Glyph::Composite((0..4).map(|_| offset(glyph, 0, 0)).collect());
    let empty_leaf = Glyph::Simple(vec![]);
    let mut glyphs = vec![empty_leaf];
    for level in 0..5 {
        glyphs.push(fan(level));
    }
    let visited = |levels: u32| (1..=levels).map(|l| 4usize.pow(l)).sum::<usize>();
    assert!(visited(3) <= MAX_COMPONENTS && visited(4) > MAX_COMPONENTS);
    let bytes = Builder::new(glyphs.clone()).font();
    assert!(outline_of(&bytes, 3).is_ok());
    assert_eq!(refuse(glyphs, 4), Error::Limit("components"));
    // Point matching must name an earlier point and one of the new ones.
    let matched = |anchor, moved| {
        Glyph::Composite(vec![
            offset(0, 0, 0),
            Component {
                glyph: 0,
                flags: 0,
                args: (anchor, moved),
                transform: vec![],
            },
        ])
    };
    assert_eq!(
        refuse(vec![good.clone(), matched(4, 0)], 1),
        Error::Malformed("matched point")
    );
    assert_eq!(
        refuse(vec![good.clone(), matched(0, 4)], 1),
        Error::Malformed("matched point")
    );
    assert_eq!(
        refuse(
            vec![good.clone(), raw(&[0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0])],
            1
        ),
        Error::Truncated("composite glyph")
    );
}

#[test]
fn loca_ranges_are_checked() {
    let mut builder = base();
    let mut tables = vec![];
    for name in [
        b"cmap", b"glyf", b"head", b"hhea", b"hmtx", b"loca", b"maxp",
    ] {
        let mut table = builder.table(name).unwrap();
        if name == b"loca" {
            // Glyph 1 ends before it starts; glyph 2 runs past glyf.
            let second = u32::from_be_bytes(table[8..12].try_into().unwrap());
            table[4..8].copy_from_slice(&(second + 2).to_be_bytes());
            table[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
        }
        tables.push((*name, table));
    }
    let bytes = assemble(0x0001_0000, &tables);
    let font = Font::parse(&bytes).unwrap();
    let mut outline = Outline::new();
    assert_eq!(font.outline(1, &mut outline), Err(Error::Malformed("loca")));
    assert_eq!(font.outline(2, &mut outline), Err(Error::Truncated("glyf")));
    builder.units_per_em = 2048;
    assert_eq!(Font::parse(&builder.font()).unwrap().units_per_em(), 2048);
}

#[test]
fn errors_name_their_kind_and_item() {
    assert_eq!(Error::Truncated("loca").to_string(), "sfnt: truncated loca");
    assert_eq!(Error::Limit("points").to_string(), "sfnt: limit points");
    assert_eq!(
        Error::Unsupported("CFF outlines").to_string(),
        "sfnt: unsupported CFF outlines"
    );
}

#[test]
fn the_component_budget_offsets_and_overlap_flags_are_exact() {
    let leaf = Glyph::Simple(vec![]);
    let many = |n: usize| Glyph::Composite((0..n).map(|_| offset(0, 0, 0)).collect());
    let bytes = Builder::new(vec![leaf.clone(), many(MAX_COMPONENTS)]).font();
    assert!(outline_of(&bytes, 1).is_ok());
    let bytes = Builder::new(vec![leaf, many(MAX_COMPONENTS + 1)]).font();
    assert_eq!(
        outline_of(&bytes, 1).unwrap_err(),
        Error::Limit("components")
    );

    // Both offset flags: the unscaled one wins.
    let f2 = |v: f32| (v * 16384.0) as i16;
    let square_glyph = Glyph::Simple(vec![square(0, 0, 500)]);
    let both = Glyph::Composite(vec![Component {
        glyph: 0,
        flags: 0x0001 | 0x0002 | 0x0008 | 0x0800 | 0x1000,
        args: (100, 100),
        transform: vec![f2(0.5)],
    }]);
    let bytes = Builder::new(vec![square_glyph.clone(), both]).font();
    let got = points(&outline_of(&bytes, 1).unwrap());
    assert_eq!((got[2].0, got[2].1), (350.0, 350.0));

    // OVERLAP_SIMPLE on a simple glyph's first flag (after its ten-byte
    // header, one contour end and the three instruction bytes), and
    // OVERLAP_COMPOUND on a component.
    let mut flagged = encode(&square_glyph);
    flagged[17] |= 0x40;
    // The same bit on any later flag is not the glyph's: the square's four
    // flags are one run, so split it and mark the second.
    let mut later = encode(&Glyph::Simple(vec![vec![
        (0, 0, true),
        (0, 500, false),
        (500, 500, true),
        (500, 0, true),
    ]]));
    assert_eq!(later[17] & 0x08, 0, "the first flag is not a run");
    later[18] |= 0x40;
    let compound = Glyph::Composite(vec![Component {
        flags: 0x0001 | 0x0002 | 0x0400,
        ..offset(0, 0, 0)
    }]);
    let bytes = Builder::new(vec![
        square_glyph,
        Glyph::Raw(flagged),
        compound,
        Glyph::Composite(vec![offset(0, 0, 0)]),
        Glyph::Raw(later),
    ])
    .font();
    let font = Font::parse(&bytes).unwrap();
    let mut outline = Outline::new();
    for (glyph, overlap) in [
        (0, false),
        (1, true),
        (0, false),
        (2, true),
        (3, false),
        (4, false),
    ] {
        font.outline(glyph, &mut outline).unwrap();
        assert_eq!(outline.overlap(), overlap, "glyph {glyph}");
        assert_eq!(outline.points().len(), 4);
    }
}
