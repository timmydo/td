//! A `srcset`'s candidates and the one chosen (HTML, "parse a srcset
//! attribute", "update the source set", "select an image source").
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use super::super::*;
    use crate::tree::Source;
    use td_css::conditions::Environment;

    fn tree(markup: &str) -> Tree {
        let page = Budget::new(64 << 20).unwrap();
        Tree::parse(markup, Source::Markup, &page).unwrap()
    }

    fn env(width: f64, dppx: f64) -> Environment {
        Environment {
            width,
            height: 800.0,
            resolution_dppx: dppx,
            ..Environment::default()
        }
    }

    fn memory() -> Budget {
        Budget::new(1 << 20).unwrap()
    }

    /// The first `img` of `markup`.
    fn first_img(tree: &Tree) -> NodeId {
        let document = tree.document();
        document
            .nodes()
            .find(|(_, node)| node.element().is_some_and(|e| e.name.as_str() == "img"))
            .map(|(id, _)| id)
            .unwrap()
    }

    /// The URL chosen for the first `img` of `markup` and its density.
    fn chosen(markup: &str, env: &Environment) -> Option<(String, f64)> {
        let tree = tree(markup);
        let img = first_img(&tree);
        let chosen = select(&tree, img, env, &memory(), |_| true)?;
        Some((tree.candidate(chosen).unwrap().to_string(), chosen.density))
    }

    fn url(markup: &str, env: &Environment) -> Option<String> {
        chosen(markup, env).map(|(url, _)| url)
    }

    /// WPT's html/semantics/embedded-content/the-img-element/srcset/
    /// parse-a-srcset-attribute.html, each `srcset` with the image
    /// chosen at 1 dppx, no `src`: written as there, character references
    /// and all, the expected URL read from an `alt` the same way; empty
    /// for none.
    const WPT: &[(&str, &str)] = &[
        ("", ""),
        (",", ""),
        (",,,", ""),
        ("  data:,a  1x  ", "data:,a"),
        ("&#x9;&#x9;data:,a&#x9;&#x9;1x&#x9;&#x9;", "data:,a"),
        ("&#xA;&#xA;data:,a&#xA;&#xA;1x&#xA;&#xA;", "data:,a"),
        (
            "&#xB;&#xB;data:,a&#xB;&#xB;1x&#xB;&#xB;",
            "&#xB;&#xB;data:,a&#xB;&#xB;1x&#xB;&#xB;",
        ),
        ("&#xC;&#xC;data:,a&#xC;&#xC;1x&#xC;&#xC;", "data:,a"),
        ("&#xD;&#xD;data:,a&#xD;&#xD;1x&#xD;&#xD;", "data:,a"),
        (
            "&#xE;&#xE;data:,a&#xE;&#xE;1x&#xE;&#xE;",
            "&#xE;&#xE;data:,a&#xE;&#xE;1x&#xE;&#xE;",
        ),
        (
            "&#xF;&#xF;data:,a&#xF;&#xF;1x&#xF;&#xF;",
            "&#xF;&#xF;data:,a&#xF;&#xF;1x&#xF;&#xF;",
        ),
        (
            "&#x10;&#x10;data:,a&#x10;&#x10;1x&#x10;&#x10;",
            "&#x10;&#x10;data:,a&#x10;&#x10;1x&#x10;&#x10;",
        ),
        ("data:,a", "data:,a"),
        ("data:,a ", "data:,a"),
        ("data:,a ,", "data:,a"),
        ("data:,a,", "data:,a"),
        ("data:,a, ", "data:,a"),
        ("data:,a,,,", "data:,a"),
        ("data:,a,, , ", "data:,a"),
        (" data:,a", "data:,a"),
        (",,,data:,a", "data:,a"),
        (" , ,,data:,a", "data:,a"),
        ("&nbsp;data:,a", "&nbsp;data:,a"),
        ("data:,a&nbsp;", "data:,a&nbsp;"),
        ("data:,a 1x", "data:,a"),
        ("data:,a 1x ", "data:,a"),
        ("data:,a 1x,", "data:,a"),
        ("data:,a ( , data:,b 1x, ), data:,c", "data:,c"),
        ("data:,a ((( , data:,b 1x, ), data:,c", "data:,c"),
        ("data:,a [ , data:,b 1x, ], data:,c", "data:,b"),
        ("data:,a { , data:,b 1x, }, data:,c", "data:,b"),
        ("data:,a \" , data:,b 1x, \", data:,c", "data:,b"),
        ("data:,a \\,data:;\\,b, data:,c", "data:;\\,b"),
        ("data:,a, data:,b (", "data:,a"),
        ("data:,a, data:,b (  ", "data:,a"),
        ("data:,a, data:,b (,", "data:,a"),
        ("data:,a, data:,b (x", "data:,a"),
        ("data:,a, data:,b ()", "data:,a"),
        ("data:,a (, data:,b", ""),
        ("data:,a /*, data:,b, data:,c */", "data:,b"),
        ("data:,a //, data:,b", "data:,b"),
        ("data:,a foo", ""),
        ("data:,a foo foo", ""),
        ("data:,a foo 1x", ""),
        ("data:,a foo 1x foo", ""),
        ("data:,a foo 1w", ""),
        ("data:,a foo 1w foo", ""),
        ("data:,a 1x 1x", ""),
        ("data:,a 1w 1w", ""),
        ("data:,a 1w 1x", ""),
        ("data:,a 1x 1w", ""),
        ("data:,a 1w 1h", "data:,a"),
        ("data:,a 1h 1w", "data:,a"),
        ("data:,a 1h 1h", ""),
        ("data:,a 1h 1x", ""),
        ("data:,a 1h 1w 1x", ""),
        ("data:,a 1x 1w 1h", ""),
        ("data:,a 1w", "data:,a"),
        ("data:,a 1h", ""),
        ("data:,a 1h foo", ""),
        ("data:,a foo 1h", ""),
        ("data:,a 0w", ""),
        ("data:,a -1w", ""),
        ("data:,a 1w -1w", ""),
        ("data:,a 1.0w", ""),
        ("data:,a 1w 1.0w", ""),
        ("data:,a 1e0w", ""),
        ("data:,a 1w 1e0w", ""),
        ("data:,a 1www", ""),
        ("data:,a 1w 1www", ""),
        ("data:,a +1w", ""),
        ("data:,a 1w +1w", ""),
        ("data:,a 1W", ""),
        ("data:,a 1w 1W", ""),
        ("data:,a Infinityw", ""),
        ("data:,a 1w Infinityw", ""),
        ("data:,a NaNw", ""),
        ("data:,a 1w NaNw", ""),
        ("data:,a 0x1w", ""),
        ("data:,a 0X1w", ""),
        ("data:,a 1&#x1;w", ""),
        ("data:,a 1&nbsp;w", ""),
        ("data:,a 1&#x1680;w", ""),
        ("data:,a 1&#x2000;w", ""),
        ("data:,a 1&#x2001;w", ""),
        ("data:,a 1&#x2002;w", ""),
        ("data:,a 1&#x2003;w", ""),
        ("data:,a 1&#x2004;w", ""),
        ("data:,a 1&#x2005;w", ""),
        ("data:,a 1&#x2006;w", ""),
        ("data:,a 1&#x2007;w", ""),
        ("data:,a 1&#x2008;w", ""),
        ("data:,a 1&#x2009;w", ""),
        ("data:,a 1&#x200A;w", ""),
        ("data:,a 1&#x200C;w", ""),
        ("data:,a 1&#x200D;w", ""),
        ("data:,a 1&#x202F;w", ""),
        ("data:,a 1&#x205F;w", ""),
        ("data:,a 1&#x3000;w", ""),
        ("data:,a 1&#xFEFF;w", ""),
        ("data:,a &#x1;1w", ""),
        ("data:,a &nbsp;1w", ""),
        ("data:,a &#x1680;1w", ""),
        ("data:,a &#x2000;1w", ""),
        ("data:,a &#x2001;1w", ""),
        ("data:,a &#x2002;1w", ""),
        ("data:,a &#x2003;1w", ""),
        ("data:,a &#x2004;1w", ""),
        ("data:,a &#x2005;1w", ""),
        ("data:,a &#x2006;1w", ""),
        ("data:,a &#x2007;1w", ""),
        ("data:,a &#x2008;1w", ""),
        ("data:,a &#x2009;1w", ""),
        ("data:,a &#x200A;1w", ""),
        ("data:,a &#x200C;1w", ""),
        ("data:,a &#x200D;1w", ""),
        ("data:,a &#x202F;1w", ""),
        ("data:,a &#x205F;1w", ""),
        ("data:,a &#x3000;1w", ""),
        ("data:,a &#xFEFF;1w", ""),
        ("data:,a 0x", "data:,a"),
        ("data:,a -0x", "data:,a"),
        ("data:,a 1x -0x", ""),
        ("data:,a -1x", ""),
        ("data:,a 1x -1x", ""),
        ("data:,a 1e0x", "data:,a"),
        ("data:,a 1E0x", "data:,a"),
        ("data:,a 1e-1x", "data:,a"),
        ("data:,a 1.5e1x", "data:,a"),
        ("data:,a -x", ""),
        ("data:,a .x", ""),
        ("data:,a -.x", ""),
        ("data:,a 1.x", ""),
        ("data:,a .5x", "data:,a"),
        ("data:,a .5e1x", "data:,a"),
        ("data:,a 1x 1.5e1x", ""),
        ("data:,a 1x 1e1.5x", ""),
        ("data:,a 1.0x", "data:,a"),
        ("data:,a 1x 1.0x", ""),
        ("data:,a +1x", ""),
        ("data:,a 1X", ""),
        ("data:,a Infinityx", ""),
        ("data:,a NaNx", ""),
        ("data:,a 0x1x", ""),
        ("data:,a 0X1x", ""),
        ("data:,a 1&#x1;x", ""),
        ("data:,a 1&nbsp;x", ""),
        ("data:,a 1&#x1680;x", ""),
        ("data:,a 1&#x2000;x", ""),
        ("data:,a 1&#x2001;x", ""),
        ("data:,a 1&#x2002;x", ""),
        ("data:,a 1&#x2003;x", ""),
        ("data:,a 1&#x2004;x", ""),
        ("data:,a 1&#x2005;x", ""),
        ("data:,a 1&#x2006;x", ""),
        ("data:,a 1&#x2007;x", ""),
        ("data:,a 1&#x2008;x", ""),
        ("data:,a 1&#x2009;x", ""),
        ("data:,a 1&#x200A;x", ""),
        ("data:,a 1&#x200C;x", ""),
        ("data:,a 1&#x200D;x", ""),
        ("data:,a 1&#x202F;x", ""),
        ("data:,a 1&#x205F;x", ""),
        ("data:,a 1&#x3000;x", ""),
        ("data:,a 1&#xFEFF;x", ""),
        ("data:,a &#x1;1x", ""),
        ("data:,a &nbsp;1x", ""),
        ("data:,a &#x1680;1x", ""),
        ("data:,a &#x2000;1x", ""),
        ("data:,a &#x2001;1x", ""),
        ("data:,a &#x2002;1x", ""),
        ("data:,a &#x2003;1x", ""),
        ("data:,a &#x2004;1x", ""),
        ("data:,a &#x2005;1x", ""),
        ("data:,a &#x2006;1x", ""),
        ("data:,a &#x2007;1x", ""),
        ("data:,a &#x2008;1x", ""),
        ("data:,a &#x2009;1x", ""),
        ("data:,a &#x200A;1x", ""),
        ("data:,a &#x200C;1x", ""),
        ("data:,a &#x200D;1x", ""),
        ("data:,a &#x202F;1x", ""),
        ("data:,a &#x205F;1x", ""),
        ("data:,a &#x3000;1x", ""),
        ("data:,a &#xFEFF;1x", ""),
        ("data:,a 1w 0h", ""),
        ("data:,a 1w -1h", ""),
        ("data:,a 1w 1.0h", ""),
        ("data:,a 1w 1e0h", ""),
        ("data:,a 1w 1hhh", ""),
        ("data:,a 1w +1h", ""),
        ("data:,a 1w 1H", ""),
        ("data:,a 1w Infinityh", ""),
        ("data:,a 1w NaNh", ""),
        ("data:,a 0x1h", ""),
        ("data:,a 0X1h", ""),
        ("data:,a 1w 1&#x1;h", ""),
        ("data:,a 1w 1&nbsp;h", ""),
        ("data:,a 1w 1&#x1680;h", ""),
        ("data:,a 1w 1&#x2000;h", ""),
        ("data:,a 1w 1&#x2001;h", ""),
        ("data:,a 1w 1&#x2002;h", ""),
        ("data:,a 1w 1&#x2003;h", ""),
        ("data:,a 1w 1&#x2004;h", ""),
        ("data:,a 1w 1&#x2005;h", ""),
        ("data:,a 1w 1&#x2006;h", ""),
        ("data:,a 1w 1&#x2007;h", ""),
        ("data:,a 1w 1&#x2008;h", ""),
        ("data:,a 1w 1&#x2009;h", ""),
        ("data:,a 1w 1&#x200A;h", ""),
        ("data:,a 1w 1&#x200C;h", ""),
        ("data:,a 1w 1&#x200D;h", ""),
        ("data:,a 1w 1&#x202F;h", ""),
        ("data:,a 1w 1&#x205F;h", ""),
        ("data:,a 1w 1&#x3000;h", ""),
        ("data:,a 1w 1&#xFEFF;h", ""),
        ("data:,a 1w &#x1;1h", ""),
        ("data:,a 1w &nbsp;1h", ""),
        ("data:,a 1w &#x1680;1h", ""),
        ("data:,a 1w &#x2000;1h", ""),
        ("data:,a 1w &#x2001;1h", ""),
        ("data:,a 1w &#x2002;1h", ""),
        ("data:,a 1w &#x2003;1h", ""),
        ("data:,a 1w &#x2004;1h", ""),
        ("data:,a 1w &#x2005;1h", ""),
        ("data:,a 1w &#x2006;1h", ""),
        ("data:,a 1w &#x2007;1h", ""),
        ("data:,a 1w &#x2008;1h", ""),
        ("data:,a 1w &#x2009;1h", ""),
        ("data:,a 1w &#x200A;1h", ""),
        ("data:,a 1w &#x200C;1h", ""),
        ("data:,a 1w &#x200D;1h", ""),
        ("data:,a 1w &#x202F;1h", ""),
        ("data:,a 1w &#x205F;1h", ""),
        ("data:,a 1w &#x3000;1h", ""),
        ("data:,a 1w &#xFEFF;1h", ""),
    ];

    #[test]
    fn wpt_parse_a_srcset_attribute() {
        let at = env(1280.0, 1.0);
        for &(srcset, expected) in WPT {
            let markup = format!("<img srcset='{srcset}' alt='{expected}'>");
            let tree = tree(&markup);
            let img = first_img(&tree);
            let want = tree.alt(img).unwrap();
            let got = select(&tree, img, &at, &memory(), |_| true)
                .map(|chosen| tree.candidate(chosen).unwrap().to_string());
            let want = (!want.is_empty()).then(|| want.to_string());
            assert_eq!(got, want, "srcset {srcset:?}");
        }
    }

    /// Each candidate as the parser gives it: its URL, where it is, and
    /// its descriptor.
    #[test]
    fn candidates_are_read_in_order_with_their_descriptors() {
        // `b,` ends at its comma; `e`'s parenthesised descriptor drops it.
        let all: Vec<_> =
            Candidates::new(" a 1x,b, c 2.5x, d 100w 50h,e (x, y) 3x, a,b 1x,f -0x").collect();
        let want = [
            ("a", 1, Descriptor::Density(1.0)),
            ("b", 6, Descriptor::Density(1.0)),
            ("c", 9, Descriptor::Density(2.5)),
            ("d", 17, Descriptor::Width(100)),
            ("a,b", 41, Descriptor::Density(1.0)),
            ("f", 48, Descriptor::Density(0.0)),
        ];
        assert_eq!(all.len(), want.len(), "{all:?}");
        for (got, (url, at, descriptor)) in all.iter().zip(want) {
            assert_eq!((got.url, got.at, got.descriptor), (url, at, descriptor));
        }
        // -0x is a density of +0.
        assert!(matches!(all[5].descriptor, Descriptor::Density(d) if d.is_sign_positive()));
        // Past u32, a width, and past MAX_DENSITY, a density, drop theirs.
        assert_eq!(Candidates::new("a 4294967296w, b 4294967295w").count(), 1);
        assert_eq!(Candidates::new("a 1000.5x, b 1000x, c 1e999x").count(), 1);
        // A data: URL keeps its commas; a non-ASCII one splits whole.
        let data: Vec<_> = Candidates::new("data:,a,b 2x, \u{e9}t\u{e9}.png 3x").collect();
        assert_eq!(data[0].url, "data:,a,b");
        assert_eq!(data[1].url, "\u{e9}t\u{e9}.png");
    }

    /// HTML's density example, and the Marie Lloyd one: the least
    /// density at least the screen's, else the greatest, `src` at 1x.
    #[test]
    fn x_candidates_and_src_choose_by_the_screens_density() {
        let lloyd = "<img src=/uploads/100-marie-lloyd.jpg \
                     srcset='/uploads/150-marie-lloyd.jpg 1.5x, /uploads/200-marie-lloyd.jpg 2x'>";
        for (dppx, want, density) in [
            (1.0, "/uploads/100-marie-lloyd.jpg", 1.0),
            (2.0, "/uploads/200-marie-lloyd.jpg", 2.0),
            (3.0, "/uploads/200-marie-lloyd.jpg", 2.0),
        ] {
            assert_eq!(
                chosen(lloyd, &env(1280.0, dppx)),
                Some((want.to_string(), density)),
                "at {dppx}"
            );
        }
        let image = "<img src=image-1x.png \
                     srcset='image-2x.png 2x, image-3x.png 3x, image-4x.png 4x'>";
        for (dppx, want) in [
            (1.0, "image-1x.png"),
            (2.0, "image-2x.png"),
            (3.0, "image-3x.png"),
            (4.0, "image-4x.png"),
        ] {
            assert_eq!(url(image, &env(800.0, dppx)).as_deref(), Some(want));
        }
        // A 1x candidate keeps `src` out of the source set; the first of
        // equal densities is chosen.
        let one = "<img src=s.png srcset='a.png 1x, b.png 1x, c.png 2x'>";
        assert_eq!(url(one, &env(800.0, 1.0)).as_deref(), Some("a.png"));
        // With no `srcset`, or a blank one, `src`; with neither, none.
        assert_eq!(
            chosen("<img src=' s.png '>", &env(800.0, 2.0)),
            Some(("s.png".to_string(), 1.0))
        );
        assert_eq!(
            url("<img src=s.png srcset=' '>", &env(800.0, 2.0)).as_deref(),
            Some("s.png")
        );
        assert_eq!(chosen("<img srcset=','>", &env(800.0, 1.0)), None);
        assert_eq!(chosen("<img alt=x>", &env(800.0, 1.0)), None);
    }

    /// HTML's swing example: `w` candidates' densities are their widths
    /// over `sizes`; the image is laid at its width over its density.
    #[test]
    fn w_candidates_choose_by_sizes() {
        let swing = |sizes: &str| {
            format!(
                "<img sizes='{sizes}' srcset='swing-200.jpg 200w, swing-400.jpg 400w, \
                 swing-800.jpg 800w, swing-1600.jpg 1600w' src=swing-400.jpg>"
            )
        };
        let full = swing("100vw");
        // width, dppx, chosen, density, width laid in 1/64 px.
        for (width, dppx, want, density, laid) in [
            (320.0, 1.0, "swing-400.jpg", 1.25, 320 * 64),
            (320.0, 2.0, "swing-800.jpg", 2.5, 320 * 64),
            (320.0, 3.0, "swing-1600.jpg", 5.0, 320 * 64),
            (1280.0, 1.0, "swing-1600.jpg", 1.25, 1280 * 64),
            // None at least 2: the densest.
            (1280.0, 2.0, "swing-1600.jpg", 1.25, 1280 * 64),
        ] {
            let (got, got_density) = chosen(&full, &env(width, dppx)).unwrap();
            assert_eq!(
                (got.as_str(), got_density),
                (want, density),
                "{width} {dppx}"
            );
            let w: f64 = got
                .trim_start_matches("swing-")
                .trim_end_matches(".jpg")
                .parse()
                .unwrap();
            assert_eq!((w * 64.0 / got_density).round() as u32, laid);
        }
        // The spec's own sizes at 1280: calc(33vw - 100px), 322.4 px.
        let spec = swing("(max-width: 30em) 100vw, (max-width: 50em) 50vw, calc(33vw - 100px)");
        let (got, density) = chosen(&spec, &env(1280.0, 1.0)).unwrap();
        assert_eq!(got, "swing-400.jpg");
        assert_eq!((400.0 * 64.0 / density).round() as u32, 20634);
        let (got, _) = chosen(&spec, &env(700.0, 1.0)).unwrap();
        assert_eq!(got, "swing-400.jpg");
        // No `sizes`: 100vw. `src` is not in a set with widths.
        let bare = "<img src=s.png srcset='a.png 100w'>";
        assert_eq!(
            chosen(bare, &env(400.0, 1.0)),
            Some(("a.png".to_string(), 0.25))
        );
        // A size of 0 makes it infinitely dense.
        let (_, zero) = chosen("<img sizes=0 srcset='a.png 100w'>", &env(400.0, 1.0)).unwrap();
        assert!(zero.is_infinite());
    }

    /// Candidates `fetchable` refuses count as absent, `src` among them
    /// once a `srcset` is read; with no `srcset`, `src` is not asked.
    #[test]
    fn unfetchable_candidates_are_left_out() {
        let mixed = tree("<img src=c.png srcset='a.svg 1x, b.png 2x'>");
        let img = first_img(&mixed);
        let png = |url: &str| !url.ends_with(".svg");
        let pick = |dppx| {
            select(&mixed, img, &env(800.0, dppx), &memory(), png)
                .and_then(|chosen| mixed.candidate(chosen))
        };
        assert_eq!(pick(1.0), Some("c.png"));
        assert_eq!(pick(2.0), Some("b.png"));
        let svg = tree("<img src=c.svg srcset='a.svg 1x, b.svg 2x'>");
        let img = first_img(&svg);
        assert_eq!(select(&svg, img, &env(800.0, 1.0), &memory(), png), None);
        let bare = tree("<img src=c.svg>");
        let img = first_img(&bare);
        assert_eq!(
            select(&bare, img, &env(800.0, 1.0), &memory(), png),
            Some(Chosen::src(img))
        );
    }

    /// Past MAX_SRCSET_BYTES a `srcset` is absent; past MAX_CANDIDATES
    /// the rest are not read; a hostile or refused `sizes` is 100vw,
    /// its charge returned.
    #[test]
    fn bounds() {
        let at = env(1000.0, 1.0);
        let pad = " ".repeat(MAX_SRCSET_BYTES - "b.png 2x".len());
        let fits = format!("<img src=a.png srcset='{pad}b.png 2x'>");
        assert_eq!(url(&fits, &env(1000.0, 2.0)).as_deref(), Some("b.png"));
        let long = format!("<img src=a.png srcset=' {pad}b.png 2x'>");
        assert_eq!(url(&long, &env(1000.0, 2.0)).as_deref(), Some("a.png"));
        // Nor is an `img` with only one a picture.
        let only = format!("<img srcset=' {pad}b.png 2x'><img srcset='{pad}b.png 2x'>");
        assert_eq!(tree(&only).pictures().len(), 1);
        let many = |n: usize| {
            let mut srcset = "a.png 9x".to_string();
            for at in 1..n {
                srcset.push_str(&format!(", c{at}.png {}x", 10 + at));
            }
            format!("<img srcset='{srcset}, best.png 5x'>")
        };
        assert_eq!(
            url(&many(MAX_CANDIDATES - 1), &env(1000.0, 5.0)).as_deref(),
            Some("best.png")
        );
        assert_ne!(
            url(&many(MAX_CANDIDATES), &env(1000.0, 5.0)).as_deref(),
            Some("best.png")
        );
        // Sizes refused, as past td-css's bound or nesting, or for room.
        let srcset = "srcset='a.png 500w, b.png 1000w, c.png 2000w'";
        let long_sizes = format!("{}10px", " ".repeat(td_css::conditions::MAX_SIZES_BYTES));
        let deep = format!("{}x{} 10px", "(".repeat(40), ")".repeat(40));
        for sizes in [long_sizes.as_str(), deep.as_str(), "calc(1px +)", "-5px"] {
            let markup = format!("<img sizes='{sizes}' {srcset}>");
            assert_eq!(url(&markup, &at).as_deref(), Some("b.png"), "{sizes:.20}");
        }
        let small = format!("<img sizes='10px' {srcset}>");
        assert_eq!(url(&small, &at).as_deref(), Some("a.png"));
        let tree = tree(&small);
        let img = first_img(&tree);
        let tiny = Budget::new(Budget::metadata_bytes() + 16).unwrap();
        let chosen = select(&tree, img, &at, &tiny, |_| true).unwrap();
        assert_eq!(tree.candidate(chosen), Some("b.png"));
        let ample = memory();
        let before = ample.used();
        let chosen = select(&tree, img, &at, &ample, |_| true).unwrap();
        assert_eq!(tree.candidate(chosen), Some("a.png"));
        assert_eq!(ample.used(), before);
    }

    /// An `img` is a picture with a `src` or a `srcset`; `src` is its
    /// `src` alone, whatever its `srcset`.
    #[test]
    fn a_srcset_makes_a_picture_and_src_stays_src() {
        // A `srcset` of commas alone is a picture, which shows none.
        let tree = tree(
            "<img srcset='a.png 2x'><img src=' '><img srcset=' , '><img srcset=' \t'>\
             <img src=b.png srcset='c.png 2x'>",
        );
        let pictures = tree.pictures();
        assert_eq!(pictures.len(), 3);
        assert_eq!(tree.src(pictures[0]), None);
        assert_eq!(tree.srcset(pictures[0]), Some("a.png 2x"));
        assert_eq!(tree.src(pictures[2]), Some("b.png"));
    }
}
