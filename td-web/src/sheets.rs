//! A page's sheets (DESIGN.md §10, "Sources"): td-css's user-agent sheet,
//! td-web's own, then the page's `style` elements in tree order with
//! their media, compiled into one set for an environment.

use crate::tree::Tree;
use td_alloc::Budget;
use td_css::cascade::{MAX_SHEETS, MAX_STYLE_BYTES};
use td_css::conditions::{ColorScheme, Environment, MediaType};
use td_css::{Origin, Sheet, StyleSet, HTML_UA_CSS};

/// td-web's own user-agent sheet.
pub const PAGE_CSS: &str = include_str!("page.css");
/// The user-agent sheets, in cascade order.
const USER_AGENT: &[&str] = &[HTML_UA_CSS, PAGE_CSS];

/// What a compilation left out of the page's own sheets: past td-css's
/// count and byte bounds, in tree order, and refused alone by a bound of
/// one sheet's.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Omitted {
    pub left_out: usize,
    pub refused: usize,
}

impl Omitted {
    pub fn count(self) -> usize {
        self.left_out.saturating_add(self.refused)
    }
}

/// The environment media queries are answered for: screen, a viewport
/// `width` by `height` CSS px, `scale` device pixels to one, light, and
/// reduced motion. Kept valid: positive and finite.
pub fn environment(width: f64, height: f64, scale: u32) -> Environment {
    let valid = |n: f64| if n.is_finite() && n >= 1.0 { n } else { 1.0 };
    Environment {
        media_type: MediaType::Screen,
        width: valid(width),
        height: valid(height),
        resolution_dppx: f64::from(scale.max(1)),
        color_scheme: ColorScheme::Light,
        reduced_motion: true,
    }
}

/// The bytes td-css counts a sheet as: its text and each media list, a
/// list at least one.
fn bytes(text: &str, media: Option<&str>) -> usize {
    text.len()
        .saturating_add(media.map_or(0, |list| list.len().max(1)))
}

/// The user-agent sheets alone, or with the page's own when `own` is set,
/// compiled for `environment` under `memory`. The page's sheets past the
/// set's count or bytes are left out first, in tree order.
pub fn compile(
    tree: &Tree,
    own: bool,
    environment: &Environment,
    memory: &Budget,
) -> Result<(StyleSet, Omitted), td_css::Error> {
    let mut omitted = Omitted::default();
    let given = if own { tree.sheets() } else { &[] };
    let mut media =
        td_alloc::BudgetVec::with_capacity(memory, given.len()).map_err(td_css::Error::Memory)?;
    let mut texts =
        td_alloc::BudgetVec::with_capacity(memory, given.len()).map_err(td_css::Error::Memory)?;
    let mut count = USER_AGENT.len();
    let mut total = USER_AGENT
        .iter()
        .fold(0usize, |sum, sheet| sum.saturating_add(sheet.len()));
    for &id in given {
        let Some((text, list)) = tree.sheet(id) else {
            continue;
        };
        let own = bytes(text, list);
        // A sheet past td-css's own bound is refused alone, not counted.
        let counted = if own > td_css::MAX_INPUT_BYTES {
            0
        } else {
            own
        };
        if count >= MAX_SHEETS || total.saturating_add(counted) > MAX_STYLE_BYTES {
            omitted.left_out += 1;
            continue;
        }
        count += 1;
        total = total.saturating_add(counted);
        texts.try_push(text).map_err(td_css::Error::Memory)?;
        media.try_push(list).map_err(td_css::Error::Memory)?;
    }
    let mut sheets = td_alloc::BudgetVec::with_capacity(memory, USER_AGENT.len() + texts.len())
        .map_err(td_css::Error::Memory)?;
    for source in USER_AGENT {
        sheets
            .try_push(Sheet::new(Origin::UserAgent, source))
            .map_err(td_css::Error::Memory)?;
    }
    for (text, list) in texts.iter().zip(media.iter()) {
        let sheet = Sheet::new(Origin::Author, text);
        let sheet = match list {
            Some(list) => sheet.with_media(std::slice::from_ref(list)),
            None => sheet,
        };
        sheets.try_push(sheet).map_err(td_css::Error::Memory)?;
    }
    let (set, refused) = StyleSet::from_sheets(&sheets, environment, memory)?;
    omitted.refused = refused.len();
    Ok((set, omitted))
}

/// Whether a property named in `text` sets a colour: `color`,
/// `background` or `border` in any case.
fn names_colour(text: &str) -> bool {
    let bytes = text.as_bytes();
    ["color", "background", "border"].iter().any(|word| {
        bytes
            .windows(word.len())
            .any(|window| window.eq_ignore_ascii_case(word.as_bytes()))
    })
}

/// Whether the page's own sheets or `style` attributes set any colour,
/// by a scan for the properties that can (DESIGN.md §10, "Colours").
pub fn coloured(tree: &Tree) -> bool {
    let sheets = tree
        .sheets()
        .iter()
        .filter_map(|&id| tree.sheet(id))
        .any(|(text, _)| names_colour(text));
    sheets
        || tree.document().nodes().any(|(_, node)| {
            node.element()
                .and_then(|element| element.attribute("style"))
                .is_some_and(names_colour)
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::tree::Source;

    fn parsed(markup: &str) -> (Tree, Budget) {
        let page = Budget::new(64 << 20).unwrap();
        (Tree::parse(markup, Source::Markup, &page).unwrap(), page)
    }

    #[test]
    fn sheets_past_the_bounds_are_left_out_in_tree_order() {
        let mut markup = String::new();
        for n in 0..70 {
            markup.push_str(&format!("<style>.c{n}{{color:red}}</style>"));
        }
        let (tree, page) = parsed(&markup);
        let environment = environment(800.0, 600.0, 1);
        let (_, omitted) = compile(&tree, true, &environment, &page).unwrap();
        assert_eq!(omitted.left_out, 70 - (MAX_SHEETS - 2));
        // One sheet past td-css's own bound is refused alone; the rest of
        // the bytes still count against the set.
        let big = "a{}".repeat(td_css::MAX_INPUT_BYTES / 3 + 1);
        let markup = format!("<style>{big}</style><style>b{{color:red}}</style>");
        let (tree, page) = parsed(&markup);
        let (_, omitted) = compile(&tree, true, &environment, &page).unwrap();
        assert_eq!(
            omitted,
            Omitted {
                left_out: 0,
                refused: 1
            }
        );
        let (_, omitted) = compile(&tree, false, &environment, &page).unwrap();
        assert_eq!(omitted, Omitted::default(), "the user-agent sheets alone");
    }

    #[test]
    fn colour_is_found_in_sheets_and_style_attributes() {
        assert!(!coloured(
            &parsed("<style>p{margin:0}</style><p style=width:1px>").0
        ));
        assert!(coloured(&parsed("<style>p{COLOR:red}</style>").0));
        assert!(coloured(&parsed("<p style='background:#fff'>").0));
        assert!(coloured(&parsed("<p style='border:1px solid'>").0));
    }

    #[test]
    fn the_environment_is_kept_valid() {
        let environment = environment(f64::NAN, -3.0, 0);
        assert_eq!(environment.width, 1.0);
        assert_eq!(environment.height, 1.0);
        assert_eq!(environment.resolution_dppx, 1.0);
        assert!(environment.reduced_motion);
    }
}
