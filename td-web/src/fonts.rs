//! A computed style's font as td-ui draws it (DESIGN.md §10, "Faces"),
//! and td-layout's metrics over td-ui's prose `Metrics`: the render
//! thread lays a page out with one, the window thread replays advances
//! with its own over the same faces at the same scale, so the two agree.

use td_css::values::{ComputedValue, Family, Property};
use td_css::ComputedStyle;
use td_layout::inline::{FontLine, FontMetrics, UNIT};
use td_ui::atlas::Style;
use td_ui::prose::{Metrics, Reach, Size};
use td_ui::raster::{GlyphStyle, Scale, TextFamily};

/// Distinct styles a measure remembers the font of: a style is the
/// pool's, so its address names it for the length of one layout.
const REMEMBERED: usize = 16;

/// What a computed style draws its text with: a prose family, a style
/// and a size in device pixels per em.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Font {
    pub family: TextFamily,
    pub style: Style,
    pub size: Size,
}

impl Font {
    /// A glyph style drawing `ink` on `ground` in this font.
    pub fn glyph(self, ink: u32, ground: u32) -> GlyphStyle {
        let base = GlyphStyle::medium(ink, ground)
            .with_family(self.family)
            .with_size(self.size);
        GlyphStyle {
            font_style: self.style,
            ..base
        }
    }
}

/// The prose family a computed family list names by its first entry
/// td-web knows, else `body` (Sans, or Serif after `F8`).
pub fn family(style: &ComputedStyle, body: TextFamily) -> TextFamily {
    let Some(ComputedValue::FontFamily(list)) = style.get(Property::FontFamily) else {
        return body;
    };
    for entry in list.iter() {
        let known = match entry {
            Family::Generic(name) => generic(name),
            Family::Named(name) => named(name.as_str()),
        };
        if let Some(family) = known {
            return family;
        }
    }
    body
}

fn generic(name: &str) -> Option<TextFamily> {
    match name {
        "monospace" | "ui-monospace" => Some(TextFamily::Mono),
        "serif" | "ui-serif" => Some(TextFamily::Serif),
        "sans-serif" | "system-ui" | "ui-sans-serif" => Some(TextFamily::Sans),
        _ => None,
    }
}

fn named(name: &str) -> Option<TextFamily> {
    if name.eq_ignore_ascii_case("noto sans") {
        Some(TextFamily::Sans)
    } else if name.eq_ignore_ascii_case("noto serif") {
        Some(TextFamily::Serif)
    } else {
        generic(name)
    }
}

/// `style`'s font at `scale`, its body text in `body`: bold from a weight
/// of 600, italic for italic and oblique, and its computed pixels times
/// the scale, rounded to a device pixel within td-ui's prose bound.
pub fn font(style: &ComputedStyle, body: TextFamily, scale: Scale) -> Font {
    let bold =
        matches!(style.get(Property::FontWeight), Some(ComputedValue::Weight(w)) if *w >= 600);
    let italic = matches!(
        style.get(Property::FontStyle),
        Some(ComputedValue::Keyword("italic" | "oblique"))
    );
    let style_of = match (bold, italic) {
        (false, false) => Style::Regular,
        (true, false) => Style::Bold,
        (false, true) => Style::Italic,
        (true, true) => Style::BoldItalic,
    };
    Font {
        family: family(style, body),
        style: style_of,
        size: size(style.font_size(), scale),
    }
}

/// CSS pixels times the scale, rounded, clamped to td-ui's prose bound.
pub fn size(pixels: f64, scale: Scale) -> Size {
    let device = pixels * scale.value() as f64;
    let rounded = if device.is_finite() && device > 0.0 {
        device.round().min(u32::MAX as f64) as u32
    } else {
        0
    };
    Size::clamped(rounded)
}

/// td-layout's metrics over td-ui's: advances and line boxes in 1/64 CSS
/// px, td-ui's device `UNIT`s divided by the scale.
pub struct Measure {
    metrics: Metrics,
    body: TextFamily,
    /// Recently asked styles by address, and their fonts. Cleared by
    /// `forget` before each layout, which names other styles.
    remembered: [(usize, Option<Font>); REMEMBERED],
}

impl Measure {
    pub fn new(metrics: Metrics, body: TextFamily) -> Self {
        Self {
            metrics,
            body,
            remembered: [(0, None); REMEMBERED],
        }
    }

    pub fn scale(&self) -> Scale {
        self.metrics.scale()
    }

    pub fn body(&self) -> TextFamily {
        self.body
    }

    pub fn metrics(&mut self) -> &mut Metrics {
        &mut self.metrics
    }

    /// Forgets the styles remembered: a styled document dropped can leave
    /// another style at a remembered address.
    pub fn forget(&mut self) {
        self.remembered = [(0, None); REMEMBERED];
    }

    /// `style`'s font, remembered by its address.
    pub fn font(&mut self, style: &ComputedStyle) -> Font {
        let address = std::ptr::from_ref(style) as usize;
        let slot = (address >> 4) % REMEMBERED;
        if let Some((at, Some(font))) = self.remembered.get(slot) {
            if *at == address {
                return *font;
            }
        }
        let found = font(style, self.body, self.metrics.scale());
        if let Some(entry) = self.remembered.get_mut(slot) {
            *entry = (address, Some(found));
        }
        found
    }

    /// A scalar's advance in `font`, in 1/64 CSS px.
    pub fn advance_in(&mut self, font: Font, scalar: char) -> u32 {
        let device = self
            .metrics
            .advance_at(font.size, font.family, font.style, scalar);
        css(device, self.metrics.scale())
    }

    /// How far ink of `font` reaches past its line box, in device pixels.
    pub fn reach(&self, font: Font) -> Reach {
        self.metrics.reach_at(font.size)
    }
}

/// Device units of 1/64 px at `scale` as 1/64 CSS px, rounded.
pub fn css(device: u32, scale: Scale) -> u32 {
    let scale = u32::try_from(scale.value()).unwrap_or(1).max(1);
    device.saturating_add(scale / 2) / scale
}

impl FontMetrics for Measure {
    fn advance(&mut self, style: &ComputedStyle, scalar: char) -> u32 {
        let font = self.font(style);
        self.advance_in(font, scalar)
    }

    fn line(&mut self, style: &ComputedStyle) -> FontLine {
        let font = self.font(style);
        let line = self.metrics.line_at(font.size);
        let scale = self.metrics.scale();
        FontLine {
            height: css(line.height.saturating_mul(UNIT), scale),
            baseline: css(line.baseline.saturating_mul(UNIT), scale),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use td_css::selectors::Element;
    use td_css::{Origin, StyleSet};

    #[derive(Clone, Copy)]
    struct P;
    impl Element for P {
        fn tag_name(&self) -> &str {
            "p"
        }
        fn attribute(&self, _: &str) -> Option<&str> {
            None
        }
        fn parent(&self) -> Option<Self> {
            None
        }
        fn previous_sibling(&self) -> Option<Self> {
            None
        }
        fn next_sibling(&self) -> Option<Self> {
            None
        }
        fn first_child(&self) -> Option<Self> {
            None
        }
        fn has_text(&self) -> bool {
            true
        }
        fn is_link(&self) -> bool {
            false
        }
    }

    fn styled(css: &str) -> ComputedStyle {
        StyleSet::new(&[(Origin::Author, css)])
            .unwrap()
            .compute(&P, None, 16.0, "")
            .unwrap()
    }

    #[test]
    fn a_family_list_names_the_first_family_td_web_knows() {
        let body = TextFamily::Serif;
        for (css, want) in [
            ("p{font-family:Georgia, serif}", TextFamily::Serif),
            ("p{font-family:'Fira Code', monospace}", TextFamily::Mono),
            ("p{font-family:ui-monospace}", TextFamily::Mono),
            ("p{font-family:system-ui, serif}", TextFamily::Sans),
            ("p{font-family:'Noto Sans'}", TextFamily::Sans),
            (
                "p{font-family:\"noto serif\", sans-serif}",
                TextFamily::Serif,
            ),
            ("p{font-family:\"td-body\"}", body),
            ("p{font-family:Helvetica, Arial}", body),
        ] {
            assert_eq!(family(&styled(css), body), want, "{css}");
        }
    }

    #[test]
    fn weight_style_and_size_choose_the_font() {
        let scale = Scale::new(2).unwrap();
        let font = font(
            &styled("p{font-weight:700;font-style:oblique;font-size:20.3px}"),
            TextFamily::Sans,
            scale,
        );
        assert_eq!(font.style, Style::BoldItalic);
        assert_eq!(font.size, Size::new(41).unwrap());
        let light = super::font(
            &styled("p{font-weight:599;font-size:1px}"),
            TextFamily::Sans,
            scale,
        );
        assert_eq!(light.style, Style::Regular);
        assert_eq!(light.size, Size::new(6).unwrap(), "clamped");
        assert_eq!(size(1000.0, scale), Size::new(192).unwrap());
        assert_eq!(size(f64::NAN, scale), Size::new(6).unwrap());
    }

    #[test]
    fn on_the_cell_every_scalar_is_a_cell_in_css_pixels_at_every_scale() {
        for scale in 1..=4 {
            let scale = Scale::new(scale).unwrap();
            let mut measure = Measure::new(Metrics::new(None, scale), TextFamily::Sans);
            let style = styled("p{font-size:40px}");
            assert_eq!(measure.advance(&style, 'x'), 8 * UNIT);
            assert_eq!(
                measure.line(&style),
                FontLine {
                    height: 16 * UNIT,
                    baseline: 14 * UNIT
                }
            );
        }
    }
}
